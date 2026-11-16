//! The merged configuration shape and its validation rules.
//!
//! These types describe what a user can put in a configuration file. The
//! layering that produces one of these lives in [`crate::load`]. Every struct
//! uses `serde(default)` so a file only has to name the values it changes, and
//! `deny_unknown_fields` so a misspelt key is an error rather than a setting
//! that silently does nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{ConfigError, Result};

/// The configuration format version written into every generated file.
///
/// Version 2 follows the core policy bump: the media policy has separate audio
/// and video bitrate caps rather than one `max_bitrate_kbps`, `client` has a
/// `theme_file` path alongside the `theme` preset name, and the loader reads
/// system drop-ins from `config.d` before the user file. The `[media]` transport
/// and ICE section was added later in the same version: because it is additive,
/// an existing version 2 file without it loads with the defaults and is not
/// rewritten to a new version.
pub const CONFIG_VERSION: u16 = 2;

/// The built-in profile, which needs no file on disk.
pub const DEFAULT_PROFILE: &str = "default";

/// Audio engine profiles the daemon understands.
const AUDIO_PROFILES: [&str; 3] = ["voice-standard", "voice-low", "voice-minimal"];

/// Device backends the daemon understands.
const AUDIO_BACKENDS: [&str; 3] = ["auto", "pipewire", "null"];

/// Sample rates the audio pipeline accepts, in hertz.
///
/// The engine only implements 48 kHz. Accepting 8, 16 or 24 kHz here would
/// pass configuration validation and then make every call fail to open its
/// engine, so unsupported rates are refused up front.
const SAMPLE_RATES: [u32; 1] = [48_000];

/// Audio engines the daemon can actually build.
const AUDIO_ENGINES: [&str; 1] = ["opus"];

/// Frame sizes the audio pipeline accepts, in milliseconds.
const FRAME_SIZES: [u16; 3] = [10, 20, 40];

/// Media transports the daemon can build.
const MEDIA_TRANSPORTS: [&str; 2] = ["webrtc", "libp2p"];

/// The most ICE servers a single call will be handed.
const MAX_ICE_SERVERS: usize = 8;

/// Longest ICE server URL, which accommodates a TURN URL with credentials.
const MAX_ICE_SERVER_BYTES: usize = 256;

/// Longest TURN username or credential accepted, in bytes.
const MAX_TURN_CREDENTIAL_BYTES: usize = 512;

/// Whether a string is usable as a profile name.
///
/// Profile names become file names under the profiles directory, so the rule
/// is deliberately narrow: no separators and no `..`, which keeps a name from
/// escaping the directory before any file is touched.
pub(crate) fn valid_profile_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Whether a string is usable as a video codec name.
fn valid_codec_name(name: &str) -> bool {
    (1..=24).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// The full configuration for the daemon, the clients and their defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub version: u16,
    pub profile: String,
    pub data_dir: Option<PathBuf>,
    pub socket: Option<PathBuf>,
    pub daemon: DaemonConfig,
    pub client: ClientConfig,
    pub audio: AudioConfig,
    pub video: VideoConfig,
    pub media: MediaConfig,
    pub plugins: PluginsConfig,
    pub policy: PolicyDefaults,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            profile: DEFAULT_PROFILE.to_string(),
            data_dir: None,
            socket: None,
            daemon: DaemonConfig::default(),
            client: ClientConfig::default(),
            audio: AudioConfig::default(),
            video: VideoConfig::default(),
            media: MediaConfig::default(),
            plugins: PluginsConfig::default(),
            policy: PolicyDefaults::default(),
        }
    }
}

impl Config {
    /// Check every bound the daemon and clients rely on.
    pub fn validate(&self) -> Result<()> {
        if self.version != CONFIG_VERSION {
            return Err(ConfigError::Invalid(format!(
                "version: expected {CONFIG_VERSION}, got {}",
                self.version
            )));
        }
        if !valid_profile_name(&self.profile) {
            return Err(ConfigError::Invalid(format!(
                "profile: {:?} is not a valid profile name",
                self.profile
            )));
        }
        validate_optional_nonempty("data_dir", &self.data_dir)?;
        validate_absolute("data_dir", &self.data_dir)?;
        validate_optional_nonempty("socket", &self.socket)?;
        validate_absolute("socket", &self.socket)?;
        self.daemon.validate()?;
        self.client.validate()?;
        self.audio.validate()?;
        self.video.validate()?;
        self.media.validate()?;
        self.plugins.validate()?;
        self.policy
            .media
            .validate()
            .map_err(|error| ConfigError::Invalid(format!("policy.media: {error}")))?;
        Ok(())
    }

