//! Lease-based master/backup election for one media session.
//!
//! This is deliberately not Raft and not STP. A voice session needs a
//! coordinator, a standby that already holds the session state, and failover in
//! roughly two to three seconds. Terms fence an old master, the lease bounds
//! how long a silent master is believed, and the priority tuple makes the
//! winner deterministic.

use std::cmp::Ordering;
use std::collections::HashMap;

use ovvc_core::{AuthorId, GuildId, HubPolicy};
use serde::{Deserialize, Serialize};

use crate::frame::{Role, SessionId};

/// How suitable this member is to coordinate the call and act as its media
/// hub. Larger is better for every field except the latencies, loss, CPU load,
/// active calls, cooldown, unreachable members and member count, where lower
/// is better.
///
/// The fields are advertised in the signed `Claim` and `Heartbeat` frames. A
/// peer cannot verify a capacity claim, but it can reject a candidate whose
/// claim is internally impossible (for example a hub that cannot reach a
/// member), and a lie only wins an election that the liar then has to serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Priority {
    /// Operational trust, not a cryptographic permission.
    pub trust_tier: u8,
    /// Direct reachability score, higher is better. Zero means unreachable.
    pub reachability: u8,
    /// Measured round-trip time in milliseconds.
    pub rtt_ms: u32,
    /// Available upload capacity.
    pub upload_kbps: u32,
    /// Battery percentage; mains-powered peers report 100.
    pub battery_pct: u8,
    /// Time this daemon has been up.
    pub uptime_secs: u64,
    /// Available download capacity. A hub receives every leaf's uplink, so
    /// download headroom matters as much as upload.
    pub download_kbps: u32,
    /// Current CPU load, 0 to 100.
    pub cpu_load_pct: u8,
    /// Current memory use, 0 to 100. A hub that mixes per listener holds one
    /// decoder and one encoder per call member, so a loaded machine is a poor
    /// hub even when its CPU sample looks acceptable.
    pub mem_used_pct: u8,
    /// Media or transport failures this daemon observed in the last sampling
    /// window. A hub that keeps failing to decode, encrypt or send cannot
    /// serve the call even if its static capacity is high.
    pub recent_failures: u8,
    /// Logical cores available to the encoder and mixer.
    pub cores: u8,
    /// Hardware encode / decode support for the video path.
    pub hardware_encode: bool,
    pub hardware_decode: bool,
    /// Calls this peer is already hosting or coordinating. A peer that is
    /// already loaded should not win another hub role.
    pub active_calls: u8,
    /// Remaining cooldown after recently serving as a hub, in milliseconds.
    pub hub_cooldown_ms: u32,
    /// Worst round-trip time to any call member, not only this peer.
    pub worst_rtt_ms: u32,
    /// Packet loss percentage to the call members.
    pub loss_pct: u8,
    /// Call members this peer cannot reach. A non-zero value makes a
    /// candidate ineligible, because a hub some leaves cannot reach cannot
    /// serve the call.
    pub unreachable_members: u8,
    /// How many members this peer measured. Advertised so another peer can
    /// see how stale the reachability numbers are.
    pub member_count: u8,
}

impl Priority {
    /// Conservative advertised defaults for a local daemon.
    ///
    /// These are conservative defaults for a caller with no sampler. The
    /// daemon replaces each field with a real sample before advertising it;
    /// the values here stay deliberately eligible so a call whose every member
    /// advertised zero capacity cannot elect no hub at all.
    pub const fn local() -> Self {
        Self {
            trust_tier: 1,
            reachability: 2,
            rtt_ms: 20,
            upload_kbps: 2_000,
            battery_pct: 100,
            uptime_secs: 0,
            download_kbps: 8_000,
            cpu_load_pct: 20,
            mem_used_pct: 50,
            recent_failures: 0,
            cores: 4,
            hardware_encode: false,
            hardware_decode: false,
            active_calls: 0,
            hub_cooldown_ms: 0,
            worst_rtt_ms: 25,
            loss_pct: 0,
            unreachable_members: 0,
            member_count: 1,
        }
    }

    /// The defaults above with the current call size recorded.
    pub const fn local_for_call(members: usize) -> Self {
        let members = if members > u8::MAX as usize {
            u8::MAX
        } else {
            members as u8
        };
        let mut priority = Self::local();
        priority.member_count = members;
        priority
    }

    /// Whether this candidate clears the eligibility floors.
    ///
    /// Eligibility comes before ranking: a peer below an upload, download,
    /// CPU, latency or loss floor is not a candidate, however high its trust
    /// tier. `unreachable_members != 0` is the hard reachability floor.
    pub fn is_eligible(&self, config: &ElectionConfig) -> bool {
        self.trust_tier >= config.min_trust_tier
            && self.reachability > 0
            && self.unreachable_members == 0
            && self.upload_kbps >= config.min_upload_kbps
            && self.download_kbps >= config.min_download_kbps
            && self.cpu_load_pct <= config.max_cpu_load_pct
            && self.active_calls <= config.max_active_calls
            && self.worst_rtt_ms.max(self.rtt_ms) <= config.max_worst_rtt_ms
            && self.loss_pct <= config.max_loss_pct
            && self.battery_pct >= config.min_battery_pct
    }

    /// Whether the live master should keep the hub role.
    ///
    /// These thresholds are tighter than the eligibility floors: a hub that
    /// is merely eligible can still be having a bad minute, and only a
    /// sustained breach triggers a new election.
    pub fn is_healthy(&self, config: &ElectionConfig) -> bool {
        self.worst_rtt_ms.max(self.rtt_ms) <= config.health_rtt_ms
            && self.loss_pct <= config.health_loss_pct
            && self.cpu_load_pct <= config.health_cpu_pct
            && self.mem_used_pct <= config.health_mem_pct
            && self.recent_failures <= config.health_failures
    }

    /// Whether this candidate clears one of `other`'s health breaches by a
    /// margin worth moving the hub for.
    ///
    /// Two peers on the same host can differ by a few points of CPU or a
    /// millisecond of RTT purely from sampling phase. Replacing a hub for that
    /// churns every listener's media key for no gain. A breach that is one
    /// margin worse than the candidate, however, is a real reason to move the
    /// role.
    pub fn is_materially_healthier_than(&self, other: &Self, config: &ElectionConfig) -> bool {
        let cpu_breach = other.cpu_load_pct > config.health_cpu_pct
            && self.cpu_load_pct.saturating_add(HEALTH_CPU_MARGIN_PCT) <= other.cpu_load_pct;
        let mem_breach = other.mem_used_pct > config.health_mem_pct
            && self.mem_used_pct.saturating_add(HEALTH_MEM_MARGIN_PCT) <= other.mem_used_pct;
        let other_rtt = other.worst_rtt_ms.max(other.rtt_ms);
        let self_rtt = self.worst_rtt_ms.max(self.rtt_ms);
        let rtt_breach = other_rtt > config.health_rtt_ms
            && self_rtt.saturating_add(HEALTH_RTT_MARGIN_MS) <= other_rtt;
        let loss_breach = other.loss_pct > config.health_loss_pct
            && self.loss_pct.saturating_add(HEALTH_LOSS_MARGIN_PCT) <= other.loss_pct;
        let failure_breach = other.recent_failures > config.health_failures
            && self.recent_failures.saturating_add(HEALTH_FAILURE_MARGIN) <= other.recent_failures;
        cpu_breach || mem_breach || rtt_breach || loss_breach || failure_breach
    }
}

