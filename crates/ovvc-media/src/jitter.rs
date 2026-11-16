//! A packet-ordering jitter buffer.
//!
//! The buffer is deliberately codec-agnostic: it holds opaque payloads keyed
//! by sequence number, hides reordering, and invents concealment frames where
//! audio was lost so the decoder upstream always receives a steady stream.
//! Sequence numbers are 16 bits on the wire and are extended to 32 bits here,
//! so the buffer survives the `65535 -> 0` wrap.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::error::{MediaError, Result};
use crate::packet::MAX_MEDIA_PAYLOAD;

/// How many packets to collect before starting playout.
pub const DEFAULT_DEPTH: usize = 3;

/// The most packets the buffer will hold before refusing more.
pub const MAX_BUFFERED: usize = 256;

/// A gap larger than this collapses the playout point rather than synthesising
/// the whole missing run.
const MAX_CONCEAL_RUN: u32 = 8;

/// A decoded media frame, real or concealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFrame {
    pub seq: u32,
    pub payload: Vec<u8>,
    pub concealed: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JitterStats {
    pub received: u64,
    pub played: u64,
    pub duplicates: u64,
    pub late: u64,
    pub lost: u64,
    pub concealed: u64,
    pub reordered: u64,
}

/// What happened to a packet offered to the buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    Duplicate,
    Late,
    BufferFull,
}

/// Extend a wrapping 16-bit sequence against the highest sequence seen.
///
/// The result is the sequence number in the same 32-bit epoch as `highest`,
/// so a packet that arrives after the wrap extends forward and a packet from
/// before it extends backward. Distances over half the 16-bit space are read
/// as belonging to the other side of `highest`.
pub fn extend_sequence(highest: u32, seq: u16) -> u32 {
    const HALF: u32 = 0x8000;
    let base = highest & !0xFFFF;
    let candidate = base | u32::from(seq);
    if candidate.wrapping_add(HALF) < highest {
        candidate.wrapping_add(0x1_0000)
    } else if candidate > highest.wrapping_add(HALF) {
        candidate.wrapping_sub(0x1_0000)
    } else {
        candidate
    }
}

#[derive(Debug, Clone)]
pub struct JitterBuffer {
    depth: usize,
    packets: BTreeMap<u32, Vec<u8>>,
    highest: Option<u32>,
    next_expected: Option<u32>,
    primed: bool,
    stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(depth: usize) -> Self {
        Self {
            depth: depth.clamp(1, 16),
            packets: BTreeMap::new(),
            highest: None,
            next_expected: None,
            primed: false,
            stats: JitterStats::default(),
        }
    }

    pub fn push(&mut self, seq: u16, payload: Vec<u8>) -> Result<PushOutcome> {
        if payload.len() > MAX_MEDIA_PAYLOAD {
            return Err(MediaError::PacketTooLarge(payload.len()));
        }

        let extended = match self.highest {
            Some(highest) => extend_sequence(highest, seq),
            None => u32::from(seq),
        };

        if self.packets.contains_key(&extended) {
            self.stats.duplicates = self.stats.duplicates.saturating_add(1);
            return Ok(PushOutcome::Duplicate);
        }
        // A sequence more than half the space *ahead* of the highest seen is
        // really a packet from the previous epoch: `extend_sequence` wraps it
        // to a huge value because a negative one cannot be represented.
        if let Some(highest) = self.highest
            && extended > highest
            && extended.wrapping_sub(highest) > 0x8000
        {
            self.stats.late = self.stats.late.saturating_add(1);
            return Ok(PushOutcome::Late);
        }
        if let Some(next) = self.next_expected
            && extended < next
        {
            self.stats.late = self.stats.late.saturating_add(1);
            return Ok(PushOutcome::Late);
        }
        if self.packets.len() >= MAX_BUFFERED {
            return Ok(PushOutcome::BufferFull);
        }

        if let Some(highest) = self.highest
            && extended < highest
        {
            self.stats.reordered = self.stats.reordered.saturating_add(1);
        }
        self.highest = Some(match self.highest {
            Some(highest) => highest.max(extended),
            None => extended,
        });
        self.packets.insert(extended, payload);
        self.stats.received = self.stats.received.saturating_add(1);
        Ok(PushOutcome::Accepted)
    }

