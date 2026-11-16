#![no_main]

//! Fuzzes the hardware-free media loopback harness.
//!
//! The input is split into records of a big-endian sequence, a payload length
//! and that many payload bytes. Every record becomes an audio packet for a
//! fixed call and author, offered to a [`LoopbackHarness`] running
//! [`PcmCodec`], with playout interleaved. The harness must never panic and
//! its counters must stay internally consistent.

use libfuzzer_sys::fuzz_target;
use ovvc_core::AuthorId;
use ovvc_media::loopback::{LoopbackHarness, PcmCodec};
use ovvc_media::{CallId, MediaKind, MediaPacket};

const CALL: CallId = CallId([1; 16]);
const AUTHOR: AuthorId = AuthorId([2; 32]);

fuzz_target!(|data: &[u8]| {
    if data.len() > 64 * 1024 {
        return;
    }

    let mut harness = LoopbackHarness::new(CALL, AUTHOR, Box::new(PcmCodec), 4);
    let mut cursor = 0usize;
    let mut offered = 0u64;

    while cursor + 3 <= data.len() {
        let seq = u16::from_be_bytes([data[cursor], data[cursor + 1]]);
        let payload_len = usize::from(data[cursor + 2]);
        cursor += 3;

        let end = cursor.saturating_add(payload_len).min(data.len());
        let payload = data[cursor..end].to_vec();
        cursor = end;

        if let Ok(packet) =
            MediaPacket::new(MediaKind::Audio, CALL, AUTHOR, seq, cursor as u64, payload)
            && harness.receive(&packet).is_ok()
        {
            offered += 1;
        }

        // Interleave playout so the harness drains and re-primes.
        let _ = harness.play();
    }

    // Drain whatever is still buffered.
    while let Ok(Some(_)) = harness.play() {}

    let stats = harness.stats();
    assert!(stats.played <= stats.received);
    assert!(stats.concealed <= stats.lost);
    assert!(stats.received <= offered);
    assert!(stats.duplicates <= offered);
    assert!(stats.late <= offered);
});
