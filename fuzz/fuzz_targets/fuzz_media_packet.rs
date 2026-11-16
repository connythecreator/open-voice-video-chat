#![no_main]

//! Fuzzes the media packet decoder and the jitter buffer together.
//!
//! Arbitrary bytes go to [`MediaPacket::decode`]. When there are at least two
//! bytes, the first two are read as a big-endian sequence and the rest become
//! an opaque payload offered to a [`JitterBuffer`], which is then popped a few
//! times. Neither stage may panic on any input.

use libfuzzer_sys::fuzz_target;
use ovvc_core::AuthorId;
use ovvc_media::call::CallId;
use ovvc_media::jitter::{JitterBuffer, MAX_BUFFERED, PushOutcome};
use ovvc_media::packet::{MAX_MEDIA_PAYLOAD, MediaKind, MediaPacket};

fuzz_target!(|data: &[u8]| {
    // Keep one iteration cheap; a real packet is far smaller than this.
    if data.len() > 64 * 1024 {
        return;
    }

    if let Ok(packet) = MediaPacket::decode(data) {
        assert!(packet.payload.len() <= MAX_MEDIA_PAYLOAD);
        let reencoded = packet.encode().expect("a decoded packet re-encodes");
        assert_eq!(
            MediaPacket::decode(&reencoded).expect("a re-encoded packet decodes"),
            packet
        );
    }

    // A deterministic known-good case: at least one packet must be accepted
    // and played, so an all-zero or always-reject implementation cannot pass
    // on inequalities alone.
    let good = MediaPacket::new(
        MediaKind::Audio,
        CallId([1; 16]),
        AuthorId([2; 32]),
        7,
        0,
        vec![1, 2, 3],
    )
    .expect("a small audio packet is valid");
    let decoded = MediaPacket::decode(&good.encode().expect("encode")).expect("decode");
    assert_eq!(decoded.payload, vec![1, 2, 3]);
    let mut known_good = JitterBuffer::new(1);
    assert_eq!(
        known_good
            .push(decoded.seq, decoded.payload.clone())
            .expect("push"),
        PushOutcome::Accepted
    );
    let played = known_good.pop().expect("the known-good packet must play");
    assert_eq!(played.payload, vec![1, 2, 3]);

    if data.len() >= 2 {
        let seq = u16::from_be_bytes([data[0], data[1]]);
        let payload = data[2..].to_vec();
        let mut buffer = JitterBuffer::new(4);
        let _ = buffer.push(seq, payload);
        assert!(buffer.buffered() <= MAX_BUFFERED);
        for _ in 0..8 {
            let _ = buffer.pop();
        }
        assert!(buffer.buffered() <= MAX_BUFFERED);
    }
});