    pub fn pop(&mut self) -> Option<MediaFrame> {
        if self.packets.is_empty() {
            self.primed = false;
            self.next_expected = None;
            return None;
        }

        if !self.primed {
            if self.packets.len() < self.depth {
                return None;
            }
            self.primed = true;
            self.next_expected = self.packets.keys().next().copied();
        }

        let next = self.next_expected?;

        if let Some(payload) = self.packets.remove(&next) {
            self.next_expected = Some(next.wrapping_add(1));
            self.stats.played = self.stats.played.saturating_add(1);
            return Some(MediaFrame {
                seq: next & 0xFFFF,
                payload,
                concealed: false,
            });
        }

        // Non-empty above, so there is always a lowest buffered sequence.
        let lowest = *self.packets.keys().next()?;
        let gap = lowest.saturating_sub(next);

        if gap > MAX_CONCEAL_RUN {
            // A long outage is not worth one frame per missing packet: skip to
            // the first packet we actually have and report one concealment.
            self.stats.lost = self.stats.lost.saturating_add(u64::from(gap));
            self.stats.concealed = self.stats.concealed.saturating_add(1);
            self.next_expected = Some(lowest);
            return Some(MediaFrame {
                seq: next & 0xFFFF,
                payload: Vec::new(),
                concealed: true,
            });
        }

        self.stats.lost = self.stats.lost.saturating_add(1);
        self.stats.concealed = self.stats.concealed.saturating_add(1);
        self.next_expected = Some(next.wrapping_add(1));
        Some(MediaFrame {
            seq: next & 0xFFFF,
            payload: Vec::new(),
            concealed: true,
        })
    }

    pub fn buffered(&self) -> usize {
        self.packets.len()
    }

    pub fn stats(&self) -> JitterStats {
        self.stats
    }

    /// Clear the counters without dropping buffered packets or playout state.
    pub fn reset_stats(&mut self) {
        self.stats = JitterStats::default();
    }

    /// Clear buffered packets and playout state, keeping the cumulative
    /// counters. Deafening needs the buffer emptied so stale audio is not
    /// replayed, but `played`, `lost` and `concealed` are reported since the
    /// call started and must not jump backwards.
    pub fn clear(&mut self) {
        self.packets.clear();
        self.highest = None;
        self.next_expected = None;
        self.primed = false;
    }

    pub fn reset(&mut self) {
        self.clear();
        self.stats = JitterStats::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(value: u8) -> Vec<u8> {
        vec![value]
    }

    #[test]
    fn plays_in_order_only_after_priming() {
        let mut buffer = JitterBuffer::new(DEFAULT_DEPTH);
        assert_eq!(
            buffer.push(10, payload(0)).expect("push"),
            PushOutcome::Accepted
        );
        assert!(buffer.pop().is_none());
        assert_eq!(
            buffer.push(11, payload(1)).expect("push"),
            PushOutcome::Accepted
        );
        assert!(buffer.pop().is_none());
        assert_eq!(
            buffer.push(12, payload(2)).expect("push"),
            PushOutcome::Accepted
        );

        for (seq, value) in [(10, 0), (11, 1), (12, 2)] {
            let frame = buffer.pop().expect("primed frame");
            assert_eq!(frame.seq, seq);
            assert_eq!(frame.payload, payload(value));
            assert!(!frame.concealed);
        }
        assert!(buffer.pop().is_none());
    }

    #[test]
    fn rejects_duplicates() {
        let mut buffer = JitterBuffer::new(1);
        assert_eq!(
            buffer.push(3, payload(3)).expect("push"),
            PushOutcome::Accepted
        );
        assert_eq!(
            buffer.push(3, payload(3)).expect("push"),
            PushOutcome::Duplicate
        );
        assert_eq!(buffer.stats().duplicates, 1);
    }

    #[test]
    fn rejects_packets_older_than_the_playout_point() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(5, payload(5)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 5);
        assert_eq!(buffer.push(5, payload(5)).expect("push"), PushOutcome::Late);
        assert_eq!(buffer.stats().late, 1);
    }

