//! The error taxonomy shared by the daemon, the IPC boundary and the
//! frontends.
//!
//! The point of the taxonomy is that a transient network condition is not
//! presented as an application failure, and an unexpected internal failure is
//! not hidden in the same red banner as "that peer went offline". The class is
//! decided where the error is known, at the daemon emission site, and travels
//! with the message instead of being guessed from the text in a UI.

use serde::{Deserialize, Serialize};

use crate::error::CoreError;
use crate::model::GuildId;

/// What kind of condition a diagnostic describes.
///
/// The order is also the display order from most to least severe. `Bug` is the
/// only class that asks the user to report anything; the rest are states the
/// software is expected to handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum ErrorClass {
    /// An unexpected internal failure, invariant violation, impossible state,
    /// unwrap or parse failure, or a protocol bug. Log it with context and ask
    /// for a bug report naming the log file.
    Bug,
    /// A transient transport or runtime condition: a network loss, peer
    /// offline, timeout, relay unavailable, busy port or audio device that
    /// would not open. Expected and retryable, never a bug report.
    Operational,
    /// The request or input named something that does not exist, used an
    /// unsupported operation, or was otherwise refused because of what the
    /// user asked for.
    User,
    /// A signature, replay, session, membership or permission rejection. The
    /// protocol refused the frame rather than the program failing.
    Security,
    /// A guild policy or configuration floor refused or restricted the
    /// operation.
    Policy,
}

impl ErrorClass {
    /// The lowercase label used in the log, the CLI and the frontend alerts.
    pub fn label(self) -> &'static str {
        match self {
            ErrorClass::Bug => "bug",
            ErrorClass::Operational => "operational",
            ErrorClass::User => "user",
            ErrorClass::Security => "security",
            ErrorClass::Policy => "policy",
        }
    }

    /// Whether this class asks the user to report a defect.
    pub fn is_bug(self) -> bool {
        matches!(self, ErrorClass::Bug)
    }

    /// Classify a core error.
    ///
    /// Not found and invalid input are user conditions; a signature failure is
    /// a security rejection; storage, I/O, decoding and impossible-id failures
    /// are bugs because the domain should not produce them from valid input.
    pub fn from_core_error(error: &CoreError) -> Self {
        match error {
            CoreError::NotFound(_) | CoreError::InvalidInput(_) | CoreError::InvalidId(_) => {
                ErrorClass::User
            }
            CoreError::EventTooLarge(_) => ErrorClass::User,
            CoreError::InvalidSignature | CoreError::Signature(_) => ErrorClass::Security,
            CoreError::Io(_)
            | CoreError::Backend(_)
            | CoreError::Postcard(_)
            | CoreError::InvalidEventId
            | CoreError::CorruptLog { .. } => ErrorClass::Bug,
        }
    }
}

impl std::fmt::Display for ErrorClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

/// A classified error message that can cross the daemon boundary.
///
/// `message` is the text the daemon already surfaces. The other fields add the
/// class, the guild and a short context label, plus where to find the log. The
/// payload deliberately carries no keys, media bytes, attachment contents or
/// configuration secrets: the daemon log writer only serialises these fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub class: ErrorClass,
    pub message: String,
    /// The guild the condition belongs to, when it is guild-scoped.
    pub guild_id: Option<GuildId>,
    /// A short stable label for the site that reported it, for example
    /// `"webrtc media"` or `"resume network"`.
    pub context: Option<String>,
    /// Where the daemon wrote the full diagnostic line, when known.
    pub log_path: Option<String>,
    /// A stable action hint, for example "report this bug with the log file".
    pub report_hint: Option<String>,
}

impl Default for Diagnostic {
    fn default() -> Self {
        Self {
            class: ErrorClass::Operational,
            message: String::new(),
            guild_id: None,
            context: None,
            log_path: None,
            report_hint: None,
        }
    }
}

impl Diagnostic {
    pub fn new(class: ErrorClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            ..Self::default()
        }
    }

    pub fn bug(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Bug, message)
    }

    pub fn operational(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Operational, message)
    }

    pub fn user(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::User, message)
    }

    pub fn security(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Security, message)
    }

    pub fn policy(message: impl Into<String>) -> Self {
        Self::new(ErrorClass::Policy, message)
    }

    pub fn with_guild(mut self, guild_id: Option<GuildId>) -> Self {
        self.guild_id = guild_id;
        self
    }

    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    pub fn with_log_path(mut self, log_path: impl Into<String>) -> Self {
        self.log_path = Some(log_path.into());
        self
    }

    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.report_hint = Some(hint.into());
        self
    }

    /// A one-line rendering for a frontend alert.
    ///
    /// A bug names the log file because that is the only actionable part of an
    /// internal failure; the other classes stay short.
    pub fn alert(&self) -> String {
        match (&self.class, &self.log_path) {
            (ErrorClass::Bug, Some(path)) => {
                format!("{} (log: {path})", self.message)
            }
            _ => self.message.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_classes_have_stable_labels_and_severity_order() {
        let table = [
            (ErrorClass::Bug, "bug"),
            (ErrorClass::Operational, "operational"),
            (ErrorClass::User, "user"),
            (ErrorClass::Security, "security"),
            (ErrorClass::Policy, "policy"),
        ];
        for (class, label) in table {
            assert_eq!(class.label(), label);
            assert_eq!(class.to_string(), label);
        }
        assert!(ErrorClass::Bug < ErrorClass::Operational);
        assert!(ErrorClass::Operational < ErrorClass::User);
        assert!(ErrorClass::Bug.is_bug());
        assert!(!ErrorClass::Operational.is_bug());
    }

    #[test]
    fn core_errors_map_to_the_taxonomy() {
        let table = [
            (CoreError::NotFound("guild".to_string()), ErrorClass::User),
            (CoreError::InvalidInput("bad".to_string()), ErrorClass::User),
            (CoreError::InvalidId("id".to_string()), ErrorClass::User),
            (CoreError::EventTooLarge(9), ErrorClass::User),
            (CoreError::InvalidSignature, ErrorClass::Security),
            (CoreError::Backend("db".to_string()), ErrorClass::Bug),
            (CoreError::InvalidEventId, ErrorClass::Bug),
            (
                CoreError::CorruptLog {
                    path: std::path::PathBuf::from("/tmp/log"),
                    reason: "torn".to_string(),
                },
                ErrorClass::Bug,
            ),
        ];
        for (error, expected) in table {
            assert_eq!(ErrorClass::from_core_error(&error), expected, "{error}");
        }
        let io: CoreError = std::io::Error::other("disk").into();
        assert_eq!(ErrorClass::from_core_error(&io), ErrorClass::Bug);
    }

    #[test]
    fn a_bug_alert_names_the_log_and_a_notice_does_not() {
        let bug = Diagnostic::bug("an impossible state")
            .with_guild(Some(GuildId([3u8; 32])))
            .with_context("test")
            .with_log_path("/tmp/ovvc/daemon.log");
        assert_eq!(
            bug.alert(),
            "an impossible state (log: /tmp/ovvc/daemon.log)"
        );
        assert_eq!(bug.class, ErrorClass::Bug);
        assert_eq!(bug.guild_id, Some(GuildId([3u8; 32])));
        assert_eq!(bug.context.as_deref(), Some("test"));

        let notice = Diagnostic::operational("peer offline");
        assert_eq!(notice.alert(), "peer offline");
        assert!(notice.log_path.is_none());
        assert!(!notice.class.is_bug());
    }

    #[test]
    fn the_default_class_is_operational_not_bug() {
        assert_eq!(Diagnostic::default().class, ErrorClass::Operational);
    }
}
