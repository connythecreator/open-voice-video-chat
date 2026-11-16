//! Sliding replay window for per-direction media counters.

/// Width of the replay window, in counters.
///
/// The window is a `u64` bitmap, so it accepts a counter up to 63 places
/// behind the highest seen counter and rejects everything older.
pub const REPLAY_WINDOW_BITS: usize = 64;

const WINDOW: u64 = REPLAY_WINDOW_BITS as u64;

/// Accepts each media counter once, in any order, within a fixed window.
///
/// The wire counter starts at one, so zero is never a valid value and
/// [`accept`](Self::accept) always rejects it. Counters ahead of the window
/// slide it forward; counters inside the window are accepted as long as their
/// bit is still clear.
#[derive(Debug, Clone, Default)]
pub struct ReplayWindow {
    /// Highest counter accepted so far, or zero when nothing has been seen.
    highest: u64,
    /// Bit `i` is set when `highest - i` has been accepted.
    bitmap: u64,
}

impl ReplayWindow {
    /// A fresh window that has accepted nothing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept `counter` if it is new and inside the window.
    ///
    /// Returns `false` for zero, for a duplicate, and for a counter that has
    /// fallen more than `REPLAY_WINDOW_BITS - 1` behind the highest one.
    pub fn accept(&mut self, counter: u64) -> bool {
        if counter == 0 {
            return false;
        }
        if self.highest == 0 {
            self.highest = counter;
            self.bitmap = 1;
            return true;
        }
        if counter > self.highest {
            let shift = counter - self.highest;
            self.bitmap = if shift >= WINDOW {
                1
            } else {
                (self.bitmap << shift) | 1
            };
            self.highest = counter;
            return true;
        }

        let delta = self.highest - counter;
        if delta >= WINDOW {
            return false;
        }
        let mask = 1u64 << delta;
        if self.bitmap & mask != 0 {
            return false;
        }
        self.bitmap |= mask;
        true
    }

    /// Forget every accepted counter.
    pub fn reset(&mut self) {
        self.highest = 0;
        self.bitmap = 0;
    }

    /// Whether [`accept`](Self::accept) would return `true`, without changing
    /// the window.
    ///
    /// The cipher uses this to reject replays before paying for AEAD, then
    /// commits with [`accept`](Self::accept) only after authentication
    /// succeeds, so a forged frame cannot burn a counter.
    pub(crate) fn is_acceptable(&self, counter: u64) -> bool {
        if counter == 0 {
            return false;
        }
        if self.highest == 0 || counter > self.highest {
            return true;
        }
        let delta = self.highest - counter;
        delta < WINDOW && self.bitmap & (1u64 << delta) == 0
    }
}