/// How far below a breached threshold the replacement must be.
const HEALTH_CPU_MARGIN_PCT: u8 = 20;
const HEALTH_MEM_MARGIN_PCT: u8 = 10;
const HEALTH_RTT_MARGIN_MS: u32 = 100;
const HEALTH_LOSS_MARGIN_PCT: u8 = 5;
const HEALTH_FAILURE_MARGIN: u8 = 2;

/// Bandwidth bucket width, in kbps.
///
/// The sampler reports what the daemon is currently sending, not a link
/// capacity probe. A star hub sends one downlink per leaf, so it always
/// measures more traffic than a leaf and would keep the hub role forever, even
/// when another member has more headroom. One-megabit buckets make ordinary
/// voice rates tie, which lets the deterministic author-id tie-break decide,
/// while a genuinely faster or slower link still ranks.
const BANDWIDTH_RANK_BUCKET_KBPS: u32 = 1_024;
/// RTT bucket width, in milliseconds.
///
/// One or two milliseconds of jitter between two peers is measurement noise,
/// not a reason to move the hub, but the same ordering field is what lets a
/// peer on a genuinely slow link rank below a close one. The eligibility and
/// health thresholds still use the raw value.
const RTT_RANK_BUCKET_MS: u32 = 25;
/// Loss bucket width, in percentage points. Below the health threshold a lost
/// packet is noise; a real lossy link still ranks below a clean one.
const LOSS_RANK_BUCKET_PCT: u8 = 5;

fn rank_bucket(value: u8, width: u8) -> u8 {
    value / width
}

fn rank_bucket_u32(value: u32, width: u32) -> u32 {
    value / width
}

impl Ord for Priority {
    fn cmp(&self, other: &Self) -> Ordering {
        self.trust_tier
            .cmp(&other.trust_tier)
            .then(self.reachability.cmp(&other.reachability))
            .then(other.unreachable_members.cmp(&self.unreachable_members))
            .then(
                rank_bucket_u32(other.worst_rtt_ms, RTT_RANK_BUCKET_MS)
                    .cmp(&rank_bucket_u32(self.worst_rtt_ms, RTT_RANK_BUCKET_MS)),
            )
            .then(
                rank_bucket_u32(other.rtt_ms, RTT_RANK_BUCKET_MS)
                    .cmp(&rank_bucket_u32(self.rtt_ms, RTT_RANK_BUCKET_MS)),
            )
            // CPU load, memory and recent failures are eligibility and health
            // signals, not ranking fields. Every daemon samples the host at a
            // different phase, and on a shared or loaded host the same
            // sustained load can differ by tens of points between two
            // samples. A run of four send failures in one window is likewise
            // a health event, not a reason to move a working hub. Ranking on
            // any of them makes whichever peer read a quiet window pre-empt on
            // every tick. A genuinely overloaded or failing peer is removed by
            // the eligibility and sustained-health rules instead.
            .then(self.hardware_encode.cmp(&other.hardware_encode))
            .then(self.hardware_decode.cmp(&other.hardware_decode))
            .then(
                rank_bucket_u32(self.upload_kbps, BANDWIDTH_RANK_BUCKET_KBPS).cmp(
                    &rank_bucket_u32(other.upload_kbps, BANDWIDTH_RANK_BUCKET_KBPS),
                ),
            )
            .then(
                rank_bucket_u32(self.download_kbps, BANDWIDTH_RANK_BUCKET_KBPS).cmp(
                    &rank_bucket_u32(other.download_kbps, BANDWIDTH_RANK_BUCKET_KBPS),
                ),
            )
            .then(self.cores.cmp(&other.cores))
            .then(other.active_calls.cmp(&self.active_calls))
            .then(other.hub_cooldown_ms.cmp(&self.hub_cooldown_ms))
            .then(
                rank_bucket(other.loss_pct, LOSS_RANK_BUCKET_PCT)
                    .cmp(&rank_bucket(self.loss_pct, LOSS_RANK_BUCKET_PCT)),
            )
            .then(self.battery_pct.cmp(&other.battery_pct))
        // `uptime_secs` is deliberately not a ranking field. Every daemon
        // samples it once a second at a slightly different phase, so two peers
        // started a fraction of a second apart compare unequal on every second
        // boundary and pre-empt each other forever. It is advertised for
        // diagnosis, and the deterministic author-id tie-break decides a true
        // tie.
        //
        // `member_count` is likewise not a ranking field. It says how many
        // members a candidate measured, so using it as an advantage would
        // reward whoever joined the call last and saw the fullest roster.
        // Reachability is enforced by `unreachable_members`, which is a floor.
    }
}

