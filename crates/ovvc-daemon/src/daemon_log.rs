//! The daemon's diagnostic log.
//!
//! One append-only file beside the identity and store, so a bug report can
//! name it and `doctor` can find it without attaching to the daemon. Lines are
//! bounded and rotated at [`LOG_LIMIT_BYTES`], keeping one predecessor.
//!
//! The writer serialises only the fields of a [`Diagnostic`]: class, guild,
//! context, message, daemon version and pid. It never receives keys, media
//! bytes, attachment contents, TURN credentials or configuration secrets, and
//! the caller must not put them in the message. A field is capped and quoted so
//! a message cannot forge a second line.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use ovvc_core::Diagnostic;
use ovvc_ipc::daemon_log_path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Rotate the current log at this size.
pub(crate) const LOG_LIMIT_BYTES: u64 = 1024 * 1024;

/// Longest context or message field written, in characters.
///
/// The point is to keep one diagnostic to one bounded line even if a peer or a
/// parser produces a very large string.
const MAX_FIELD_CHARS: usize = 4096;

/// A concurrency-safe, size-bounded diagnostic log.
pub(crate) struct DaemonLog {
    path: PathBuf,
    version: &'static str,
    pid: u32,
    limit: u64,
    /// One writer at a time. The file is opened in append mode as well, but the
    /// rotate-then-write sequence must not interleave or two lines could land
    /// in different files.
    lock: Mutex<()>,
}

impl DaemonLog {
    pub(crate) fn new(data_dir: &Path, version: &'static str, pid: u32) -> Self {
        Self::with_limit(data_dir, version, pid, LOG_LIMIT_BYTES)
    }

    pub(crate) fn with_limit(data_dir: &Path, version: &'static str, pid: u32, limit: u64) -> Self {
        Self {
            path: daemon_log_path(data_dir),
            version,
            pid,
            limit,
            lock: Mutex::new(()),
        }
    }

    /// The log path reported by status, doctor and a bug-class diagnostic.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Append one line for `diagnostic`.
    ///
    /// A write failure is reported on stderr and otherwise ignored: logging is
    /// observability, never a reason to fail a daemon operation.
    pub(crate) fn write(&self, diagnostic: &Diagnostic) {
        let line = self.format(diagnostic);
        let _guard = self
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(error) = self.rotate_if_needed(line.len() as u64) {
            eprintln!(
                "ovvc-daemon: could not rotate {}: {error}",
                self.path.display()
            );
        }
        match open_append(&self.path) {
            Ok(mut file) => {
                if let Err(error) = file.write_all(line.as_bytes()) {
                    eprintln!(
                        "ovvc-daemon: could not write {}: {error}",
                        self.path.display()
                    );
                }
            }
            Err(error) => eprintln!(
                "ovvc-daemon: could not open {}: {error}",
                self.path.display()
            ),
        }
    }

    fn format(&self, diagnostic: &Diagnostic) -> String {
        let timestamp = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
        let guild = diagnostic
            .guild_id
            .map(|guild_id| guild_id.to_hex())
            .unwrap_or_else(|| "-".to_string());
        let context = diagnostic.context.as_deref().unwrap_or("-");
        format!(
            "{timestamp} class={} pid={} version={} guild={guild} context={} message={}\n",
            diagnostic.class.label(),
            self.pid,
            self.version,
            quote(context),
            quote(&diagnostic.message),
        )
    }

    /// Rename the current file to the single predecessor when the next line
    /// would push it past the limit.
    fn rotate_if_needed(&self, incoming: u64) -> std::io::Result<()> {
        let Ok(metadata) = fs::metadata(&self.path) else {
            return Ok(());
        };
        if metadata.len().saturating_add(incoming) <= self.limit {
            return Ok(());
        }
        let previous = previous_log_path(&self.path);
        // A failed removal must not stop the rotation; the rename replaces it.
        let _ = fs::remove_file(&previous);
        match fs::rename(&self.path, &previous) {
            Ok(()) => Ok(()),
            Err(error) => {
                // If the rename is refused, truncate instead of growing without
                // bound. The one line that triggered rotation is kept.
                let file = open_truncate(&self.path)?;
                drop(file);
                Err(error)
            }
        }
    }
}

fn previous_log_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_default();
    name.push(".1");
    path.with_file_name(name)
}

#[cfg(unix)]
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    // A file left by an older build or a hand copy must not stay world
    // readable once the daemon appends to it.
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    Ok(file)
}

