//! Runtime configuration for a call's WebRTC transport.

use serde::{Deserialize, Serialize};

use crate::error::{Result, RtcError};

/// Largest accepted ICE server URL, in bytes.
pub const MAX_ICE_URL_BYTES: usize = 256;

/// Largest accepted TURN username or credential, in bytes.
pub const MAX_TURN_CREDENTIAL_BYTES: usize = 512;

/// Lowest negotiation timeout a manager accepts, in milliseconds.
///
/// A shorter value would turn the one-second reconcile tick into a retry
/// storm, so the floor is deliberately above the tick interval.
pub const MIN_NEGOTIATION_TIMEOUT_MS: u64 = 100;

/// Highest negotiation timeout a manager accepts, in milliseconds.
pub const MAX_NEGOTIATION_TIMEOUT_MS: u64 = 300_000;

/// Frame durations Opus is allowed to carry in this transport.
pub const ALLOWED_FRAME_MS: [u16; 3] = [10, 20, 40];

/// Lowest Opus bitrate the transport accepts, in kbps.
pub const MIN_BITRATE_KBPS: u16 = 8;

/// Highest Opus bitrate the transport accepts, in kbps.
pub const MAX_BITRATE_KBPS: u16 = 512;

/// How the transport should behave for one call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtcConfig {
    /// STUN and TURN URLs, in `stun:`, `turns:` or `turn:` form.
    pub ice_servers: Vec<String>,
    /// Username presented to every TURN server. Required when any URL is
    /// `turn:` or `turns:`.
    pub turn_username: String,
    /// Credential presented to every TURN server. Required when any URL is
    /// `turn:` or `turns:`.
    pub turn_credential: String,
    /// Gather and use relay candidates only.
    pub force_relay: bool,
    /// Accept and gather loopback (`127.0.0.0/8`, `::1`) candidates. Off for
    /// production peers, on for two peers in one process or on one machine.
    pub allow_loopback_candidates: bool,
    /// Accept and gather private LAN addresses (`10/8`, `172.16/12`,
    /// `192.168/16`, unique-local IPv6). On by default because LAN calls are
    /// the supported direct path; a relay-only deployment can turn it off.
    pub allow_lan_candidates: bool,
    /// Opus frame duration written with each sample.
    pub audio_frame_ms: u16,
    /// Maximum send bitrate advertised to the RTP sender.
    pub audio_bitrate_kbps: u16,
    /// How long a peer may stay in `new`/`connecting` before the manager tears
    /// it down and negotiates again. Doubles after each failed attempt.
    pub negotiation_timeout_ms: u64,
}

impl Default for RtcConfig {
    fn default() -> Self {
        Self {
            ice_servers: Vec::new(),
            turn_username: String::new(),
            turn_credential: String::new(),
            force_relay: false,
            allow_loopback_candidates: false,
            allow_lan_candidates: true,
            audio_frame_ms: 20,
            audio_bitrate_kbps: 16,
            negotiation_timeout_ms: 5_000,
        }
    }
}

impl RtcConfig {
    /// Check the configuration against the ranges the transport supports.
    pub fn validate(&self) -> Result<()> {
        if !ALLOWED_FRAME_MS.contains(&self.audio_frame_ms) {
            return Err(RtcError::Config(format!(
                "audio_frame_ms must be one of {ALLOWED_FRAME_MS:?}, got {}",
                self.audio_frame_ms
            )));
        }
        if !(MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS).contains(&self.audio_bitrate_kbps) {
            return Err(RtcError::Config(format!(
                "audio_bitrate_kbps must be {MIN_BITRATE_KBPS}..={MAX_BITRATE_KBPS}, got {}",
                self.audio_bitrate_kbps
            )));
        }
        for url in &self.ice_servers {
            validate_ice_url(url)?;
        }
        // `rtc` refuses to build a PeerConnection for a turn:/turns: URL with
        // empty credentials. Surface that here so a bad configuration fails
        // when the call starts, not one second later in the reconcile loop.
        if self.ice_servers.iter().any(|url| is_turn_url(url)) {
            if self.turn_username.is_empty() || self.turn_credential.is_empty() {
                return Err(RtcError::Config(
                    "a turn:/turns: ICE server requires both turn_username and turn_credential"
                        .to_string(),
                ));
            }
            for (field, value) in [
                ("turn_username", &self.turn_username),
                ("turn_credential", &self.turn_credential),
            ] {
                if value.len() > MAX_TURN_CREDENTIAL_BYTES {
                    return Err(RtcError::Config(format!(
                        "{field} is {} bytes, at most {MAX_TURN_CREDENTIAL_BYTES} are accepted",
                        value.len()
                    )));
                }
            }
        }
        if !(MIN_NEGOTIATION_TIMEOUT_MS..=MAX_NEGOTIATION_TIMEOUT_MS)
            .contains(&self.negotiation_timeout_ms)
        {
            return Err(RtcError::Config(format!(
                "negotiation_timeout_ms must be {MIN_NEGOTIATION_TIMEOUT_MS}..={MAX_NEGOTIATION_TIMEOUT_MS}, got {}",
                self.negotiation_timeout_ms
            )));
        }
        Ok(())
    }
}

fn is_turn_url(url: &str) -> bool {
    url.starts_with("turn:") || url.starts_with("turns:")
}

