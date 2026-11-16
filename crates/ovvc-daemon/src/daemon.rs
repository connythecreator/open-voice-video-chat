//! Shared daemon state, request dispatch, and client-event fanout.
//!
//! One process owns the `Client` (and therefore the redb store and every
//! libp2p node). Attached frontends only ever see this request/event surface,
//! so a CLI, TUI, GUI and web shell can run side by side against one identity
//! without fighting over the store lock.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use ovvc_audio::{
    AudioCallConfig, AudioCommand, AudioEvent, AudioEventEnvelope, AudioHandle, AudioStats,
    BackendKind, EngineConfig, HubMixer, ListenerFrame,
};
use ovvc_client::{Client, ClientError, ClientEvent, Invite, NetworkOptions};
use ovvc_control::{ControlFrame, ControlKind, Priority, SessionId, VoiceSnapshot, now_ms};
use ovvc_core::model::now_unix;
use ovvc_core::{
    AuthorId, ChannelId, ChannelKind, Diagnostic, ErrorClass, EventKind, GuildId, GuildPolicy,
    SearchHit as CoreSearchHit, has_mention_syntax,
};
use ovvc_crypto::{CryptoError, KeyTopology, MediaKeyring};
use ovvc_media::CallTopology;
use ovvc_ipc::{
    ChannelTreeDto, ConnectionStateDto, DAEMON_CAPABILITIES, DaemonCapabilities, DaemonEvent,
    DaemonStatusDto, IpcError, NetworkOptionsDto, NotificationSettings, Op, PeerSnapshot,
    PolicyView, Reply, VoiceAudioStats,
};
use ovvc_media::call::CallId;
use ovvc_rtc::{RtcEvent, RtcManager, RtcSignal, RtcStats};
use tokio::sync::{broadcast, mpsc, watch};

use crate::capacity::{CapacitySampler, FailureCounter, SampleInput, TransportSample};
use crate::control_hub::{ControlHub, HubOutcome};
use crate::daemon_log::DaemonLog;
use crate::network_state::{self, SavedNetwork};
use crate::notifications::{NotificationFilter, notification_reason};
use crate::settings;

/// How many events a slow frontend may fall behind before it sees a lag
/// notification. Events are best-effort by design: a frontend that needs an
/// exact history reloads it from storage.
const EVENT_CAPACITY: usize = 512;

/// How many RTC events one call or the shared pump may buffer.
///
/// Signalling and stats are delivered in order, while RTP is dropped by the
/// manager when this fills. A capacity well above the audio jitter buffer
/// keeps a normal burst intact without letting a flooding peer grow memory.
const RTC_EVENT_CAPACITY: usize = 256;

/// How far ahead of an aligned local keyring a received `VoiceKey` epoch may
/// be.
///
/// Epochs advance by one per roster change, so a gap bigger than this is a
/// stale or hostile frame that would otherwise force a long rotation loop. A
/// keyring that has not yet aligned with the call may adopt the first
/// authenticated epoch it sees, however far ahead, so a joiner is not locked
/// out of a long-lived call.
const MAX_KEY_EPOCH_ADVANCE: u32 = 64;

/// How many remote-driven epoch advances an aligned keyring accepts between
/// two roster rotations.
///
/// One is not enough: every member rotates its own epoch when the roster
/// changes, so after a join or a hub move the peers can be several epochs
/// apart. A member that stopped at the first higher epoch it heard would stay
/// on a key the others had already rotated past, and every downlink would fail
/// to decrypt. The budget is small enough that a member sending increasing
/// epochs cannot spin the rotation indefinitely.
const MAX_EPOCH_ADVANCES_PER_ROTATION: u8 = 8;

/// How often the current epoch key is republished while a call is live.
///
/// The control plane is best effort: a `VoiceKey` that is never delivered, or
/// a publish the hub rate-limits, leaves the peer without a key until the
/// roster changes again. A periodic republish costs one small signed frame
/// every few seconds and recovers on its own.
const KEY_REPUBLISH_INTERVAL_MS: u64 = 3_000;

/// How long a join waits for the audio service's first report.
///
/// The service publishes its first stats sample immediately after opening the
/// backend, so this is only a bound on a device that hangs. A timeout is
/// reported to the joiner instead of leaving a silent call.
const START_AUDIO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long one call must wait between automatic resync announcements.
///
/// A late joiner exposes the roster divergence on every frame it sends, so a
/// burst would otherwise produce one re-announcement and key republish per
/// frame. Each announce consumes the local author's control rate tokens, and
/// the session checks membership before its limiter, so an unthrottled burst
/// can starve the heartbeat that keeps the local member in the call. One
/// repair per second converges just as fast and cannot be used to evict the
/// local member.
const VOICE_RESYNC_COOLDOWN_MS: u64 = 1_000;

#[derive(Debug, Default)]
struct GuildConnection {
    listen: Vec<String>,
    external: Vec<String>,
    peers: Vec<String>,
}

#[derive(Debug, Default)]
struct NetworkState {
    guilds: HashMap<GuildId, GuildConnection>,
}

impl NetworkState {
    fn entry(&mut self, guild_id: GuildId) -> &mut GuildConnection {
        self.guilds.entry(guild_id).or_default()
    }

    fn remove(&mut self, guild_id: GuildId) {
        self.guilds.remove(&guild_id);
    }

    fn add_listen(&mut self, guild_id: GuildId, address: String) {
        push_unique(&mut self.entry(guild_id).listen, address);
    }

    fn add_external(&mut self, guild_id: GuildId, address: String) {
        push_unique(&mut self.entry(guild_id).external, address);
    }

    fn remove_external(&mut self, guild_id: GuildId, address: &str) {
        if let Some(connection) = self.guilds.get_mut(&guild_id) {
            connection.external.retain(|existing| existing != address);
        }
    }

    fn add_peer(&mut self, guild_id: GuildId, peer: String) {
        push_unique(&mut self.entry(guild_id).peers, peer);
    }

    fn remove_peer(&mut self, guild_id: GuildId, peer: &str) {
        if let Some(connection) = self.guilds.get_mut(&guild_id) {
            connection.peers.retain(|existing| existing != peer);
        }
    }
}

fn push_unique(list: &mut Vec<String>, value: String) {
    if !list.iter().any(|existing| existing == &value) {
        list.push(value);
    }
}

/// One running audio service and the last counters it reported.
struct AudioCall {
    handle: AudioHandle,
    /// Distinguishes this service from a replacement on the same call key, so
    /// a late `Stopped` or `Error` from the old thread cannot retire the new
    /// one.
    generation: u64,
    last_stats: Option<VoiceAudioStats>,
}

/// One running WebRTC transport and the last peer snapshot sampled from it.
struct RtcCall {
    manager: RtcManager,
    /// Matches `AudioCall::generation`'s purpose: a late event from a closed
    /// manager must not act on the transport that replaced it.
    generation: u64,
    last_stats: RtcStats,
}

/// An RTC event tagged with the call that produced it, carried to the pump.
#[derive(Debug, Clone)]
pub struct RtcEventEnvelope {
    guild_id: GuildId,
    channel_id: ChannelId,
    generation: u64,
    event: RtcEvent,
}

/// Per-call media encryption state shared by the libp2p and WebRTC paths.
struct CallCrypto {
    /// The one keyring every transport for this call uses. The RTC manager
    /// holds a clone, so a rotation here is visible to its RTP path.
    keyring: Arc<RwLock<MediaKeyring>>,
    /// The call roster the current epoch key was published for.
    roster: BTreeSet<AuthorId>,
    /// The hub this keyring's pairwise keys were selected for. A change means
    /// a takeover: the old leaf-to-hub ciphers are useless and the epoch must
    /// advance so the leaves install the new hub's keys.
    hub: Option<AuthorId>,
    /// Peer public keys already installed, so a repeated `VoiceKey` frame does
    /// not rebuild a cipher and reset its send counter and replay window.
    installed: HashMap<AuthorId, (u32, [u8; 32])>,
    /// The epoch and public key last accepted by the hub, with the time it was
    /// published. `None` means the publish failed or was lost before it could
    /// be confirmed, so the reconcile tick must publish again.
    published: Option<(u32, [u8; 32], u64)>,
    /// Whether this keyring has ever converged with the call's epoch, either
    /// by installing a peer key or by adopting a peer's epoch. A fresh keyring
    /// may jump straight to the call's epoch once; after that the advance cap
    /// applies.
    aligned: bool,
    /// How many remote `VoiceKey` frames have forced an advance since the last
    /// local roster rotation. A small budget lets a member climb to the
    /// highest epoch the call has reached after a join or takeover without
    /// letting one peer spin the rotation forever.
    advances_since_rotation: u8,
    /// Frames this daemon dropped for a missing key, wrong epoch, replay or
    /// AEAD failure, plus frames it could not encrypt for a target.
    errors: u64,
}

/// The hub mixer and downlink sequence counters for one star call in which the
/// local author is the hub.
struct HubCallState {
    /// Decodes every leaf and the hub microphone, mixes per listener and owns
    /// the hub's local playback.
    mixer: HubMixer,
    /// The next sequence number to stamp on each listener's mixed stream. Each
    /// listener is a separate downlink, so a shared counter would look like a
    /// reordered stream.
    seq: HashMap<AuthorId, u16>,
}

/// What a received `VoiceKey` epoch should do to the local keyring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpochUpdate {
    /// The epoch is stale, too far ahead of an aligned keyring, or this roster
    /// term has already been advanced once.
    Ignore,
    /// Move straight to this epoch with a fresh local secret.
    Advance(u32),
}

/// The repair a divergent call view needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VoiceResync {
    /// Re-announce the local `VoiceJoin`. The sender is a member the local
    /// call is missing and the local author is already in the call, so the
    /// sender needs to learn about us.
    Announce,
    /// Ask the call to announce itself. The sender is sending call traffic
    /// but the local roster has not caught up, so the sender holds membership
    /// information we are missing.
    Request,
}

/// A detected divergence and whether the rate-limited repair may run now.
///
/// `allowed` is false when a repair for this call already ran inside the
/// cooldown. The frame is still a divergence, so it must be swallowed rather
/// than surfaced as a rejected control frame, but it needs no second announce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VoiceResyncDue {
    channel_id: ChannelId,
    action: VoiceResync,
    allowed: bool,
}

/// Decide how a received epoch relates to the local keyring.
///
/// A fresh keyring may adopt the first authenticated epoch it sees, however
/// far ahead it is, because a joiner starts at epoch one while the call may
/// have rotated hundreds of times. Once aligned, the advance cap applies and
/// at most [`MAX_EPOCH_ADVANCES_PER_ROTATION`] remote-driven advances are
/// accepted per roster rotation. The budget is what lets every member reach
/// the highest epoch after a join or takeover; without it a member that
/// stopped at an intermediate epoch would keep keys the others had rotated
/// past and every downlink would fail to decrypt.
fn plan_epoch_update(
    current: u32,
    observed: u32,
    aligned: bool,
    advances_since_rotation: u8,
) -> EpochUpdate {
    if observed <= current {
        return EpochUpdate::Ignore;
    }
    if !aligned {
        return EpochUpdate::Advance(observed);
    }
    if advances_since_rotation >= MAX_EPOCH_ADVANCES_PER_ROTATION
        || observed - current > MAX_KEY_EPOCH_ADVANCE
    {
        return EpochUpdate::Ignore;
    }
    EpochUpdate::Advance(observed)
}

/// Whether the current epoch key still needs publishing.
///
/// A missing record means the last publish was rejected or never confirmed. A
/// changed epoch or key always needs a publish. Otherwise the key is
/// republished after [`KEY_REPUBLISH_INTERVAL_MS`], which recovers from a
/// frame the network dropped.
fn key_publication_due(
    published: Option<(u32, [u8; 32], u64)>,
    epoch: u32,
    public_key: [u8; 32],
    now: u64,
) -> bool {
    match published {
        None => true,
        Some((published_epoch, published_key, published_at)) => {
            published_epoch != epoch
                || published_key != public_key
                || now.saturating_sub(published_at) >= KEY_REPUBLISH_INTERVAL_MS
        }
    }
}

pub struct Daemon {
    pub client: Arc<Client>,
    /// The layered configuration this process started with.
    pub config: Arc<ovvc_config::Config>,
    pub data_dir: PathBuf,
    pub socket: PathBuf,
    pub started_at: u64,
    pub events: broadcast::Sender<DaemonEvent>,
    /// Audio service events, consumed by the audio pump.
    audio_events: broadcast::Sender<AudioEventEnvelope>,
    /// Audio services keyed by the call they carry.
    audio: tokio::sync::Mutex<HashMap<(GuildId, ChannelId), AudioCall>>,
    /// Next audio service generation, so events can be matched to the thread
    /// that produced them.
    audio_generation: AtomicU64,
    /// WebRTC transports keyed by the call they carry. Empty unless
    /// `media.transport` is `"webrtc"`.
    rtc: tokio::sync::Mutex<HashMap<(GuildId, ChannelId), RtcCall>>,
    /// Next WebRTC generation, matching the audio service rule above.
    rtc_generation: AtomicU64,
    /// Media keyrings keyed by the call they belong to. Voice media is always
    /// encrypted, so a call without an entry has no usable media path.
    crypto: tokio::sync::Mutex<HashMap<(GuildId, ChannelId), CallCrypto>>,
    /// Hub mixers for star calls this daemon is currently serving. Empty for a
    /// leaf, a mesh call or a direct pair.
    hub_mix: Mutex<HashMap<(GuildId, ChannelId), HubCallState>>,
    /// The measured capacity advertised in the election. Replaced every
    /// control tick, never left as the constant placeholder.
    capacity: Mutex<CapacitySampler>,
    /// Media-path failures since start. The capacity sampler turns the delta
    /// between ticks into `Priority::recent_failures`, so a hub that keeps
    /// failing to decode or send loses its role.
    media_failures: FailureCounter,
    /// Raw libp2p media bytes, for the bandwidth sample on the transport that
    /// does not expose socket counters. WebRTC uses its own RTP counters.
    media_bytes_sent: AtomicU64,
    media_bytes_received: AtomicU64,
    /// RTC events from every call's manager, drained by the RTC pump.
    rtc_events: mpsc::Sender<RtcEventEnvelope>,
    /// Taken once by the RTC pump at startup.
    rtc_event_rx: tokio::sync::Mutex<Option<mpsc::Receiver<RtcEventEnvelope>>>,
    network: Mutex<NetworkState>,
    control: tokio::sync::Mutex<ControlHub>,
    /// Event ids already raised as notifications, bounded so a resync does not
    /// notify twice and memory stays flat.
    notifications: Mutex<NotificationFilter>,
    /// Notification preferences, shared with the desktop notifier so a mute
    /// applies to both the in-app alert and the popup.
    settings: Arc<Mutex<NotificationSettings>>,
    /// Which guilds were online when the daemon last stopped, so a restart
    /// rejoins the peer network instead of silently going offline.
    saved_network: Mutex<Vec<SavedNetwork>>,
    /// Guilds a frontend took offline in this session. Resume must not bring
    /// one of these back while it is still working through its list: the click
    /// wins over the saved intent, and the intent is kept for the next start.
    session_offline: Mutex<HashSet<GuildId>>,
    /// Last automatic voice resync per call, so a burst of diverging frames
    /// produces one announcement rather than one per frame.
    voice_resync_at: Mutex<HashMap<(GuildId, ChannelId), u64>>,
    /// The bounded diagnostic log beside the store.
    log: DaemonLog,
    shutdown: watch::Sender<bool>,
}