#[cfg(not(unix))]
fn open_append(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(unix)]
fn open_truncate(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_truncate(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

/// Quote one field so a control character cannot forge a record boundary.
fn quote(value: &str) -> String {
    let value: String = value.chars().take(MAX_FIELD_CHARS).collect();
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if character.is_control() => {
                out.push_str(&format!("\\u{{{:x}}}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ovvc_core::{Diagnostic, ErrorClass, GuildId};

    use super::*;

    fn diagnostic(class: ErrorClass, message: &str) -> Diagnostic {
        Diagnostic::new(class, message)
            .with_guild(Some(GuildId([0xab; 32])))
            .with_context("resume network")
    }

    #[test]
    fn a_line_names_the_class_guild_context_version_and_pid() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = DaemonLog::new(dir.path(), "9.9.9", 4242);
        log.write(&diagnostic(
            ErrorClass::Operational,
            "could not rejoin the peer network",
        ));

        let line = fs::read_to_string(log.path()).expect("log");
        assert!(line.ends_with('\n'), "{line}");
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(line.contains("class=operational"), "{line}");
        assert!(line.contains("pid=4242"), "{line}");
        assert!(line.contains("version=9.9.9"), "{line}");
        assert!(
            line.contains(&format!("guild={}", GuildId([0xab; 32]).to_hex())),
            "{line}"
        );
        assert!(line.contains("context=\"resume network\""), "{line}");
        assert!(
            line.contains("message=\"could not rejoin the peer network\""),
            "{line}"
        );
        // A timestamp comes first, in RFC 3339 form.
        assert!(line.starts_with("20"), "{line}");
        assert!(line.contains('T') && line.contains('Z'), "{line}");
    }

    #[test]
    fn only_the_documented_fields_are_written() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = DaemonLog::new(dir.path(), "0.1.0", 7);
        let mut report = diagnostic(ErrorClass::Bug, "an impossible state");
        report.report_hint = Some("TURN-CREDENTIAL-CANARY".to_string());
        report.log_path = Some("/not/written/from/the/payload".to_string());
        log.write(&report);

        let line = fs::read_to_string(log.path()).expect("log");
        // The hint and the payload's own log path are guidance for the UI, not
        // log fields, and a secret placed there must not be written.
        assert!(!line.contains("TURN-CREDENTIAL-CANARY"), "{line}");
        assert!(!line.contains("/not/written/from/the/payload"), "{line}");
        for field in [
            "class=", "pid=", "version=", "guild=", "context=", "message=",
        ] {
            assert_eq!(line.matches(field).count(), 1, "{field} in {line}");
        }
    }

    #[test]
    fn a_message_cannot_forge_a_second_record() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = DaemonLog::new(dir.path(), "0.1.0", 7);
        log.write(&diagnostic(
            ErrorClass::Security,
            "bad frame\n2026-01-01T00:00:00Z class=bug pid=1 message=\"forged\"",
        ));

        let contents = fs::read_to_string(log.path()).expect("log");
        assert_eq!(contents.lines().count(), 1, "{contents}");
        // The forged record is still visible text, but only inside the quoted
        // message field, after an escaped newline.
        assert!(
            contents.contains("message=\"bad frame\\n2026-01-01T00:00:00Z"),
            "{contents}"
        );
    }

    #[test]
    fn the_log_rotates_at_the_bound_and_keeps_one_predecessor() {
        let dir = tempfile::tempdir().expect("temp dir");
        // A small limit keeps the test to a few writes.
        let log = DaemonLog::with_limit(dir.path(), "0.1.0", 7, 256);
        for index in 0..20 {
            log.write(&diagnostic(
                ErrorClass::Operational,
                &format!("retry number {index}"),
            ));
        }

        let current = fs::read_to_string(log.path()).expect("current log");
        let previous = fs::read_to_string(previous_log_path(log.path())).expect("previous log");
        assert!(!previous.is_empty(), "rotation must keep a predecessor");
        assert!(!current.is_empty(), "rotation must keep the current log");
        // Bounded means neither file keeps the whole 20-line history.
        assert!(previous.lines().count() < 20, "{previous}");
        assert!(current.lines().count() < 20, "{current}");
        // The current file can overshoot by at most the one line that
        // triggered rotation, never by a whole additional log.
        assert!(fs::metadata(log.path()).expect("metadata").len() <= 1024);
    }

    #[test]
    fn concurrent_writers_do_not_interleave_records() {
        let dir = tempfile::tempdir().expect("temp dir");
        let log = Arc::new(DaemonLog::with_limit(dir.path(), "0.1.0", 7, 1024 * 1024));
        let mut threads = Vec::new();
        for writer in 0..8 {
            let log = Arc::clone(&log);
            threads.push(std::thread::spawn(move || {
                for index in 0..50 {
                    log.write(&diagnostic(
                        ErrorClass::Operational,
                        &format!("writer {writer} record {index}"),
                    ));
                }
            }));
        }
        for thread in threads {
            thread.join().expect("writer thread");
        }

        let contents = fs::read_to_string(log.path()).expect("log");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 400, "one complete line per write");
        for line in lines {
            assert!(line.contains("class=operational"), "{line}");
            assert!(line.contains("message=\"writer "), "{line}");
            // Every record has exactly one class field, so no two writes were
            // spliced into one line.
            assert_eq!(line.matches("class=").count(), 1, "{line}");
        }
    }
}
