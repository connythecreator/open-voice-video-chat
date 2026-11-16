//! Layered configuration for ovvc.
//!
//! The crate follows the Arch-like rule the project settled on: built-in
//! defaults, then a system file, a user file, a named profile, the environment
//! and finally explicit flags. Each layer only names what it changes.
//!
//! ```no_run
//! use ovvc_config::{ConfigPaths, Overrides, load};
//!
//! let paths = ConfigPaths::with_roots(
//!     std::path::PathBuf::from("/tmp/ovvc-doc"),
//!     std::path::PathBuf::from("/etc/ovvc/config.toml"),
//! );
//! let loaded = load(&paths, Overrides::default()).unwrap();
//! println!("frame size: {} ms", loaded.config.audio.frame_ms);
//! ```

pub mod config;
pub mod error;
pub mod load;

pub use config::{
    AudioConfig, CONFIG_VERSION, ClientConfig, Config, DEFAULT_PROFILE, DaemonConfig, MediaConfig,
    NetworkDefaults, PluginsConfig, PolicyDefaults, VideoConfig,
};
pub use error::{ConfigError, Result};
pub use load::{
    ConfigLayer, ConfigPaths, ConfigSource, LoadedConfig, Overrides, init, load,
    system_dropin_files,
};