    /// A commented, fully populated file that can be written by `ovvc config
    /// init`. It parses back to [`Config::default`].
    pub fn default_toml() -> String {
        let body = Self::default().to_toml().unwrap_or_default();
        format!(
            "# ovvc configuration (version {CONFIG_VERSION}).\n\
             # Layers, weakest first: built-in defaults < system file < system drop-ins (config.d/*.toml, name order) < user file < profile < environment < flags.\n\
             # Every value below is the built-in default.\n\
             # The daemon tray is on by default (set daemon.tray = false to opt out); GNOME needs a StatusNotifier/AppIndicator shell extension to show it.\n\n{body}"
        )
    }

    /// Serialise the configuration as a TOML document.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|error| ConfigError::Serialize(error.to_string()))
    }
}

/// Daemon process and network defaults.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Desktop notifications for mentions and replies. Off by default: a
    /// daemon started from a terminal should not pop up on its own.
    pub notify: bool,
    /// Show the StatusNotifierItem tray icon. On by default so a daemon
    /// started directly, or spawned by the GUI or TUI, has a visible way to
    /// stop without a task manager. Set `tray = false` to opt out; a build
    /// without the `tray` feature logs that the tray is unavailable and keeps
    /// running. GNOME needs a StatusNotifier/AppIndicator shell extension to
    /// display SNI icons, and the daemon still works without one.
    pub tray: bool,
    /// Bring the peer network up on start. Off by default; a frontend that
    /// spawns the daemon passes `--online`.
    pub online: bool,
    pub network: NetworkDefaults,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            notify: false,
            tray: true,
            online: false,
            network: NetworkDefaults::default(),
        }
    }
}

impl DaemonConfig {
    fn validate(&self) -> Result<()> {
        self.network.validate()
    }
}

/// How the daemon starts its transport.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NetworkDefaults {
    pub listen: Vec<String>,
    pub relay: Option<String>,
    pub upnp: bool,
    pub natpmp: bool,
}

impl NetworkDefaults {
    fn validate(&self) -> Result<()> {
        if self.listen.len() > 16 {
            return Err(ConfigError::Invalid(format!(
                "daemon.network.listen: at most 16 addresses, got {}",
                self.listen.len()
            )));
        }
        for address in &self.listen {
            if !(1..=256).contains(&address.len()) {
                return Err(ConfigError::Invalid(format!(
                    "daemon.network.listen: each address must be 1 to 256 bytes, got {}",
                    address.len()
                )));
            }
        }
        if let Some(relay) = &self.relay
            && relay.len() > 256
        {
            return Err(ConfigError::Invalid(format!(
                "daemon.network.relay: at most 256 bytes, got {}",
                relay.len()
            )));
        }
        Ok(())
    }
}

/// Client presentation settings.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// A named preset, such as `"phosphor"`. The preset is a starting point
    /// rather than a file path.
    pub theme: Option<String>,
    /// An absolute theme file to load on top of the preset.
    pub theme_file: Option<PathBuf>,
    pub keybindings: BTreeMap<String, String>,
}

impl ClientConfig {
    fn validate(&self) -> Result<()> {
        if let Some(theme) = &self.theme
            && theme.len() > 64
        {
            return Err(ConfigError::Invalid(format!(
                "client.theme: at most 64 bytes, got {}",
                theme.len()
            )));
        }
        validate_optional_nonempty("client.theme_file", &self.theme_file)?;
        // A relative theme file would resolve against each frontend process's
        // working directory, so the same config could pick a different theme
        // depending on where `ovvc-tui` or `ovvc-gui` was launched.
        validate_absolute("client.theme_file", &self.theme_file)?;
        if self.keybindings.len() > 128 {
            return Err(ConfigError::Invalid(format!(
                "client.keybindings: at most 128 entries, got {}",
                self.keybindings.len()
            )));
        }
        for action in self.keybindings.keys() {
            if !(1..=64).contains(&action.len()) {
                return Err(ConfigError::Invalid(format!(
                    "client.keybindings: each key must be 1 to 64 bytes, got {}",
                    action.len()
                )));
            }
        }
        Ok(())
    }
}