impl Daemon {
    pub fn new(
        client: Arc<Client>,
        config: Arc<ovvc_config::Config>,
        data_dir: PathBuf,
        socket: PathBuf,
        shutdown: watch::Sender<bool>,
    ) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let (audio_events, _) = broadcast::channel(EVENT_CAPACITY);
        let (rtc_events, rtc_event_rx) = mpsc::channel(RTC_EVENT_CAPACITY);
        let control =
            ControlHub::with_media_policy(client.identity().clone(), config.policy.media.clone());
        let settings = Arc::new(Mutex::new(settings::load(&data_dir)));
        let saved_network = Mutex::new(network_state::load(&data_dir));
        let log = DaemonLog::new(&data_dir, env!("CARGO_PKG_VERSION"), std::process::id());
        Self {
            client,
            config,
            data_dir,
            socket,
            started_at: now_unix(),
            events,
            audio_events,
            audio: tokio::sync::Mutex::new(HashMap::new()),
            audio_generation: AtomicU64::new(0),
            rtc: tokio::sync::Mutex::new(HashMap::new()),
            rtc_generation: AtomicU64::new(0),
            crypto: tokio::sync::Mutex::new(HashMap::new()),
            hub_mix: Mutex::new(HashMap::new()),
            capacity: Mutex::new(CapacitySampler::new()),
            media_failures: FailureCounter::default(),
            media_bytes_sent: AtomicU64::new(0),
            media_bytes_received: AtomicU64::new(0),
            rtc_events,
            rtc_event_rx: tokio::sync::Mutex::new(Some(rtc_event_rx)),
            network: Mutex::new(NetworkState::default()),
            control: tokio::sync::Mutex::new(control),
            notifications: Mutex::new(NotificationFilter::new()),
            settings,
            saved_network,
            session_offline: Mutex::new(HashSet::new()),
            voice_resync_at: Mutex::new(HashMap::new()),
            log,
            shutdown,
        }
    }

    /// The shared settings handle, for the desktop notifier.
    pub fn settings_handle(&self) -> Arc<Mutex<NotificationSettings>> {
        Arc::clone(&self.settings)
    }

    pub fn notification_settings(&self) -> NotificationSettings {
        self.settings
            .lock()
            .expect("settings mutex poisoned")
            .clone()
    }

    /// Apply one preference change to the daemon's copy and persist it.
    ///
    /// The read, the change and the write happen under the same lock, so two
    /// frontends changing different fields cannot clobber one another with a
    /// stale whole-struct copy. If the write fails the in-memory copy is left
    /// untouched, so the daemon never reports a setting it has not saved.
    fn update_notification_settings(
        &self,
        change: impl FnOnce(&mut NotificationSettings),
    ) -> Result<(), IpcError> {
        let mut current = self.settings.lock().expect("settings mutex poisoned");
        let mut next = current.clone();
        change(&mut next);
        settings::save(&self.data_dir, &next)
            .map_err(|error| IpcError::internal(format!("save notification settings: {error}")))?;
        *current = next;
        Ok(())
    }

    pub fn emit(&self, event: DaemonEvent) {
        let _ = self.events.send(event);
    }

    /// The daemon's diagnostic log path, for status and a bug report.
    pub fn log_path(&self) -> String {
        self.log.path().display().to_string()
    }

    /// Classify, log and broadcast one condition.
    ///
    /// The class is passed by the emission site, which is the only place that
    /// knows whether a transport failure is expected or an invariant broke.
    /// Every diagnostic carries the log path, and a bug also carries a stable
    /// report hint so a frontend never has to guess.
    pub fn report(
        &self,
        class: ErrorClass,
        guild_id: Option<GuildId>,
        context: &str,
        message: impl Into<String>,
    ) {
        let diagnostic = Diagnostic::new(class, message)
            .with_guild(guild_id)
            .with_context(context);
        self.report_diagnostic(diagnostic);
    }

    /// Log and broadcast a diagnostic that the caller already built.
    pub fn report_diagnostic(&self, mut diagnostic: Diagnostic) {
        if diagnostic.log_path.is_none() {
            diagnostic.log_path = Some(self.log_path());
        }
        if diagnostic.class.is_bug() && diagnostic.report_hint.is_none() {
            diagnostic.report_hint =
                Some("unexpected internal failure; report it with this log file".to_string());
        }
        self.log.write(&diagnostic);
        self.emit(DaemonEvent::Diagnostic(diagnostic));
    }

    /// Write a diagnostic to the log without broadcasting it.
    ///
    /// Used for retry loops and connection lifecycle noise: an operator can
    /// still see the reason, but a frontend is not woken every thirty seconds
    /// for a condition it already knows about.
    pub fn log_only(
        &self,
        class: ErrorClass,
        guild_id: Option<GuildId>,
        context: &str,
        message: impl Into<String>,
    ) {
        let diagnostic = Diagnostic::new(class, message)
            .with_guild(guild_id)
            .with_context(context)
            .with_log_path(self.log_path());
        self.log.write(&diagnostic);
    }

    pub fn request_shutdown(&self) {
        let _ = self.shutdown.send(true);
    }

    // --- peer-network intent ----------------------------------------------

    /// Remember that this guild should be online, and with which addresses.
    ///
    /// Called only after the client accepted the request, so a refused listen
    /// address is never written into the next start. The lock is held across
    /// the write: two frontends connecting at once must not overwrite each
    /// other with a stale whole-file snapshot, the same rule the settings file
    /// follows.
    fn remember_online(&self, intent: SavedNetwork) {
        let mut saved = self
            .saved_network
            .lock()
            .expect("network intent mutex poisoned");
        saved.retain(|existing| existing.guild_id != intent.guild_id);
        saved.push(intent);
        persist_network_intent(&self.data_dir, &saved);
    }

    /// Drop a guild's record, for a guild the person left.
    ///
    /// [ go offline ] is deliberately not recorded: it stops the node for this
    /// session, but the intent that the guild belongs online stays, so the next
    /// start rejoins it. Otherwise a frontend that disconnects on exit (the CLI
    /// chat does) would quietly strand the machine.
    fn forget_guild(&self, guild_id: GuildId) {
        let hex = guild_id.to_hex();
        let mut saved = self
            .saved_network
            .lock()
            .expect("network intent mutex poisoned");
        saved.retain(|existing| existing.guild_id != hex);
        persist_network_intent(&self.data_dir, &saved);
    }

    /// Take a guild offline for the rest of this session, without forgetting
    /// that it belongs online next time.
    fn mark_session_offline(&self, guild_id: GuildId) {
        self.session_offline
            .lock()
            .expect("session offline mutex poisoned")
            .insert(guild_id);
    }

    /// A frontend asked for the guild again, so resume may connect it.
    fn mark_session_online(&self, guild_id: GuildId) {
        self.session_offline
            .lock()
            .expect("session offline mutex poisoned")
            .remove(&guild_id);
    }

    fn is_session_offline(&self, guild_id: GuildId) -> bool {
        self.session_offline
            .lock()
            .expect("session offline mutex poisoned")
            .contains(&guild_id)
    }

    /// Rejoin the guilds this daemon was online in before it last stopped.
    ///
    /// `online_by_default` is set by an interactive frontend that spawned the
    /// daemon (the GUI or TUI). It means "I opened the app, bring my guilds
    /// up", so guilds with no saved intent are connected on an ephemeral port.
    /// A daemon started by a one-shot CLI command leaves guilds it has never
    /// seen online alone, because `ovvc history` should not open a socket.
    pub async fn resume_network(&self, online_by_default: bool) {
        let guilds = match self.client.list_guilds() {
            Ok(guilds) => guilds,
            Err(error) => {
                eprintln!("ovvc-daemon: could not list guilds to resume the network: {error}");
                return;
            }
        };

        // Prune, merge and persist under one lock. Holding it across the write
        // stops a concurrent connect from being overwritten by this task's
        // older snapshot.
        let targets: Vec<SavedNetwork> = {
            let mut saved = self
                .saved_network
                .lock()
                .expect("network intent mutex poisoned");
            let before = (
                saved.len(),
                saved
                    .iter()
                    .map(|entry| entry.guild_id.clone())
                    .collect::<Vec<_>>(),
            );
            // A guild that was left, or whose store was reset, must not keep a
            // record that makes every future start try to rejoin it.
            saved.retain(|entry| {
                entry
                    .guild_id()
                    .is_some_and(|guild_id| guilds.iter().any(|guild| guild.id == guild_id))
            });
            if online_by_default {
                for guild in &guilds {
                    if !saved
                        .iter()
                        .any(|entry| entry.guild_id == guild.id.to_hex())
                    {
                        saved.push(SavedNetwork::new(
                            guild.id,
                            true,
                            &NetworkOptionsDto {
                                listen: vec!["/ip4/0.0.0.0/tcp/0".to_string()],
                                ..NetworkOptionsDto::default()
                            },
                        ));
                    }
                }
            }
            let after = (
                saved.len(),
                saved
                    .iter()
                    .map(|entry| entry.guild_id.clone())
                    .collect::<Vec<_>>(),
            );
            if before != after {
                persist_network_intent(&self.data_dir, &saved);
            }
            saved.iter().filter(|entry| entry.online).cloned().collect()
        };
        let had_targets = !targets.is_empty();

        for intent in targets {
            let Some(guild_id) = intent.guild_id() else {
                continue;
            };
            // A frontend can attach and take the guild offline while this
            // task is still working through the list. The click wins.
            if self.is_session_offline(guild_id) {
                continue;
            }
            let still_online = self
                .saved_network
                .lock()
                .expect("network intent mutex poisoned")
                .iter()
                .any(|entry| entry.guild_id == intent.guild_id && entry.online);
            if !still_online {
                continue;
            }
            let options = match network_options(intent.options()) {
                Ok(options) => options,
                Err(error) => {
                    self.report(
                        ErrorClass::User,
                        Some(guild_id),
                        "resume network",
                        format!("could not rejoin commune {}: {error}", guild_id.short()),
                    );
                    continue;
                }
            };
            match self.client.connect(guild_id, options).await {
                Ok(()) => {
                    // The connect itself takes time. A frontend may have sent
                    // [ go offline ] or left the guild while it was starting;
                    // in that case undo the node rather than leave it running.
                    let cancelled = self.is_session_offline(guild_id);
                    if cancelled {
                        self.client.disconnect(guild_id);
                        self.network
                            .lock()
                            .expect("network state mutex poisoned")
                            .remove(guild_id);
                        continue;
                    }
                    self.network
                        .lock()
                        .expect("network state mutex poisoned")
                        .entry(guild_id);
                }
                Err(error) => {
                    // A frontend may not be attached yet, and the event
                    // broadcast has no replay, so a failed resume would
                    // otherwise leave no trace at all. The repair pass retries
                    // it; this line is what tells an operator why.
                    self.report(
                        ErrorClass::Operational,
                        Some(guild_id),
                        "resume network",
                        format!(
                            "could not rejoin the peer network for commune {}: {error}",
                            guild_id.short()
                        ),
                    );
                }
            }
        }

        // A DM is not in the saved intent list, so its node would otherwise
        // stay down after a restart even though the announcement is in the
        // store. Only do this when this daemon is doing network work anyway;
        // a one-shot `ovvc history` with everything offline must not open a
        // socket.
        if (online_by_default || had_targets)
            && let Err(error) = self.client.resume_direct_messages().await
        {
            self.log_only(
                ErrorClass::Operational,
                None,
                "direct messages",
                format!("could not resume direct messages: {error}"),
            );
        }
    }

    /// Bring back any guild that is meant to be online but whose node is gone.
    ///
    /// A start can fail because a saved port was busy, and a node loop can
    /// exit on its own. Without this the guild stays offline until the daemon
    /// is restarted or a person clicks, so the ticker retries every
    /// `NETWORK_REPAIR_INTERVAL`. A guild a frontend took offline in this
    /// session is left alone, and a connect that finishes after such a click
    /// is undone, so the retry can never resurrect it.
    pub async fn repair_network(&self) {
        let targets: Vec<SavedNetwork> = self
            .saved_network
            .lock()
            .expect("network intent mutex poisoned")
            .iter()
            .filter(|entry| entry.online)
            .cloned()
            .collect();
        for intent in targets {
            let Some(guild_id) = intent.guild_id() else {
                continue;
            };
            if self.is_session_offline(guild_id) || self.client.is_connected(guild_id) {
                continue;
            }
            let options = match network_options(intent.options()) {
                Ok(options) => options,
                Err(error) => {
                    self.log_only(
                        ErrorClass::User,
                        Some(guild_id),
                        "resume network",
                        format!("could not retry guild {}: {error}", guild_id.short()),
                    );
                    continue;
                }
            };
            match self.client.connect(guild_id, options).await {
                Ok(()) => {
                    if self.is_session_offline(guild_id) {
                        self.client.disconnect(guild_id);
                        self.network
                            .lock()
                            .expect("network state mutex poisoned")
                            .remove(guild_id);
                    } else {
                        self.network
                            .lock()
                            .expect("network state mutex poisoned")
                            .entry(guild_id);
                    }
                }
                Err(error) => self.log_only(
                    ErrorClass::Operational,
                    Some(guild_id),
                    "resume network",
                    format!(
                        "could not bring guild {} back online: {error}",
                        guild_id.short()
                    ),
                ),
            }
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.events.subscribe()
    }

    // --- call audio ------------------------------------------------------

    /// Subscribe to the audio services' media, speaking and stats events.
    pub fn subscribe_audio(&self) -> broadcast::Receiver<AudioEventEnvelope> {
        self.audio_events.subscribe()
    }

    /// Start the audio service for a call, idempotently.
    ///
    /// The service thread builds its own backend, which is required for the
    /// `!Send` PipeWire backend. This function stores the command handle and
    /// waits for the service's first stats sample, so a backend that cannot
    /// open is reported to the joiner in the `VoiceJoin` reply rather than
    /// only on the event stream. The central pump still turns the service's
    /// events into network traffic and frontend events.
    /// The codec, jitter and pipeline settings every audio path for a call
    /// shares. The engine and the hub mixer are built from the same values, so
    /// the hub's per-listener encoders match the leaves' decoders.
    fn audio_engine_config(&self) -> Result<EngineConfig, String> {
        let settings = &self.config.audio;
        let bitrate_kbps = ovvc_media::VoiceProfile::from_name(&settings.profile)
            .map(ovvc_media::VoiceProfile::bitrate_kbps)
            .unwrap_or(16);
        let mut engine = EngineConfig {
            sample_rate: settings.sample_rate,
            frame_ms: settings.frame_ms,
            bitrate_kbps,
            jitter_depth: 3,
            ..EngineConfig::default()
        };
        let pipelines = match &settings.pipeline {
            Some(path) => ovvc_audio::pipeline_config::load(path).map_err(|error| {
                format!(
                    "could not load the audio pipeline from {}: {error}",
                    path.display()
                )
            })?,
            None => ovvc_audio::default_pipelines(),
        };
        engine.capture_pipeline = pipelines.capture;
        engine.playback_pipeline = pipelines.playback;
        Ok(engine)
    }

    pub async fn start_audio(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    ) -> Result<(), String> {
        let key = (guild_id, channel_id);
        // Hold the audio map lock across the check, the service spawn and the
        // insert. `spawn` is synchronous, so no await happens while the lock
        // is held, and two concurrent `VoiceJoin` requests cannot both spawn a
        // service for the same call and then drop one of the two handles.
        let mut audio = self.audio.lock().await;
        if audio.contains_key(&key) {
            return Ok(());
        }

        let engine = match self.audio_engine_config() {
            Ok(engine) => engine,
            Err(message) => {
                self.report(
                    ErrorClass::User,
                    Some(guild_id),
                    "audio pipeline",
                    message.clone(),
                );
                return Err(message);
            }
        };

        let settings = &self.config.audio;
        let backend = BackendKind::parse(&settings.backend).unwrap_or_default();
        // An explicit backend this build cannot open must fail here rather
        // than start a service that immediately exits.
        if let Err(error) = ovvc_audio::validate_backend(backend) {
            let message = format!("could not start call audio: {error}");
            self.report(
                ErrorClass::User,
                Some(guild_id),
                "audio backend",
                message.clone(),
            );
            return Err(message);
        }
        let generation = self.audio_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let config = AudioCallConfig {
            guild_id,
            channel_id,
            call_id: CallId::for_channel(channel_id),
            local_author: self.client.author_id(),
            engine,
            backend,
            muted,
            deafened,
            push_to_talk,
            events: self.audio_events.clone(),
            generation,
        };
        // Subscribe before spawning so the first sample cannot be missed.
        let mut events = self.audio_events.subscribe();
        let (handle, _thread) = match ovvc_audio::spawn(config) {
            Ok(pair) => pair,
            Err(error) => {
                let message = format!("could not start call audio: {error}");
                self.report(
                    ErrorClass::Operational,
                    Some(guild_id),
                    "audio service",
                    message.clone(),
                );
                return Err(message);
            }
        };
        audio.insert(
            key,
            AudioCall {
                handle,
                generation,
                last_stats: None,
            },
        );
        // Release the map before waiting on the service's first report, which
        // is an await and must not hold the lock.
        drop(audio);

        // The service publishes its first stats sample immediately after the
        // backend opens. A sample means the call has media; an error or a
        // timeout means it does not, and the caller must not be told the join
        // succeeded silently.
        let deadline = tokio::time::Instant::now() + START_AUDIO_TIMEOUT;
        loop {
            match tokio::time::timeout_at(deadline, events.recv()).await {
                Ok(Ok(envelope))
                    if envelope.guild_id == guild_id
                        && envelope.channel_id == channel_id
                        && envelope.generation == generation =>
                {
                    match envelope.event {
                        AudioEvent::Stats(_) => return Ok(()),
                        AudioEvent::Error(message) => return Err(message),
                        AudioEvent::Stopped => {
                            return Err("the audio service stopped before it reported a backend"
                                .to_string());
                        }
                        _ => {}
                    }
                }
                Ok(Ok(_)) | Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                Ok(Err(broadcast::error::RecvError::Closed)) => {
                    return Err("the audio event channel closed".to_string());
                }
                Err(_) => {
                    return Err(format!(
                        "the audio service did not report within {}s",
                        START_AUDIO_TIMEOUT.as_secs()
                    ));
                }
            }
        }
    }

    /// Stop the audio service for a call, if one is running.
    pub async fn stop_audio(&self, guild_id: GuildId, channel_id: ChannelId) {
        let call = self.audio.lock().await.remove(&(guild_id, channel_id));
        if let Some(call) = call {
            let _ = call.handle.send(AudioCommand::Stop);
        }
    }

    /// Stop every call running in a guild, for disconnect and leaving.
    pub async fn stop_audio_guild(&self, guild_id: GuildId) {
        let keys = self.audio_keys(guild_id).await;
        for (guild_id, channel_id) in keys {
            self.stop_audio(guild_id, channel_id).await;
        }
    }

    /// Stop every audio service, for daemon shutdown.
    pub async fn stop_all_audio(&self) {
        let keys: Vec<(GuildId, ChannelId)> = self.audio.lock().await.keys().copied().collect();
        for (guild_id, channel_id) in keys {
            self.stop_audio(guild_id, channel_id).await;
        }
    }

    async fn audio_keys(&self, guild_id: GuildId) -> Vec<(GuildId, ChannelId)> {
        self.audio
            .lock()
            .await
            .keys()
            .filter(|(call_guild, _)| *call_guild == guild_id)
            .copied()
            .collect()
    }

    // --- per-call media keys ---------------------------------------------

    /// The keyring for one call, if media keys have been set up.
    async fn call_keyring(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Option<Arc<RwLock<MediaKeyring>>> {
        self.crypto
            .lock()
            .await
            .get(&(guild_id, channel_id))
            .map(|call| Arc::clone(&call.keyring))
    }

    /// Ensure the call has a keyring and rotate it when the roster changed or
    /// `force_rotate` is set. Returns the shared keyring and whether a new
    /// epoch key must be published.
    ///
    /// A fresh keyring starts at epoch one; a join request forces one rotation
    /// so the first epoch published is tied to the roster the join produced.
    async fn sync_call_crypto(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        roster: &[AuthorId],
        hub: Option<AuthorId>,
        announced_epoch: u32,
        force_rotate: bool,
    ) -> Option<(Arc<RwLock<MediaKeyring>>, bool)> {
        let roster: BTreeSet<AuthorId> = roster.iter().copied().collect();
        let key = (guild_id, channel_id);
        let mut crypto = self.crypto.lock().await;
        let Some(entry) = crypto.get_mut(&key) else {
            let mut keyring =
                MediaKeyring::generate(self.client.author_id(), CallId::for_channel(channel_id));
            if force_rotate {
                keyring.rotate();
            }
            let keyring = Arc::new(RwLock::new(keyring));
            crypto.insert(
                key,
                CallCrypto {
                    keyring: Arc::clone(&keyring),
                    roster,
                    hub,
                    installed: HashMap::new(),
                    published: None,
                    aligned: false,
                    advances_since_rotation: 0,
                    errors: 0,
                },
            );
            return Some((keyring, true));
        };
        // A changed hub is a takeover: the pairwise key set changes from the
        // old hub's leaves to the new hub's leaves, so every old cipher must
        // go and the epoch must advance past anything the call has announced.
        // Without the floor a new hub whose local epoch lagged would publish a
        // key the leaves ignore as stale.
        let hub_changed = entry.hub != hub;
        let changed = force_rotate || entry.roster != roster || hub_changed;
        if changed {
            // A rotation clears every peer cipher; peers republish their keys
            // for the new epoch when they see ours. The new epoch is a fresh
            // roster term, so one remote advance is allowed again.
            let mut keys = entry.keyring.write().ok()?;
            if hub_changed || force_rotate {
                let next = keys.epoch().max(announced_epoch).saturating_add(1).max(1);
                keys.rotate_to(next).ok()?;
            } else {
                keys.rotate();
            }
            drop(keys);
            entry.roster = roster;
            entry.hub = hub;
            entry.installed.clear();
            entry.published = None;
            entry.advances_since_rotation = 0;
        }
        Some((Arc::clone(&entry.keyring), changed))
    }

    /// Publish the local media public key for the call's current epoch.
    ///
    /// Returns whether the hub accepted the frame. The acceptance is recorded
    /// on the call so a rejected publish is retried on the next reconcile
    /// tick; the periodic republish also recovers a frame the network lost
    /// after the hub accepted it.
    async fn publish_call_key(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        members: &[AuthorId],
        keyring: Arc<RwLock<MediaKeyring>>,
    ) -> bool {
        let published = match keyring.read() {
            Ok(keys) => Some((keys.epoch(), keys.public_key())),
            Err(_) => None,
        };
        let Some((epoch, public_key)) = published else {
            return false;
        };
        let outcome = {
            let mut hub = self.control.lock().await;
            hub.publish_voice_key(guild_id, channel_id, members, epoch, public_key, now_ms())
        };
        match outcome {
            Ok(outcome) => {
                self.broadcast_outcome(outcome);
                if let Some(entry) = self.crypto.lock().await.get_mut(&(guild_id, channel_id)) {
                    let still_current = match entry.keyring.read() {
                        Ok(keys) => keys.epoch() == epoch && keys.public_key() == public_key,
                        Err(_) => false,
                    };
                    if still_current {
                        entry.published = Some((epoch, public_key, now_ms()));
                    }
                }
                true
            }
            Err(error) => {
                if let Some(entry) = self.crypto.lock().await.get_mut(&(guild_id, channel_id)) {
                    entry.published = None;
                }
                self.report(
                    ErrorClass::Operational,
                    Some(guild_id),
                    "media key",
                    format!("could not publish the media key: {error}"),
                );
                false
            }
        }
    }

    /// Align the call's keyring with the voice roster and publish a new epoch
    /// key when it changed, when the last publish was not confirmed, or when
    /// the periodic republish is due.
    ///
    /// A local join forces a rotation; a remote join or leave rotates because
    /// the roster changed. A call the local author is not in has no keyring at
    /// all, so a stale key cannot keep a dead transport usable.
    async fn refresh_call_keys(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        force_rotate: bool,
    ) {
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        let local_author = self.client.author_id();
        let Some(call) = voice_call(&snapshot, channel_id) else {
            self.drop_call_crypto(guild_id, channel_id).await;
            return;
        };
        if !call
            .members
            .iter()
            .any(|member| member.author == local_author)
        {
            self.drop_call_crypto(guild_id, channel_id).await;
            return;
        }
        let roster: Vec<AuthorId> = call.members.iter().map(|member| member.author).collect();
        let Some((keyring, changed)) = self
            .sync_call_crypto(
                guild_id,
                channel_id,
                &roster,
                call.hub,
                call.key_epoch,
                force_rotate,
            )
            .await
        else {
            return;
        };
        let republish = {
            let crypto = self.crypto.lock().await;
            match crypto.get(&(guild_id, channel_id)) {
                Some(entry) => match entry.keyring.read() {
                    Ok(keys) => key_publication_due(
                        entry.published,
                        keys.epoch(),
                        keys.public_key(),
                        now_ms(),
                    ),
                    Err(_) => false,
                },
                None => false,
            }
        };
        if changed || republish {
            self.publish_call_key(guild_id, channel_id, &members, keyring)
                .await;
        }
    }

    /// Install a peer's authenticated media key and converge on its epoch.
    ///
    /// An older epoch is ignored. A newer epoch advances the local keyring and
    /// republishes the local key, so the two sides meet on one epoch rather
    /// than one silently keeping media for a past epoch. The session has
    /// already verified the signature and that the sender is a call member.
    async fn handle_remote_voice_key(&self, guild_id: GuildId, frame: &ControlFrame) {
        let ControlKind::VoiceKey {
            channel_id,
            epoch,
            public_key,
        } = &frame.kind
        else {
            return;
        };
        let (channel_id, epoch, public_key) = (*channel_id, *epoch, *public_key);
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        let local_author = self.client.author_id();
        let Some(call) = voice_call(&snapshot, channel_id) else {
            return;
        };
        if !call
            .members
            .iter()
            .any(|member| member.author == local_author)
        {
            return;
        }
        if !call
            .members
            .iter()
            .any(|member| member.author == frame.from)
        {
            return;
        }
        let members: Vec<AuthorId> = call.members.iter().map(|member| member.author).collect();
        // Only a key peer for this member's side of the topology may install a
        // cipher. In a star a leaf accepts the hub's key and nothing else, so
        // a leaf-to-leaf key can never be created even if both leaves publish.
        let key_topology = KeyTopology::for_call_with_hub(&call.topology, call.hub_policy);
        if !key_topology.includes(local_author, frame.from, &members) {
            return;
        }
        let Some((keyring, local_changed)) = self
            .sync_call_crypto(
                guild_id,
                channel_id,
                &members,
                call.hub,
                call.key_epoch,
                false,
            )
            .await
        else {
            return;
        };
        let advanced = {
            let mut crypto = self.crypto.lock().await;
            let Some(entry) = crypto.get_mut(&(guild_id, channel_id)) else {
                return;
            };
            if entry.installed.get(&frame.from) == Some(&(epoch, public_key)) {
                return;
            }
            let mut keys = match entry.keyring.write() {
                Ok(keys) => keys,
                Err(_) => return,
            };
            let mut advanced = false;
            let was_aligned = entry.aligned;
            if let EpochUpdate::Advance(target) = plan_epoch_update(
                keys.epoch(),
                epoch,
                entry.aligned,
                entry.advances_since_rotation,
            ) && keys.rotate_to(target).is_ok()
            {
                advanced = true;
                entry.aligned = true;
                // Initial alignment does not use up this roster term's
                // budget; each later advance does.
                if was_aligned {
                    entry.advances_since_rotation = entry.advances_since_rotation.saturating_add(1);
                }
                entry.installed.clear();
            }
            if epoch == keys.epoch() {
                match keys.install_peer(frame.from, epoch, public_key) {
                    Ok(()) => {
                        entry.installed.insert(frame.from, (epoch, public_key));
                        entry.aligned = true;
                    }
                    Err(_) => entry.errors = entry.errors.saturating_add(1),
                }
            }
            advanced
        };
        if advanced || local_changed {
            self.publish_call_key(guild_id, channel_id, &members, keyring)
                .await;
        }
    }

    /// Decrypt one wire media packet for the local call.
    ///
    /// A packet with no keyring, no peer key, the wrong epoch, a replayed
    /// counter or a bad tag returns `None` and is counted. Plaintext never
    /// leaves this function for a frame that did not authenticate.
    async fn decrypt_media(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: &ovvc_media::MediaPacket,
        key_author: AuthorId,
    ) -> Option<ovvc_media::MediaPacket> {
        let keyring = self.call_keyring(guild_id, channel_id).await?;
        // The pairwise key belongs to the transport sender, which in a star
        // is the only peer this member shares a key with. The packet's author
        // is preserved so the audio path still attributes the stream.
        let plaintext = match keyring.read() {
            Ok(keys) => keys.decrypt_from(key_author, &packet.payload),
            Err(_) => Err(CryptoError::NoPeerKey),
        };
        match plaintext {
            Ok(payload) => ovvc_media::MediaPacket::new(
                packet.kind,
                packet.call_id,
                packet.author,
                packet.seq,
                packet.timestamp_ms,
                payload,
            )
            .ok(),
            Err(_) => {
                self.record_crypto_errors(guild_id, channel_id, 1).await;
                None
            }
        }
    }

    /// Count dropped or unencryptable frames for one call.
    async fn record_crypto_errors(&self, guild_id: GuildId, channel_id: ChannelId, count: u64) {
        if let Some(call) = self.crypto.lock().await.get_mut(&(guild_id, channel_id)) {
            call.errors = call.errors.saturating_add(count);
        }
        // A per-frame key miss during roster convergence is media noise, not a
        // health failure. The election's `recent_failures` field counts
        // peer-level and send failures instead, which are naturally bounded;
        // otherwise one joining member's first second would look like a hub
        // health breach.
    }

    /// Forget one call's keyring and every peer key in it.
    async fn drop_call_crypto(&self, guild_id: GuildId, channel_id: ChannelId) {
        self.crypto.lock().await.remove(&(guild_id, channel_id));
        self.hub_mixers().remove(&(guild_id, channel_id));
    }

    /// Forget every call keyring in a guild, for disconnect and leaving.
    async fn drop_guild_crypto(&self, guild_id: GuildId) {
        self.crypto
            .lock()
            .await
            .retain(|(call_guild, _), _| *call_guild != guild_id);
    }

    /// Forget every call keyring, for daemon shutdown.
    pub async fn clear_all_crypto(&self) {
        self.crypto.lock().await.clear();
        self.hub_mixers().clear();
    }

    // --- WebRTC media -----------------------------------------------------

    /// Whether this daemon carries call media over WebRTC.
    fn rtc_enabled(&self) -> bool {
        self.config.media.transport == "webrtc"
    }

    /// Hand the RTC event receiver to the pump. Called once at startup.
    pub async fn take_rtc_events(&self) -> Option<mpsc::Receiver<RtcEventEnvelope>> {
        self.rtc_event_rx.lock().await.take()
    }

    /// Start the WebRTC transport for a call, idempotently.
    ///
    /// The manager needs the call, the local author and the media settings; the
    /// per-call event stream is forwarded to the central pump so the manager
    /// itself stays free of daemon types.
    async fn start_rtc(&self, guild_id: GuildId, channel_id: ChannelId) -> bool {
        if !self.rtc_enabled() {
            return false;
        }
        // The daemon owns the keyring so the same keys serve both transports.
        // Without one there is no end-to-end key to publish, and starting a
        // manager would silently create an unshared one.
        let Some(keyring) = self.call_keyring(guild_id, channel_id).await else {
            self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "webrtc media",
                "call media keys are not ready; WebRTC media was not started",
            );
            return false;
        };
        let mut rtc = self.rtc.lock().await;
        if rtc.contains_key(&(guild_id, channel_id)) {
            return true;
        }
        let bitrate_kbps = ovvc_media::VoiceProfile::from_name(&self.config.audio.profile)
            .map(ovvc_media::VoiceProfile::bitrate_kbps)
            .unwrap_or(16);
        let config = ovvc_rtc::RtcConfig {
            ice_servers: self.config.media.ice_servers.clone(),
            turn_username: self.config.media.turn_username.clone().unwrap_or_default(),
            turn_credential: self
                .config
                .media
                .turn_credential
                .clone()
                .unwrap_or_default(),
            force_relay: self.config.media.force_relay,
            allow_loopback_candidates: self.config.media.allow_loopback_candidates,
            allow_lan_candidates: self.config.media.allow_lan_candidates,
            audio_frame_ms: self.config.audio.frame_ms,
            audio_bitrate_kbps: bitrate_kbps,
            negotiation_timeout_ms: 5_000,
        };
        let generation = self.rtc_generation.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, mut rx) = mpsc::channel(RTC_EVENT_CAPACITY);
        let manager = match RtcManager::new(
            self.client.author_id(),
            CallId::for_channel(channel_id),
            config,
            tx,
        ) {
            Ok(mut manager) => {
                // Attach the call's shared keyring before any peer connection
                // exists, so every track reader and sender sees the same keys.
                manager.set_keyring(keyring);
                manager
            }
            Err(error) => {
                self.report(
                    ErrorClass::Operational,
                    Some(guild_id),
                    "webrtc media",
                    format!("could not start WebRTC media: {error}"),
                );
                return false;
            }
        };
        rtc.insert(
            (guild_id, channel_id),
            RtcCall {
                manager,
                generation,
                last_stats: RtcStats::default(),
            },
        );
        let events = self.rtc_events.clone();
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let envelope = RtcEventEnvelope {
                    guild_id,
                    channel_id,
                    generation,
                    event,
                };
                // RTP and the (rate-limited) error reports may be dropped when
                // the pump is behind; they replace themselves or are already
                // summarised. SDP/ICE and stats are awaited so a full queue
                // cannot lose the negotiation.
                let sent = match &envelope.event {
                    RtcEvent::Signal { .. } | RtcEvent::Stats(_) => {
                        events.send(envelope).await.is_ok()
                    }
                    RtcEvent::Media { .. } | RtcEvent::Error(_) => matches!(
                        events.try_send(envelope),
                        Ok(()) | Err(mpsc::error::TrySendError::Full(_))
                    ),
                };
                if !sent {
                    return;
                }
            }
        });
        true
    }

    /// Close the WebRTC transport for one call, if one is running.
    async fn stop_rtc(&self, guild_id: GuildId, channel_id: ChannelId) {
        let call = self.rtc.lock().await.remove(&(guild_id, channel_id));
        if let Some(mut call) = call {
            call.manager.close().await;
        }
    }

    /// Close every WebRTC transport in a guild, for disconnect and leaving.
    async fn stop_rtc_guild(&self, guild_id: GuildId) {
        let keys: Vec<(GuildId, ChannelId)> = self
            .rtc
            .lock()
            .await
            .keys()
            .filter(|(call_guild, _)| *call_guild == guild_id)
            .copied()
            .collect();
        for (call_guild, channel_id) in keys {
            self.stop_rtc(call_guild, channel_id).await;
        }
    }

    /// Close every WebRTC transport, for daemon shutdown.
    pub async fn stop_all_rtc(&self) {
        let keys: Vec<(GuildId, ChannelId)> = self.rtc.lock().await.keys().copied().collect();
        for (guild_id, channel_id) in keys {
            self.stop_rtc(guild_id, channel_id).await;
        }
    }

    /// Align the call's peer connections with its current voice roster.
    ///
    /// The lower author offers, which [`RtcManager::ensure_peer`] handles. A
    /// member who left is closed; a member who joined (or whose previous
    /// connection failed) is dialled again. When the local author is no longer
    /// in the call the whole transport is closed.
    async fn reconcile_rtc(&self, guild_id: GuildId, channel_id: ChannelId) {
        // The roster drives both transports. This also creates the keyring on
        // the first reconcile and rotates it after a membership change, so a
        // rotation gap can never leave a transport running on old keys.
        self.refresh_call_keys(guild_id, channel_id, false).await;
        if !self.rtc_enabled() {
            return;
        }
        let local_author = self.client.author_id();
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        let Some(call) = voice_call(&snapshot, channel_id) else {
            self.stop_rtc(guild_id, channel_id).await;
            return;
        };
        if !call
            .members
            .iter()
            .any(|member| member.author == local_author)
        {
            self.stop_rtc(guild_id, channel_id).await;
            return;
        }
        // A star is a flat hub: the hub dials every leaf and a leaf dials only
        // the hub. A mesh (including the two-member direct pair) dials every
        // other member. This is the media-peer half of the topology; the key
        // set is narrowed separately in `handle_remote_voice_key`.
        let remotes: HashSet<AuthorId> = media_peers(&call, local_author).into_iter().collect();
        // Keep the hub mixer's listener set equal to the current leaves, and
        // drop the mixer entirely when this daemon is not the hub.
        self.reconcile_hub_mixer(guild_id, &call, local_author);
        if !self.start_rtc(guild_id, channel_id).await {
            return;
        }

        let mut failure = None;
        {
            let mut rtc = self.rtc.lock().await;
            let Some(rtc_call) = rtc.get_mut(&(guild_id, channel_id)) else {
                return;
            };
            // `stats` names every live peer; a failed one is already gone from
            // the manager's map, so this is the truthful current set.
            let live: Vec<AuthorId> = rtc_call
                .manager
                .stats()
                .await
                .peers
                .iter()
                .map(|peer| peer.author)
                .collect();
            for author in live {
                if !remotes.contains(&author) {
                    rtc_call.manager.remove_peer(author).await;
                }
            }
            for author in remotes {
                if let Err(error) = rtc_call.manager.ensure_peer(author).await {
                    failure = Some(format!(
                        "could not connect WebRTC peer {}: {error}",
                        author.short()
                    ));
                }
            }
        }
        if let Some(message) = failure {
            self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "webrtc media",
                message,
            );
        }
    }

    /// The transport fields for a `VoiceAudioStats` sample.
    async fn rtc_metrics(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> (String, u32, i64, bool) {
        let transport = self.config.media.transport.clone();
        if transport != "webrtc" {
            return (transport, 0, 0, false);
        }
        let rtc = self.rtc.lock().await;
        let Some(call) = rtc.get(&(guild_id, channel_id)) else {
            return (transport, 0, 0, false);
        };
        let rtt_ms = call
            .last_stats
            .peers
            .iter()
            .map(|peer| peer.rtt_ms)
            .max()
            .unwrap_or(0);
        let packets_lost = call
            .last_stats
            .peers
            .iter()
            .fold(0i64, |total, peer| total.saturating_add(peer.packets_lost));
        let relayed = call.last_stats.peers.iter().any(|peer| peer.relayed);
        (transport, rtt_ms, packets_lost, relayed)
    }

    /// The end-to-end encryption fields for a `VoiceAudioStats` sample.
    ///
    /// `epoch` and the local error counter come from the call's keyring; the
    /// WebRTC peer state and its decrypt counter come from the last transport
    /// sample. On both transports `e2ee` is true only when every live call
    /// peer has a key for the current epoch, so a keyless peer that causes
    /// every frame to be dropped cannot be reported as a sealed call. A call
    /// with no keyring reports `e2ee = false` rather than pretending the
    /// plaintext it is dropping was secure.
    async fn e2ee_metrics(&self, guild_id: GuildId, channel_id: ChannelId) -> (bool, u32, u64) {
        let (keyring, roster, local_errors) = {
            let crypto = self.crypto.lock().await;
            match crypto.get(&(guild_id, channel_id)) {
                Some(call) => (
                    Some(Arc::clone(&call.keyring)),
                    call.roster.clone(),
                    call.errors,
                ),
                None => (None, BTreeSet::new(), 0),
            }
        };
        let Some(keyring) = keyring else {
            return (false, 0, local_errors);
        };
        let local_author = self.client.author_id();
        // A star call intentionally holds only the leaf-to-hub keys, so the
        // sealed state is "every key peer has a cipher", not "every call
        // member does". A leaf that cannot decrypt another leaf is correct,
        // not an unsealed call. When no live snapshot exists yet, the crypto
        // roster is the best available member list and the all-pairs rule is
        // the conservative reading.
        let key_peers = {
            let members = self.members(guild_id);
            let snapshot = {
                let mut hub = self.control.lock().await;
                hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
            };
            match voice_call(&snapshot, channel_id) {
                Some(call) => KeyTopology::for_call_with_hub(&call.topology, call.hub_policy)
                    .peers_for(local_author, &members),
                None => roster
                    .iter()
                    .copied()
                    .filter(|author| *author != local_author)
                    .collect(),
            }
        };
        let live = self.live_media_authors(guild_id).await;
        let (epoch, peers_have_keys) = match keyring.read() {
            Ok(keys) => {
                let epoch = keys.epoch();
                // A member with no live transport peer cannot be sealed for;
                // its key is not required until it reconnects. Without this a
                // hub that just died would hold the call at `e2ee = false`
                // for the rest of the voice member lease.
                let peers_have_keys = key_peers
                    .iter()
                    .filter(|author| live.as_ref().is_none_or(|live| live.contains(author)))
                    .all(|author| keys.peer_epoch(*author) == Some(epoch));
                (epoch, peers_have_keys)
            }
            Err(_) => (0, false),
        };
        if !self.rtc_enabled() {
            return (epoch != 0 && peers_have_keys, epoch, local_errors);
        }
        let rtc = self.rtc.lock().await;
        let stats = rtc
            .get(&(guild_id, channel_id))
            .map(|call| call.last_stats.clone())
            .unwrap_or_default();
        // A call with no peers yet is still encrypted. Once peers exist, every
        // live one must have a key for the transport to count as sealed. The
        // WebRTC transport reports each live peer's key state directly; the
        // libp2p path has no such list, so its roster check above is the only
        // signal that a member is still keyless.
        // A peer that is no longer connected is not part of the sealed call,
        // even though the voice roster keeps it until its lease expires.
        let e2ee = epoch != 0
            && stats
                .peers
                .iter()
                .filter(|peer| peer.state == "connected")
                .all(|peer| peer.e2ee);
        let errors = stats.decrypt_errors.saturating_add(local_errors);
        (e2ee, epoch, errors)
    }

    /// Poll every live transport once and fold the snapshot into its call.
    pub async fn sample_rtc_stats(&self) {
        let mut samples = Vec::new();
        {
            let rtc = self.rtc.lock().await;
            for ((guild_id, channel_id), call) in rtc.iter() {
                samples.push((
                    *guild_id,
                    *channel_id,
                    call.generation,
                    call.manager.stats().await,
                ));
            }
        }
        for (guild_id, channel_id, generation, stats) in samples {
            self.handle_rtc_event(RtcEventEnvelope {
                guild_id,
                channel_id,
                generation,
                event: RtcEvent::Stats(stats),
            })
            .await;
        }
    }

    /// Apply one WebRTC event from a call's manager.
    pub async fn handle_rtc_event(&self, envelope: RtcEventEnvelope) {
        let RtcEventEnvelope {
            guild_id,
            channel_id,
            generation,
            event,
        } = envelope;
        let current = self
            .rtc
            .lock()
            .await
            .get(&(guild_id, channel_id))
            .is_some_and(|call| call.generation == generation);
        if !current {
            return;
        }
        match event {
            RtcEvent::Signal { to, signal } => {
                self.send_rtc_signal(guild_id, channel_id, to, signal).await;
            }
            RtcEvent::Media { from: _, packet } => {
                self.deliver_rtc_media(guild_id, channel_id, packet).await;
            }
            RtcEvent::Stats(stats) => {
                let mut rtc = self.rtc.lock().await;
                if let Some(call) = rtc.get_mut(&(guild_id, channel_id))
                    && call.generation == generation
                {
                    call.last_stats = stats;
                }
            }
            RtcEvent::Error(message) => {
                // The manager has already closed the failed peer by the time
                // this lands. The call and every other peer keep running.
                self.media_failures.record();
                self.report(
                    ErrorClass::Operational,
                    Some(guild_id),
                    "webrtc media",
                    message,
                );
            }
        }
    }

    /// Write a captured Opus payload to every connected RTC peer.
    async fn send_rtc_audio(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
    ) {
        let rtc = self.rtc.lock().await;
        let Some(call) = rtc.get(&(guild_id, channel_id)) else {
            return;
        };
        // A rejected packet is an encoder bug and superseded by the next
        // frame, so it is dropped rather than turned into a frontend event.
        let _ = call.manager.send_audio(&packet).await;
    }

    /// Hand a decrypted RTP payload to the call's audio path.
    ///
    /// A leaf plays one mixed stream from the hub. The hub feeds each leaf's
    /// plaintext frame into its mixer instead and never hands it to the mesh
    /// engine, because the mixer owns the hub's local playback.
    async fn deliver_rtc_media(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
    ) {
        let local = self.client.author_id();
        if let Some(call) = self.call_snapshot(guild_id, channel_id).await {
            if !media_peers(&call, local).contains(&packet.author) {
                return;
            }
            if local_is_hub(&call, local) {
                self.reconcile_hub_mixer(guild_id, &call, local);
                self.hub_receive(guild_id, channel_id, &packet);
                return;
            }
        }
        let audio = self.audio.lock().await;
        if let Some(call) = audio.get(&(guild_id, channel_id)) {
            let _ = call.handle.send(AudioCommand::Media(packet));
        }
    }

    /// Sign one RTC signalling frame for `to` and send it to that peer only.
    async fn send_rtc_signal(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        to: AuthorId,
        signal: RtcSignal,
    ) {
        let kind = match signal {
            RtcSignal::Offer { sdp } => ControlKind::RtcOffer {
                channel_id,
                to,
                sdp,
            },
            RtcSignal::Answer { sdp } => ControlKind::RtcAnswer {
                channel_id,
                to,
                sdp,
            },
            RtcSignal::Candidate { candidate, mid } => ControlKind::RtcCandidate {
                channel_id,
                to,
                candidate,
                mid,
            },
        };
        let members = self.members(guild_id);
        let outcome = {
            let mut hub = self.control.lock().await;
            hub.rtc_signal(guild_id, channel_id, &members, kind, now_ms())
        };
        match outcome {
            Ok(outcome) => self.broadcast_outcome(outcome),
            Err(error) => self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "rtc signalling",
                format!("could not sign RTC signalling: {error}"),
            ),
        }
    }

    /// Route an authenticated RTC control frame to the call's manager.
    ///
    /// The control hub has already verified the signature, session, freshness,
    /// membership and replay nonce. This only filters by target and translates
    /// the frame into the transport's signalling type.
    async fn handle_rtc_control(&self, guild_id: GuildId, frame: &ControlFrame) {
        if !self.rtc_enabled() {
            return;
        }
        let Some((channel_id, to)) = frame.kind.rtc_target() else {
            return;
        };
        if to != self.client.author_id() {
            return;
        }
        let key = (guild_id, channel_id);
        if !self.rtc.lock().await.contains_key(&key) {
            self.reconcile_rtc(guild_id, channel_id).await;
        }
        let mut failure = None;
        {
            let mut rtc = self.rtc.lock().await;
            let Some(call) = rtc.get_mut(&key) else {
                return;
            };
            let result = match &frame.kind {
                ControlKind::RtcOffer { sdp, .. } => {
                    call.manager
                        .on_signal(frame.from, RtcSignal::Offer { sdp: sdp.clone() })
                        .await
                }
                ControlKind::RtcAnswer { sdp, .. } => {
                    call.manager
                        .on_signal(frame.from, RtcSignal::Answer { sdp: sdp.clone() })
                        .await
                }
                ControlKind::RtcCandidate { candidate, mid, .. } => {
                    call.manager
                        .on_signal(
                            frame.from,
                            RtcSignal::Candidate {
                                candidate: candidate.clone(),
                                mid: mid.clone(),
                            },
                        )
                        .await
                }
                ControlKind::RtcBye { .. } => {
                    call.manager.remove_peer(frame.from).await;
                    Ok(())
                }
                _ => return,
            };
            if let Err(error) = result {
                failure = Some(format!(
                    "WebRTC signal from {} failed: {error}",
                    frame.from.short()
                ));
            }
        }
        if let Some(message) = failure {
            self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "rtc signalling",
                message,
            );
        }
    }

    /// Pass new mute/deafen/push-to-talk gates to a running call.
    async fn set_audio_state(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        muted: bool,
        deafened: bool,
        push_to_talk: bool,
    ) {
        let audio = self.audio.lock().await;
        if let Some(call) = audio.get(&(guild_id, channel_id)) {
            let _ = call.handle.send(AudioCommand::SetState {
                muted,
                deafened,
                push_to_talk,
            });
        }
    }

    /// Tell a running call whether the push-to-talk key is held.
    async fn set_push_to_talk_held(&self, guild_id: GuildId, channel_id: ChannelId, held: bool) {
        let audio = self.audio.lock().await;
        if let Some(call) = audio.get(&(guild_id, channel_id)) {
            let _ = call.handle.send(AudioCommand::PushToTalkHeld(held));
        }
    }

    /// Whether the stored service for this call is the one that emitted an
    /// event. A stale event from a stopped thread must be ignored, or a late
    /// `Stopped` could retire the replacement service started by a rejoin.
    async fn audio_generation_matches(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        generation: u64,
    ) -> bool {
        self.audio
            .lock()
            .await
            .get(&(guild_id, channel_id))
            .is_some_and(|call| call.generation == generation)
    }

    /// Remove the call's service if it is still `generation`, returning
    /// whether it was removed.
    async fn remove_audio_if_generation(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        generation: u64,
    ) -> bool {
        let mut audio = self.audio.lock().await;
        if audio
            .get(&(guild_id, channel_id))
            .is_some_and(|call| call.generation == generation)
        {
            audio.remove(&(guild_id, channel_id));
            true
        } else {
            false
        }
    }

    /// Handle one event from a call's audio service.
    pub async fn handle_audio_event(&self, envelope: AudioEventEnvelope) {
        let AudioEventEnvelope {
            guild_id,
            channel_id,
            generation,
            event,
        } = envelope;
        match event {
            AudioEvent::Media(packet) => {
                if self
                    .audio_generation_matches(guild_id, channel_id, generation)
                    .await
                {
                    let local = self.client.author_id();
                    let call = self.call_snapshot(guild_id, channel_id).await;
                    if let Some(call) = call.as_ref().filter(|call| local_is_hub(call, local)) {
                        // The hub's own microphone is one source of the mix,
                        // not a packet to relay verbatim. The mixer includes it
                        // in every listener's mix and excludes it from the
                        // hub's local playback.
                        self.reconcile_hub_mixer(guild_id, call, local);
                        self.hub_receive(guild_id, channel_id, &packet);
                    } else if self.rtc_enabled() {
                        self.send_rtc_audio(guild_id, channel_id, packet).await;
                    } else {
                        self.forward_media(guild_id, channel_id, packet).await;
                    }
                }
            }
            AudioEvent::Speaking(speaking) => {
                if self
                    .audio_generation_matches(guild_id, channel_id, generation)
                    .await
                {
                    self.apply_local_speaking(guild_id, channel_id, speaking)
                        .await;
                }
            }
            AudioEvent::Stats(stats) => {
                self.apply_audio_stats(guild_id, channel_id, generation, stats)
                    .await;
            }
            AudioEvent::Error(message) => {
                if self
                    .remove_audio_if_generation(guild_id, channel_id, generation)
                    .await
                {
                    self.report(
                        ErrorClass::Operational,
                        Some(guild_id),
                        "audio service",
                        message,
                    );
                }
            }
            AudioEvent::Stopped => {
                self.remove_audio_if_generation(guild_id, channel_id, generation)
                    .await;
            }
        }
    }

    /// Send one captured packet to the other members of the call, sealed for
    /// each target author.
    ///
    /// The control hub is the source of truth for the roster, so a call that
    /// has ended, or a snapshot that does not name any other member, simply
    /// drops the packet rather than dialling a stale peer. The packet's
    /// payload is replaced with the per-author ciphertext before it is sent;
    /// a target without an installed key is skipped and counted, never sent
    /// plaintext.
    async fn forward_media(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
    ) {
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        let Some(call) = voice_call(&snapshot, channel_id) else {
            return;
        };
        let local_author = self.client.author_id();
        // A leaf sends its uplink only to the hub; the hub sends its own
        // voice to every leaf; a mesh sends to every other member. It is the
        // same topology rule the media peer set uses.
        let authors = media_peers(&call, local_author);
        if authors.is_empty() {
            return;
        }
        let Some(keyring) = self.call_keyring(guild_id, channel_id).await else {
            self.record_crypto_errors(guild_id, channel_id, authors.len() as u64)
                .await;
            return;
        };
        let mut sealed = Vec::with_capacity(authors.len());
        let mut failed = 0u64;
        match keyring.read() {
            Ok(keys) => {
                for author in &authors {
                    match keys.encrypt_for(*author, &packet.payload) {
                        Ok(ciphertext) => sealed.push((*author, ciphertext)),
                        Err(_) => failed += 1,
                    }
                }
            }
            Err(_) => failed = authors.len() as u64,
        }
        if failed > 0 {
            self.record_crypto_errors(guild_id, channel_id, failed)
                .await;
        }
        for (author, ciphertext) in sealed {
            let encrypted = match ovvc_media::MediaPacket::new(
                packet.kind,
                packet.call_id,
                packet.author,
                packet.seq,
                packet.timestamp_ms,
                ciphertext,
            ) {
                Ok(encrypted) => encrypted,
                Err(_) => {
                    self.record_crypto_errors(guild_id, channel_id, 1).await;
                    continue;
                }
            };
            // A frame that cannot be sent is superseded by the next one, so
            // the error is counted for the capacity sample rather than raised
            // as a per-packet frontend event.
            self.media_bytes_sent
                .fetch_add(encrypted.payload.len() as u64, Ordering::Relaxed);
            if self
                .client
                .send_media(guild_id, channel_id, encrypted, &[author])
                .is_err()
            {
                self.media_failures.record();
            }
        }
    }

    /// Publish the local speaking indicator when the detector changes.
    ///
    /// Only a change is signed and broadcast, so a steady microphone does not
    /// flood the control session with identical frames.
    async fn apply_local_speaking(&self, guild_id: GuildId, channel_id: ChannelId, speaking: bool) {
        let local_author = self.client.author_id();
        let members = self.members(guild_id);
        let outcome = {
            let mut hub = self.control.lock().await;
            let snapshot = hub.voice_snapshot(guild_id, channel_id, &members, now_ms());
            let Some(mut state) = voice_call(&snapshot, channel_id).and_then(|call| {
                call.members
                    .iter()
                    .find(|member| member.author == local_author)
                    .map(|member| member.state)
            }) else {
                return;
            };
            if state.speaking == speaking {
                return;
            }
            state.speaking = speaking;
            hub.set_voice_state(guild_id, channel_id, &members, state, now_ms())
        };
        match outcome {
            Ok(outcome) => self.broadcast_outcome(outcome),
            Err(error) => self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "voice state",
                format!("could not publish the speaking state: {error}"),
            ),
        }
    }

    /// Store and publish one stats sample from a call's audio service.
    async fn apply_audio_stats(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        generation: u64,
        stats: AudioStats,
    ) {
        // The engine keeps a decoder per author it has ever heard, so its own
        // `peers` counter cannot fall when a member leaves. Cap it by the live
        // call roster, which keeps the value meaning "mixed streams" while
        // letting it drop as members leave.
        let roster_peers = self.voice_peer_count(guild_id, channel_id).await;
        let local_author = self.client.author_id();
        // On a hub-role call the mixer, not the mesh engine, decodes each
        // leaf. The engine's `peers` counter is therefore zero on the hub, but
        // the frontend and the verification harness need the number of leaf
        // streams the hub is actually decoding, which is the mixer's non-local
        // source count.
        let mixer_peers = {
            let mixers = self.hub_mixers();
            mixers.get(&(guild_id, channel_id)).map(|state| {
                state
                    .mixer
                    .sources()
                    .filter(|author| *author != local_author)
                    .count()
            })
        };
        // A leaf reports its current media peers, not every author its engine
        // ever decoded. A member that was a direct pair before the third
        // member joined leaves a decoder behind, and the engine's historical
        // counter would then claim two downlinks where the star has one.
        let peers = match mixer_peers {
            Some(count) => count,
            None => match self.call_snapshot(guild_id, channel_id).await {
                Some(call) if local_is_hub(&call, local_author) => 0,
                Some(call) => media_peers(&call, local_author).len(),
                None => stats.peers.min(roster_peers),
            },
        };
        let (transport, rtt_ms, packets_lost, relayed) =
            self.rtc_metrics(guild_id, channel_id).await;
        let (e2ee, epoch, e2ee_errors) = self.e2ee_metrics(guild_id, channel_id).await;
        let stats = VoiceAudioStats {
            backend: stats.backend,
            captured: stats.captured,
            packets: stats.packets,
            played: stats.played,
            concealed: stats.concealed,
            lost: stats.lost,
            decode_errors: stats.decode_errors,
            peers,
            xruns: stats.xruns,
            latency_ms: stats.latency_ms,
            peak: stats.peak,
            rms: stats.rms,
            transport,
            rtt_ms,
            packets_lost,
            relayed,
            e2ee,
            epoch,
            e2ee_errors,
        };
        {
            let mut audio = self.audio.lock().await;
            let Some(call) = audio.get_mut(&(guild_id, channel_id)) else {
                return;
            };
            if call.generation != generation {
                // A stopped service's last sample must not overwrite the
                // counters of the service that replaced it.
                return;
            }
            call.last_stats = Some(stats.clone());
        }
        self.emit(DaemonEvent::VoiceAudio {
            guild_id,
            channel_id,
            stats,
        });
    }

    /// The call members this daemon currently has a usable transport peer for,
    /// or `None` when the transport has not produced a peer snapshot yet.
    ///
    /// A dead hub stays in the 20-second voice roster after it disappears, so
    /// the roster alone is not a liveness signal. Sealed-state reporting and
    /// the hub's listener set use this to avoid treating a dead member as a
    /// peer that should still have a key.
    async fn live_media_authors(&self, guild_id: GuildId) -> Option<HashSet<AuthorId>> {
        if self.rtc_enabled() {
            let rtc = self.rtc.lock().await;
            let mut live = HashSet::new();
            for ((call_guild, _), call) in rtc.iter() {
                if *call_guild != guild_id {
                    continue;
                }
                for peer in &call.last_stats.peers {
                    if peer.state == "connected" {
                        live.insert(peer.author);
                    }
                }
            }
            return (!live.is_empty()).then_some(live);
        }
        let members = self.client.list_members(guild_id).ok()?;
        let state = self.client.connection_state(guild_id);
        let live: HashSet<AuthorId> = state
            .peers
            .iter()
            .filter_map(|peer| author_for_peer(&peer.to_string(), &members))
            .collect();
        (!live.is_empty()).then_some(live)
    }

    /// The live voice call for one channel, if the local session has it.
    async fn call_snapshot(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> Option<VoiceSnapshot> {
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        voice_call(&snapshot, channel_id)
    }

    /// Other members named by the call's current voice snapshot.
    async fn voice_peer_count(&self, guild_id: GuildId, channel_id: ChannelId) -> usize {
        let local_author = self.client.author_id();
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        voice_call(&snapshot, channel_id)
            .map(|call| {
                call.members
                    .iter()
                    .filter(|member| member.author != local_author)
                    .count()
            })
            .unwrap_or(0)
    }

    /// Accept a media frame from the network and hand it to the local engine.
    async fn handle_incoming_media(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: ovvc_media::MediaPacket,
        from: &str,
    ) {
        // With WebRTC selected the libp2p media path is closed: an incoming
        // frame could only be a stale sender and must not enter the call.
        if self.rtc_enabled() {
            return;
        }
        let members = self.client.list_members(guild_id).unwrap_or_default();
        let Some(author) = author_for_peer(from, &members) else {
            return;
        };
        // The transport proves who sent the frame; the packet's own author
        // field is not trusted unless the two agree.
        if author != packet.author {
            return;
        }
        // A guild member outside the call cannot inject audio into it.
        let authors: Vec<AuthorId> = members.iter().map(|member| member.author).collect();
        let in_call = {
            let mut hub = self.control.lock().await;
            let snapshot = hub.voice_snapshot(guild_id, channel_id, &authors, now_ms());
            voice_call(&snapshot, channel_id)
                .is_some_and(|call| call.members.iter().any(|member| member.author == author))
        };
        if !in_call {
            return;
        }
        // A star member exchanges media only with its topology neighbour. A
        // leaf that receives a direct leaf-to-leaf frame rejects it before any
        // key or decode work, which is the media-plane half of "no leaf to
        // leaf media".
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &authors, now_ms())
        };
        let Some(call) = voice_call(&snapshot, channel_id) else {
            return;
        };
        let local_author = self.client.author_id();
        if !media_peers(&call, local_author).contains(&author) {
            // A direct leaf-to-leaf frame in a star is a protocol violation.
            // It is dropped before decrypt; it is not a hub health sample.
            return;
        }
        // The payload is ciphertext on the wire. A missing key, wrong epoch,
        // replay or bad tag drops the frame and is counted; it never reaches
        // the decoder as plaintext.
        let Some(packet) = self
            .decrypt_media(guild_id, channel_id, &packet, author)
            .await
        else {
            return;
        };
        self.media_bytes_received
            .fetch_add(packet.payload.len() as u64, Ordering::Relaxed);
        if call.hub == Some(local_author) && call.topology.is_star() {
            // The hub owns the mix and its own playback for this call. Feeding
            // the same leaf packet to the mesh engine as well would play it
            // twice.
            self.reconcile_hub_mixer(guild_id, &call, local_author);
            self.hub_receive(guild_id, channel_id, &packet);
            return;
        }
        let audio = self.audio.lock().await;
        if let Some(call) = audio.get(&(guild_id, channel_id)) {
            let _ = call.handle.send(AudioCommand::Media(packet));
        }
    }

    // --- star hub media ---------------------------------------------------

    fn hub_mixers(&self) -> std::sync::MutexGuard<'_, HashMap<(GuildId, ChannelId), HubCallState>> {
        self.hub_mix
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Keep the hub mixer's listeners and sources equal to the current call.
    ///
    /// A listener is a leaf that receives a per-listener mix; a source is any
    /// member whose frames feed the mix, including the hub's own microphone.
    /// The mixer is removed when this daemon stops being the hub, which also
    /// stops the local playback path from mixing stale sources.
    fn reconcile_hub_mixer(&self, guild_id: GuildId, call: &VoiceSnapshot, local: AuthorId) {
        let key = (guild_id, call.channel_id);
        if !local_is_hub(call, local) {
            self.hub_mixers().remove(&key);
            return;
        }
        let members: HashSet<AuthorId> = call.members.iter().map(|member| member.author).collect();
        let listeners = media_peers(call, local);
        let mut mixers = self.hub_mixers();
        if !mixers.contains_key(&key) {
            let config = match self.audio_engine_config() {
                Ok(config) => config,
                Err(message) => {
                    self.log_only(ErrorClass::Operational, Some(guild_id), "hub mixer", message);
                    return;
                }
            };
            let mixer = match HubMixer::new(local, &config) {
                Ok(mixer) => mixer,
                Err(error) => {
                    self.log_only(
                        ErrorClass::Operational,
                        Some(guild_id),
                        "hub mixer",
                        format!("could not build the hub mixer: {error}"),
                    );
                    return;
                }
            };
            mixers.insert(
                key,
                HubCallState {
                    mixer,
                    seq: HashMap::new(),
                },
            );
        }
        let Some(state) = mixers.get_mut(&key) else {
            return;
        };
        for listener in &listeners {
            match state.mixer.add_listener(*listener) {
                Ok(_) => {}
                Err(error) => {
                    self.log_only(
                        ErrorClass::Operational,
                        Some(guild_id),
                        "hub mixer",
                        format!("could not add listener {}: {error}", listener.short()),
                    );
                }
            }
        }
        let registered: Vec<AuthorId> = state.mixer.listeners().collect();
        for listener in registered {
            if !listeners.contains(&listener) {
                state.mixer.remove_listener(listener);
                state.seq.remove(&listener);
            }
        }
        let sources: Vec<AuthorId> = state.mixer.sources().collect();
        for source in sources {
            if source != local && !members.contains(&source) {
                state.mixer.remove_source(source);
            }
        }
    }

    /// Feed one decrypted packet into the local hub mixer.
    fn hub_receive(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: &ovvc_media::MediaPacket,
    ) {
        let mut mixers = self.hub_mixers();
        let Some(state) = mixers.get_mut(&(guild_id, channel_id)) else {
            return;
        };
        // A rejected frame is a per-frame condition (an oversized payload or
        // a call above the mixer's source limit), not a health sample. The
        // mixer's own counter records it.
        let _ = state
            .mixer
            .receive(packet.author, packet.seq, packet.payload.clone());
    }

    /// Pump every hub mixer once per frame period.
    ///
    /// This is the hub's media clock: one tick decodes each source once, mixes
    /// the hub's local playback from the leaves, and produces one encoded
    /// frame per listener. It is called by the media ticker in `main.rs`, not
    /// by the control tick, because the output cadence is the Opus frame
    /// period rather than one second.
    pub async fn media_tick(&self) {
        let keys: Vec<(GuildId, ChannelId)> = self.hub_mixers().keys().copied().collect();
        for (guild_id, channel_id) in keys {
            self.pump_hub_call(guild_id, channel_id).await;
        }
    }

    async fn pump_hub_call(&self, guild_id: GuildId, channel_id: ChannelId) {
        let local = self.client.author_id();
        let members = self.members(guild_id);
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
        };
        let Some(call) = voice_call(&snapshot, channel_id) else {
            self.hub_mixers().remove(&(guild_id, channel_id));
            return;
        };
        if !local_is_hub(&call, local) {
            self.hub_mixers().remove(&(guild_id, channel_id));
            return;
        }
        self.reconcile_hub_mixer(guild_id, &call, local);
        let now = now_ms();
        let (playback, outputs) = {
            let mut mixers = self.hub_mixers();
            let Some(state) = mixers.get_mut(&(guild_id, channel_id)) else {
                return;
            };
            let frame = match state.mixer.pump() {
                Ok(Some(frame)) => frame,
                Ok(None) => return,
                Err(error) => {
                    self.media_failures.record();
                    self.log_only(
                        ErrorClass::Operational,
                        Some(guild_id),
                        "hub mixer",
                        format!("mixer pump failed: {error}"),
                    );
                    return;
                }
            };
            let mut outputs = Vec::with_capacity(frame.listener_frames.len());
            for ListenerFrame { listener, payload, .. } in frame.listener_frames {
                let seq = state.seq.entry(listener).or_insert(0);
                let packet = match ovvc_media::MediaPacket::new(
                    ovvc_media::MediaKind::Audio,
                    CallId::for_channel(channel_id),
                    local,
                    *seq,
                    now,
                    payload,
                ) {
                    Ok(packet) => packet,
                    Err(_) => {
                        self.media_failures.record();
                        continue;
                    }
                };
                *seq = seq.wrapping_add(1);
                outputs.push((listener, packet));
            }
            (frame.local_playback, outputs)
        };

        if let Some(samples) = playback {
            let audio = self.audio.lock().await;
            if let Some(call) = audio.get(&(guild_id, channel_id)) {
                let _ = call.handle.send(AudioCommand::Playback(samples));
            }
        }

        for (listener, packet) in outputs {
            if self.rtc_enabled() {
                self.send_rtc_media_to(guild_id, channel_id, &packet, listener)
                    .await;
            } else {
                self.send_sealed_media_to(guild_id, channel_id, &packet, listener)
                    .await;
            }
        }
    }

    /// Send one encoded packet to `target` over the libp2p path, sealing it
    /// with that target's pairwise key.
    async fn send_sealed_media_to(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: &ovvc_media::MediaPacket,
        target: AuthorId,
    ) {
        let Some(keyring) = self.call_keyring(guild_id, channel_id).await else {
            self.record_crypto_errors(guild_id, channel_id, 1).await;
            return;
        };
        let sealed = match keyring.read() {
            Ok(keys) => keys.encrypt_for(target, &packet.payload),
            Err(_) => Err(CryptoError::NoPeerKey),
        };
        let ciphertext = match sealed {
            Ok(ciphertext) => ciphertext,
            Err(_) => {
                self.record_crypto_errors(guild_id, channel_id, 1).await;
                return;
            }
        };
        let encrypted = match ovvc_media::MediaPacket::new(
            packet.kind,
            packet.call_id,
            packet.author,
            packet.seq,
            packet.timestamp_ms,
            ciphertext,
        ) {
            Ok(encrypted) => encrypted,
            Err(_) => {
                self.record_crypto_errors(guild_id, channel_id, 1).await;
                return;
            }
        };
        self.media_bytes_sent
            .fetch_add(encrypted.payload.len() as u64, Ordering::Relaxed);
        if self
            .client
            .send_media(guild_id, channel_id, encrypted, &[target])
            .is_err()
        {
            self.media_failures.record();
        }
    }

    /// Send one encoded packet to `target` over WebRTC. The manager seals it
    /// for the target, so the payload passed here is still plaintext Opus.
    async fn send_rtc_media_to(
        &self,
        guild_id: GuildId,
        channel_id: ChannelId,
        packet: &ovvc_media::MediaPacket,
        target: AuthorId,
    ) {
        let rtc = self.rtc.lock().await;
        let Some(call) = rtc.get(&(guild_id, channel_id)) else {
            return;
        };
        if call
            .manager
            .send_audio_to(packet, &[target])
            .await
            .is_err()
        {
            self.media_failures.record();
        }
    }

    /// Read the real capacity sample the election will advertise.
    ///
    /// WebRTC exposes RTP byte counters and RTT, so the transport fields come
    /// from the last peer snapshot. The libp2p path has no such counters, so
    /// the daemon's own payload counters stand in; CPU, memory, active calls,
    /// uptime and failures are the same on both.
    async fn sample_capacity(&self) -> Priority {
        let (rtt_ms, packets_lost, bytes_sent, bytes_received) = if self.rtc_enabled() {
            let rtc = self.rtc.lock().await;
            let mut rtt = 0u32;
            let mut lost = 0i64;
            let mut sent = 0u64;
            let mut received = 0u64;
            for call in rtc.values() {
                for peer in &call.last_stats.peers {
                    rtt = rtt.max(peer.rtt_ms);
                    lost = lost.saturating_add(peer.packets_lost);
                    sent = sent.saturating_add(peer.bytes_sent);
                    received = received.saturating_add(peer.bytes_received);
                }
            }
            (rtt, lost, sent, received)
        } else {
            (
                0,
                0,
                self.media_bytes_sent.load(Ordering::Relaxed),
                self.media_bytes_received.load(Ordering::Relaxed),
            )
        };
        let active_calls = self.audio.lock().await.len().min(u8::MAX as usize) as u8;
        let input = SampleInput {
            transport: TransportSample {
                rtt_ms,
                packets_lost,
                bytes_sent,
                bytes_received,
            },
            active_calls,
            failures: self.media_failures.total(),
            uptime_secs: now_unix().saturating_sub(self.started_at),
        };
        let mut sampler = self
            .capacity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        sampler.sample(now_ms(), input)
    }

    // --- request dispatch ------------------------------------------------

    pub async fn dispatch(&self, op: Op) -> Result<Reply, IpcError> {
        match op {
            Op::Identity => Ok(self.identity_reply()),
            Op::Status => Ok(Reply::Status(self.status())),
            Op::ListGuilds => Ok(Reply::Guilds(self.client.list_guilds().map_err(to_ipc)?)),
            Op::GetGuild { guild_id } => Ok(Reply::Guild(
                self.client.get_guild(guild_id).map_err(to_ipc)?,
            )),
            Op::CreateGuild { name } => Ok(Reply::GuildCreated(
                self.client.create_guild(&name).map_err(to_ipc)?,
            )),
            Op::ListChannels { guild_id } => Ok(Reply::Channels(
                self.client.list_channels(guild_id).map_err(to_ipc)?,
            )),
            Op::CreateChannel { guild_id, name } => Ok(Reply::Channel(
                self.client
                    .create_channel(guild_id, &name)
                    .map_err(to_ipc)?,
            )),
            Op::CreateChannelIn {
                guild_id,
                name,
                kind,
                parent,
            } => Ok(Reply::Channel(
                self.client
                    .create_channel_in(guild_id, &name, kind, parent)
                    .map_err(to_ipc)?,
            )),
            Op::CreateCategory { guild_id, name } => Ok(Reply::Category(
                self.client
                    .create_category(guild_id, &name)
                    .map_err(to_ipc)?,
            )),
            Op::RenameChannel {
                guild_id,
                channel_id,
                name,
            } => {
                self.client
                    .rename_channel(guild_id, channel_id, &name)
                    .map_err(to_ipc)?;
                Ok(Reply::Unit)
            }
            Op::MoveChannel {
                guild_id,
                channel_id,
                parent,
            } => {
                self.client
                    .move_channel(guild_id, channel_id, parent)
                    .map_err(to_ipc)?;
                Ok(Reply::Unit)
            }
            Op::ChannelTree { guild_id } => Ok(Reply::ChannelTree(ChannelTreeDto {
                categories: self.client.list_categories(guild_id).map_err(to_ipc)?,
                channels: self.client.list_channel_views(guild_id).map_err(to_ipc)?,
            })),
            Op::NotificationSettings => {
                Ok(Reply::NotificationSettings(self.notification_settings()))
            }
            Op::SetDesktopNotifications { enabled } => {
                self.update_notification_settings(|settings| settings.desktop = enabled)?;
                Ok(Reply::Unit)
            }
            Op::SetDoNotDisturb { enabled } => {
                self.update_notification_settings(|settings| settings.dnd = enabled)?;
                Ok(Reply::Unit)
            }
            Op::SetQuietHours { quiet_hours } => {
                self.update_notification_settings(|settings| settings.quiet_hours = quiet_hours)?;
                Ok(Reply::Unit)
            }
            Op::SetChannelMuted { channel_id, muted } => {
                self.update_notification_settings(|settings| {
                    toggle_id(&mut settings.muted_channels, channel_id, muted);
                })?;
                Ok(Reply::Unit)
            }
            Op::SetGuildMuted { guild_id, muted } => {
                self.update_notification_settings(|settings| {
                    toggle_id(&mut settings.muted_guilds, guild_id, muted);
                })?;
                Ok(Reply::Unit)
            }
            Op::ListMembers { guild_id } => Ok(Reply::Members(
                self.client.list_members(guild_id).map_err(to_ipc)?,
            )),
            Op::SetDisplayName {
                guild_id,
                display_name,
            } => {
                self.client
                    .set_display_name(guild_id, &display_name)
                    .map_err(to_ipc)?;
                Ok(Reply::Unit)
            }
            Op::SetProfile {
                guild_id,
                about,
                pronouns,
                links,
            } => Ok(Reply::Event(
                self.client
                    .set_profile(guild_id, &about, &pronouns, &links)
                    .map_err(to_ipc)?,
            )),
            Op::GetMember { guild_id, author } => Ok(Reply::Member(
                self.client
                    .member_profile(guild_id, author)
                    .map_err(to_ipc)?,
            )),
            Op::OpenDirectMessage { context, peer } => {
                let guild = self
                    .client
                    .open_direct_message(context, peer)
                    .await
                    .map_err(to_ipc)?;
                // The client started the DM node with the addresses it knew
                // in the context guild. Remember the guild as online so a
                // daemon restart resumes it on the default bind; mDNS and the
                // guild's DHT record are how it finds the peer again.
                let guild_id = guild.id;
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .entry(guild_id);
                self.mark_session_online(guild_id);
                self.remember_online(SavedNetwork::new(
                    guild_id,
                    true,
                    &NetworkOptionsDto::default(),
                ));
                Ok(Reply::GuildCreated(guild))
            }
            Op::ListDirectMessages => Ok(Reply::DirectMessages(
                self.client.list_direct_messages().map_err(to_ipc)?,
            )),
            Op::LeaveGuild { guild_id } => {
                self.stop_audio_guild(guild_id).await;
                self.stop_rtc_guild(guild_id).await;
                self.drop_guild_crypto(guild_id).await;
                self.client.leave_guild(guild_id).map_err(to_ipc)?;
                self.client.disconnect(guild_id);
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .remove(guild_id);
                // A guild that was left must not be dialled again on the next
                // start just because it was online when the person left it,
                // nor by a resume that is still running right now.
                self.forget_guild(guild_id);
                self.mark_session_offline(guild_id);
                Ok(Reply::Unit)
            }
            Op::History {
                guild_id,
                channel_id,
                limit,
            } => Ok(Reply::Events(
                self.client
                    .history(guild_id, channel_id, limit)
                    .map_err(to_ipc)?,
            )),
            Op::HistoryBefore {
                guild_id,
                channel_id,
                cursor,
                limit,
            } => Ok(Reply::HistoryPage(
                self.client
                    .history_before(guild_id, channel_id, cursor, limit)
                    .map_err(to_ipc)?,
            )),
            Op::GetEvent { guild_id, event_id } => Ok(Reply::OptionalEvent(
                self.client.get_event(guild_id, event_id).map_err(to_ipc)?,
            )),
            Op::PostMessage {
                guild_id,
                channel_id,
                body,
            } => Ok(Reply::Event(
                self.client
                    .post_message(guild_id, channel_id, &body)
                    .map_err(to_ipc)?,
            )),
            Op::PostMessageFull {
                guild_id,
                channel_id,
                body,
                reply_to,
                mentions,
            } => Ok(Reply::Event(
                self.client
                    .post_message_full(guild_id, channel_id, &body, reply_to, &mentions)
                    .map_err(to_ipc)?,
            )),
            Op::PostMessageRich {
                guild_id,
                channel_id,
                body,
                reply_to,
                mentions,
                attachments,
            } => Ok(Reply::Event(
                self.client
                    .post_message_rich(
                        guild_id,
                        channel_id,
                        &body,
                        reply_to,
                        &mentions,
                        &attachments,
                    )
                    .map_err(to_ipc)?,
            )),
            Op::PutBlob { bytes } => {
                Ok(Reply::BlobId(self.client.put_blob(&bytes).map_err(to_ipc)?))
            }
            Op::GetBlob { hash } => Ok(Reply::Blob(self.client.get_blob(hash).map_err(to_ipc)?)),
            Op::HasBlob { hash } => Ok(Reply::Bool(self.client.has_blob(hash).map_err(to_ipc)?)),
            Op::FetchBlob { guild_id, hash } => Ok(Reply::Blob(Some(
                self.client
                    .fetch_blob(guild_id, hash)
                    .await
                    .map_err(to_ipc)?,
            ))),
            Op::EditMessage {
                guild_id,
                target,
                body,
            } => Ok(Reply::Event(
                self.client
                    .edit_message(guild_id, target, &body)
                    .map_err(to_ipc)?,
            )),
            Op::DeleteMessage { guild_id, target } => Ok(Reply::Event(
                self.client
                    .delete_message(guild_id, target)
                    .map_err(to_ipc)?,
            )),
            Op::AddReaction {
                guild_id,
                target,
                emoji,
            } => Ok(Reply::Event(
                self.client
                    .add_reaction(guild_id, target, &emoji)
                    .map_err(to_ipc)?,
            )),
            Op::RemoveReaction {
                guild_id,
                target,
                emoji,
            } => Ok(Reply::Event(
                self.client
                    .remove_reaction(guild_id, target, &emoji)
                    .map_err(to_ipc)?,
            )),
            Op::MarkRead {
                guild_id,
                channel_id,
                event_id,
            } => Ok(Reply::Event(
                self.client
                    .mark_read(guild_id, channel_id, event_id)
                    .map_err(to_ipc)?,
            )),
            Op::UnreadCounts { guild_id } => Ok(Reply::Unread(
                self.client.unread_counts(guild_id).map_err(to_ipc)?,
            )),
            Op::Search {
                guild_id,
                query,
                limit,
            } => Ok(Reply::Search(
                self.client
                    .search(guild_id, &query, limit)
                    .map_err(to_ipc)?
                    .into_iter()
                    .filter_map(|hit| search_hit(guild_id, hit))
                    .collect(),
            )),
            Op::CreateInvite {
                guild_id,
                peers,
                expires_in_secs,
            } => {
                // An invite with no address cannot be dialled by anyone, so
                // minting one only hands the user a dead code. Refuse here as
                // well as in the GUI, because the CLI talks to this op too.
                let dialable: Vec<String> = peers
                    .into_iter()
                    .filter(|address| {
                        !address.contains("/ip4/0.0.0.0/") && !address.contains("/ip6/::/")
                    })
                    .collect();
                if dialable.is_empty() {
                    return Err(IpcError::invalid(
                        "an invite needs at least one dialable address; go online first",
                    ));
                }
                Ok(Reply::Invite(
                    self.client
                        .create_invite(guild_id, dialable, expires_in_secs)
                        .map_err(to_ipc)?,
                ))
            }
            Op::JoinGuild { invite, options } => {
                let invite = Invite::decode(&invite).map_err(IpcError::from)?;
                // The invite's addresses are appended to `options` by the
                // client for this call only; save the union so a restart can
                // dial the inviter again without a fresh code.
                let mut intent = SavedNetwork::new(invite.body.guild_id, true, &options);
                for address in &invite.body.addresses {
                    if !intent.dial.contains(address) {
                        intent.dial.push(address.clone());
                    }
                }
                let options = network_options(options)?;
                let guild = self
                    .client
                    .join_guild(&invite, options)
                    .await
                    .map_err(to_ipc)?;
                let guild_id = guild.id;
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .entry(guild_id);
                self.mark_session_online(guild_id);
                self.remember_online(intent);
                Ok(Reply::Guild(Some(guild)))
            }
            Op::Connect { guild_id, options } => {
                let intent = SavedNetwork::new(guild_id, true, &options);
                let options = network_options(options)?;
                self.client
                    .connect(guild_id, options)
                    .await
                    .map_err(to_ipc)?;
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .entry(guild_id);
                self.mark_session_online(guild_id);
                self.remember_online(intent);
                Ok(Reply::Unit)
            }
            Op::Disconnect { guild_id } => {
                self.stop_audio_guild(guild_id).await;
                self.stop_rtc_guild(guild_id).await;
                self.drop_guild_crypto(guild_id).await;
                self.client.disconnect(guild_id);
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .remove(guild_id);
                // The saved intent stays (the next start rejoins), but resume
                // must not put this guild back up during this session.
                self.mark_session_offline(guild_id);
                Ok(Reply::Unit)
            }
            Op::ConnectionState { guild_id } => {
                Ok(Reply::Connection(self.connection_state(guild_id)))
            }
            Op::Lookup { guild_id, author } => {
                self.client.lookup(guild_id, author).map_err(to_ipc)?;
                Ok(Reply::Unit)
            }
            Op::SetTyping {
                guild_id,
                channel_id,
                typing,
            } => {
                let members = self.members(guild_id);
                let outcome = {
                    let mut hub = self.control.lock().await;
                    hub.set_typing(
                        guild_id,
                        SessionId::for_guild(guild_id),
                        &members,
                        channel_id,
                        typing,
                        now_ms(),
                    )
                    .map_err(control_error)?
                };
                self.broadcast_outcome(outcome);
                Ok(Reply::Unit)
            }
            Op::SetPresence { guild_id, status } => {
                let members = self.members(guild_id);
                let outcome = {
                    let mut hub = self.control.lock().await;
                    hub.set_presence(
                        guild_id,
                        SessionId::for_guild(guild_id),
                        &members,
                        status,
                        now_ms(),
                    )
                    .map_err(control_error)?
                };
                self.broadcast_outcome(outcome);
                Ok(Reply::Unit)
            }
            Op::ControlSnapshot {
                guild_id,
                session_id,
            } => {
                let members = self.members(guild_id);
                let mut hub = self.control.lock().await;
                Ok(Reply::Control(hub.snapshot(
                    guild_id,
                    session_id,
                    &members,
                    now_ms(),
                )))
            }
            Op::ControlJoin {
                guild_id,
                session_id,
                display_name,
            } => {
                let members = self.members(guild_id);
                let outcome = {
                    let mut hub = self.control.lock().await;
                    hub.join(guild_id, session_id, &members, &display_name, now_ms())
                        .map_err(control_error)?
                };
                let snapshot = outcome.snapshot.clone();
                self.broadcast_outcome(outcome);
                Ok(Reply::Control(snapshot))
            }
            Op::ControlLeave {
                guild_id,
                session_id,
            } => {
                let members = self.members(guild_id);
                let outcome = {
                    let mut hub = self.control.lock().await;
                    hub.leave(guild_id, session_id, &members, now_ms())
                        .map_err(control_error)?
                };
                let snapshot = outcome.snapshot.clone();
                self.broadcast_outcome(outcome);
                Ok(Reply::Control(snapshot))
            }
            Op::VoiceJoin {
                guild_id,
                channel_id,
                muted,
                deafened,
                push_to_talk,
            } => {
                if !self.is_voice_channel(guild_id, channel_id)? {
                    return Err(IpcError::invalid("not a voice channel"));
                }
                let members = self.members(guild_id);
                let local = self.client.author_id();
                let (outcome, added) = {
                    let mut hub = self.control.lock().await;
                    // Only undo the control join below when this request
                    // actually added the local member. A concurrent join that
                    // lost the start_audio race must not tear down the call the
                    // winning request just reported as joined.
                    let added = voice_call(
                        &hub.voice_snapshot(guild_id, channel_id, &members, now_ms()),
                        channel_id,
                    )
                    .map(|call| !call.members.iter().any(|member| member.author == local))
                    .unwrap_or(true);
                    let outcome = hub
                        .voice_join(
                            guild_id,
                            channel_id,
                            &members,
                            muted,
                            deafened,
                            push_to_talk,
                            now_ms(),
                        )
                        .map_err(control_error)?;
                    (outcome, added)
                };
                let snapshot = voice_call(&outcome.snapshot, channel_id);
                self.broadcast_outcome(outcome);
                // Create the call's keyring and publish its first epoch before
                // any media can flow. The join forces a rotation, so the first
                // published epoch is tied to the roster the join produced.
                self.refresh_call_keys(guild_id, channel_id, true).await;
                // The call now exists locally, so start capturing and playing
                // even if the only member is this one. If the audio service
                // cannot open, undo the join and return the reason in this
                // reply rather than leaving a silent call behind.
                if let Err(error) = self
                    .start_audio(guild_id, channel_id, muted, deafened, push_to_talk)
                    .await
                {
                    if added {
                        let members = self.members(guild_id);
                        let rolled_back = {
                            let mut hub = self.control.lock().await;
                            hub.voice_leave(guild_id, channel_id, &members, now_ms())
                        };
                        if let Ok(outcome) = rolled_back {
                            self.broadcast_outcome(outcome);
                        }
                        self.stop_audio(guild_id, channel_id).await;
                        self.stop_rtc(guild_id, channel_id).await;
                        self.drop_call_crypto(guild_id, channel_id).await;
                    }
                    return Err(IpcError::unsupported(format!(
                        "the call would have no audio and was not joined: {error}"
                    )));
                }
                // The roster join already carries these flags, but the audio
                // engine starts with them off. Apply them to the new service
                // (or an existing one, since start_audio is idempotent) so a
                // member who was muted, deafened or on push-to-talk before
                // this call does not transmit or play until they change it.
                self.set_audio_state(guild_id, channel_id, muted, deafened, push_to_talk)
                    .await;
                // The call exists now, so build the WebRTC transport and dial
                // every remote member already in it.
                self.reconcile_rtc(guild_id, channel_id).await;
                Ok(Reply::Voice(snapshot))
            }
            Op::VoiceLeave {
                guild_id,
                channel_id,
            } => {
                if !self.is_voice_channel(guild_id, channel_id)? {
                    return Err(IpcError::invalid("not a voice channel"));
                }
                let members = self.members(guild_id);
                let outcome = {
                    let mut hub = self.control.lock().await;
                    hub.voice_leave(guild_id, channel_id, &members, now_ms())
                        .map_err(control_error)?
                };
                let snapshot = voice_call(&outcome.snapshot, channel_id);
                self.broadcast_outcome(outcome);
                self.stop_audio(guild_id, channel_id).await;
                self.stop_rtc(guild_id, channel_id).await;
                self.drop_call_crypto(guild_id, channel_id).await;
                Ok(Reply::Voice(snapshot))
            }
            Op::SetVoiceState {
                guild_id,
                channel_id,
                muted,
                deafened,
                push_to_talk,
                speaking,
                video,
                screen_share,
            } => {
                if !self.is_voice_channel(guild_id, channel_id)? {
                    return Err(IpcError::invalid("not a voice channel"));
                }
                let members = self.members(guild_id);
                let state = ovvc_control::VoiceMemberState {
                    muted,
                    deafened,
                    push_to_talk,
                    speaking,
                    video,
                    screen_share,
                };
                let outcome = {
                    let mut hub = self.control.lock().await;
                    hub.set_voice_state(guild_id, channel_id, &members, state, now_ms())
                        .map_err(control_error)?
                };
                let snapshot = voice_call(&outcome.snapshot, channel_id);
                self.broadcast_outcome(outcome);
                self.set_audio_state(guild_id, channel_id, muted, deafened, push_to_talk)
                    .await;
                Ok(Reply::Voice(snapshot))
            }
            Op::SetPushToTalkHeld {
                guild_id,
                channel_id,
                held,
            } => {
                if !self.is_voice_channel(guild_id, channel_id)? {
                    return Err(IpcError::invalid("not a voice channel"));
                }
                self.set_push_to_talk_held(guild_id, channel_id, held).await;
                Ok(Reply::Unit)
            }
            Op::VoiceSnapshot {
                guild_id,
                channel_id,
            } => {
                // A text channel never carries a call; report that rather than
                // creating a session for it.
                if !self.is_voice_channel(guild_id, channel_id).unwrap_or(false) {
                    return Ok(Reply::Voice(None));
                }
                let members = self.members(guild_id);
                let snapshot = {
                    let mut hub = self.control.lock().await;
                    hub.voice_snapshot(guild_id, channel_id, &members, now_ms())
                };
                Ok(Reply::Voice(voice_call(&snapshot, channel_id)))
            }
            Op::VoiceAudio {
                guild_id,
                channel_id,
            } => {
                let stats = self
                    .audio
                    .lock()
                    .await
                    .get(&(guild_id, channel_id))
                    .and_then(|call| call.last_stats.clone());
                Ok(Reply::VoiceAudio(stats))
            }
            Op::ShutdownDaemon => Ok(Reply::Unit),
            Op::Capabilities => Ok(Reply::Capabilities(self.capabilities())),
            Op::Policy { guild_id } => {
                // A stale or invented guild id must be distinguishable from a
                // guild that is present and using the default policy.
                if self.client.get_guild(guild_id).map_err(to_ipc)?.is_none() {
                    return Err(IpcError::not_found(format!("commune {guild_id}")));
                }
                Ok(Reply::Policy(self.policy_view()))
            }
        }
    }

    /// The guild policy read model for one guild.
    ///
    /// Signed policy events do not exist yet, so the guild half is the shipped
    /// `standard()` preset, documented here as the default until they land.
    /// The client floor and the effective policy are real: they come from this
    /// daemon's configuration and the same `with_client_floor` the rest of the
    /// code uses. This only reports values, it does not enforce them.
    fn policy_view(&self) -> PolicyView {
        // Signed policy events are Phase 5. Until they land, the configured
        // media policy stands in for the commune's, so the read model and the
        // control hub derive the same call scope and hub choice.
        let mut guild = GuildPolicy::standard();
        guild.media = self.config.policy.media.clone();
        let client_floor = self.config.policy.client_floor;
        let effective = guild.with_client_floor(&client_floor);
        let raised_by_client = raised_axes(&guild, &effective);
        PolicyView {
            guild,
            client_floor,
            effective,
            raised_by_client,
        }
    }

    fn identity_reply(&self) -> Reply {
        let peer_id = self
            .client
            .list_guilds()
            .unwrap_or_default()
            .into_iter()
            .find_map(|guild| self.client.peer_id(guild.id))
            .map(|peer| peer.to_string());
        Reply::Identity {
            author_id: self.client.author_id(),
            public_key_hex: self.client.identity().public_key_hex(),
            peer_id,
        }
    }

    /// What this build actually compiled and selected.
    ///
    /// `auto` is reported as `auto` on a build that can try PipeWire, because
    /// it silently falls back to the null backend when the device or server
    /// cannot open. The per-call stats name the backend that actually started,
    /// and [`resolved_backend`](ovvc_audio::resolved_backend) only names
    /// `null` when the build has no live backend to try. Video is always
    /// descriptor-only in this tree and is reported as such rather than as a
    /// working engine.
    pub fn capabilities(&self) -> DaemonCapabilities {
        let requested = BackendKind::parse(&self.config.audio.backend).unwrap_or_default();
        let mut features = Vec::new();
        if cfg!(feature = "notify") {
            features.push("notify".to_string());
        }
        if cfg!(feature = "tray") {
            features.push("tray".to_string());
        }
        if ovvc_audio::pipewire_compiled() {
            features.push("live-audio".to_string());
        }
        features.push("e2ee".to_string());
        DaemonCapabilities {
            audio_backends_compiled: ovvc_audio::compiled_backends()
                .into_iter()
                .map(|name| name.to_string())
                .collect(),
            audio_backend_selected: ovvc_audio::resolved_backend(requested).to_string(),
            audio_codecs: vec!["opus".to_string()],
            media_transports_compiled: vec!["webrtc".to_string(), "libp2p".to_string()],
            media_transport_selected: self.config.media.transport.clone(),
            features,
            video_descriptor_only: true,
        }
    }

    /// The protocol capability names this daemon serves, for `Welcome`.
    ///
    /// Computing the list here keeps the handshake tied to the daemon that is
    /// actually running rather than to a bare constant copied into `main`.
    pub fn capability_names(&self) -> Vec<String> {
        DAEMON_CAPABILITIES
            .iter()
            .map(|name| (*name).to_string())
            .collect()
    }

    fn status(&self) -> DaemonStatusDto {
        let connected_guilds = self
            .client
            .list_guilds()
            .unwrap_or_default()
            .into_iter()
            .filter(|guild| self.client.is_connected(guild.id))
            .map(|guild| guild.id)
            .collect();
        DaemonStatusDto {
            version: env!("CARGO_PKG_VERSION").to_string(),
            pid: std::process::id(),
            data_dir: self.data_dir.display().to_string(),
            socket: self.socket.display().to_string(),
            started_at: self.started_at,
            uptime_secs: now_unix().saturating_sub(self.started_at),
            author_id: self.client.author_id(),
            connected_guilds,
            log_path: self.log_path(),
        }
    }

    /// Connected peers across every guild, for the tray status line.
    #[cfg(feature = "tray")]
    pub fn peer_count(&self) -> usize {
        self.client
            .list_guilds()
            .unwrap_or_default()
            .into_iter()
            .map(|guild| self.client.connection_state(guild.id).peers.len())
            .sum()
    }

    fn members(&self, guild_id: GuildId) -> Vec<AuthorId> {
        self.client
            .list_members(guild_id)
            .unwrap_or_default()
            .into_iter()
            .map(|member| member.author)
            .collect()
    }

    /// Whether `channel_id` is a voice channel in this guild. Voice ops refuse
    /// a text channel rather than creating a control session that can never
    /// carry audio.
    fn is_voice_channel(&self, guild_id: GuildId, channel_id: ChannelId) -> Result<bool, IpcError> {
        Ok(self
            .client
            .list_channel_views(guild_id)
            .map_err(to_ipc)?
            .into_iter()
            .any(|view| view.id == channel_id && view.kind == ChannelKind::Voice))
    }

    pub fn connection_state(&self, guild_id: GuildId) -> ConnectionStateDto {
        let members = self.client.list_members(guild_id).unwrap_or_default();
        // Read the node's own snapshot rather than the daemon's event-fed
        // copy. The event stream is best effort: a PeerConnected that lands
        // before the pump subscribes, or a burst that overflows the broadcast
        // channel, used to leave this reply saying 0 peers beside a perfectly
        // healthy connection for the rest of the daemon's life.
        let live = self.client.connection_state(guild_id);
        let peers = live
            .peers
            .into_iter()
            .map(|peer| {
                let peer = peer.to_string();
                PeerSnapshot {
                    author: author_for_peer(&peer, &members),
                    peer,
                    connected: true,
                    addresses: Vec::new(),
                }
            })
            .collect();
        ConnectionStateDto {
            guild_id,
            connected: live.connected,
            peer_id: live.peer_id.map(|peer| peer.to_string()),
            listen_addresses: live
                .listen_addresses
                .into_iter()
                .map(|address| address.to_string())
                .collect(),
            external_addresses: live
                .external_addresses
                .into_iter()
                .map(|address| address.to_string())
                .collect(),
            peers,
        }
    }

    // --- network event fanout -------------------------------------------

    pub async fn handle_client_event(&self, event: ClientEvent) {
        match event {
            ClientEvent::Listening { guild_id, address } => {
                let address = address.to_string();
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .add_listen(guild_id, address.clone());
                self.emit(DaemonEvent::Listening { guild_id, address });
            }
            ClientEvent::ExternalAddr { guild_id, address } => {
                let address = address.to_string();
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .add_external(guild_id, address.clone());
                self.emit(DaemonEvent::ExternalAddr { guild_id, address });
            }
            ClientEvent::ExternalAddrRemoved { guild_id, address } => {
                let address = address.to_string();
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .remove_external(guild_id, &address);
                self.emit(DaemonEvent::ExternalAddrRemoved { guild_id, address });
            }
            ClientEvent::PeerConnected { guild_id, peer } => {
                let peer = peer.to_string();
                let author = author_for_peer(
                    &peer,
                    &self.client.list_members(guild_id).unwrap_or_default(),
                );
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .add_peer(guild_id, peer.clone());
                self.emit(DaemonEvent::PeerConnected {
                    guild_id,
                    peer,
                    author,
                });
            }
            ClientEvent::PeerDisconnected { guild_id, peer } => {
                let peer = peer.to_string();
                let author = author_for_peer(
                    &peer,
                    &self.client.list_members(guild_id).unwrap_or_default(),
                );
                self.network
                    .lock()
                    .expect("network state mutex poisoned")
                    .remove_peer(guild_id, &peer);
                self.emit(DaemonEvent::PeerDisconnected {
                    guild_id,
                    peer,
                    author,
                });
            }
            ClientEvent::HolePunch {
                guild_id,
                peer,
                success,
            } => self.emit(DaemonEvent::HolePunch {
                guild_id,
                peer: peer.to_string(),
                success,
            }),
            ClientEvent::Synced { guild_id, peer } => self.emit(DaemonEvent::Synced {
                guild_id,
                peer: peer.to_string(),
            }),
            ClientEvent::Event { guild_id, event } => {
                // The client has already created the DM guild and started its
                // node. Record the guild as online here so a daemon restart
                // resumes it even though the person never opened it from this
                // frontend, and the status bar can show it.
                if let EventKind::DirectMessageOpened { dm, .. } = &event.body.kind {
                    self.network
                        .lock()
                        .expect("network state mutex poisoned")
                        .entry(*dm);
                    self.mark_session_online(*dm);
                    self.remember_online(SavedNetwork::new(
                        *dm,
                        true,
                        &NetworkOptionsDto::default(),
                    ));
                }
                let local = event.body.author == self.client.author_id();
                self.emit(DaemonEvent::Event {
                    guild_id,
                    event: event.clone(),
                    local,
                });
                self.maybe_notify(guild_id, &event);
            }
            ClientEvent::Control { guild_id, frame } => {
                self.apply_remote_control(guild_id, frame).await;
            }
            ClientEvent::Media {
                guild_id,
                channel_id,
                packet,
                from,
            } => {
                self.handle_incoming_media(guild_id, channel_id, packet, &from)
                    .await;
            }
            ClientEvent::LookingUp { .. } | ClientEvent::PeerFound { .. } => {}
            ClientEvent::LookupFailed {
                guild_id,
                author,
                reason,
            } => self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "peer lookup",
                format!("could not find {author}: {reason}"),
            ),
            ClientEvent::Error { guild_id, message } => {
                // `ovvc-net` reports these as untyped strings, so this is the
                // one boundary where the class is inferred from the text.
                // Everything that knows its cause sets it at the emission site.
                let class = classify_client_message(&message);
                self.report(class, guild_id, "network", message);
            }
            ClientEvent::Diagnostic(diagnostic) => self.report_diagnostic(diagnostic),
            // `ClientEvent` is non-exhaustive; a newer client event that this
            // daemon does not understand is ignored rather than fatal.
            _ => {}
        }
    }

    /// Broadcast a notification when a newly imported message mentions the
    /// local author or replies to one of their messages.
    fn maybe_notify(&self, guild_id: GuildId, event: &ovvc_core::SignedEvent) {
        let local_author = self.client.author_id();
        if event.body.author == local_author {
            return;
        }
        let Some(body) = event.body.kind.message_body() else {
            return;
        };
        let Some(channel_id) = event.body.channel_id else {
            return;
        };
        let Ok(event_id) = event.id() else {
            return;
        };

        // A muted channel, a muted guild, do-not-disturb or quiet hours all
        // stop here: no frontend learns about it, so the in-app badge and the
        // desktop popup stay silent together.
        if settings::suppresses(
            &self.notification_settings(),
            guild_id,
            channel_id,
            now_unix(),
        ) {
            return;
        }

        // Only touch the roster when the text could actually name someone.
        let roster = if has_mention_syntax(body) {
            self.client.list_members(guild_id).unwrap_or_default()
        } else {
            Vec::new()
        };
        let local_display_name = roster
            .iter()
            .find(|member| member.author == local_author)
            .map(|member| member.display_name.clone())
            .unwrap_or_default();
        let reply_author = event.body.kind.reply_to().and_then(|target| {
            self.client
                .get_event(guild_id, target)
                .ok()
                .flatten()
                .map(|target| target.body.author)
        });

        let Some(reason) = notification_reason(
            event,
            local_author,
            &local_display_name,
            &roster,
            reply_author,
        ) else {
            return;
        };

        let first_time = self
            .notifications
            .lock()
            .expect("notification filter mutex poisoned")
            .first_time(event_id);
        if !first_time {
            return;
        }

        self.emit(DaemonEvent::Notification {
            guild_id,
            channel_id,
            event_id,
            author: event.body.author,
            body: body.to_string(),
            reason,
        });
    }

    /// Find the local channel whose derived voice session is `session_id`.
    ///
    /// A coordinator frame such as `Heartbeat` carries no channel id, but it
    /// travels on the session derived from one. Every live local call (audio,
    /// RTC or crypto) names a candidate channel, so one of them hashes to this
    /// session when the local author is in the call.
    async fn local_voice_channel_for_session(
        &self,
        guild_id: GuildId,
        session_id: SessionId,
    ) -> Option<ChannelId> {
        let mut keys: Vec<(GuildId, ChannelId)> =
            self.crypto.lock().await.keys().copied().collect();
        keys.extend(self.audio.lock().await.keys().copied());
        keys.extend(self.rtc.lock().await.keys().copied());
        keys.into_iter()
            .filter(|(call_guild, _)| *call_guild == guild_id)
            .find(|(call_guild, channel_id)| {
                SessionId::for_voice_channel(*call_guild, *channel_id) == session_id
            })
            .map(|(_, channel_id)| channel_id)
    }

    /// Record that a call was resynced, returning false while the per-call
    /// cooldown is still running.
    ///
    /// The decision and the timestamp live together so two frames cannot both
    /// pass the check between a read and a write.
    fn resync_slot(&self, guild_id: GuildId, channel_id: ChannelId, now: u64) -> bool {
        let key = (guild_id, channel_id);
        let mut last = self
            .voice_resync_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(at) = last.get(&key)
            && now.saturating_sub(*at) < VOICE_RESYNC_COOLDOWN_MS
        {
            return false;
        }
        last.insert(key, now);
        true
    }

    /// Decide whether a received frame shows that this daemon's call roster
    /// has diverged from the sender's.
    ///
    /// This is the late-joiner signature: the local author is in the call but
    /// the frame's author is not, even though the author is sending call
    /// traffic on the call's session. A `VoiceJoin` is repaired by announcing
    /// ourselves, because the sender has just told us who they are; every
    /// other call frame is repaired by asking the call to announce, because
    /// the sender has membership information we are missing.
    ///
    /// Only a verified frame from a guild member may trigger a repair. The
    /// session checks membership after the resync decision, so without this
    /// check a removed member or a leaked invite could name itself and force
    /// an announce and key republish for every frame it sends.
    async fn voice_resync_due(
        &self,
        guild_id: GuildId,
        members: &[AuthorId],
        frame: &ControlFrame,
    ) -> Option<VoiceResyncDue> {
        if !members.contains(&frame.from) || frame.verify().is_err() {
            return None;
        }
        if matches!(
            &frame.kind,
            ControlKind::VoiceLeave { .. } | ControlKind::VoiceSyncRequest { .. }
        ) {
            return None;
        }
        let channel_id = match frame.kind.voice_channel() {
            Some(channel_id) => channel_id,
            None if frame.session_id != SessionId::for_guild(guild_id) => {
                self.local_voice_channel_for_session(guild_id, frame.session_id)
                    .await?
            }
            None => return None,
        };
        let snapshot = {
            let mut hub = self.control.lock().await;
            hub.voice_snapshot(guild_id, channel_id, members, now_ms())
        };
        let call = voice_call(&snapshot, channel_id)?;
        let local = self.client.author_id();
        let local_in_call = call.members.iter().any(|member| member.author == local);
        let sender_in_call = call
            .members
            .iter()
            .any(|member| member.author == frame.from);
        if !local_in_call || sender_in_call {
            return None;
        }
        let action = if matches!(&frame.kind, ControlKind::VoiceJoin { .. }) {
            VoiceResync::Announce
        } else {
            VoiceResync::Request
        };
        Some(VoiceResyncDue {
            channel_id,
            action,
            allowed: self.resync_slot(guild_id, channel_id, now_ms()),
        })
    }

    /// Announce the local membership of a call and republish its current key.
    ///
    /// The key is published at once rather than waiting for the periodic
    /// republish, so the other side can install it in the same exchange that
    /// taught it this daemon is in the call.
    async fn announce_call_membership(&self, guild_id: GuildId, channel_id: ChannelId) {
        let members = self.members(guild_id);
        let outcome = {
            let mut hub = self.control.lock().await;
            hub.reannounce_voice_join(guild_id, channel_id, &members, now_ms())
        };
        match outcome {
            Ok(Some(outcome)) => self.broadcast_outcome(outcome),
            Ok(None) => return,
            Err(error) => {
                self.report(
                    ErrorClass::Operational,
                    Some(guild_id),
                    "voice resync",
                    format!("could not re-announce the call membership: {error}"),
                );
                return;
            }
        }
        if let Some(keyring) = self.call_keyring(guild_id, channel_id).await {
            let members = self.members(guild_id);
            self.publish_call_key(guild_id, channel_id, &members, keyring)
                .await;
        }
    }

    /// Ask the members of a live call to re-announce themselves.
    async fn request_voice_resync(&self, guild_id: GuildId, channel_id: ChannelId) {
        let members = self.members(guild_id);
        let outcome = {
            let mut hub = self.control.lock().await;
            hub.voice_sync_request(guild_id, channel_id, &members, now_ms())
        };
        match outcome {
            Ok(outcome) => self.broadcast_outcome(outcome),
            Err(error) => self.report(
                ErrorClass::Operational,
                Some(guild_id),
                "voice resync",
                format!("could not request a call resync: {error}"),
            ),
        }
    }

    async fn apply_remote_control(&self, guild_id: GuildId, frame: ControlFrame) {
        // Voice frames must name an existing voice channel in this guild and
        // travel on that channel's derived session. The session layer also
        // binds the two, but rejecting here avoids creating a session for a
        // channel id that does not exist.
        if let Some(channel_id) = frame.kind.voice_channel() {
            if SessionId::for_voice_channel(guild_id, channel_id) != frame.session_id {
                self.report(
                    ErrorClass::Security,
                    Some(guild_id),
                    "voice control",
                    "rejected voice control frame on the wrong session",
                );
                return;
            }
            if !self.is_voice_channel(guild_id, channel_id).unwrap_or(false) {
                self.report(
                    ErrorClass::Security,
                    Some(guild_id),
                    "voice control",
                    "rejected voice control frame for a non-voice channel",
                );
                return;
            }
        }
        let members = self.members(guild_id);
        // A late guild joiner and the members already in the call hold two
        // different rosters. The next frame from either side exposes the
        // divergence, and this is where the repair is decided. It must be
        // computed before the frame is applied, because applying a
        // `VoiceJoin` is exactly what heals one side.
        let resync = self.voice_resync_due(guild_id, &members, &frame).await;
        let outcome = {
            let mut hub = self.control.lock().await;
            hub.on_remote(&frame, &members, now_ms())
        };
        match outcome {
            Ok(outcome) => {
                self.broadcast_outcome(outcome);
                // The hub verified the frame, so an RTC signal is authentic
                // and may be handed to the media transport. A media key is
                // installed before the roster reconcile so the new epoch is
                // usable as soon as the peer connection is dialled.
                if matches!(&frame.kind, ControlKind::VoiceKey { .. }) {
                    self.handle_remote_voice_key(guild_id, &frame).await;
                }
                if frame.kind.rtc_target().is_some() {
                    self.handle_rtc_control(guild_id, &frame).await;
                } else if matches!(
                    frame.kind,
                    ControlKind::VoiceJoin { .. } | ControlKind::VoiceLeave { .. }
                ) && let Some(channel_id) = frame.kind.voice_channel()
                {
                    // Only a join or leave changes the peer set; heartbeats and
                    // state updates arrive every second and need no redial.
                    self.reconcile_rtc(guild_id, channel_id).await;
                }
                if let Some(due) = resync
                    && due.allowed
                {
                    match due.action {
                        VoiceResync::Announce => {
                            self.announce_call_membership(guild_id, due.channel_id)
                                .await;
                        }
                        VoiceResync::Request => {
                            self.request_voice_resync(guild_id, due.channel_id).await;
                        }
                    }
                }
                // A sync request is already the repair. Answer it by
                // re-announcing, which is a no-op outside the call. The
                // session accepts a sync request from a guild member outside
                // the call on purpose, so the same membership check and
                // cooldown as the automatic repair apply here.
                if let ControlKind::VoiceSyncRequest { channel_id } = &frame.kind
                    && members.contains(&frame.from)
                    && frame.verify().is_ok()
                    && self.resync_slot(guild_id, *channel_id, now_ms())
                {
                    self.announce_call_membership(guild_id, *channel_id).await;
                }
            }
            Err(error) => {
                // A frame rejected because the local call has not caught up
                // with its author is the divergence itself, not a protocol
                // error. Answering with a resync is the repair; surfacing an
                // error here would hide it under a rejected-frame message.
                // Only an authenticated divergence is a repair: a non-member
                // (`NotAMember`) must never be able to provoke an announce.
                if matches!(&error, ovvc_control::ControlError::Invalid(_))
                    && let Some(due) = resync
                {
                    if due.allowed {
                        match due.action {
                            VoiceResync::Announce => {
                                self.announce_call_membership(guild_id, due.channel_id)
                                    .await;
                            }
                            VoiceResync::Request => {
                                self.request_voice_resync(guild_id, due.channel_id).await;
                            }
                        }
                    }
                } else if is_stale_voice_control(&error, &frame, &members) {
                    // A restart or a transient stale call view can make a
                    // genuine guild member look outside the call. This is
                    // convergence noise, not a rejected frame a person should
                    // see. Log it for diagnosis and let the next join or
                    // roster exchange heal the state. A non-member never
                    // reaches here because the session returns NotAMember
                    // first.
                    self.log_only(
                        ErrorClass::Operational,
                        Some(guild_id),
                        "control frame",
                        format!("ignored stale voice control frame: {error}"),
                    );
                } else {
                    let class = classify_control_error(&error);
                    self.report(
                        class,
                        Some(guild_id),
                        "control frame",
                        format!("rejected control frame: {error}"),
                    );
                }
            }
        }
    }

    pub async fn expire_control(&self) {
        let now = now_ms();
        // Sample before the heartbeat so the frame this tick signs carries the
        // current real capacity rather than last tick's value or the constant
        // placeholder.
        let priority = self.sample_capacity().await;
        {
            let mut hub = self.control.lock().await;
            hub.set_local_priority(priority);
            // Renew the local heartbeats first: the lease and the member
            // last-seen stamps must be current before the expiry pass
            // considers dropping anything. A call with no user input then
            // stays up indefinitely.
            match hub.heartbeat(now) {
                Ok(outgoing) => {
                    for frame in outgoing {
                        self.client.broadcast_control(&frame);
                    }
                }
                Err(error) => {
                    self.report(
                        ErrorClass::Operational,
                        None,
                        "control heartbeat",
                        format!("could not sign control heartbeat: {error}"),
                    );
                }
            }
            for event in hub.expire(now) {
                self.emit(event);
            }
        }
        // A member lease can expire while the daemon still has an audio
        // service running for that call. The snapshot, not the engine, decides
        // whether this identity is still a member.
        self.reconcile_audio(now).await;
    }

    /// Stop transports whose call no longer contains the local author.
    async fn reconcile_audio(&self, now: u64) {
        // The RTC manager exists even when the audio service failed to start,
        // so its calls must be reconciled from both maps. Iterating only
        // `self.audio` left a manager and its sockets alive until shutdown.
        let mut keys: HashSet<(GuildId, ChannelId)> =
            self.audio.lock().await.keys().copied().collect();
        keys.extend(self.rtc.lock().await.keys().copied());
        // A keyring can outlive both transports if the audio service failed to
        // start, so it is a third source of calls that need reconciling.
        keys.extend(self.crypto.lock().await.keys().copied());
        let local_author = self.client.author_id();
        for (guild_id, channel_id) in keys {
            let members = self.members(guild_id);
            let snapshot = {
                let mut hub = self.control.lock().await;
                hub.voice_snapshot(guild_id, channel_id, &members, now)
            };
            let still_in_call = voice_call(&snapshot, channel_id).is_some_and(|call| {
                call.members
                    .iter()
                    .any(|member| member.author == local_author)
            });
            if !still_in_call {
                self.stop_audio(guild_id, channel_id).await;
                self.stop_rtc(guild_id, channel_id).await;
                self.drop_call_crypto(guild_id, channel_id).await;
            } else {
                // A member lease that expired changes the call's peer set even
                // though no control frame was produced.
                self.reconcile_rtc(guild_id, channel_id).await;
            }
        }
    }

    fn broadcast_outcome(&self, outcome: HubOutcome) {
        for event in outcome.events {
            self.emit(event);
        }
        for frame in outcome.outgoing {
            match frame.kind.rtc_target() {
                // SDP and ICE candidates are addressed to one author and
                // carry private addresses, so they never go to the guild.
                Some((_, to)) => self.client.send_control_to(frame.guild_id, to, &frame),
                None => self.client.broadcast_control(&frame),
            }
        }
    }
}

