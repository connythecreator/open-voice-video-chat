use std::collections::{HashMap, VecDeque};

use ovvc_core::AuthorId;

use crate::error::{ControlError, Result};
use crate::frame::{ControlFrame, SessionId};

const REPLAY_WINDOW_MS: u64 = 5 * 60 * 1000;
const MAX_NONCES_PER_SESSION: usize = 256;
const MAX_TRACKED_SESSIONS: usize = 4096;

/// Detects a frame that is delivered twice inside the freshness window.
///
/// A signature proves who sent a frame, not that it arrived once. Without this,
/// a relay could replay a `Leave` or an old `State` and change a session's view
/// at will. Nonces are tracked per `(author, session)` with a bounded window.
#[derive(Debug)]
pub struct ReplayGuard {
    entries: HashMap<(AuthorId, SessionId), VecDeque<(u64, u64)>>,
    last_prune_ms: u64,
}

impl Default for ReplayGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayGuard {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_prune_ms: 0,
        }
    }

    pub fn check_and_record(&mut self, frame: &ControlFrame, now_ms: u64) -> Result<()> {
        self.prune(now_ms);
        let key = (frame.from, frame.session_id);
        let nonces = self.entries.entry(key).or_default();
        if nonces.iter().any(|(nonce, _)| *nonce == frame.nonce) {
            return Err(ControlError::Replay);
        }
        nonces.push_back((frame.nonce, frame.timestamp_ms));
        while nonces.len() > MAX_NONCES_PER_SESSION {
            nonces.pop_front();
        }
        if self.entries.len() > MAX_TRACKED_SESSIONS {
            // A peer can churn session ids. Dropping the oldest half is better
            // than growing without bound; the cost is that an ancient replay
            // becomes possible again, which the freshness window also blocks.
            let keys: Vec<_> = self
                .entries
                .keys()
                .take(self.entries.len() / 2)
                .copied()
                .collect();
            for key in keys {
                self.entries.remove(&key);
            }
        }
        Ok(())
    }

    fn prune(&mut self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_prune_ms) < 30_000 {
            return;
        }
        self.last_prune_ms = now_ms;
        let cutoff = now_ms.saturating_sub(REPLAY_WINDOW_MS);
        self.entries.retain(|_, nonces| {
            nonces.retain(|(_, timestamp)| *timestamp >= cutoff);
            !nonces.is_empty()
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    last_ms: u64,
}

/// A small token bucket per author.
///
/// Control traffic is latency-sensitive, so the limit is generous for a human
/// (typing markers and heartbeats) and tight enough that a buggy or hostile
/// peer cannot use the control path to starve event sync.
#[derive(Debug)]
pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    buckets: HashMap<AuthorId, Bucket>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(30.0, 10.0)
    }
}

impl RateLimiter {
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity: capacity.max(1.0),
            refill_per_sec: refill_per_sec.max(0.1),
            buckets: HashMap::new(),
        }
    }

    pub fn check(&mut self, author: AuthorId, now_ms: u64) -> Result<()> {
        let bucket = self.buckets.entry(author).or_insert(Bucket {
            tokens: self.capacity,
            last_ms: now_ms,
        });
        let elapsed = now_ms.saturating_sub(bucket.last_ms) as f64 / 1000.0;
        bucket.tokens = (bucket.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        bucket.last_ms = now_ms;
        if bucket.tokens < 1.0 {
            return Err(ControlError::RateLimited);
        }
        bucket.tokens -= 1.0;
        Ok(())
    }

    pub fn tracked_authors(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{Capabilities, ControlKind};
    use ovvc_core::{GuildId, Identity};

    fn frame(identity: &Identity, nonce: u64) -> ControlFrame {
        let guild = GuildId([3; 32]);
        ControlFrame::sign(
            identity,
            guild,
            SessionId::for_guild(guild),
            0,
            ControlKind::Join {
                display_name: "peer".to_string(),
                capabilities: Capabilities::default(),
            },
            nonce,
        )
        .unwrap()
    }

    #[test]
    fn duplicate_nonce_is_rejected() {
        let identity = Identity::generate();
        let now = crate::frame::now_ms();
        let frame = frame(&identity, 1);
        let mut guard = ReplayGuard::new();
        guard.check_and_record(&frame, now).unwrap();
        assert!(matches!(
            guard.check_and_record(&frame, now),
            Err(ControlError::Replay)
        ));
    }

    #[test]
    fn different_authors_have_independent_nonce_spaces() {
        let alice = Identity::generate();
        let bob = Identity::generate();
        let now = crate::frame::now_ms();
        let mut guard = ReplayGuard::new();
        guard.check_and_record(&frame(&alice, 1), now).unwrap();
        guard.check_and_record(&frame(&bob, 1), now).unwrap();
    }

    #[test]
    fn rate_limit_drains_and_refills() {
        let identity = Identity::generate();
        let mut limiter = RateLimiter::new(3.0, 1.0);
        let now = 1_000_000;
        for _ in 0..3 {
            limiter.check(identity.author_id(), now).unwrap();
        }
        assert!(matches!(
            limiter.check(identity.author_id(), now),
            Err(ControlError::RateLimited)
        ));
        limiter.check(identity.author_id(), now + 1000).unwrap();
    }

    #[test]
    fn replay_entries_age_out() {
        let identity = Identity::generate();
        let now = crate::frame::now_ms();
        let mut guard = ReplayGuard::new();
        let first = frame(&identity, 1);
        guard.check_and_record(&first, now).unwrap();
        // Prune happens after 30 s and drops frames older than five minutes.
        guard
            .check_and_record(&frame(&identity, 2), now + REPLAY_WINDOW_MS + 31_000)
            .unwrap();
        // The old entry is gone, so the same nonce can be recorded again only
        // if it is still inside the freshness window; here it is not, which is
        // why the guard alone does not replace check_freshness.
        let old = first;
        guard
            .check_and_record(&old, now + REPLAY_WINDOW_MS + 31_000)
            .unwrap();
    }
}
