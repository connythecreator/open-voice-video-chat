//! Signalling messages exchanged between peers.
//!
//! The daemon carries these over libp2p request-response, alongside identity
//! and control frames. They are deliberately plain data: an SDP blob, an ICE
//! candidate and its media-section mid. The size bounds keep a hostile or
//! broken peer from making the daemon allocate an unbounded buffer before the
//! WebRTC stack ever sees the payload.

use serde::{Deserialize, Serialize};

use crate::error::{Result, RtcError};

/// Largest accepted SDP document, in bytes.
pub const MAX_SDP_BYTES: usize = 48 * 1024;

/// Largest accepted ICE candidate string, in bytes.
pub const MAX_CANDIDATE_BYTES: usize = 2 * 1024;

/// Largest accepted media-section mid, in bytes.
pub const MAX_MID_BYTES: usize = 64;

/// One signalling message for a peer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RtcSignal {
    /// The offerer's session description.
    Offer { sdp: String },
    /// The answerer's session description.
    Answer { sdp: String },
    /// A trickled ICE candidate. `mid` names the media section, when the
    /// sender supplied one.
    Candidate {
        candidate: String,
        mid: Option<String>,
    },
}

impl RtcSignal {
    /// Check the message against the size limits.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Offer { sdp } | Self::Answer { sdp } => {
                if sdp.is_empty() || sdp.len() > MAX_SDP_BYTES {
                    return Err(RtcError::Signal(format!(
                        "SDP must be 1..={MAX_SDP_BYTES} bytes, got {}",
                        sdp.len()
                    )));
                }
            }
            Self::Candidate { candidate, mid } => {
                if candidate.is_empty() || candidate.len() > MAX_CANDIDATE_BYTES {
                    return Err(RtcError::Signal(format!(
                        "ICE candidate must be 1..={MAX_CANDIDATE_BYTES} bytes, got {}",
                        candidate.len()
                    )));
                }
                if let Some(mid) = mid
                    && mid.len() > MAX_MID_BYTES
                {
                    return Err(RtcError::Signal(format!(
                        "candidate mid must be at most {MAX_MID_BYTES} bytes, got {}",
                        mid.len()
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_bounds_are_enforced() {
        assert!(
            RtcSignal::Offer {
                sdp: "v=0\r\n".to_owned()
            }
            .validate()
            .is_ok()
        );
        assert!(RtcSignal::Offer { sdp: String::new() }.validate().is_err());
        assert!(
            RtcSignal::Offer {
                sdp: "v".repeat(MAX_SDP_BYTES)
            }
            .validate()
            .is_ok()
        );
        assert!(
            RtcSignal::Offer {
                sdp: "v".repeat(MAX_SDP_BYTES + 1)
            }
            .validate()
            .is_err()
        );
        assert!(
            RtcSignal::Answer {
                sdp: "v".repeat(MAX_SDP_BYTES + 1)
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn candidate_and_mid_bounds_are_enforced() {
        let candidate = "candidate:1 1 udp 1 127.0.0.1 1 typ host";
        assert!(
            RtcSignal::Candidate {
                candidate: candidate.to_owned(),
                mid: None,
            }
            .validate()
            .is_ok()
        );
        assert!(
            RtcSignal::Candidate {
                candidate: String::new(),
                mid: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            RtcSignal::Candidate {
                candidate: "c".repeat(MAX_CANDIDATE_BYTES),
                mid: Some("m".repeat(MAX_MID_BYTES)),
            }
            .validate()
            .is_ok()
        );
        assert!(
            RtcSignal::Candidate {
                candidate: "c".repeat(MAX_CANDIDATE_BYTES + 1),
                mid: None,
            }
            .validate()
            .is_err()
        );
        assert!(
            RtcSignal::Candidate {
                candidate: candidate.to_owned(),
                mid: Some("m".repeat(MAX_MID_BYTES + 1)),
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn signals_round_trip_through_json() {
        let signals = [
            RtcSignal::Offer {
                sdp: "v=0\r\n".to_owned(),
            },
            RtcSignal::Answer {
                sdp: "v=0\r\no=- 1 1 IN IP4 127.0.0.1\r\n".to_owned(),
            },
            RtcSignal::Candidate {
                candidate: "candidate:1 1 udp 1 127.0.0.1 1 typ host".to_owned(),
                mid: Some("0".to_owned()),
            },
        ];
        for signal in signals {
            let json = serde_json::to_string(&signal).expect("serialize");
            let decoded: RtcSignal = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(decoded, signal);
        }
    }
}