fn voice_call(
    snapshot: &ovvc_control::ControlSnapshot,
    channel_id: ChannelId,
) -> Option<ovvc_control::VoiceSnapshot> {
    snapshot
        .voice
        .iter()
        .find(|call| call.channel_id == channel_id)
        .cloned()
}

/// The members this author exchanges media with directly.
///
/// A mesh (and the two-member direct pair, which is a mesh) talks to every
/// other member. A flat star talks only along its parent and child edges, so a
/// leaf sees the hub and the hub sees every leaf, never a leaf-to-leaf pair.
fn media_peers(call: &VoiceSnapshot, local: AuthorId) -> Vec<AuthorId> {
    let members: Vec<AuthorId> = call.members.iter().map(|member| member.author).collect();
    match &call.topology {
        CallTopology::Mesh => members
            .into_iter()
            .filter(|author| *author != local)
            .collect(),
        CallTopology::Star { .. } => call.topology.neighbours(local),
    }
}

/// Whether this author is the media hub of a star call.
fn local_is_hub(call: &VoiceSnapshot, local: AuthorId) -> bool {
    call.topology.is_star() && call.hub == Some(local)
}

/// Whether a rejected voice frame is a stale local call view rather than a
/// frame a person should see as an error.
///
/// A daemon that has just restarted can be missing the local call or session
/// while the other side is still sending coordinator, key or RTC frames for
/// it. The session reports those as `Invalid("... outside the call")`, but
/// the sender is an authenticated guild member and the next join or roster
/// exchange repairs the view. Keep the exact messages narrow: a non-member is
/// `NotAMember`, and a malformed frame, wrong session or bad signature must
/// still reach the normal error path.
fn is_stale_voice_control(
    error: &ovvc_control::ControlError,
    frame: &ControlFrame,
    members: &[AuthorId],
) -> bool {
    if !members.contains(&frame.from) || frame.verify().is_err() {
        return false;
    }
    let ovvc_control::ControlError::Invalid(message) = error else {
        return false;
    };
    matches!(
        message.as_str(),
        "voice coordinator frame from a member outside the call"
            | "voice key from a member outside the call"
            | "rtc signalling from a member outside the call"
    )
}