/// Audio engine selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AudioConfig {
    pub engine: String,
    /// Device backend: `"auto"`, `"pipewire"` or `"null"`.
    pub backend: String,
    pub pipeline: Option<PathBuf>,
    pub sample_rate: u32,
    pub frame_ms: u16,
    pub profile: String,
    pub ptt_only: bool,
    pub plugins: Vec<String>,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            engine: "opus".to_string(),
            backend: "auto".to_string(),
            pipeline: None,
            sample_rate: 48_000,
            frame_ms: 20,
            profile: "voice-standard".to_string(),
            ptt_only: false,
            plugins: Vec::new(),
        }
    }
}

impl AudioConfig {
    fn validate(&self) -> Result<()> {
        if !AUDIO_ENGINES.contains(&self.engine.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "audio.engine: {:?} is not one of {AUDIO_ENGINES:?}",
                self.engine
            )));
        }
        if !AUDIO_BACKENDS.contains(&self.backend.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "audio.backend: {:?} is not one of {AUDIO_BACKENDS:?}",
                self.backend
            )));
        }
        if !AUDIO_PROFILES.contains(&self.profile.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "audio.profile: {:?} is not one of {AUDIO_PROFILES:?}",
                self.profile
            )));
        }
        if !SAMPLE_RATES.contains(&self.sample_rate) {
            return Err(ConfigError::Invalid(format!(
                "audio.sample_rate: {} is not one of {SAMPLE_RATES:?}",
                self.sample_rate
            )));
        }
        if !FRAME_SIZES.contains(&self.frame_ms) {
            return Err(ConfigError::Invalid(format!(
                "audio.frame_ms: {} is not one of {FRAME_SIZES:?}",
                self.frame_ms
            )));
        }
        validate_optional_nonempty("audio.pipeline", &self.pipeline)?;
        // The daemon loads a relative path against its own working
        // directory, which differs when a desktop launcher starts it. A
        // pipeline must be absolute so it means the same file everywhere.
        validate_absolute("audio.pipeline", &self.pipeline)?;
        if self.ptt_only {
            return Err(ConfigError::Invalid(
                "audio.ptt_only is not implemented yet; use /voice ptt with the hold key"
                    .to_string(),
            ));
        }
        if !self.plugins.is_empty() {
            return Err(ConfigError::Invalid(
                "audio.plugins is not implemented yet; the engine loads no plugins".to_string(),
            ));
        }
        Ok(())
    }
}

/// Video engine selection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VideoConfig {
    pub engine: String,
    pub codecs: Vec<String>,
    pub hardware_accel: bool,
    pub screen_share_codec: String,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            engine: "webrtc".to_string(),
            codecs: vec![
                "vp9".to_string(),
                "vp8".to_string(),
                "av1".to_string(),
                "h264".to_string(),
            ],
            hardware_accel: true,
            screen_share_codec: "h264".to_string(),
        }
    }
}

impl VideoConfig {
    fn validate(&self) -> Result<()> {
        if !(1..=32).contains(&self.engine.len()) {
            return Err(ConfigError::Invalid(format!(
                "video.engine: must be 1 to 32 bytes, got {}",
                self.engine.len()
            )));
        }
        if !(1..=16).contains(&self.codecs.len()) {
            return Err(ConfigError::Invalid(format!(
                "video.codecs: must have 1 to 16 entries, got {}",
                self.codecs.len()
            )));
        }
        let mut seen = BTreeSet::new();
        for codec in &self.codecs {
            if !valid_codec_name(codec) {
                return Err(ConfigError::Invalid(format!(
                    "video.codecs: {codec:?} must be 1 to 24 lowercase ascii, digit or '-' bytes"
                )));
            }
            if !seen.insert(codec.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "video.codecs: {codec:?} appears more than once"
                )));
            }
        }
        if !valid_codec_name(&self.screen_share_codec) {
            return Err(ConfigError::Invalid(format!(
                "video.screen_share_codec: {:?} must be 1 to 24 lowercase ascii, digit or '-' bytes",
                self.screen_share_codec
            )));
        }
        Ok(())
    }
}