impl PartialOrd for Priority {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candidate {
    pub author: AuthorId,
    pub priority: Priority,
}

impl Candidate {
    /// Strictly better than `other`, with a deterministic author-id tie-break.
    /// A lower author id wins so every peer computes the same result.
    fn better_than(&self, other: &Candidate) -> bool {
        self.priority > other.priority
            || (self.priority == other.priority && self.author < other.author)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElectionConfig {
    /// How long a master's claim is believed without a heartbeat.
    pub lease_ms: u64,
    /// Expected heartbeat interval.
    pub heartbeat_ms: u64,
    /// After a change, a better candidate must wait this long before it can
    /// preempt. Without it, every RTT jitter would churn the master.
    pub preemption_dampen_ms: u64,
    /// Extra grace after the lease before a backup takes over, covering one
    /// lost heartbeat.
    pub takeover_grace_ms: u64,
    /// How long a claim stays live without a fresh heartbeat for display and
    /// roster purposes. This is the media plane's member timeout. It is not
    /// the promotion window, because a peer can be dead for most of 20 s and
    /// must not become the master or backup in that gap.
    pub claim_timeout_ms: u64,
    /// How fresh a claim must be before its owner can be promoted. This is
    /// shorter than the member timeout and covers a few missed heartbeats, so
    /// a dead backup is not promoted when the master dies inside the 20 s
    /// member window.
    pub candidate_freshness_ms: u64,
    /// Eligibility floors. A candidate below one is not ranked.
    pub min_upload_kbps: u32,
    pub min_download_kbps: u32,
    pub max_cpu_load_pct: u8,
    pub max_active_calls: u8,
    pub max_worst_rtt_ms: u32,
    pub max_loss_pct: u8,
    pub min_battery_pct: u8,
    pub min_trust_tier: u8,
    /// Health thresholds for a master already in office. A sustained breach
    /// triggers a fresh election instead of waiting for the lease.
    pub health_rtt_ms: u32,
    pub health_loss_pct: u8,
    pub health_cpu_pct: u8,
    pub health_mem_pct: u8,
    /// Recent failures a master may report and keep serving. A sustained run
    /// of decode, encrypt or send failures is a health breach even when the
    /// static capacity sample is fine.
    pub health_failures: u8,
    pub health_sustain_ms: u64,
}

impl Default for ElectionConfig {
    fn default() -> Self {
        Self {
            lease_ms: 3000,
            heartbeat_ms: 500,
            preemption_dampen_ms: 5000,
            takeover_grace_ms: 500,
            claim_timeout_ms: 20_000,
            candidate_freshness_ms: 3000,
            min_upload_kbps: 256,
            min_download_kbps: 512,
            max_cpu_load_pct: 90,
            max_active_calls: 4,
            max_worst_rtt_ms: 500,
            max_loss_pct: 20,
            min_battery_pct: 5,
            min_trust_tier: 0,
            health_rtt_ms: 250,
            health_loss_pct: 8,
            health_cpu_pct: 85,
            health_mem_pct: 92,
            health_failures: 3,
            health_sustain_ms: 3000,
        }
    }
}

impl ElectionConfig {
    /// Raise the trust floor from a commune hub policy.
    pub fn with_hub_policy(mut self, hub: &HubPolicy) -> Self {
        self.min_trust_tier = self.min_trust_tier.max(hub.min_trust_tier);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElectionChange {
    Unchanged,
    MasterChanged {
        old: Option<AuthorId>,
        new: Option<AuthorId>,
        term: u64,
    },
    BackupChanged {
        old: Option<AuthorId>,
        new: Option<AuthorId>,
    },
}

impl ElectionChange {
    pub fn is_change(&self) -> bool {
        !matches!(self, Self::Unchanged)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElectionView {
    pub term: u64,
    pub master: Option<Candidate>,
    pub backup: Option<Candidate>,
    pub lease_until_ms: u64,
    pub candidates: Vec<Candidate>,
}

/// One candidate's advertised priority and when it was last heard.
#[derive(Debug, Clone, Copy)]
struct Claim {
    priority: Priority,
    last_seen_ms: u64,
    /// A hub that was replaced for a health breach is not a candidate again
    /// until this deadline, so its next heartbeat cannot immediately take the
    /// role back and oscillate with the replacement.
    degraded_until_ms: u64,
}

impl Claim {
    fn is_live(&self, now_ms: u64, timeout_ms: u64) -> bool {
        now_ms.saturating_sub(self.last_seen_ms) <= timeout_ms
    }

    fn is_candidate(&self, now_ms: u64, timeout_ms: u64) -> bool {
        self.is_live(now_ms, timeout_ms) && now_ms >= self.degraded_until_ms
    }
}

#[derive(Debug)]
pub struct Election {
    pub guild_id: GuildId,
    pub session_id: SessionId,
    config: ElectionConfig,
    term: u64,
    master: Option<Candidate>,
    backup: Option<Candidate>,
    lease_until_ms: u64,
    last_change_ms: u64,
    /// Candidate claims with a last-seen stamp. A claim without liveness is a
    /// dead member that can become the backup after its master fails, and
    /// then be elected master on the next round, so eligibility alone is not
    /// enough.
    claims: HashMap<AuthorId, Claim>,
    /// The clock of the most recent election call, used by [`Election::view`]
    /// and candidate filtering without threading a timestamp through every
    /// helper.
    now_ms: u64,
    /// When a master's lease has expired, only a candidate heard after this
    /// instant may be promoted. This is the positive-liveness rule: a claim
    /// that was merely fresh before the master died is not proof that its
    /// owner is alive after the takeover began. Cleared when a new master is
    /// elected.
    takeover_after_ms: u64,
    /// How many claims were present when the current master was elected. A
    /// claim from a member who was not in that round is new information, so it
    /// may pre-empt without the full dampening window; the dampening is for
    /// jitter between the same candidates, not for a candidate that arrived
    /// while the first claimant was alone.
    master_elected_claims: usize,
    /// When the current master first reported an unhealthy sample. Cleared by
    /// a healthy heartbeat or a new master.
    health_breach_since: Option<u64>,
}

impl Election {
    pub fn new(guild_id: GuildId, session_id: SessionId, config: ElectionConfig) -> Self {
        Self {
            guild_id,
            session_id,
            config,
            term: 0,
            master: None,
            backup: None,
            lease_until_ms: 0,
            last_change_ms: 0,
            claims: HashMap::new(),
            now_ms: 0,
            takeover_after_ms: 0,
            master_elected_claims: 0,
            health_breach_since: None,
        }
    }

    pub fn config(&self) -> ElectionConfig {
        self.config
    }

    pub fn term(&self) -> u64 {
        self.term
    }

    pub fn master(&self) -> Option<&Candidate> {
        self.master.as_ref()
    }

    pub fn backup(&self) -> Option<&Candidate> {
        self.backup.as_ref()
    }

    pub fn role_for(&self, author: AuthorId) -> Role {
        if self.master.as_ref().is_some_and(|c| c.author == author) {
            Role::Master
        } else if self.backup.as_ref().is_some_and(|c| c.author == author) {
            Role::Backup
        } else {
            Role::Member
        }
    }

    pub fn view(&self) -> ElectionView {
        let mut candidates: Vec<_> = self
            .claims
            .iter()
            .filter(|(_, claim)| self.visible(claim, self.now_ms))
            .map(|(author, claim)| Candidate {
                author: *author,
                priority: claim.priority,
            })
            .collect();
        candidates.sort_by(|a, b| {
            let a_eligible = a.priority.is_eligible(&self.config);
            let b_eligible = b.priority.is_eligible(&self.config);
            match (a_eligible, b_eligible) {
                (true, false) => return Ordering::Less,
                (false, true) => return Ordering::Greater,
                _ => {}
            }
            if a.better_than(b) {
                Ordering::Less
            } else if b.better_than(a) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        });
        ElectionView {
            term: self.term,
            master: self.master.clone(),
            backup: self.backup.clone(),
            lease_until_ms: self.lease_until_ms,
            candidates,
        }
    }

    pub fn claim(&mut self, author: AuthorId, priority: Priority, now_ms: u64) -> ElectionChange {
        self.now_ms = now_ms;
        let degraded_until_ms = self
            .claims
            .get(&author)
            .map_or(0, |claim| claim.degraded_until_ms);
        self.claims.insert(
            author,
            Claim {
                priority,
                last_seen_ms: now_ms,
                degraded_until_ms,
            },
        );
        self.recompute(now_ms, true)
    }

    pub fn heartbeat(
        &mut self,
        author: AuthorId,
        role: Role,
        priority: Priority,
        now_ms: u64,
    ) -> ElectionChange {
        self.now_ms = now_ms;
        let degraded_until_ms = self
            .claims
            .get(&author)
            .map_or(0, |claim| claim.degraded_until_ms);
        self.claims.insert(
            author,
            Claim {
                priority,
                last_seen_ms: now_ms,
                degraded_until_ms,
            },
        );
        // Liveness, not the role word, renews a live master's lease. During
        // convergence two peers can hold different views for a moment: the
        // peer this election considers master may still call itself a member
        // after being pre-empted elsewhere. Refusing to renew its lease on that
        // basis makes the disagreement self-reinforcing, because the master
        // then falls silent in this view and a third peer is elected.
        if self.master.as_ref().is_some_and(|c| c.author == author) {
            // A bad sample does not immediately churn the master. It starts a
            // timer, and only a sustained breach replaces the hub. A healthy
            // sample clears the timer, so one noisy RTT reading is harmless.
            if priority.is_healthy(&self.config) {
                self.health_breach_since = None;
            } else {
                let since = *self.health_breach_since.get_or_insert(now_ms);
                if now_ms.saturating_sub(since) >= self.config.health_sustain_ms {
                    return self.replace_unhealthy_master(author, priority, now_ms);
                }
            }
            self.master = Some(Candidate { author, priority });
            self.lease_until_ms = now_ms.saturating_add(self.config.lease_ms);
            return ElectionChange::Unchanged;
        }
        if self.backup.as_ref().is_some_and(|c| c.author == author) && role == Role::Backup {
            self.backup = Some(Candidate { author, priority });
            // A backup that grows past the master is evidence for a new
            // election, not a permanent second place. The ordinary dampening
            // still applies, so one improved sample does not churn the hub.
            let better = self
                .master
                .as_ref()
                .is_some_and(|current| Candidate { author, priority }.better_than(current));
            if better {
                return self.recompute(now_ms, true);
            }
            return ElectionChange::Unchanged;
        }
        // A heartbeat from a peer that is not the local master can still be
        // the evidence that this peer's view is stale. If the sender ranks
        // above the local master, allow the ordinary pre-emption dampening to
        // replace it. Without this a peer that became master first can keep
        // renewing its own lease forever while every other peer elected
        // somebody else, which is a split brain rather than an election.
        let better_than_local_master = self.master.as_ref().is_some_and(|current| {
            current.author != author && Candidate { author, priority }.better_than(current)
        });
        self.recompute(now_ms, better_than_local_master)
    }

    /// Replace a master whose health breach has lasted past the sustain
    /// window. If no eligible backup or candidate exists, the master keeps the
    /// role rather than dropping the call.
    fn replace_unhealthy_master(
        &mut self,
        author: AuthorId,
        priority: Priority,
        now_ms: u64,
    ) -> ElectionChange {
        self.health_breach_since = None;
        // Replacing an unhealthy hub with an equally unhealthy peer only moves
        // the cost to another machine and rotates the media key epoch for
        // nothing. Wait for a candidate that clears the breach by a real
        // margin instead.
        let unhealthy = Candidate { author, priority };
        let Some(next) = self.best_materially_healthier_candidate(&unhealthy) else {
            self.lease_until_ms = now_ms.saturating_add(self.config.lease_ms);
            return ElectionChange::Unchanged;
        };
        // Park the unhealthy hub out of both roles for the health window. It
        // keeps its claim and its liveness, but it cannot be the replacement's
        // backup or take the role straight back on the next heartbeat.
        if let Some(claim) = self.claims.get_mut(&author) {
            claim.degraded_until_ms = now_ms
                .saturating_add(self.config.health_sustain_ms.max(self.config.preemption_dampen_ms));
        }
        self.term = self.term.saturating_add(1);
        self.last_change_ms = now_ms;
        self.master_elected_claims = self.claims.len();
        self.master = Some(next.clone());
        self.lease_until_ms = now_ms.saturating_add(self.config.lease_ms);
        self.takeover_after_ms = 0;
        self.backup = self.best_candidate_excluding_many(&[author, next.author]);
        ElectionChange::MasterChanged {
            old: Some(author),
            new: Some(next.author),
            term: self.term,
        }
    }

    /// Expire silent claims and a silent master. Called from the daemon's
    /// maintenance tick.
    pub fn tick(&mut self, now_ms: u64) -> ElectionChange {
        self.now_ms = now_ms;

        // A claim is live only while its owner is still heartbeating. Without
        // this a dead master's claim survives its own lease, becomes the
        // replacement's backup and can be elected master again later.
        let stale: Vec<AuthorId> = self
            .claims
            .iter()
            .filter(|(_, claim)| !claim.is_live(now_ms, self.config.claim_timeout_ms))
            .map(|(author, _)| *author)
            .collect();
        let stale_master = self
            .master
            .as_ref()
            .is_some_and(|candidate| stale.contains(&candidate.author));
        let stale_backup = self.backup.as_ref().is_some_and(|candidate| {
            self.claims.get(&candidate.author).is_none_or(|claim| {
                !self.positively_live(claim)
                    || !claim.is_candidate(now_ms, self.config.candidate_freshness_ms)
            })
        });
        for author in &stale {
            self.claims.remove(author);
        }
        if stale_master {
            let old = self.master.as_ref().map(|candidate| candidate.author);
            // The stale claim is gone, so recompute treats the master as
            // expired, increments the term once and picks the best live
            // candidate. A master with no live candidate becomes None.
            self.lease_until_ms = 0;
            self.last_change_ms = now_ms;
            self.health_breach_since = None;
            self.recompute(now_ms, true);
            return ElectionChange::MasterChanged {
                old,
                new: self.master.as_ref().map(|candidate| candidate.author),
                term: self.term,
            };
        }
        if stale_backup {
            let old_backup = self.backup.as_ref().map(|candidate| candidate.author);
            self.backup = None;
            if self.master.is_some() {
                let change = self.recompute(now_ms, false);
                if change.is_change() {
                    return change;
                }
            }
            if old_backup.is_some() {
                return ElectionChange::BackupChanged {
                    old: old_backup,
                    new: self.backup.as_ref().map(|candidate| candidate.author),
                };
            }
        }

        let Some(master) = self.master.clone() else {
            return ElectionChange::Unchanged;
        };
        if now_ms
            <= self
                .lease_until_ms
                .saturating_add(self.config.takeover_grace_ms)
        {
            return ElectionChange::Unchanged;
        }

        // The master is silent. Record the expiry instant as the positive
        // liveness threshold, then drop its claim before choosing a
        // replacement, so a dead master cannot become the backup or the next
        // master.
        let expiry = self
            .lease_until_ms
            .saturating_add(self.config.takeover_grace_ms);
        if self.takeover_after_ms == 0 {
            self.takeover_after_ms = expiry;
        }
        self.claims.remove(&master.author);
        // The backup already holds the session state, so it takes over by
        // incrementing the term, but only after it proves it is alive past the
        // master's expiry; otherwise the best positively live candidate does.
        let next = self
            .fresh_backup(now_ms)
            .or_else(|| self.best_candidate_excluding(master.author));
        self.term = self.term.saturating_add(1);
        self.last_change_ms = now_ms;
        self.master_elected_claims = self.claims.len();
        self.health_breach_since = None;
        match next {
            Some(candidate) => {
                self.master = Some(candidate.clone());
                self.lease_until_ms = now_ms.saturating_add(self.config.lease_ms);
                self.backup =
                    self.best_candidate_excluding_many(&[master.author, candidate.author]);
                self.takeover_after_ms = 0;
                ElectionChange::MasterChanged {
                    old: Some(master.author),
                    new: Some(candidate.author),
                    term: self.term,
                }
            }
            None => {
                self.master = None;
                self.backup = None;
                self.lease_until_ms = 0;
                ElectionChange::MasterChanged {
                    old: Some(master.author),
                    new: None,
                    term: self.term,
                }
            }
        }
    }

    /// Accept a coordinator's authoritative view. Only a strictly newer term
    /// wins; an old master cannot take the session back by repeating an old
    /// `State` frame.
    pub fn observe_state(
        &mut self,
        term: u64,
        master: Option<AuthorId>,
        backup: Option<AuthorId>,
        now_ms: u64,
    ) -> bool {
        if term <= self.term {
            return false;
        }
        self.now_ms = now_ms;
        self.term = term;
        self.master = master.map(|author| Candidate {
            author,
            priority: self
                .claims
                .get(&author)
                .map_or_else(Priority::default, |claim| claim.priority),
        });
        self.backup = backup.map(|author| Candidate {
            author,
            priority: self
                .claims
                .get(&author)
                .map_or_else(Priority::default, |claim| claim.priority),
        });
        self.lease_until_ms = now_ms.saturating_add(self.config.lease_ms);
        self.last_change_ms = now_ms;
        self.master_elected_claims = self.claims.len();
        if self.master.is_some() {
            self.takeover_after_ms = 0;
        }
        self.health_breach_since = None;
        true
    }

    pub fn remove(&mut self, author: AuthorId, now_ms: u64) -> ElectionChange {
        self.now_ms = now_ms;
        self.claims.remove(&author);
        let was_master = self.master.as_ref().is_some_and(|c| c.author == author);
        if was_master {
            self.master = None;
            self.lease_until_ms = 0;
            self.term = self.term.saturating_add(1);
            self.last_change_ms = now_ms;
            self.health_breach_since = None;
        }
        if self.backup.as_ref().is_some_and(|c| c.author == author) {
            self.backup = None;
        }
        let change = self.recompute(now_ms, was_master);
        if was_master && matches!(change, ElectionChange::Unchanged) {
            ElectionChange::MasterChanged {
                old: Some(author),
                new: self.master.as_ref().map(|c| c.author),
                term: self.term,
            }
        } else {
            change
        }
    }

    /// Whether a claim is positively live after the takeover began.
    fn positively_live(&self, claim: &Claim) -> bool {
        self.takeover_after_ms == 0 || claim.last_seen_ms > self.takeover_after_ms
    }

    /// Whether a claim may be promoted to master or backup.
    fn promotable(&self, claim: &Claim, now_ms: u64) -> bool {
        self.positively_live(claim)
            && claim.is_candidate(now_ms, self.config.candidate_freshness_ms)
            && claim.priority.is_eligible(&self.config)
    }

    /// Whether a claim belongs in the visible candidate list.
    fn visible(&self, claim: &Claim, now_ms: u64) -> bool {
        self.positively_live(claim)
            && claim.is_candidate(now_ms, self.config.candidate_freshness_ms)
    }

    /// The backup, but only while its claim is positively live and fresh
    /// enough to promote. A dead backup must not be the fast path when the
    /// master dies inside the 20 s member timeout.
    fn fresh_backup(&self, now_ms: u64) -> Option<Candidate> {
        let candidate = self.backup.as_ref()?;
        let claim = self.claims.get(&candidate.author)?;
        self.promotable(claim, now_ms).then_some(Candidate {
            author: candidate.author,
            priority: claim.priority,
        })
    }

    fn best_candidate_excluding(&self, author: AuthorId) -> Option<Candidate> {
        self.best_candidate(&[author])
    }

    fn best_candidate_excluding_many(&self, exclude: &[AuthorId]) -> Option<Candidate> {
        self.best_candidate(exclude)
    }

    fn best_candidate(&self, exclude: &[AuthorId]) -> Option<Candidate> {
        self.claims
            .iter()
            .filter(|(candidate, claim)| {
                !exclude.contains(*candidate) && self.promotable(claim, self.now_ms)
            })
            .map(|(candidate, claim)| Candidate {
                author: *candidate,
                priority: claim.priority,
            })
            .max_by(|a, b| {
                if a.better_than(b) {
                    Ordering::Greater
                } else if b.better_than(a) {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
    }

    /// The best eligible candidate whose own sample is healthy.
    ///
    /// A health breach must replace the hub with a *healthy* peer. When every
    /// candidate shares the same bad sample, for example a loaded host where
    /// CPU, memory and RTT are measured system-wide, rotating only moves the
    /// role to an equally bad peer and churns the call's key epoch. In that
    /// case the current master keeps serving until a healthy candidate exists.
    fn best_healthy_candidate(&self, exclude: &[AuthorId]) -> Option<Candidate> {
        self.claims
            .iter()
            .filter(|(candidate, claim)| {
                !exclude.contains(*candidate)
                    && self.promotable(claim, self.now_ms)
                    && claim.priority.is_healthy(&self.config)
            })
            .map(|(candidate, claim)| Candidate {
                author: *candidate,
                priority: claim.priority,
            })
            .max_by(|a, b| {
                if a.better_than(b) {
                    Ordering::Greater
                } else if b.better_than(a) {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
    }

    /// The best eligible candidate that clears `master`'s health breach by a
    /// margin, so ordinary sampling noise does not move the hub.
    fn best_materially_healthier_candidate(&self, master: &Candidate) -> Option<Candidate> {
        self.claims
            .iter()
            .filter(|(author, claim)| {
                **author != master.author
                    && self.promotable(claim, self.now_ms)
                    && claim.priority.is_healthy(&self.config)
                    && claim
                        .priority
                        .is_materially_healthier_than(&master.priority, &self.config)
            })
            .map(|(author, claim)| Candidate {
                author: *author,
                priority: claim.priority,
            })
            .max_by(|a, b| {
                if a.better_than(b) {
                    Ordering::Greater
                } else if b.better_than(a) {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
    }

    fn recompute(&mut self, now_ms: u64, allow_preempt: bool) -> ElectionChange {
        self.now_ms = now_ms;
        let old_master = self.master.as_ref().map(|c| c.author);
        let old_backup = self.backup.as_ref().map(|c| c.author);

        // Route every lease-expiry decision through the same grace window.
        // When the master is silent, remember the expiry instant and require
        // candidates to prove positive liveness after it before promotion.
        let lease_expired = now_ms
            > self
                .lease_until_ms
                .saturating_add(self.config.takeover_grace_ms);
        if lease_expired && self.master.is_some() && self.takeover_after_ms == 0 {
            self.takeover_after_ms = self
                .lease_until_ms
                .saturating_add(self.config.takeover_grace_ms);
        }
        let best = self.best_candidate(&[]);
        let best_healthy = self.best_healthy_candidate(&[]);
        let best_healthy_replacement = self
            .master
            .as_ref()
            .and_then(|master| self.best_materially_healthier_candidate(master));

        let master_eligible = self
            .master
            .as_ref()
            .is_some_and(|candidate| candidate.priority.is_eligible(&self.config));
        // Claims that arrive in the same millisecond are treated as a single
        // round, so simultaneous joins converge on the best candidate. Once a
        // master has held office for a while, a better candidate waits out the
        // dampening window before it can preempt.
        let new_claim_arrived = self.claims.len() > self.master_elected_claims;
        let dampening_passed = now_ms == self.last_change_ms
            || now_ms.saturating_sub(self.last_change_ms) >= self.config.preemption_dampen_ms
            || new_claim_arrived;

        let mut master_changed = false;
        // Choose whether to replace the current master and with whom.
        //
        // * No master: elect the best eligible candidate, healthy or not.
        // * Lease expired: the master is silent, so replace it with the best
        //   eligible candidate, or drop it when none exists.
        // * Master below an eligibility floor: replace it only with a healthy
        //   eligible candidate. When every peer reports the same bad sample,
        //   as a loaded host does for CPU and memory, rotating to another
        //   equally bad peer would churn the call and buy nothing.
        // * Healthy eligible master: ordinary dampened pre-emption.
        let (take_best, chosen) = match &self.master {
            None => (best.is_some(), best.clone()),
            Some(current) => {
                if lease_expired {
                    match &best {
                        Some(candidate) if candidate.author != current.author => {
                            (true, Some(candidate.clone()))
                        }
                        Some(_) => (false, None),
                        None => (true, None),
                    }
                } else if !master_eligible {
                    // A healthy master that fell below an eligibility floor,
                    // for example a bandwidth floor, may be replaced by any
                    // healthy eligible peer. A master that is also unhealthy
                    // must be replaced only by one that clears the breach by a
                    // margin, so sampling noise on a shared host does not
                    // churn the role.
                    let replacement = if current.priority.is_healthy(&self.config) {
                        best_healthy
                            .as_ref()
                            .filter(|candidate| candidate.author != current.author)
                    } else {
                        best_healthy_replacement.as_ref()
                    };
                    match replacement {
                        Some(candidate) => (true, Some(candidate.clone())),
                        None => (false, None),
                    }
                } else {
                    let replace = best.as_ref().is_some_and(|candidate| {
                        candidate.author != current.author
                            && allow_preempt
                            && dampening_passed
                            && candidate.better_than(current)
                    });
                    (replace, best.clone())
                }
            }
        };

        if take_best {
            match chosen {
                Some(candidate) => {
                    if old_master != Some(candidate.author) {
                        self.term = self.term.saturating_add(1);
                        self.last_change_ms = now_ms;
                        master_changed = true;
                    }
                    self.master = Some(candidate);
                    self.lease_until_ms = now_ms.saturating_add(self.config.lease_ms);
                }
                None => {
                    if self.master.is_some() {
                        self.master = None;
                        self.term = self.term.saturating_add(1);
                        self.last_change_ms = now_ms;
                        master_changed = true;
                    }
                    self.lease_until_ms = 0;
                }
            }
        }

        let master_author = self.master.as_ref().map(|c| c.author);
        if master_author.is_some() {
            // The current round is complete; later heartbeats are updates from
            // candidates already considered, so normal dampening applies. A
            // master has proved liveness, so the takeover requirement is done.
            // The backup may still be a fresh claim that has not heartbeated
            // since the expiry, but it cannot be promoted on a later master
            // failure without proving liveness against that failure's expiry.
            self.master_elected_claims = self.claims.len();
            self.takeover_after_ms = 0;
        }
        self.backup = master_author.and_then(|master| self.best_candidate_excluding(master));

        if master_changed {
            self.health_breach_since = None;
            return ElectionChange::MasterChanged {
                old: old_master,
                new: master_author,
                term: self.term,
            };
        }
        let new_backup = self.backup.as_ref().map(|c| c.author);
        if new_backup != old_backup {
            return ElectionChange::BackupChanged {
                old: old_backup,
                new: new_backup,
            };
        }
        ElectionChange::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::GuildId;

    fn election() -> Election {
        let guild = GuildId([1; 32]);
        Election::new(
            guild,
            SessionId::for_guild(guild),
            ElectionConfig {
                lease_ms: 1000,
                heartbeat_ms: 200,
                preemption_dampen_ms: 2000,
                takeover_grace_ms: 100,
                ..ElectionConfig::default()
            },
        )
    }

    fn priority(trust: u8) -> Priority {
        Priority {
            trust_tier: trust,
            ..Priority::local()
        }
    }

    #[test]
    fn highest_priority_becomes_master_and_runner_up_is_backup() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        let carol = AuthorId([3; 32]);
        election.claim(alice, priority(1), 0);
        election.claim(bob, priority(5), 0);
        election.claim(carol, priority(3), 0);
        assert_eq!(election.master().unwrap().author, bob);
        assert_eq!(election.backup().unwrap().author, carol);
        assert_eq!(election.role_for(carol), Role::Backup);
    }

    #[test]
    fn equal_priority_ties_break_on_author_id() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(bob, priority(1), 0);
        election.claim(alice, priority(1), 0);
        assert_eq!(election.master().unwrap().author, alice);
    }

    #[test]
    fn preemption_is_dampened_while_lease_is_live() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        // Both claims arrive in the opening round, so the master is not a
        // solo claimant that a later arrival may replace immediately.
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        // Bob's capacity improves, but alice's lease is live and the
        // dampening window has not passed.
        let change = election.claim(bob, priority(9), 1500);
        assert!(!matches!(change, ElectionChange::MasterChanged { .. }));
        assert_eq!(election.master().unwrap().author, alice);
        // Still inside the window, and alice keeps the lease alive.
        election.heartbeat(alice, Role::Master, priority(5), 2000);
        election.claim(bob, priority(9), 2500);
        assert_eq!(election.master().unwrap().author, alice);
        // Window passed; bob preempts.
        let change = election.claim(bob, priority(9), 3500);
        assert!(matches!(change, ElectionChange::MasterChanged { .. }));
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn silent_master_hands_off_to_the_backup() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        assert_eq!(election.backup().unwrap().author, bob);
        let term = election.term();
        // Lease ends at 2000, grace at 2100. The master is removed, but the
        // backup has not proved liveness after the expiry yet, so the election
        // waits for its next heartbeat instead of promoting a possibly dead
        // claim.
        assert_eq!(election.tick(2050), ElectionChange::Unchanged);
        let change = election.tick(2101);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(alice),
                new: None,
                term: term + 1,
            }
        );
        assert!(election.master().is_none());

        // The live backup heartbeats after the expiry and takes over.
        let change = election.heartbeat(bob, Role::Backup, priority(4), 2200);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: None,
                new: Some(bob),
                term: term + 2,
            }
        );
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn heartbeat_renews_the_lease() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        election.claim(alice, priority(5), 1000);
        election.heartbeat(alice, Role::Master, priority(5), 1900);
        assert_eq!(election.tick(2500), ElectionChange::Unchanged);
        assert_eq!(election.master().unwrap().author, alice);
    }

    #[test]
    fn older_state_terms_are_fenced_out() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        assert!(election.observe_state(3, Some(alice), Some(bob), 1000));
        assert_eq!(election.master().unwrap().author, alice);
        assert!(!election.observe_state(2, Some(bob), None, 1100));
        assert_eq!(election.master().unwrap().author, alice);
        assert!(election.observe_state(4, Some(bob), None, 1200));
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn leaving_master_immediately_elects_a_replacement() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        let change = election.remove(alice, 1100);
        assert!(matches!(change, ElectionChange::MasterChanged { .. }));
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn capacity_ranks_upload_download_cpu_and_hardware() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        let base = Priority::local();
        // Bob wins on upload even though Alice has the lower author id.
        election.claim(
            alice,
            Priority {
                upload_kbps: 400,
                ..base
            },
            0,
        );
        election.claim(
            bob,
            Priority {
                upload_kbps: 4_000,
                ..base
            },
            0,
        );
        assert_eq!(election.master().unwrap().author, bob);

        let carol = AuthorId([3; 32]);
        election.claim(
            carol,
            Priority {
                upload_kbps: 4_000,
                download_kbps: 12_000,
                ..base
            },
            0,
        );
        assert_eq!(
            election.master().unwrap().author,
            carol,
            "download headroom breaks the upload tie"
        );

        let dave = AuthorId([4; 32]);
        election.claim(
            dave,
            Priority {
                upload_kbps: 4_000,
                download_kbps: 12_000,
                cpu_load_pct: 5,
                hardware_encode: true,
                hardware_decode: true,
                ..base
            },
            0,
        );
        assert_eq!(election.master().unwrap().author, dave);
    }

    #[test]
    fn an_ineligible_candidate_is_never_ranked() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        // Alice has the highest trust tier but cannot upload enough.
        election.claim(
            alice,
            Priority {
                trust_tier: 9,
                upload_kbps: 10,
                ..Priority::local()
            },
            0,
        );
        election.claim(bob, Priority::local(), 0);
        assert_eq!(election.master().unwrap().author, bob);
        assert_eq!(
            election.view().candidates.first().unwrap().author,
            bob,
            "eligible candidates sort before ineligible ones"
        );

        // A hub that cannot reach a member is ineligible even with capacity.
        let carol = AuthorId([3; 32]);
        election.claim(
            carol,
            Priority {
                trust_tier: 9,
                upload_kbps: 9_000,
                unreachable_members: 1,
                ..Priority::local()
            },
            0,
        );
        assert_ne!(election.master().unwrap().author, carol);
    }

    #[test]
    fn call_load_and_cooldown_break_a_capacity_tie() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(
            alice,
            Priority {
                active_calls: 2,
                ..Priority::local()
            },
            0,
        );
        election.claim(bob, Priority::local(), 0);
        assert_eq!(election.master().unwrap().author, bob);

        let carol = AuthorId([3; 32]);
        election.claim(
            carol,
            Priority {
                hub_cooldown_ms: 30_000,
                ..Priority::local()
            },
            0,
        );
        assert_ne!(
            election.master().unwrap().author,
            carol,
            "a peer cooling down after a hub term ranks below a fresh peer"
        );
    }

    #[test]
    fn a_sustained_health_breach_replaces_the_master() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        let term = election.term();

        let bad = Priority {
            cpu_load_pct: 99,
            ..priority(5)
        };
        // The first breach starts the timer but does not churn the master.
        assert_eq!(
            election.heartbeat(alice, Role::Master, bad, 1500),
            ElectionChange::Unchanged
        );
        election.heartbeat(alice, Role::Master, bad, 2500);
        assert_eq!(election.master().unwrap().author, alice);
        // A healthy sample clears it, so the next breach starts over.
        election.heartbeat(alice, Role::Master, priority(5), 3000);
        assert_eq!(
            election.heartbeat(alice, Role::Master, bad, 3500),
            ElectionChange::Unchanged
        );
        // Keep the backup fresh so the replacement is the live candidate, not
        // a stale claim from the opening round.
        election.heartbeat(bob, Role::Backup, priority(4), 6400);
        // Sustain is 3000 ms: 3500 + 3000 = 6500.
        let change = election.heartbeat(alice, Role::Master, bad, 6500);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(alice),
                new: Some(bob),
                term: term + 1,
            }
        );
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn a_better_peer_heartbeat_converges_a_solo_master() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        // Bob claimed first, alone, and believes it is the coordinator. This
        // is the state that caused a split brain between three daemons.
        election.claim(bob, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, bob);

        // Alice's heartbeat is new information from a candidate that was not
        // in Bob's opening round. It converges immediately rather than waiting
        // for a dampening window that would let Bob renew its own lease
        // forever.
        let change = election.heartbeat(alice, Role::Master, priority(9), 1500);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(bob),
                new: Some(alice),
                term: election.term(),
            }
        );
        assert_eq!(election.master().unwrap().author, alice);
    }

    #[test]
    fn a_silent_master_is_removed_at_takeover_and_cannot_become_backup() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        let carol = AuthorId([3; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        election.claim(carol, priority(3), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        assert_eq!(election.backup().unwrap().author, bob);

        // Lease is 1000 ms and the grace is 100 ms. The dead master is removed
        // and no candidate has proved liveness after the expiry yet, so the
        // election waits for a heartbeat instead of promoting a possibly dead
        // claim.
        let change = election.tick(2101);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(alice),
                new: None,
                term: 2,
            }
        );
        assert!(election.master().is_none());

        // The live backup proves itself after the expiry and takes over, with
        // the third member as backup. The dead master does not return.
        let change = election.heartbeat(bob, Role::Backup, priority(4), 2200);
        assert!(matches!(
            change,
            ElectionChange::MasterChanged {
                old: None,
                new: Some(new),
                ..
            } if new == bob
        ));
        assert_eq!(election.master().unwrap().author, bob);
        assert_eq!(election.backup().unwrap().author, carol);
        assert!(
            !election.view().candidates.iter().any(|c| c.author == alice),
            "a dead master must not remain a candidate"
        );
        assert!(!election.claims.contains_key(&alice));
    }

    #[test]
    fn a_claim_expires_after_the_member_timeout() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);