fn search_hit(guild_id: GuildId, hit: CoreSearchHit) -> Option<ovvc_ipc::SearchHit> {
    let channel_id = hit.event.body.channel_id?;
    Some(ovvc_ipc::SearchHit {
        guild_id,
        channel_id,
        event: hit.event,
        body: hit.body,
    })
}

/// Write the intent file, logging rather than failing the request: a full disk
/// must not turn a working [ go online ] into an error the person cannot fix.
fn persist_network_intent(data_dir: &std::path::Path, saved: &[SavedNetwork]) {
    if let Err(error) = network_state::save_all(data_dir, saved) {
        eprintln!("ovvc-daemon: could not record the peer-network intent: {error}");
    }
}

fn network_options(options: NetworkOptionsDto) -> Result<NetworkOptions, IpcError> {
    let mut parsed = NetworkOptions {
        relay: None,
        upnp: options.upnp,
        natpmp: options.natpmp,
        ..NetworkOptions::default()
    };
    for address in options.listen {
        parsed.listen.push(address.parse().map_err(|error| {
            IpcError::invalid(format!("invalid listen address {address}: {error}"))
        })?);
    }
    for address in options.dial {
        parsed.dial.push(address.parse().map_err(|error| {
            IpcError::invalid(format!("invalid dial address {address}: {error}"))
        })?);
    }
    if let Some(relay) = options.relay {
        parsed.relay = Some(relay.parse().map_err(|error| {
            IpcError::invalid(format!("invalid relay address {relay}: {error}"))
        })?);
    }
    Ok(parsed)
}

