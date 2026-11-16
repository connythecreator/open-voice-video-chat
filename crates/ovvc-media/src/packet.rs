//! The on-wire media packet.
//!
//! Media bypasses the signed event log: a voice frame is latency-sensitive and
//! worthless once stale. The packet therefore carries only the routing fields
//! a peer needs and a bounded payload. Authentication of the media stream is a
//! later concern; the call id and author are copied from the control plane so
//! a peer can at least attribute the packet to the negotiated call.

use ovvc_core::AuthorId;

use crate::call::CallId;
use crate::error::{MediaError, Result};

pub const MEDIA_VERSION: u8 = 1;

/// The largest payload a single packet may carry.
pub const MAX_MEDIA_PAYLOAD: usize = 1200;

/// Fixed header size: version, kind, call id, author, seq, timestamp, length.
const HEADER_LEN: usize = 1 + 1 + 16 + 32 + 2 + 8 + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MediaKind {
    Audio,
    Video,
    Screen,
}

impl MediaKind {
    const fn to_byte(self) -> u8 {
        match self {
            MediaKind::Audio => 0,
            MediaKind::Video => 1,
            MediaKind::Screen => 2,
        }
    }

    const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0 => Some(MediaKind::Audio),
            1 => Some(MediaKind::Video),
            2 => Some(MediaKind::Screen),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MediaPacket {
    pub version: u8,
    pub kind: MediaKind,
    pub call_id: CallId,
    pub author: AuthorId,
    pub seq: u16,
    pub timestamp_ms: u64,
    pub payload: Vec<u8>,
}

impl MediaPacket {
    pub fn new(
        kind: MediaKind,
        call_id: CallId,
        author: AuthorId,
        seq: u16,
        timestamp_ms: u64,
        payload: Vec<u8>,
    ) -> Result<Self> {
        if payload.len() > MAX_MEDIA_PAYLOAD {
            return Err(MediaError::PacketTooLarge(payload.len()));
        }
        Ok(Self {
            version: MEDIA_VERSION,
            kind,
            call_id,
            author,
            seq,
            timestamp_ms,
            payload,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        if self.payload.len() > MAX_MEDIA_PAYLOAD {
            return Err(MediaError::PacketTooLarge(self.payload.len()));
        }
        let payload_len = u16::try_from(self.payload.len())
            .map_err(|_| MediaError::PacketTooLarge(self.payload.len()))?;

        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(self.version);
        out.push(self.kind.to_byte());
        out.extend_from_slice(&self.call_id.0);
        out.extend_from_slice(&self.author.0);
        out.extend_from_slice(&self.seq.to_be_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        out.extend_from_slice(&payload_len.to_be_bytes());
        out.extend_from_slice(&self.payload);
        Ok(out)
    }

    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(MediaError::Malformed(format!(
                "packet is {} bytes, shorter than the {HEADER_LEN}-byte header",
                bytes.len()
            )));
        }

        if bytes[0] != MEDIA_VERSION {
            return Err(MediaError::Malformed(format!(
                "unsupported media version {}",
                bytes[0]
            )));
        }

        let kind = MediaKind::from_byte(bytes[1])
            .ok_or_else(|| MediaError::Malformed(format!("unknown media kind {}", bytes[1])))?;

        let mut call_id = [0u8; 16];
        call_id.copy_from_slice(&bytes[2..18]);

        let mut author = [0u8; 32];
        author.copy_from_slice(&bytes[18..50]);

        let seq = u16::from_be_bytes([bytes[50], bytes[51]]);

        let timestamp_bytes: [u8; 8] = bytes[52..60]
            .try_into()
            .map_err(|_| MediaError::Malformed("truncated timestamp".to_string()))?;
        let timestamp_ms = u64::from_be_bytes(timestamp_bytes);

        let payload_len = usize::from(u16::from_be_bytes([bytes[60], bytes[61]]));
        if payload_len > MAX_MEDIA_PAYLOAD {
            return Err(MediaError::PacketTooLarge(payload_len));
        }

        let expected = HEADER_LEN
            .checked_add(payload_len)
            .ok_or_else(|| MediaError::Malformed("packet length overflow".to_string()))?;
        if bytes.len() < expected {
            return Err(MediaError::Malformed(format!(
                "packet is {} bytes, expected {expected}",
                bytes.len()
            )));
        }
        if bytes.len() > expected {
            return Err(MediaError::Malformed(format!(
                "{} trailing bytes after the payload",
                bytes.len() - expected
            )));
        }

        Ok(Self {
            version: bytes[0],
            kind,
            call_id: CallId(call_id),
            author: AuthorId(author),
            seq,
            timestamp_ms,
            payload: bytes[HEADER_LEN..expected].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(kind: MediaKind) -> MediaPacket {
        MediaPacket::new(
            kind,
            CallId::for_channel(ovvc_core::ChannelId([3; 16])),
            AuthorId([4; 32]),
            513,
            1_700_000_000_000,
            vec![9, 8, 7, 6],
        )
        .expect("packet")
    }

    #[test]
    fn round_trips_every_kind() {
        for kind in [MediaKind::Audio, MediaKind::Video, MediaKind::Screen] {
            let original = packet(kind);
            let encoded = original.encode().expect("encode");
            let decoded = MediaPacket::decode(&encoded).expect("decode");
            assert_eq!(decoded, original);
        }
    }

    #[test]
    fn an_empty_payload_round_trips() {
        let original = MediaPacket::new(
            MediaKind::Audio,
            CallId([1; 16]),
            AuthorId([2; 32]),
            0,
            0,
            Vec::new(),
        )
        .expect("packet");
        let encoded = original.encode().expect("encode");
        assert_eq!(MediaPacket::decode(&encoded).expect("decode"), original);
    }

    #[test]
    fn truncated_packets_are_rejected() {
        let encoded = packet(MediaKind::Audio).encode().expect("encode");
        for length in 0..encoded.len() {
            assert!(
                MediaPacket::decode(&encoded[..length]).is_err(),
                "truncation to {length} bytes must fail"
            );
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut encoded = packet(MediaKind::Audio).encode().expect("encode");
        encoded.push(0);
        assert!(matches!(
            MediaPacket::decode(&encoded),
            Err(MediaError::Malformed(_))
        ));
    }

    #[test]
    fn a_wrong_version_is_rejected() {
        let mut encoded = packet(MediaKind::Audio).encode().expect("encode");
        encoded[0] = MEDIA_VERSION + 1;
        assert!(matches!(
            MediaPacket::decode(&encoded),
            Err(MediaError::Malformed(_))
        ));
    }

    #[test]
    fn an_unknown_kind_is_rejected() {
        let mut encoded = packet(MediaKind::Audio).encode().expect("encode");
        encoded[1] = 9;
        assert!(matches!(
            MediaPacket::decode(&encoded),
            Err(MediaError::Malformed(_))
        ));
    }

    #[test]
    fn an_oversized_payload_is_refused_on_construction() {
        let result = MediaPacket::new(
            MediaKind::Audio,
            CallId([1; 16]),
            AuthorId([2; 32]),
            0,
            0,
            vec![0u8; MAX_MEDIA_PAYLOAD + 1],
        );
        assert!(matches!(result, Err(MediaError::PacketTooLarge(_))));
    }

    #[test]
    fn an_oversized_declared_length_is_rejected() {
        let mut encoded = packet(MediaKind::Audio).encode().expect("encode");
        let oversized = (MAX_MEDIA_PAYLOAD + 1) as u16;
        encoded[60..62].copy_from_slice(&oversized.to_be_bytes());
        assert!(matches!(
            MediaPacket::decode(&encoded),
            Err(MediaError::PacketTooLarge(_))
        ));
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        for length in 0..300usize {
            let garbage = vec![0xAB; length];
            let _ = MediaPacket::decode(&garbage);
        }
        for length in 0..300usize {
            let mut garbage: Vec<u8> = (0..length).map(|index| index as u8).collect();
            if length > 1 {
                garbage[1] %= 3;
            }
            let _ = MediaPacket::decode(&garbage);
        }
    }

    #[test]
    fn postcard_round_trips_every_kind_and_decode_rejects_a_wrong_version() {
        for kind in [MediaKind::Audio, MediaKind::Video, MediaKind::Screen] {
            let original = packet(kind);
            let bytes = postcard::to_allocvec(&original).expect("postcard encode");
            let decoded: MediaPacket = postcard::from_bytes(&bytes).expect("postcard decode");
            assert_eq!(decoded, original);
        }

        // The postcard path carries the version unchanged; the wire decoder
        // must still refuse a packet from a future version.
        let mut encoded = packet(MediaKind::Audio).encode().expect("encode");
        encoded[0] = MEDIA_VERSION + 1;
        assert!(matches!(
            MediaPacket::decode(&encoded),
            Err(MediaError::Malformed(_))
        ));
    }
}