/// Media transport and ICE configuration.
///
/// This section is additive in configuration version 2: a file written before
/// it existed simply gets the defaults, so no version bump is needed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MediaConfig {
    /// How media is carried: `"webrtc"` or `"libp2p"`.
    pub transport: String,
    /// STUN/TURN URLs handed to the media engine. Empty means host candidates
    /// only.
    pub ice_servers: Vec<String>,
    /// Username for every `turn:`/`turns:` server. Both this and
    /// `turn_credential` are required when a TURN URL is configured, because
    /// the WebRTC stack refuses to build a peer without them.
    pub turn_username: Option<String>,
    /// Credential for every `turn:`/`turns:` server.
    pub turn_credential: Option<String>,
    /// Gather and accept loopback candidates. Off in production; a test or a
    /// same-machine pair sets it so two peers on one host can connect.
    pub allow_loopback_candidates: bool,
    /// Accept private LAN candidates. On by default because a LAN call is the
    /// supported direct path; a relay-only deployment turns it off.
    pub allow_lan_candidates: bool,
    /// Refuse to pair host or server-reflexive candidates and relay everything.
    pub force_relay: bool,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            transport: "webrtc".to_string(),
            ice_servers: Vec::new(),
            turn_username: None,
            turn_credential: None,
            allow_loopback_candidates: false,
            allow_lan_candidates: true,
            force_relay: false,
        }
    }
}

impl MediaConfig {
    fn validate(&self) -> Result<()> {
        if !MEDIA_TRANSPORTS.contains(&self.transport.as_str()) {
            return Err(ConfigError::Invalid(format!(
                "media.transport: {:?} is not one of {MEDIA_TRANSPORTS:?}",
                self.transport
            )));
        }
        if self.ice_servers.len() > MAX_ICE_SERVERS {
            return Err(ConfigError::Invalid(format!(
                "media.ice_servers: at most {MAX_ICE_SERVERS} servers, got {}",
                self.ice_servers.len()
            )));
        }
        for server in &self.ice_servers {
            if !(1..=MAX_ICE_SERVER_BYTES).contains(&server.len()) {
                return Err(ConfigError::Invalid(format!(
                    "media.ice_servers: each server must be 1 to {MAX_ICE_SERVER_BYTES} bytes, got {}",
                    server.len()
                )));
            }
            if !is_ice_server_url(server) {
                return Err(ConfigError::Invalid(format!(
                    "media.ice_servers: {server:?} must start with stun:, turn: or turns:"
                )));
            }
        }
        // `rtc` refuses a TURN URL without a username and credential while it
        // builds the peer connection, which would fail one second after the
        // call starts. Refuse the file up front instead.
        let has_turn = self.ice_servers.iter().any(|server| is_turn_url(server));
        if has_turn && !valid_turn_credential(&self.turn_username) {
            return Err(ConfigError::Invalid(
                "media.turn_username is required and must not be empty when a turn: or turns: server is configured"
                    .to_string(),
            ));
        }
        if has_turn && !valid_turn_credential(&self.turn_credential) {
            return Err(ConfigError::Invalid(
                "media.turn_credential is required and must not be empty when a turn: or turns: server is configured"
                    .to_string(),
            ));
        }
        for (field, value) in [
            ("media.turn_username", &self.turn_username),
            ("media.turn_credential", &self.turn_credential),
        ] {
            if let Some(value) = value
                && value.len() > MAX_TURN_CREDENTIAL_BYTES
            {
                return Err(ConfigError::Invalid(format!(
                    "{field}: at most {MAX_TURN_CREDENTIAL_BYTES} bytes, got {}",
                    value.len()
                )));
            }
        }
        // Forcing a relay without a relay server can only produce a call that
        // never connects, so refuse the combination up front.
        if self.force_relay && !self.ice_servers.iter().any(|server| is_turn_url(server)) {
            return Err(ConfigError::Invalid(
                "media.force_relay requires a turn: or turns: server".to_string(),
            ));
        }
        Ok(())
    }
}