fn validate_ice_url(url: &str) -> Result<()> {
    let scheme_ok =
        url.starts_with("stun:") || url.starts_with("turns:") || url.starts_with("turn:");
    if !scheme_ok {
        return Err(RtcError::Config(format!(
            "ICE server URL {url:?} must start with stun:, turns: or turn:"
        )));
    }
    if url.len() > MAX_ICE_URL_BYTES {
        return Err(RtcError::Config(format!(
            "ICE server URL {url:?} is {} bytes, at most {MAX_ICE_URL_BYTES} are accepted",
            url.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_match_the_interim_media_path() {
        let config = RtcConfig::default();
        assert!(config.ice_servers.is_empty());
        assert!(config.turn_username.is_empty());
        assert!(config.turn_credential.is_empty());
        assert!(!config.force_relay);
        assert!(!config.allow_loopback_candidates);
        assert!(config.allow_lan_candidates);
        assert_eq!(config.audio_frame_ms, 20);
        assert_eq!(config.audio_bitrate_kbps, 16);
        assert_eq!(config.negotiation_timeout_ms, 5_000);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn frame_durations_are_restricted_to_opus_sizes() {
        for frame_ms in ALLOWED_FRAME_MS {
            let config = RtcConfig {
                audio_frame_ms: frame_ms,
                ..RtcConfig::default()
            };
            assert!(config.validate().is_ok(), "{frame_ms} ms must be accepted");
        }
        for frame_ms in [0, 1, 15, 30, 60, u16::MAX] {
            let config = RtcConfig {
                audio_frame_ms: frame_ms,
                ..RtcConfig::default()
            };
            assert!(config.validate().is_err(), "{frame_ms} ms must be rejected");
        }
    }

    #[test]
    fn bitrate_bounds_are_inclusive() {
        for kbps in [MIN_BITRATE_KBPS, 16, 64, MAX_BITRATE_KBPS] {
            let config = RtcConfig {
                audio_bitrate_kbps: kbps,
                ..RtcConfig::default()
            };
            assert!(config.validate().is_ok(), "{kbps} kbps must be accepted");
        }
        for kbps in [0, MIN_BITRATE_KBPS - 1, MAX_BITRATE_KBPS + 1, u16::MAX] {
            let config = RtcConfig {
                audio_bitrate_kbps: kbps,
                ..RtcConfig::default()
            };
            assert!(config.validate().is_err(), "{kbps} kbps must be rejected");
        }
    }

    #[test]
    fn ice_urls_must_use_a_supported_scheme() {
        for url in [
            "stun:stun.example.org:3478",
            "turn:turn.example.org:3478",
            "turns:turn.example.org:5349",
        ] {
            let config = RtcConfig {
                ice_servers: vec![url.to_owned()],
                turn_username: "alice".to_owned(),
                turn_credential: "secret".to_owned(),
                ..RtcConfig::default()
            };
            assert!(config.validate().is_ok(), "{url} must be accepted");
        }
        for url in [
            "",
            "http://example.org",
            "STUN:stun.example.org",
            "stun.example.org",
            "turn",
        ] {
            let config = RtcConfig {
                ice_servers: vec![url.to_owned()],
                ..RtcConfig::default()
            };
            assert!(config.validate().is_err(), "{url} must be rejected");
        }
    }

    #[test]
    fn ice_urls_are_bounded() {
        let at_limit = format!("stun:{}", "a".repeat(MAX_ICE_URL_BYTES - 5));
        assert_eq!(at_limit.len(), MAX_ICE_URL_BYTES);
        let config = RtcConfig {
            ice_servers: vec![at_limit],
            ..RtcConfig::default()
        };
        assert!(config.validate().is_ok());

        let over_limit = format!("stun:{}", "a".repeat(MAX_ICE_URL_BYTES - 4));
        assert_eq!(over_limit.len(), MAX_ICE_URL_BYTES + 1);
        let config = RtcConfig {
            ice_servers: vec![over_limit],
            ..RtcConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn force_relay_is_carried_without_changing_validation() {
        let config = RtcConfig {
            force_relay: true,
            ice_servers: vec!["turn:turn.example.org:3478".to_owned()],
            turn_username: "alice".to_owned(),
            turn_credential: "secret".to_owned(),
            ..RtcConfig::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn a_turn_server_requires_both_credentials() {
        for (username, credential) in [("", ""), ("alice", ""), ("", "secret")] {
            let config = RtcConfig {
                ice_servers: vec!["turn:turn.example.org:3478".to_owned()],
                turn_username: username.to_owned(),
                turn_credential: credential.to_owned(),
                ..RtcConfig::default()
            };
            assert!(
                config.validate().is_err(),
                "turn with username {username:?} and credential {credential:?} must be refused"
            );
        }

        let config = RtcConfig {
            ice_servers: vec!["stun:stun.example.org:3478".to_owned()],
            ..RtcConfig::default()
        };
        assert!(
            config.validate().is_ok(),
            "STUN alone needs no credentials"
        );
    }

    #[test]
    fn negotiation_timeout_is_bounded() {
        for timeout in [MIN_NEGOTIATION_TIMEOUT_MS, 5_000, MAX_NEGOTIATION_TIMEOUT_MS] {
            let config = RtcConfig {
                negotiation_timeout_ms: timeout,
                ..RtcConfig::default()
            };
            assert!(config.validate().is_ok(), "{timeout} ms must be accepted");
        }
        for timeout in [0, 1, MIN_NEGOTIATION_TIMEOUT_MS - 1, MAX_NEGOTIATION_TIMEOUT_MS + 1] {
            let config = RtcConfig {
                negotiation_timeout_ms: timeout,
                ..RtcConfig::default()
            };
            assert!(config.validate().is_err(), "{timeout} ms must be refused");
        }
    }
}