    #[test]
    fn a_packet_from_before_the_first_seen_sequence_is_late() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(0, payload(0)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 0);
        assert_eq!(
            buffer.push(65_535, payload(1)).expect("push"),
            PushOutcome::Late
        );
        assert_eq!(buffer.stats().late, 1);
    }

    #[test]
    fn reordered_packets_play_in_sequence() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(0, payload(0)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 0);
        buffer.push(2, payload(2)).expect("push");
        buffer.push(1, payload(1)).expect("push");
        assert_eq!(buffer.stats().reordered, 1);
        assert_eq!(buffer.pop().expect("frame").seq, 1);
        assert_eq!(buffer.pop().expect("frame").seq, 2);
    }

    #[test]
    fn a_lost_packet_yields_one_concealed_frame() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(0, payload(0)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 0);
        buffer.push(2, payload(2)).expect("push");

        let concealed = buffer.pop().expect("concealment");
        assert_eq!(concealed.seq, 1);
        assert!(concealed.concealed);
        assert!(concealed.payload.is_empty());

        let played = buffer.pop().expect("real frame after the gap");
        assert_eq!(played.seq, 2);
        assert!(!played.concealed);

        let stats = buffer.stats();
        assert_eq!(stats.lost, 1);
        assert_eq!(stats.concealed, 1);
        assert_eq!(stats.played, 2);
    }

    #[test]
    fn sequence_wraps_from_65535_to_zero() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(65_535, payload(1)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 65_535);
        buffer.push(0, payload(2)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 0);
        assert_eq!(buffer.stats().late, 0);
    }

    #[test]
    fn extend_sequence_crosses_the_wrap() {
        assert_eq!(extend_sequence(65_535, 0), 65_536);
        assert_eq!(extend_sequence(65_536, 65_535), 65_535);
        // Before the first epoch this wraps below zero, which `u32` spells as
        // the maximum value; `push` then classifies it as late.
        assert_eq!(extend_sequence(0, 65_535), u32::MAX);
        assert_eq!(extend_sequence(100, 101), 101);
    }

    #[test]
    fn refuses_more_than_the_buffer_limit() {
        let mut buffer = JitterBuffer::new(16);
        for seq in 0..MAX_BUFFERED {
            let outcome = buffer.push(seq as u16, Vec::new()).expect("push");
            assert_eq!(outcome, PushOutcome::Accepted);
        }
        assert_eq!(
            buffer.push(MAX_BUFFERED as u16, Vec::new()).expect("push"),
            PushOutcome::BufferFull
        );
        assert_eq!(buffer.buffered(), MAX_BUFFERED);
    }

    #[test]
    fn refuses_oversized_payloads() {
        let mut buffer = JitterBuffer::new(1);
        let oversized = vec![0u8; MAX_MEDIA_PAYLOAD + 1];
        assert!(matches!(
            buffer.push(0, oversized),
            Err(MediaError::PacketTooLarge(_))
        ));
        assert_eq!(buffer.buffered(), 0);
    }

    #[test]
    fn a_large_gap_jumps_the_playout_point() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(0, payload(0)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 0);
        buffer.push(100, payload(100)).expect("push");

        let concealed = buffer.pop().expect("concealment");
        assert!(concealed.concealed);
        assert_eq!(concealed.seq, 1);
        assert_eq!(buffer.stats().concealed, 1);
        assert_eq!(buffer.stats().lost, 99);

        let played = buffer.pop().expect("jumped-to frame");
        assert_eq!(played.seq, 100);
        assert!(!played.concealed);
    }

    #[test]
    fn clear_keeps_the_cumulative_counters() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(0, payload(0)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 0);
        buffer.push(2, payload(2)).expect("push");
        assert!(buffer.pop().expect("concealment").concealed);
        let before = buffer.stats();
        assert!(before.played > 0 && before.concealed > 0);

        buffer.clear();
        assert_eq!(buffer.buffered(), 0);
        assert_eq!(buffer.stats(), before);
        assert!(buffer.pop().is_none());
        buffer.push(5, payload(5)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 5);
    }

    #[test]
    fn reset_clears_buffered_packets_and_stats() {
        let mut buffer = JitterBuffer::new(1);
        buffer.push(0, payload(0)).expect("push");
        buffer.pop();
        buffer.reset();
        assert_eq!(buffer.buffered(), 0);
        assert_eq!(buffer.stats(), JitterStats::default());
        assert!(buffer.pop().is_none());
        // A first packet after reset starts a fresh sequence epoch.
        buffer.push(1, payload(1)).expect("push");
        assert_eq!(buffer.pop().expect("frame").seq, 1);
    }
}