fn author_for_peer(peer: &str, members: &[ovvc_core::MemberInfo]) -> Option<AuthorId> {
    members
        .iter()
        .find(|member| {
            ovvc_client::peer_id_for_author(member.author)
                .map(|candidate| candidate.to_string() == peer)
                .unwrap_or(false)
        })
        .map(|member| member.author)
}

/// Add an id to a preference list, or remove it, without duplicating it.
///
/// A frontend that sends the op twice (a double click, or a retry after a
/// dropped connection) must not leave two copies in the file.
fn toggle_id<T: PartialEq>(list: &mut Vec<T>, id: T, present: bool) {
    if present {
        if !list.contains(&id) {
            list.push(id);
        }
    } else if let Some(index) = list.iter().position(|item| *item == id) {
        list.remove(index);
    }
}

/// Axis names the client floor raised, in a stable display order.
///
/// `with_client_floor` can only raise an axis, so comparing the effective
/// policy against the guild policy is enough to name what changed. The order
/// is fixed so a frontend can render the list without sorting it.
fn raised_axes(guild: &GuildPolicy, effective: &GuildPolicy) -> Vec<String> {
    let mut raised = Vec::new();
    let axes: [(&str, bool); 7] = [
        ("discovery", guild.discovery != effective.discovery),
        ("transport", guild.transport != effective.transport),
        ("padding", guild.padding != effective.padding),
        (
            "author_identity",
            guild.author_identity != effective.author_identity,
        ),
        ("timestamps", guild.timestamps != effective.timestamps),
        (
            "hardware_keys",
            guild.hardware_keys != effective.hardware_keys,
        ),
        ("plugins", guild.plugins != effective.plugins),
    ];
    for (name, differs) in axes {
        if differs {
            raised.push(name.to_string());
        }
    }
    raised
}