/// Whether an optional TURN credential is present and non-empty.
fn valid_turn_credential(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|value| !value.is_empty())
}

/// Whether a string is a STUN or TURN URL the media engine will accept.
fn is_ice_server_url(server: &str) -> bool {
    server.starts_with("stun:") || server.starts_with("turn:") || server.starts_with("turns:")
}

/// Whether a string asks the media engine to relay through a TURN server.
fn is_turn_url(server: &str) -> bool {
    server.starts_with("turn:") || server.starts_with("turns:")
}

/// Plugin loading policy and search paths.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginsConfig {
    pub policy: ovvc_core::PluginPolicy,
    pub dirs: Vec<PathBuf>,
    pub allowed: Vec<String>,
}

impl PluginsConfig {
    fn validate(&self) -> Result<()> {
        if self.dirs.len() > 32 {
            return Err(ConfigError::Invalid(format!(
                "plugins.dirs: at most 32 directories, got {}",
                self.dirs.len()
            )));
        }
        for dir in &self.dirs {
            if dir.as_os_str().is_empty() {
                return Err(ConfigError::Invalid(
                    "plugins.dirs: a directory path must not be empty".to_string(),
                ));
            }
        }
        if self.allowed.len() > 256 {
            return Err(ConfigError::Invalid(format!(
                "plugins.allowed: at most 256 entries, got {}",
                self.allowed.len()
            )));
        }
        for plugin in &self.allowed {
            if !(1..=128).contains(&plugin.len()) {
                return Err(ConfigError::Invalid(format!(
                    "plugins.allowed: each entry must be 1 to 128 bytes, got {}",
                    plugin.len()
                )));
            }
        }
        Ok(())
    }
}

/// Policy floors a member applies on top of whatever a guild requires.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyDefaults {
    pub client_floor: ovvc_core::ClientPolicyFloor,
    pub media: ovvc_core::MediaPolicy,
}

/// Reject `Some(path)` when the path is the empty string.
fn validate_optional_nonempty(field: &str, path: &Option<PathBuf>) -> Result<()> {
    if let Some(path) = path
        && path.as_os_str().is_empty()
    {
        return Err(ConfigError::Invalid(format!(
            "{field}: a path must not be empty"
        )));
    }
    Ok(())
}

