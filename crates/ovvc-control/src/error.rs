use ovvc_core::CoreError;

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error(transparent)]
    Core(#[from] CoreError),

    #[error("postcard error: {0}")]
    Postcard(#[from] postcard::Error),

    #[error("control frame is too large: {0} bytes")]
    TooLarge(usize),

    #[error("unsupported control version: {0}")]
    UnsupportedVersion(u16),

    #[error("invalid control signature")]
    InvalidSignature,

    #[error("control frame is stale or from the future")]
    Stale,

    #[error("replayed control frame")]
    Replay,

    #[error("sender is not a member of this guild")]
    NotAMember,

    #[error("rate limit exceeded")]
    RateLimited,

    #[error("control frame from another guild or session")]
    WrongSession,

    #[error("invalid control frame: {0}")]
    Invalid(String),

    /// A voice action was well-formed but the media state machine refused it,
    /// for example a join whose profile sets do not intersect.
    #[error(transparent)]
    Media(#[from] ovvc_media::MediaError),
}

pub type Result<T> = std::result::Result<T, ControlError>;