fn to_ipc(error: ClientError) -> IpcError {
    match error {
        ClientError::Core(core) => IpcError::from(core),
        ClientError::Network(error) => IpcError::network(error.to_string()),
        ClientError::NotConnected(guild_id) => {
            IpcError::not_connected(format!("commune {guild_id} is not connected"))
        }
        ClientError::Timeout(duration) => {
            IpcError::timeout(format!("timed out after {duration:?}"))
        }
    }
}

fn control_error(error: ovvc_control::ControlError) -> IpcError {
    IpcError::invalid(error.to_string())
}

/// The taxonomy class for a rejected control frame.
///
/// Signature, replay, membership, session and freshness failures are protocol
/// security rejections. A rate limit and a media-state refusal are operational
/// conditions the control plane is expected to hit. A malformed authenticated
/// frame is a security rejection rather than a bug, because the local software
/// did not fail; the peer sent something the protocol forbids.
fn classify_control_error(error: &ovvc_control::ControlError) -> ErrorClass {
    use ovvc_control::ControlError;
    match error {
        ControlError::InvalidSignature
        | ControlError::Replay
        | ControlError::NotAMember
        | ControlError::WrongSession
        | ControlError::Stale
        | ControlError::TooLarge(_)
        | ControlError::UnsupportedVersion(_)
        | ControlError::Postcard(_)
        | ControlError::Invalid(_) => ErrorClass::Security,
        ControlError::RateLimited | ControlError::Media(_) => ErrorClass::Operational,
        ControlError::Core(error) => ErrorClass::from_core_error(error),
    }
}