        // The claim timeout is 20 s. Tick past it with no heartbeat.
        let change = election.tick(21_001);
        assert!(matches!(
            change,
            ElectionChange::MasterChanged {
                old: Some(old),
                new: None,
                ..
            } if old == alice
        ));
        assert!(election.master().is_none());
        assert!(election.backup().is_none());
        assert!(election.view().candidates.is_empty());
    }

    #[test]
    fn a_stale_master_hands_over_to_a_live_backup_and_reports_the_old_holder() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        // Bob keeps heartbeating; Alice goes silent. At the claim timeout
        // Alice is removed and the event still names her as the old master.
        election.heartbeat(bob, Role::Backup, priority(4), 20_500);
        let change = election.tick(21_001);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(alice),
                new: Some(bob),
                term: 2,
            }
        );
        assert_eq!(election.master().unwrap().author, bob);
        assert!(!election.claims.contains_key(&alice));
    }

    #[test]
    fn a_stale_backup_is_cleared_from_the_role() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        assert_eq!(election.backup().unwrap().author, bob);

        // Alice keeps heartbeating. Bob goes silent past the 3 s promotion
        // freshness, so he must not remain the advertised backup.
        election.heartbeat(alice, Role::Master, priority(5), 3900);
        let change = election.tick(4001);
        assert_eq!(
            change,
            ElectionChange::BackupChanged {
                old: Some(bob),
                new: None,
            }
        );
        assert_eq!(election.master().unwrap().author, alice);
        assert!(election.backup().is_none());
    }

    #[test]
    fn a_fresh_but_dead_backup_is_not_promoted_at_master_expiry() {
        // The verifier's exact timing: A last at 1000, B backup last at 1600,
        // C live last at 4000, and the master's lease plus grace expires at
        // 4500. B is still fresh by the 3 s window, but it has not been heard
        // after the expiry, so the preferred-backup path must reject it and
        // wait for the live third member.
        let guild = GuildId([1; 32]);
        let mut election = Election::new(
            guild,
            SessionId::for_guild(guild),
            ElectionConfig::default(),
        );
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        let carol = AuthorId([3; 32]);
        election.claim(alice, priority(9), 1000);
        election.claim(bob, priority(5), 1600);
        election.claim(carol, priority(4), 4000);
        assert_eq!(election.master().unwrap().author, alice);
        assert_eq!(election.backup().unwrap().author, bob);

        let change = election.tick(4501);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(alice),
                new: None,
                term: 2,
            }
        );
        assert!(
            election.master().is_none(),
            "the fresh but dead backup must not be promoted"
        );

        // Carol heartbeats after the expiry and is the live survivor.
        let change = election.heartbeat(carol, Role::Member, priority(4), 4700);
        assert!(matches!(
            change,
            ElectionChange::MasterChanged {
                old: None,
                new: Some(new),
                ..
            } if new == carol
        ));
        assert_eq!(election.master().unwrap().author, carol);
        assert!(election.backup().is_none(), "the dead backup is not promoted");
    }

    #[test]
    fn a_dead_backup_is_not_promoted_when_the_master_dies_inside_the_member_timeout() {
        let guild = GuildId([1; 32]);
        let config = ElectionConfig {
            lease_ms: 5000,
            takeover_grace_ms: 100,
            ..ElectionConfig::default()
        };
        let mut election = Election::new(guild, SessionId::for_guild(guild), config);
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        let carol = AuthorId([3; 32]);
        election.claim(alice, priority(9), 1000);
        election.claim(bob, priority(5), 1000);
        election.claim(carol, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        assert_eq!(election.backup().unwrap().author, bob);

        // Bob (backup) and Alice (master) both go silent. Carol keeps
        // heartbeating. Alice's lease is still live at 4500, so the heartbeat
        // does not change the master. Bob's claim is already past the 3 s
        // promotion freshness but still inside the 20 s member timeout.
        election.heartbeat(carol, Role::Member, priority(4), 4500);
        assert_eq!(election.master().unwrap().author, alice);

        // Alice's lease expires. Neither Bob nor Carol has proved liveness
        // after the expiry yet, so the preferred backup path is rejected and
        // the election waits rather than promoting the dead backup.
        let change = election.tick(6101);
        assert_eq!(
            change,
            ElectionChange::MasterChanged {
                old: Some(alice),
                new: None,
                term: 2,
            }
        );
        assert!(election.master().is_none());

        // Carol heartbeats after the expiry and is the only positively live
        // candidate, so she takes over and Bob is never promoted.
        let change = election.heartbeat(carol, Role::Member, priority(4), 6200);
        assert!(matches!(
            change,
            ElectionChange::MasterChanged {
                old: None,
                new: Some(new),
                ..
            } if new == carol
        ));
        assert_eq!(election.master().unwrap().author, carol);
        assert_ne!(election.master().unwrap().author, bob);
        assert!(
            !election
                .view()
                .candidates
                .iter()
                .any(|candidate| candidate.author == alice),
            "the dead master must not remain a candidate"
        );
    }

    #[test]
    fn an_unhealthy_master_is_parked_out_of_the_backup_role() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        let carol = AuthorId([3; 32]);
        election.claim(alice, priority(9), 1000);
        election.claim(bob, priority(5), 1000);
        election.claim(carol, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        assert_eq!(election.backup().unwrap().author, bob);

        // 88 is above the health threshold of 85 but below the eligibility
        // ceiling of 90, so this measures the health replacement rather than
        // a normal eligibility pre-emption.
        let bad = Priority {
            cpu_load_pct: 88,
            ..priority(9)
        };
        election.heartbeat(alice, Role::Master, bad, 1500);
        // Keep the master's lease live while the health breach accumulates,
        // and keep both replacements fresh, so the test measures the health
        // park rather than a lease expiry or the freshness window.
        election.heartbeat(alice, Role::Master, bad, 3000);
        election.heartbeat(alice, Role::Master, bad, 4000);
        election.heartbeat(bob, Role::Backup, priority(5), 4400);
        election.heartbeat(carol, Role::Member, priority(4), 4400);
        let change = election.heartbeat(alice, Role::Master, bad, 4500);
        assert!(
            matches!(change, ElectionChange::MasterChanged { .. }),
            "expected a replacement, got {change:?}"
        );
        assert_eq!(election.master().unwrap().author, bob);
        assert_eq!(
            election.backup().unwrap().author,
            carol,
            "the unhealthy old master must not be the backup"
        );

        // A healthy sample from the old master does not take the role back
        // while it is still parked.
        election.heartbeat(alice, Role::Master, priority(9), 5000);
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn a_backup_that_overtakes_the_master_pre_empts_after_dampening() {
        let mut election = election();
        let alice = AuthorId([1; 32]);
        let bob = AuthorId([2; 32]);
        election.claim(alice, priority(5), 1000);
        election.claim(bob, priority(4), 1000);
        assert_eq!(election.master().unwrap().author, alice);
        assert_eq!(election.backup().unwrap().author, bob);

        // Inside the dampening window the improved backup is recorded but the
        // master does not churn.
        let early = election.heartbeat(bob, Role::Backup, priority(9), 1500);
        assert!(!matches!(early, ElectionChange::MasterChanged { .. }));
        assert_eq!(election.master().unwrap().author, alice);

        // Past the 2 s dampening window the backup pre-empts.
        let change = election.heartbeat(bob, Role::Backup, priority(9), 3500);
        assert!(matches!(change, ElectionChange::MasterChanged { .. }));
        assert_eq!(election.master().unwrap().author, bob);
    }

    #[test]
    fn rtt_and_loss_breaches_also_trigger_re_election() {
        for bad in [
            Priority {
                worst_rtt_ms: 900,
                ..priority(5)
            },
            Priority {
                loss_pct: 40,
                ..priority(5)
            },
        ] {
            let mut election = election();
            let alice = AuthorId([1; 32]);
            let bob = AuthorId([2; 32]);
            election.claim(alice, priority(5), 1000);
            election.claim(bob, priority(4), 1000);
            election.heartbeat(alice, Role::Master, bad, 1500);
            // Keep the backup fresh so the replacement is live.
            election.heartbeat(bob, Role::Backup, priority(4), 4400);
            let change = election.heartbeat(alice, Role::Master, bad, 4500);
            assert!(
                matches!(change, ElectionChange::MasterChanged { new: Some(new), .. } if new == bob),
                "a sustained health breach must replace the hub"
            );
        }
    }
}
