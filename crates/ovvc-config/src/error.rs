use std::path::PathBuf;

/// Everything that can stop a configuration from loading.
///
/// The variants carry the path and the original error so `ovvc config
/// validate` can name the file and line a user needs to fix, rather than a
/// generic "invalid configuration".
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not determine a configuration directory; set OVVC_CONFIG_DIR")]
    NoConfigDir,

    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("could not parse {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// A merged document that draws on more than one file cannot be blamed on
    /// the most specific one, so name every contributor instead.
    #[error("could not parse the merged configuration from {sources}: {source}")]
    ParseMerged {
        sources: String,
        #[source]
        source: toml::de::Error,
    },

    #[error("could not serialize the merged configuration: {0}")]
    Serialize(String),

    #[error("configuration profile {0:?} does not exist")]
    MissingProfile(String),

    #[error("configuration profiles form a cycle: {0}")]
    ProfileCycle(String),

    #[error("configuration profile chain is deeper than {0} levels: {1}")]
    ProfileTooDeep(usize, String),

    #[error("configuration profile name {0:?} is invalid")]
    InvalidProfileName(String),

    #[error("{path} already exists; pass --force to replace it")]
    AlreadyExists { path: PathBuf },

    #[error("invalid configuration: {0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;