/// Classify one of `ovvc-net`'s untyped error strings.
///
/// The network layer reports transport failures and rejected events through the
/// same `ClientEvent::Error` string. That is the only boundary where this code
/// inspects message text: every daemon emission site that knows its cause sets
/// the class explicitly, and `ovvc-control` uses typed errors.
fn classify_client_message(message: &str) -> ErrorClass {
    let lower = message.to_ascii_lowercase();
    if lower.contains("signature")
        || lower.contains("replay")
        || lower.contains("not a member")
        || lower.contains("rejected event")
        || lower.contains("unauthor")
        || lower.contains("invalid frame")
    {
        ErrorClass::Security
    } else if lower.contains("not found") || lower.contains("invalid input") {
        ErrorClass::User
    } else {
        ErrorClass::Operational
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::watch;

    fn test_daemon(dir: &std::path::Path) -> Daemon {
        test_daemon_with_config(dir, ovvc_config::Config::default())
    }

    fn test_daemon_with_config(dir: &std::path::Path, config: ovvc_config::Config) -> Daemon {
        let client = Arc::new(Client::open_local(dir).expect("in-process client"));
        let (shutdown, _) = watch::channel(false);
        Daemon::new(
            client,
            Arc::new(config),
            dir.to_path_buf(),
            dir.join("daemon.sock"),
            shutdown,
        )
    }

    #[test]
    fn a_bug_diagnostic_is_logged_and_names_the_log() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let mut events = daemon.subscribe();

        daemon.report(
            ErrorClass::Bug,
            None,
            "test",
            "an impossible state was reached",
        );

        match events.try_recv().expect("diagnostic event") {
            DaemonEvent::Diagnostic(diagnostic) => {
                assert_eq!(diagnostic.class, ErrorClass::Bug);
                assert_eq!(
                    diagnostic.log_path.as_deref(),
                    Some(daemon.log_path().as_str())
                );
                assert!(diagnostic.report_hint.is_some());
                assert_eq!(diagnostic.context.as_deref(), Some("test"));
            }
            other => panic!("expected a diagnostic, got {other:?}"),
        }

        let log = std::fs::read_to_string(daemon.log_path()).expect("daemon log");
        assert!(log.contains("class=bug"), "{log}");
        assert!(log.contains("context=\"test\""), "{log}");
        assert!(
            log.contains("message=\"an impossible state was reached\""),
            "{log}"
        );
    }

    #[test]
    fn control_and_network_failures_are_classified_without_the_ui_string_matching() {
        use ovvc_control::ControlError;

        let table = [
            (ControlError::InvalidSignature, ErrorClass::Security),
            (ControlError::Replay, ErrorClass::Security),
            (ControlError::NotAMember, ErrorClass::Security),
            (ControlError::WrongSession, ErrorClass::Security),
            (ControlError::Stale, ErrorClass::Security),
            (ControlError::RateLimited, ErrorClass::Operational),
            (
                ControlError::Invalid("bad shape".to_string()),
                ErrorClass::Security,
            ),
            (
                ControlError::Core(ovvc_core::CoreError::InvalidInput("bad".to_string())),
                ErrorClass::User,
            ),
        ];
        for (error, expected) in table {
            assert_eq!(classify_control_error(&error), expected, "{error}");
        }

        let messages = [
            (
                "rejected event from peer: invalid signature",
                ErrorClass::Security,
            ),
            ("replayed control frame from peer", ErrorClass::Security),
            (
                "sending to a peer that is not a member",
                ErrorClass::Security,
            ),
            ("guild not found", ErrorClass::User),
            ("connection timed out", ErrorClass::Operational),
            (
                "could not dial /ip4/127.0.0.1/tcp/9",
                ErrorClass::Operational,
            ),
        ];
        for (message, expected) in messages {
            assert_eq!(classify_client_message(message), expected, "{message}");
        }
    }

    fn voice_call(daemon: &Daemon) -> (GuildId, ChannelId) {
        let guild = daemon.client.create_guild("audio-test").expect("guild");
        let channel = daemon
            .client
            .create_channel_in(guild.id, "standup", ChannelKind::Voice, None)
            .expect("voice channel");
        (guild.id, channel.id)
    }

    async fn start(daemon: &Daemon) -> (GuildId, ChannelId, u64) {
        let (guild_id, channel_id) = voice_call(daemon);
        daemon
            .start_audio(guild_id, channel_id, false, false, false)
            .await
            .expect("the audio service starts");
        let generation = daemon
            .audio
            .lock()
            .await
            .get(&(guild_id, channel_id))
            .expect("the audio service was not stored")
            .generation;
        (guild_id, channel_id, generation)
    }

    #[tokio::test]
    async fn rtc_teardown_does_not_depend_on_an_audio_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id) = voice_call(&daemon);

        // A call whose audio service failed to start still has an RTC manager.
        let (tx, _rx) = mpsc::channel(8);
        let manager = RtcManager::new(
            daemon.client.author_id(),
            CallId::for_channel(channel_id),
            ovvc_rtc::RtcConfig::default(),
            tx,
        )
        .expect("RTC manager");
        daemon.rtc.lock().await.insert(
            (guild_id, channel_id),
            RtcCall {
                manager,
                generation: 1,
                last_stats: RtcStats::default(),
            },
        );
        assert!(
            daemon.audio.lock().await.is_empty(),
            "the premise is a call with no audio service"
        );

        // The local author is not in the call snapshot, so the transport must
        // be closed even though it is absent from the audio map.
        daemon.reconcile_audio(now_ms()).await;
        assert!(
            daemon.rtc.lock().await.is_empty(),
            "the RTC manager outlived the local member's call"
        );
    }

    #[tokio::test]
    async fn a_stale_stopped_event_does_not_remove_a_replacement_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id, generation) = start(&daemon).await;

        // A late Stopped from a service that has already been replaced must
        // leave the live entry alone.
        daemon
            .handle_audio_event(AudioEventEnvelope {
                guild_id,
                channel_id,
                generation: generation.wrapping_add(1),
                event: AudioEvent::Stopped,
            })
            .await;
        assert!(
            daemon
                .audio
                .lock()
                .await
                .contains_key(&(guild_id, channel_id)),
            "a stale Stopped retired the live service"
        );

        daemon
            .handle_audio_event(AudioEventEnvelope {
                guild_id,
                channel_id,
                generation,
                event: AudioEvent::Stopped,
            })
            .await;
        assert!(
            !daemon
                .audio
                .lock()
                .await
                .contains_key(&(guild_id, channel_id)),
            "the matching Stopped did not retire the service"
        );
    }

    #[tokio::test]
    async fn an_error_event_removes_the_dead_service_and_clears_its_stats() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id, generation) = start(&daemon).await;

        daemon
            .handle_audio_event(AudioEventEnvelope {
                guild_id,
                channel_id,
                generation,
                event: AudioEvent::Error("device unplugged".to_string()),
            })
            .await;

        assert!(
            !daemon
                .audio
                .lock()
                .await
                .contains_key(&(guild_id, channel_id)),
            "an error left a handle to the exited service in the map"
        );
        assert!(
            daemon
                .audio
                .lock()
                .await
                .get(&(guild_id, channel_id))
                .is_none(),
            "the old stats must not outlive the service"
        );
    }

    #[tokio::test]
    async fn a_stale_stats_event_does_not_overwrite_the_new_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id, generation) = start(&daemon).await;

        let stats = |captured: u64| AudioStats {
            backend: "null".to_string(),
            captured,
            packets: captured,
            played: 0,
            concealed: 0,
            lost: 0,
            decode_errors: 0,
            peers: 0,
            xruns: 0,
            latency_ms: 20,
            peak: 0,
            rms: 0.0,
        };
        daemon
            .handle_audio_event(AudioEventEnvelope {
                guild_id,
                channel_id,
                generation,
                event: AudioEvent::Stats(stats(10)),
            })
            .await;
        daemon
            .handle_audio_event(AudioEventEnvelope {
                guild_id,
                channel_id,
                generation: generation.wrapping_add(1),
                event: AudioEvent::Stats(stats(99)),
            })
            .await;

        let stored = daemon
            .audio
            .lock()
            .await
            .get(&(guild_id, channel_id))
            .and_then(|call| call.last_stats.clone())
            .expect("stats");
        assert_eq!(
            stored.captured, 10,
            "a stale stats event replaced the live counters"
        );
    }

    #[tokio::test]
    async fn plaintext_replayed_and_keyless_media_are_dropped_and_counted() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id) = voice_call(&daemon);
        let local = daemon.client.author_id();
        let peer = AuthorId([9; 32]);
        let call_id = CallId::for_channel(channel_id);

        // Two keyrings for the same call, each with the other's public key.
        let mut peer_keys = MediaKeyring::generate(peer, call_id);
        let mut local_keys = MediaKeyring::generate(local, call_id);
        local_keys
            .install_peer(peer, local_keys.epoch(), peer_keys.public_key())
            .expect("peer key installs");
        peer_keys
            .install_peer(local, peer_keys.epoch(), local_keys.public_key())
            .expect("local key installs");
        let epoch = local_keys.epoch();
        daemon.crypto.lock().await.insert(
            (guild_id, channel_id),
            CallCrypto {
                keyring: Arc::new(RwLock::new(local_keys)),
                roster: [local, peer].into_iter().collect(),
                hub: None,
                installed: HashMap::from([(peer, (epoch, peer_keys.public_key()))]),
                published: None,
                aligned: true,
                advances_since_rotation: 0,
                errors: 0,
            },
        );

        let ciphertext = peer_keys
            .encrypt_for(local, b"opus frame")
            .expect("peer encrypts");
        let wire = ovvc_media::MediaPacket::new(
            ovvc_media::MediaKind::Audio,
            call_id,
            peer,
            1,
            0,
            ciphertext,
        )
        .expect("wire packet");
        let decoded = daemon
            .decrypt_media(guild_id, channel_id, &wire, peer)
            .await
            .expect("authenticated ciphertext decrypts");
        assert_eq!(decoded.payload, b"opus frame");
        assert_eq!(decoded.author, peer);

        // The same counter again is a replay and must not play twice.
        assert!(
            daemon
                .decrypt_media(guild_id, channel_id, &wire, peer)
                .await
                .is_none(),
            "a replayed frame reached the decoder"
        );

        // A plaintext payload is not a valid sealed frame.
        let plaintext = ovvc_media::MediaPacket::new(
            ovvc_media::MediaKind::Audio,
            call_id,
            peer,
            2,
            0,
            b"plaintext".to_vec(),
        )
        .expect("plaintext packet");
        assert!(
            daemon
                .decrypt_media(guild_id, channel_id, &plaintext, peer)
                .await
                .is_none(),
            "a plaintext packet reached the decoder"
        );

        // A peer with no installed key cannot have its frame decrypted.
        if let Some(keyring) = daemon.call_keyring(guild_id, channel_id).await {
            keyring.write().expect("keyring").remove_peer(peer);
        }
        let ciphertext = peer_keys
            .encrypt_for(local, b"another frame")
            .expect("peer encrypts");
        let keyless = ovvc_media::MediaPacket::new(
            ovvc_media::MediaKind::Audio,
            call_id,
            peer,
            3,
            0,
            ciphertext,
        )
        .expect("keyless packet");
        assert!(
            daemon
                .decrypt_media(guild_id, channel_id, &keyless, peer)
                .await
                .is_none(),
            "a packet without a peer key reached the decoder"
        );

        let errors = daemon
            .crypto
            .lock()
            .await
            .get(&(guild_id, channel_id))
            .expect("crypto entry")
            .errors;
        assert_eq!(errors, 3, "each dropped frame must be counted once");
    }

    async fn next_voice_key(
        events: &mut broadcast::Receiver<DaemonEvent>,
        guild_id: GuildId,
        channel_id: ChannelId,
    ) -> (u32, [u8; 32]) {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                match events.recv().await.expect("event channel open") {
                    DaemonEvent::Control {
                        guild_id: event_guild,
                        frame,
                        ..
                    } if event_guild == guild_id => {
                        if let ControlKind::VoiceKey {
                            channel_id: event_channel,
                            epoch,
                            public_key,
                        } = &frame.kind
                            && *event_channel == channel_id
                        {
                            return (*epoch, *public_key);
                        }
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("a VoiceKey frame")
    }

    #[tokio::test]
    async fn a_lost_voice_key_is_republished_on_the_next_reconcile() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id) = voice_call(&daemon);
        let members = daemon.members(guild_id);
        daemon
            .control
            .lock()
            .await
            .voice_join(
                guild_id,
                channel_id,
                &members,
                false,
                false,
                false,
                now_ms(),
            )
            .expect("local join");
        let mut events = daemon.events.subscribe();

        daemon.refresh_call_keys(guild_id, channel_id, true).await;
        let first = next_voice_key(&mut events, guild_id, channel_id).await;

        // The first frame was never confirmed: either the hub rejected the
        // publish or the network dropped it. The reconcile tick must send the
        // same epoch key again even though the roster has not changed.
        daemon
            .crypto
            .lock()
            .await
            .get_mut(&(guild_id, channel_id))
            .expect("crypto entry")
            .published = None;
        daemon.refresh_call_keys(guild_id, channel_id, false).await;
        let second = next_voice_key(&mut events, guild_id, channel_id).await;

        assert_eq!(
            first, second,
            "the retry must republish the current epoch, not a different key"
        );

        // The republished frame is usable: a peer that missed the first one can
        // install the same epoch key and decrypt media from this daemon.
        let (epoch, daemon_public) = second;
        let local = daemon.client.author_id();
        let peer = AuthorId([0x42; 32]);
        let call_id = CallId::for_channel(channel_id);
        let mut peer_keys = MediaKeyring::generate(peer, call_id);
        peer_keys
            .rotate_to(epoch)
            .expect("peer aligns to the epoch");
        peer_keys
            .install_peer(local, epoch, daemon_public)
            .expect("peer installs the republished key");
        {
            let mut crypto = daemon.crypto.lock().await;
            crypto
                .get_mut(&(guild_id, channel_id))
                .expect("crypto entry")
                .keyring
                .write()
                .expect("keyring")
                .install_peer(peer, epoch, peer_keys.public_key())
                .expect("daemon installs the peer key");
        }
        let frame = daemon
            .call_keyring(guild_id, channel_id)
            .await
            .expect("keyring")
            .read()
            .expect("keyring")
            .encrypt_for(peer, b"opus frame")
            .expect("daemon encrypts for the peer");
        assert_eq!(
            peer_keys
                .decrypt_from(local, &frame)
                .expect("peer decrypts"),
            b"opus frame"
        );
    }

    #[test]
    fn an_aligned_keyring_takes_a_bounded_number_of_remote_advances() {
        // A fresh keyring adopts the call's observed epoch even from far away,
        // which is what lets a joiner reach a call that has rotated for a long
        // time.
        assert_eq!(
            plan_epoch_update(2, 101, false, 0),
            EpochUpdate::Advance(101)
        );
        // Once aligned, a normal one-step advance is still accepted, and it
        // may repeat until the per-rotation budget is spent so members that
        // rotated independently can meet at the highest epoch.
        assert_eq!(
            plan_epoch_update(101, 102, true, 0),
            EpochUpdate::Advance(102)
        );
        assert_eq!(
            plan_epoch_update(102, 103, true, 1),
            EpochUpdate::Advance(103)
        );
        assert_eq!(
            plan_epoch_update(
                103,
                104,
                true,
                MAX_EPOCH_ADVANCES_PER_ROTATION - 1
            ),
            EpochUpdate::Advance(104)
        );
        // The budget is exhausted: a member cannot spin the rotation forever.
        assert_eq!(
            plan_epoch_update(104, 105, true, MAX_EPOCH_ADVANCES_PER_ROTATION),
            EpochUpdate::Ignore
        );
        // A jump beyond the cap is refused once aligned.
        assert_eq!(
            plan_epoch_update(101, 102 + MAX_KEY_EPOCH_ADVANCE, true, 0),
            EpochUpdate::Ignore
        );
        // A stale epoch is always ignored.
        assert_eq!(
            plan_epoch_update(101, 100, true, 0),
            EpochUpdate::Ignore
        );
    }

    #[test]
    fn a_publish_is_due_when_unconfirmed_changed_or_after_the_interval() {
        let key = [3u8; 32];
        assert!(key_publication_due(None, 4, key, 10));
        assert!(!key_publication_due(Some((4, key, 10)), 4, key, 10));
        assert!(key_publication_due(
            Some((4, key, 10)),
            4,
            key,
            10 + KEY_REPUBLISH_INTERVAL_MS
        ));
        assert!(key_publication_due(Some((3, key, 10)), 4, key, 10));
        assert!(key_publication_due(Some((4, [9u8; 32], 10)), 4, key, 10));
    }

    #[tokio::test]
    async fn a_keyless_libp2p_peer_is_not_reported_as_e2ee() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = ovvc_config::Config::default();
        config.media.transport = "libp2p".to_string();
        let daemon = test_daemon_with_config(dir.path(), config);
        let (guild_id, channel_id) = voice_call(&daemon);
        let local = daemon.client.author_id();
        let peer = AuthorId([7; 32]);
        let call_id = CallId::for_channel(channel_id);
        let local_keys = MediaKeyring::generate(local, call_id);
        let epoch = local_keys.epoch();
        daemon.crypto.lock().await.insert(
            (guild_id, channel_id),
            CallCrypto {
                keyring: Arc::new(RwLock::new(local_keys)),
                roster: [local, peer].into_iter().collect(),
                hub: None,
                installed: HashMap::new(),
                published: None,
                aligned: true,
                advances_since_rotation: 0,
                errors: 0,
            },
        );

        let (e2ee, reported_epoch, _) = daemon.e2ee_metrics(guild_id, channel_id).await;
        assert_eq!(reported_epoch, epoch);
        assert!(
            !e2ee,
            "a peer with no key must not be reported as an encrypted call"
        );

        // Once the peer's authenticated key is installed, the call is sealed.
        let peer_keys = MediaKeyring::generate(peer, call_id);
        {
            let mut crypto = daemon.crypto.lock().await;
            let entry = crypto
                .get_mut(&(guild_id, channel_id))
                .expect("crypto entry");
            entry
                .keyring
                .write()
                .expect("keyring")
                .install_peer(peer, epoch, peer_keys.public_key())
                .expect("peer key");
        }
        let (e2ee, _, _) = daemon.e2ee_metrics(guild_id, channel_id).await;
        assert!(e2ee, "a keyed peer pair must report E2EE");
    }

    #[tokio::test]
    async fn a_non_member_frame_cannot_trigger_a_voice_resync() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id) = voice_call(&daemon);
        let members = daemon.members(guild_id);

        // Put the local author in the call, so the only thing that can stop a
        // resync is the sender not being on the guild roster.
        {
            let mut hub = daemon.control.lock().await;
            hub.voice_join(
                guild_id,
                channel_id,
                &members,
                false,
                false,
                false,
                now_ms(),
            )
            .expect("local join");
        }

        let stranger = ovvc_core::Identity::from_seed([44u8; 32]);
        let frame = ControlFrame::sign(
            &stranger,
            guild_id,
            SessionId::for_voice_channel(guild_id, channel_id),
            0,
            ControlKind::VoiceHeartbeat { channel_id },
            1,
        )
        .expect("sign a stranger frame");
        assert!(
            !members.contains(&stranger.author_id()),
            "the premise is a sender outside the guild roster"
        );
        assert!(
            daemon
                .voice_resync_due(guild_id, &members, &frame)
                .await
                .is_none(),
            "a non-member provoked an announce and key republish"
        );
    }

    #[tokio::test]
    async fn a_burst_of_diverging_frames_produces_one_resync() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id) = voice_call(&daemon);
        let local = daemon.client.author_id();
        let peer = ovvc_core::Identity::from_seed([45u8; 32]);
        let peer_author = peer.author_id();
        let members = vec![local, peer_author];

        {
            let mut hub = daemon.control.lock().await;
            hub.voice_join(
                guild_id,
                channel_id,
                &members,
                false,
                false,
                false,
                now_ms(),
            )
            .expect("local join");
        }

        // A guild member outside the local call is the late-joiner signature.
        let frame = ControlFrame::sign(
            &peer,
            guild_id,
            SessionId::for_voice_channel(guild_id, channel_id),
            0,
            ControlKind::VoiceHeartbeat { channel_id },
            1,
        )
        .expect("sign a late-joiner frame");

        let first = daemon
            .voice_resync_due(guild_id, &members, &frame)
            .await
            .expect("the divergence was not detected");
        assert!(first.allowed, "the first divergence should repair");
        let second = daemon
            .voice_resync_due(guild_id, &members, &frame)
            .await
            .expect("a rate-limited divergence should still be recognised");
        assert!(
            !second.allowed,
            "a burst of diverging frames produced a second resync"
        );
        assert_eq!(second.channel_id, channel_id);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_voice_joins_spawn_one_audio_service() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = Arc::new(test_daemon(dir.path()));
        let (guild_id, channel_id) = voice_call(&daemon);

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let daemon = daemon.clone();
            tasks.push(tokio::spawn(async move {
                daemon
                    .start_audio(guild_id, channel_id, false, false, false)
                    .await
            }));
        }
        for task in tasks {
            assert!(
                task.await.expect("join task").is_ok(),
                "a concurrent join was told the audio service failed"
            );
        }
        assert_eq!(
            daemon.audio.lock().await.len(),
            1,
            "a concurrent join spawned a second audio service"
        );
    }

    #[tokio::test]
    async fn a_join_whose_audio_pipeline_cannot_load_is_rolled_back() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut config = ovvc_config::Config::default();
        config.audio.pipeline = Some(dir.path().join("missing-pipeline.toml"));
        let daemon = test_daemon_with_config(dir.path(), config);
        let (guild_id, channel_id) = voice_call(&daemon);
        let local = daemon.client.author_id();

        let result = daemon
            .dispatch(Op::VoiceJoin {
                guild_id,
                channel_id,
                muted: false,
                deafened: false,
                push_to_talk: false,
            })
            .await;
        assert!(
            result.is_err(),
            "a VoiceJoin with an unreadable pipeline must reply with an error"
        );
        assert!(daemon.audio.lock().await.is_empty(), "audio map not empty");
        assert!(daemon.rtc.lock().await.is_empty(), "rtc map not empty");
        assert!(
            daemon.crypto.lock().await.is_empty(),
            "crypto map not empty"
        );

        let snapshot = match daemon
            .dispatch(Op::VoiceSnapshot {
                guild_id,
                channel_id,
            })
            .await
            .expect("voice snapshot")
        {
            Reply::Voice(snapshot) => snapshot,
            other => panic!("unexpected snapshot reply: {other:?}"),
        };
        assert!(
            snapshot
                .map(|snapshot| { !snapshot.members.iter().any(|member| member.author == local) })
                .unwrap_or(true),
            "the local member survived a failed audio start"
        );
    }

    #[test]
    fn only_verified_members_with_a_stale_call_view_are_ignored() {
        let guild_id = GuildId([7u8; 32]);
        let channel_id = ChannelId([9u8; 16]);
        let session_id = SessionId::for_voice_channel(guild_id, channel_id);
        let local = ovvc_core::Identity::from_seed([70u8; 32]);
        let peer = ovvc_core::Identity::from_seed([71u8; 32]);
        let stranger = ovvc_core::Identity::from_seed([72u8; 32]);
        let members = vec![local.author_id(), peer.author_id()];
        let frame = ControlFrame::sign(
            &peer,
            guild_id,
            session_id,
            0,
            ControlKind::VoiceHeartbeat { channel_id },
            1,
        )
        .expect("sign a member frame");
        let stale = ovvc_control::ControlError::Invalid(
            "voice coordinator frame from a member outside the call".to_string(),
        );
        let key = ovvc_control::ControlError::Invalid(
            "voice key from a member outside the call".to_string(),
        );
        let rtc = ovvc_control::ControlError::Invalid(
            "rtc signalling from a member outside the call".to_string(),
        );
        for error in [&stale, &key, &rtc] {
            assert!(
                is_stale_voice_control(error, &frame, &members),
                "a verified member's stale call frame must be convergence noise: {error}"
            );
        }

        let malformed = ovvc_control::ControlError::Invalid(
            "rtc target must be another guild member".to_string(),
        );
        assert!(
            !is_stale_voice_control(&malformed, &frame, &members),
            "an unrelated Invalid frame must stay an error"
        );
        assert!(
            !is_stale_voice_control(&ovvc_control::ControlError::NotAMember, &frame, &members),
            "a non-member rejection must stay security"
        );

        let stranger_frame = ControlFrame::sign(
            &stranger,
            guild_id,
            session_id,
            0,
            ControlKind::VoiceHeartbeat { channel_id },
            2,
        )
        .expect("sign a stranger frame");
        assert!(
            !is_stale_voice_control(&stale, &stranger_frame, &members),
            "a sender outside the guild roster must not be ignored"
        );

        let mut forged = frame.clone();
        forged.signature[0] ^= 0xff;
        assert!(
            !is_stale_voice_control(&stale, &forged, &members),
            "a forged frame must not be treated as convergence"
        );
    }

    fn author(index: u8) -> AuthorId {
        AuthorId([index; 32])
    }

    fn call_with_topology(
        channel_id: ChannelId,
        members: &[AuthorId],
        topology: CallTopology,
        hub: Option<AuthorId>,
    ) -> VoiceSnapshot {
        VoiceSnapshot {
            channel_id,
            call_id: CallId::for_channel(channel_id),
            members: members
                .iter()
                .map(|author| ovvc_media::VoiceMember {
                    author: *author,
                    joined_at_ms: 0,
                    last_seen_ms: 0,
                    state: ovvc_media::VoiceMemberState {
                        muted: false,
                        deafened: false,
                        push_to_talk: false,
                        speaking: false,
                        video: false,
                        screen_share: false,
                    },
                    profiles: ovvc_media::ProfileSet::ALL,
                })
                .collect(),
            topology,
            hub,
            scope: ovvc_core::CallScope::Star,
            hub_policy: ovvc_core::HubPolicy::elected(),
            key_epoch: 1,
            codec: None,
            started_at_ms: 0,
            updated_at_ms: 0,
            master: hub,
            backup: None,
            term: 1,
        }
    }

    #[test]
    fn a_star_leaf_addresses_media_only_to_the_hub() {
        let (leaf_a, hub, leaf_b) = (author(1), author(2), author(3));
        let members = [leaf_a, hub, leaf_b];
        let topology = ovvc_media::topology_for(&members, ovvc_core::CallScope::Star, Some(hub));
        let call = call_with_topology(ChannelId([7; 16]), &members, topology, Some(hub));

        assert_eq!(media_peers(&call, leaf_a), vec![hub]);
        assert_eq!(media_peers(&call, leaf_b), vec![hub]);
        let hub_peers = media_peers(&call, hub);
        assert_eq!(hub_peers.len(), 2, "the hub talks to every leaf");
        assert!(hub_peers.contains(&leaf_a) && hub_peers.contains(&leaf_b));
        assert!(
            !media_peers(&call, leaf_a).contains(&leaf_b),
            "a leaf must never open a leaf-to-leaf media peer"
        );
        assert!(local_is_hub(&call, hub));
        assert!(!local_is_hub(&call, leaf_a));
    }

    #[test]
    fn a_mesh_and_a_direct_pair_still_address_every_other_member() {
        let (a, b, c) = (author(1), author(2), author(3));
        let members = [a, b, c];
        let mesh = call_with_topology(
            ChannelId([8; 16]),
            &members,
            ovvc_media::topology_for(&members, ovvc_core::CallScope::Mesh, Some(c)),
            None,
        );
        assert_eq!(media_peers(&mesh, a), vec![b, c]);
        assert_eq!(media_peers(&mesh, b), vec![a, c]);

        let pair = [a, b];
        let direct = call_with_topology(
            ChannelId([9; 16]),
            &pair,
            ovvc_media::topology_for(&pair, ovvc_core::CallScope::Star, Some(a)),
            None,
        );
        assert_eq!(media_peers(&direct, a), vec![b]);
        assert_eq!(media_peers(&direct, b), vec![a]);
        assert!(!local_is_hub(&direct, a), "a direct pair is not a star");
    }

    #[test]
    fn only_the_hub_builds_a_mixer_and_it_names_the_other_leaves() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let guild_id = GuildId([4; 32]);
        let channel_id = ChannelId([10; 16]);
        let (leaf_a, hub, leaf_b) = (author(1), author(2), author(3));
        let members = [leaf_a, hub, leaf_b];
        let topology = ovvc_media::topology_for(&members, ovvc_core::CallScope::Star, Some(hub));
        let call = call_with_topology(channel_id, &members, topology, Some(hub));

        daemon.reconcile_hub_mixer(guild_id, &call, leaf_a);
        assert!(
            daemon.hub_mixers().is_empty(),
            "a leaf must not build a hub mixer"
        );

        daemon.reconcile_hub_mixer(guild_id, &call, hub);
        {
            let mut mixers = daemon.hub_mixers();
            let state = mixers
                .get_mut(&(guild_id, channel_id))
                .expect("the hub builds a mixer");
            let listeners: Vec<AuthorId> = state.mixer.listeners().collect();
            assert_eq!(
                listeners,
                vec![leaf_a, leaf_b],
                "every other member is a listener"
            );
            assert!(
                !listeners.contains(&hub),
                "the hub never sends a mix to itself"
            );
        }

        // A leaf leaves: its listener and source must disappear without a
        // rebuild of the remaining call.
        let remaining = [hub, leaf_b];
        let shrunk = call_with_topology(
            channel_id,
            &remaining,
            CallTopology::Star {
                hub,
                leaves: vec![leaf_b],
            },
            Some(hub),
        );
        daemon.reconcile_hub_mixer(guild_id, &shrunk, hub);
        let mut mixers = daemon.hub_mixers();
        let state = mixers.get_mut(&(guild_id, channel_id)).expect("mixer");
        assert_eq!(state.mixer.listeners().collect::<Vec<_>>(), vec![leaf_b]);
    }

    #[tokio::test]
    async fn a_hub_change_rotates_the_epoch_and_clears_the_old_hub_key() {
        let dir = tempfile::tempdir().expect("temp dir");
        let daemon = test_daemon(dir.path());
        let (guild_id, channel_id) = voice_call(&daemon);
        let local = daemon.client.author_id();
        let old_hub = author(2);
        let new_hub = author(3);
        let roster = [local, old_hub];

        let (keyring, _) = daemon
            .sync_call_crypto(guild_id, channel_id, &roster, Some(old_hub), 0, true)
            .await
            .expect("first keyring");
        let epoch_before = keyring.read().expect("keyring").epoch();
        let old_hub_keys = MediaKeyring::generate(old_hub, CallId::for_channel(channel_id));
        keyring
            .write()
            .expect("keyring")
            .install_peer(old_hub, epoch_before, old_hub_keys.public_key())
            .expect("the old hub's key installs");

        // The control plane has moved the call to a new hub and announced the
        // epoch the call has already reached. The keyring must advance past
        // it, not publish a stale lower epoch.
        let (keyring, changed) = daemon
            .sync_call_crypto(
                guild_id,
                channel_id,
                &roster,
                Some(new_hub),
                epoch_before,
                false,
            )
            .await
            .expect("the rotated keyring");
        assert!(changed, "a hub change must force a rotation");
        let keys = keyring.read().expect("keyring");
        assert!(
            keys.epoch() > epoch_before,
            "the new epoch must exceed the announced one: {} <= {}",
            keys.epoch(),
            epoch_before
        );
        assert_eq!(
            keys.peer_epoch(old_hub),
            None,
            "the old hub's pairwise cipher must not survive the takeover"
        );
        drop(keys);

        let new_hub_keys = MediaKeyring::generate(new_hub, CallId::for_channel(channel_id));
        let new_epoch = keyring.read().expect("keyring").epoch();
        keyring
            .write()
            .expect("keyring")
            .install_peer(new_hub, new_epoch, new_hub_keys.public_key())
            .expect("the new hub's key installs at the new epoch");
    }
}
