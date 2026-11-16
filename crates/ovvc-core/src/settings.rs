//! Local notification preferences.
//!
//! These live in `ovvc-core` rather than the IPC protocol because they describe
//! how a client should behave, not how the daemon talks to it. The daemon
//! persists them and every frontend reads the same copy.

use serde::{Deserialize, Serialize};

use crate::model::{ChannelId, GuildId};

/// A quiet-hours window as minutes past midnight, evaluated in UTC.
///
/// Frontends label it as UTC. Carrying a timezone per window would need a tz
/// database in the daemon, which is not there yet, and pretending local time
/// would quietly fire at the wrong hour after a DST change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuietHours {
    pub start_minute: u16,
    pub end_minute: u16,
}

impl QuietHours {
    /// Whether `minute` (minutes past midnight) falls inside the window. A
    /// window that wraps midnight, such as 22:00 to 07:00, is supported.
    pub fn contains(&self, minute: u16) -> bool {
        if self.start_minute == self.end_minute {
            return false;
        }
        if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute).contains(&minute)
        } else {
            minute >= self.start_minute || minute < self.end_minute
        }
    }
}

/// How the local author wants to be notified. Held by the daemon so every
/// frontend sees the same answer, and persisted so it survives a restart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationSettings {
    /// Raise desktop notifications at all.
    pub desktop: bool,
    /// Suppress every notification, including mentions and replies.
    pub dnd: bool,
    pub quiet_hours: Option<QuietHours>,
    pub muted_guilds: Vec<GuildId>,
    pub muted_channels: Vec<ChannelId>,
}

impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            desktop: true,
            dnd: false,
            quiet_hours: None,
            muted_guilds: Vec::new(),
            muted_channels: Vec::new(),
        }
    }
}

/// Minutes past midnight in UTC.
pub fn utc_minute(now_unix: u64) -> u16 {
    ((now_unix / 60) % 1440) as u16
}

impl NotificationSettings {
    /// Whether a message in `channel_id` should raise no alert at all.
    ///
    /// Do-not-disturb, a muted guild or channel, and quiet hours all mean the
    /// same thing: no in-app alert and no desktop popup. `desktop` is
    /// deliberately not consulted, because turning off popups should not also
    /// silence the in-app mention badge.
    ///
    /// The daemon and the TUI both call this, so a mute cannot be honoured in
    /// one frontend and ignored in the other.
    pub fn suppresses(&self, guild_id: GuildId, channel_id: ChannelId, now_unix: u64) -> bool {
        self.muted_channels.contains(&channel_id) || self.suppresses_guild(guild_id, now_unix)
    }

    /// The same rule for an event that names no channel, which older wire
    /// events can do: the guild-wide reasons still apply, but there is no
    /// channel mute to match.
    pub fn suppresses_guild(&self, guild_id: GuildId, now_unix: u64) -> bool {
        if self.dnd || self.muted_guilds.contains(&guild_id) {
            return true;
        }
        self.quiet_hours
            .is_some_and(|quiet| quiet.contains(utc_minute(now_unix)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn guild(seed: u8) -> GuildId {
        GuildId([seed; 32])
    }

    fn channel(seed: u8) -> ChannelId {
        ChannelId([seed; 16])
    }

    #[test]
    fn the_defaults_allow_everything() {
        let settings = NotificationSettings::default();
        assert!(!settings.suppresses(guild(1), channel(2), 0));
    }

    #[test]
    fn dnd_and_mutes_suppress() {
        let settings = NotificationSettings {
            dnd: true,
            ..Default::default()
        };
        assert!(settings.suppresses(guild(1), channel(2), 0));

        let settings = NotificationSettings {
            muted_guilds: vec![guild(1)],
            ..Default::default()
        };
        assert!(settings.suppresses(guild(1), channel(2), 0));
        assert!(!settings.suppresses(guild(3), channel(2), 0));

        let settings = NotificationSettings {
            muted_channels: vec![channel(2)],
            ..Default::default()
        };
        assert!(settings.suppresses(guild(1), channel(2), 0));
        assert!(!settings.suppresses(guild(1), channel(4), 0));
    }

    #[test]
    fn a_channel_less_event_still_honours_the_guild_wide_reasons() {
        // Older wire events carry no channel id; a channel mute cannot match
        // them, but dnd and a muted guild still apply.
        let settings = NotificationSettings {
            muted_channels: vec![channel(2)],
            ..Default::default()
        };
        assert!(!settings.suppresses_guild(guild(1), 0));

        let settings = NotificationSettings {
            muted_guilds: vec![guild(1)],
            ..Default::default()
        };
        assert!(settings.suppresses_guild(guild(1), 0));
        assert!(!settings.suppresses_guild(guild(3), 0));

        let settings = NotificationSettings {
            quiet_hours: Some(QuietHours {
                start_minute: 22 * 60,
                end_minute: 7 * 60,
            }),
            ..Default::default()
        };
        assert!(settings.suppresses_guild(guild(1), 23 * 3600));
        assert!(!settings.suppresses_guild(guild(1), 12 * 3600));
    }

    #[test]
    fn quiet_hours_wrap_midnight() {
        let settings = NotificationSettings {
            quiet_hours: Some(QuietHours {
                start_minute: 22 * 60,
                end_minute: 7 * 60,
            }),
            ..Default::default()
        };
        // 23:00 UTC and 06:30 UTC are inside, 12:00 UTC is not.
        assert!(settings.suppresses(guild(1), channel(2), 23 * 3600));
        assert!(settings.suppresses(guild(1), channel(2), 6 * 3600 + 1800));
        assert!(!settings.suppresses(guild(1), channel(2), 12 * 3600));
    }
}