/// Reject a relative path, which would otherwise resolve against each
/// process's working directory and differ between the daemon and a client.
fn validate_absolute(field: &str, path: &Option<PathBuf>) -> Result<()> {
    if let Some(path) = path
        && !path.is_absolute()
    {
        return Err(ConfigError::Invalid(format!(
            "{field}: {} must be an absolute path",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate_and_default_toml_round_trips() {
        let defaults = Config::default();
        defaults.validate().unwrap();

        let text = Config::default_toml();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed, defaults);
    }

    #[test]
    fn the_tray_is_on_by_default_and_can_be_disabled() {
        let config = Config::default();
        assert!(
            config.daemon.tray,
            "a daemon with no explicit flag or config must start the tray"
        );

        // The generated file is the user path: it has to say `tray = true`
        // so a daemon started from it gets the same default.
        let text = Config::default_toml();
        assert!(text.contains("tray = true"), "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert!(parsed.daemon.tray);
        parsed.validate().unwrap();

        // The explicit opt-out survives a partial file, which is the only
        // way a headless install turns the tray off.
        let disabled: Config = toml::from_str("[daemon]\ntray = false\n").unwrap();
        assert!(!disabled.daemon.tray);
        assert!(!disabled.daemon.notify, "the other defaults still apply");
        disabled.validate().unwrap();
    }

    #[test]
    fn client_theme_preset_and_theme_file_are_separate() {
        let mut config = Config::default();
        config.client.theme = Some("phosphor".to_string());
        config.client.theme_file = Some(PathBuf::from("/home/user/themes/custom.toml"));
        config.validate().unwrap();

        config.client.theme = Some("x".repeat(65));
        assert!(config.validate().is_err(), "theme is capped at 64 bytes");

        config.client.theme = Some("phosphor".to_string());
        config.client.theme_file = Some(PathBuf::new());
        assert!(config.validate().is_err(), "theme_file must not be empty");

        config.client.theme_file = Some(PathBuf::from("relative/theme.toml"));
        assert!(
            config.validate().is_err(),
            "a relative theme_file would resolve against each process's cwd"
        );

        config.client.theme_file = None;
        config.validate().unwrap();
    }

    #[test]
    fn an_unknown_key_is_refused_by_deserialisation() {
        let error = toml::from_str::<Config>("\n[audio]\nframe_size = 20\n").unwrap_err();
        assert!(error.to_string().contains("frame_size"), "{error}");
    }

    #[test]
    fn a_bad_frame_size_is_invalid_not_a_parse_error() {
        let mut config = Config::default();
        config.audio.frame_ms = 25;
        match config.validate() {
            Err(ConfigError::Invalid(message)) => assert!(message.contains("audio.frame_ms")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn profile_names_cannot_escape_the_profiles_directory() {
        for name in ["", ".", "..", "../evil", "a/b", "a\\b", "with space"] {
            let config = Config {
                profile: name.to_string(),
                ..Config::default()
            };
            assert!(config.validate().is_err(), "{name:?} should be refused");
        }
        for name in ["default", "work", "home.lab", "voice_v2", "A-1"] {
            let config = Config {
                profile: name.to_string(),
                ..Config::default()
            };
            assert!(config.validate().is_ok(), "{name:?} should be accepted");
        }
    }

    #[test]
    fn video_codecs_must_be_unique_and_well_formed() {
        let mut config = Config::default();
        config.video.codecs = vec!["opus".to_string(), "opus".to_string()];
        assert!(config.validate().is_err());

        config.video.codecs = vec!["VP9".to_string()];
        assert!(config.validate().is_err());

        config.video.codecs = Vec::new();
        assert!(config.validate().is_err());

        config.video.codecs = vec!["vp9".to_string()];
        config.video.screen_share_codec = "h264".to_string();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn relative_data_dir_and_socket_are_rejected() {
        let config = Config {
            socket: Some(PathBuf::from("ovvc.sock")),
            ..Config::default()
        };
        assert!(config.validate().is_err());

        let config = Config {
            socket: Some(PathBuf::from("/run/ovvc/daemon.sock")),
            data_dir: Some(PathBuf::from("relative/data")),
            ..Config::default()
        };
        assert!(config.validate().is_err());

        let config = Config {
            socket: Some(PathBuf::from("/run/ovvc/daemon.sock")),
            data_dir: Some(PathBuf::from("/var/lib/ovvc")),
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn audio_bounds_are_checked() {
        let mut config = Config::default();
        config.audio.sample_rate = 44_100;
        assert!(config.validate().is_err());
        config.audio.sample_rate = 8_000;
        assert!(
            config.validate().is_err(),
            "8 kHz validates but the engine only runs at 48 kHz"
        );

        config.audio.sample_rate = 48_000;
        config.audio.profile = "voice-turbo".to_string();
        assert!(config.validate().is_err());

        config.audio.profile = "voice-low".to_string();
        config.audio.pipeline = Some(PathBuf::new());
        assert!(config.validate().is_err());

        config.audio.pipeline = Some(PathBuf::from("relative/pipelines.toml"));
        assert!(
            config.validate().is_err(),
            "a relative pipeline path depends on the daemon's cwd"
        );

        config.audio.pipeline = None;
        config.audio.backend = "alsa".to_string();
        assert!(config.validate().is_err());
        for backend in ["auto", "pipewire", "null"] {
            config.audio.backend = backend.to_string();
            assert!(config.validate().is_ok(), "{backend} should be accepted");
        }

        config.audio.engine = "codec2".to_string();
        assert!(
            config.validate().is_err(),
            "only the opus engine is implemented"
        );
        config.audio.engine = "opus".to_string();

        config.audio.plugins = vec!["denoise".to_string()];
        assert!(config.validate().is_err(), "plugins are not loaded");
        config.audio.plugins.clear();

        config.audio.ptt_only = true;
        assert!(config.validate().is_err(), "ptt_only is not applied");
    }

    #[test]
    fn media_defaults_are_webrtc_with_no_ice_servers() {
        let config = Config::default();
        assert_eq!(config.media.transport, "webrtc");
        assert!(config.media.ice_servers.is_empty());
        assert!(config.media.turn_username.is_none());
        assert!(config.media.turn_credential.is_none());
        assert!(!config.media.allow_loopback_candidates);
        assert!(config.media.allow_lan_candidates);
        assert!(!config.media.force_relay);
        config.validate().unwrap();

        let text = Config::default_toml();
        assert!(text.contains("[media]"), "{text}");
        assert!(text.contains("transport = \"webrtc\""), "{text}");
        let parsed: Config = toml::from_str(&text).unwrap();
        assert_eq!(parsed.media, config.media);
    }

    #[test]
    fn a_custom_stun_server_validates() {
        let mut config = Config::default();
        config.media.ice_servers = vec!["stun:stun.example.org:3478".to_string()];
        config.validate().unwrap();

        for transport in ["webrtc", "libp2p"] {
            config.media.transport = transport.to_string();
            config.validate().unwrap();
        }
    }

    #[test]
    fn an_unknown_media_transport_fails() {
        let mut config = Config::default();
        config.media.transport = "carrier-pigeon".to_string();
        match config.validate() {
            Err(ConfigError::Invalid(message)) => assert!(message.contains("media.transport")),
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn force_relay_without_a_turn_server_fails() {
        let mut config = Config::default();
        config.media.force_relay = true;
        assert!(config.validate().is_err());

        // A STUN server cannot relay, so it does not satisfy force_relay.
        config.media.ice_servers = vec!["stun:stun.example.org:3478".to_string()];
        assert!(config.validate().is_err());

        config.media.ice_servers = vec!["turn:turn.example.org:3478".to_string()];
        // A TURN URL needs credentials before it can be valid.
        config.media.turn_username = Some("alice".to_string());
        config.media.turn_credential = Some("secret".to_string());
        config.validate().unwrap();

        config.media.ice_servers = vec!["turns:turn.example.org:5349".to_string()];
        config.validate().unwrap();
    }

    #[test]
    fn turn_servers_require_username_and_credential() {
        let mut config = Config::default();
        config.media.ice_servers = vec!["turn:turn.example.org:3478".to_string()];
        assert!(
            config.validate().is_err(),
            "a TURN URL without credentials must be refused at load"
        );

        config.media.turn_username = Some("alice".to_string());
        assert!(config.validate().is_err(), "the credential is still missing");

        config.media.turn_credential = Some(String::new());
        assert!(config.validate().is_err(), "an empty credential is not usable");

        config.media.turn_credential = Some("secret".to_string());
        config.validate().unwrap();

        // STUN alone does not need credentials even if empty values linger.
        config.media.ice_servers = vec!["stun:stun.example.org:3478".to_string()];
        config.media.turn_username = None;
        config.media.turn_credential = None;
        config.validate().unwrap();

        config.media.turn_credential = Some("x".repeat(MAX_TURN_CREDENTIAL_BYTES + 1));
        assert!(config.validate().is_err(), "oversized credentials are refused");
    }

    #[test]
    fn loopback_candidates_are_refused_by_default() {
        let mut config = Config::default();
        assert!(!config.media.allow_loopback_candidates);
        assert!(config.media.allow_lan_candidates);
        config.media.allow_loopback_candidates = true;
        config.media.allow_lan_candidates = false;
        config.validate().unwrap();
    }

    #[test]
    fn ice_servers_are_bounded_and_must_be_stun_or_turn_urls() {
        let mut config = Config::default();
        config.media.ice_servers = vec!["http://example.org".to_string()];
        assert!(config.validate().is_err(), "only stun/turn urls are usable");

        config.media.ice_servers = vec!["".to_string()];
        assert!(config.validate().is_err(), "an empty url is not usable");

        config.media.ice_servers = vec!["stun:x".to_string(); MAX_ICE_SERVERS + 1];
        assert!(config.validate().is_err(), "at most eight servers");
    }
}
