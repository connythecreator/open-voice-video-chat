//! Notification preferences, persisted next to the store.
//!
//! The daemon owns these so every attached frontend reads and writes the same
//! answer, and a restart does not silently re-enable a mute. The filtering
//! decision is a pure function so it can be tested without a clock.

use std::fs;
use std::path::{Path, PathBuf};

use ovvc_core::NotificationSettings;
use ovvc_core::{ChannelId, GuildId};

pub const SETTINGS_FILE: &str = "notifications.json";

pub fn settings_path(data_dir: &Path) -> PathBuf {
    data_dir.join(SETTINGS_FILE)
}

/// Read the settings file, falling back to the defaults when it is missing or
/// unreadable. A corrupt file must not stop the daemon from starting.
pub fn load(data_dir: &Path) -> NotificationSettings {
    fs::read(settings_path(data_dir))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Write the settings atomically: a crash mid-write leaves the old file intact
/// rather than a half-written one that fails to parse.
pub fn save(data_dir: &Path, settings: &NotificationSettings) -> std::io::Result<()> {
    let path = settings_path(data_dir);
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(settings).map_err(std::io::Error::other)?;
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, &path)
}

/// Whether a message in `channel_id` should be swallowed entirely.
///
/// Do-not-disturb, a muted guild or channel, and quiet hours all mean the same
/// thing at this layer: no [`ovvc_ipc::DaemonEvent::Notification`] is emitted,
/// so no frontend shows an in-app alert either. `desktop` is deliberately not
/// consulted here, because turning off desktop popups should not also silence
/// the in-app badge. The rule itself lives in `ovvc-core` so the TUI applies
/// the same one to the alerts it raises locally.
pub fn suppresses(
    settings: &NotificationSettings,
    guild_id: GuildId,
    channel_id: ChannelId,
    now_unix: u64,
) -> bool {
    settings.suppresses(guild_id, channel_id, now_unix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_core::QuietHours;

    fn guild(seed: u8) -> GuildId {
        GuildId([seed; 32])
    }

    fn channel(seed: u8) -> ChannelId {
        ChannelId([seed; 16])
    }

    #[test]
    fn defaults_allow_everything() {
        let settings = NotificationSettings::default();
        assert!(settings.desktop);
        assert!(!suppresses(&settings, guild(1), channel(2), 0));
    }

    #[test]
    fn dnd_mutes_every_channel() {
        let settings = NotificationSettings {
            dnd: true,
            ..Default::default()
        };
        assert!(suppresses(&settings, guild(1), channel(2), 0));
    }

    #[test]
    fn a_muted_guild_mutes_its_channels() {
        let settings = NotificationSettings {
            muted_guilds: vec![guild(1)],
            ..Default::default()
        };
        assert!(suppresses(&settings, guild(1), channel(2), 0));
        assert!(!suppresses(&settings, guild(3), channel(2), 0));
    }

    #[test]
    fn a_muted_channel_mutes_only_itself() {
        let settings = NotificationSettings {
            muted_channels: vec![channel(2)],
            ..Default::default()
        };
        assert!(suppresses(&settings, guild(1), channel(2), 0));
        assert!(!suppresses(&settings, guild(1), channel(3), 0));
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let quiet = QuietHours {
            start_minute: 22 * 60,
            end_minute: 7 * 60,
        };
        assert!(quiet.contains(23 * 60));
        assert!(quiet.contains(0));
        assert!(quiet.contains(6 * 60 + 59));
        assert!(!quiet.contains(7 * 60));
        assert!(!quiet.contains(12 * 60));
    }

    #[test]
    fn quiet_hours_suppress_only_inside_the_window() {
        let quiet = QuietHours {
            start_minute: 60,
            end_minute: 120,
        };
        let settings = NotificationSettings {
            quiet_hours: Some(quiet),
            ..Default::default()
        };
        let at = |minute: u64| minute * 60;
        assert!(suppresses(&settings, guild(1), channel(2), at(61)));
        assert!(!suppresses(&settings, guild(1), channel(2), at(121)));
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let dir = std::env::temp_dir().join(format!("ovvc-settings-{}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let settings = NotificationSettings {
            desktop: false,
            dnd: true,
            quiet_hours: Some(QuietHours {
                start_minute: 10,
                end_minute: 20,
            }),
            muted_guilds: vec![guild(4)],
            muted_channels: vec![channel(5)],
        };
        save(&dir, &settings).expect("save");
        assert_eq!(load(&dir), settings);
        fs::remove_dir_all(&dir).expect("clean up");
    }
}
