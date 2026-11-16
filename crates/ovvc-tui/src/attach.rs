//! Parsing and metadata for `/attach`.
//!
//! The command reads a file from disk and announces it on a message. Only the
//! metadata building lives here so it can be tested without touching the
//! filesystem or the client; the caller does the read and the blob upload.

use std::path::{Path, PathBuf};

use ovvc_client::Attachment;

/// Split `/attach` arguments into a path and an optional caption.
///
/// A path may be quoted when it contains spaces: `/attach "my file.txt" look`
/// gives the path `my file.txt` and the caption `look`. Without quotes the
/// first whitespace-separated token is the path and everything after it is the
/// caption, so an unquoted path with spaces simply leaves the rest to the
/// caption, which is the least surprising behaviour for a terminal.
pub fn split_attach_args(rest: &str) -> Option<(PathBuf, String)> {
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    if let Some(inner) = rest.strip_prefix('"') {
        let (path, tail) = inner.split_once('"')?;
        if path.is_empty() {
            return None;
        }
        return Some((PathBuf::from(path), tail.trim().to_string()));
    }
    let (path, caption) = match rest.split_once(char::is_whitespace) {
        Some((path, caption)) => (path, caption.trim()),
        None => (rest, ""),
    };
    (!path.is_empty()).then(|| (PathBuf::from(path), caption.to_string()))
}

/// Build the metadata for a file whose bytes are already read.
///
/// The name and MIME checks live in `ovvc-core` so the GUI stages a file the
/// same way this command does.
pub fn attachment_for(path: &Path, bytes: &[u8]) -> Result<Attachment, String> {
    ovvc_core::attachment_for_file(path, bytes).map_err(|error| error.to_string())
}

/// Human readable size for a file chip, shared with the other frontends.
pub use ovvc_client::human_size;

#[cfg(test)]
mod tests {
    use super::*;
    use ovvc_client::{MAX_ATTACHMENT_NAME_BYTES, hash_bytes};

    #[test]
    fn attach_arguments_split_path_and_caption() {
        assert_eq!(
            split_attach_args("notes.txt"),
            Some((PathBuf::from("notes.txt"), String::new()))
        );
        assert_eq!(
            split_attach_args("notes.txt see this"),
            Some((PathBuf::from("notes.txt"), "see this".to_string()))
        );
        assert_eq!(
            split_attach_args("\"my notes.txt\" see this"),
            Some((PathBuf::from("my notes.txt"), "see this".to_string()))
        );
        assert_eq!(
            split_attach_args("\"my notes.txt\""),
            Some((PathBuf::from("my notes.txt"), String::new()))
        );
        assert_eq!(split_attach_args(""), None);
        assert_eq!(split_attach_args("   "), None);
        assert_eq!(split_attach_args("\"unterminated"), None);
        assert_eq!(split_attach_args("\"\""), None);
    }

    #[test]
    fn metadata_uses_the_file_name_and_a_guessed_mime() {
        let bytes = b"some bytes";
        let attachment = attachment_for(&PathBuf::from("/tmp/holiday.PNG"), bytes).unwrap();
        assert_eq!(attachment.name, "holiday.PNG");
        assert_eq!(attachment.mime, "image/png");
        assert_eq!(attachment.size, bytes.len() as u64);
        assert_eq!(attachment.hash, hash_bytes(bytes));

        let unknown = attachment_for(&PathBuf::from("/tmp/thing.xyz"), bytes).unwrap();
        assert_eq!(unknown.mime, "application/octet-stream");
    }

    #[test]
    fn unsafe_file_names_are_refused() {
        let bytes = b"x";
        assert!(attachment_for(&PathBuf::from("/"), bytes).is_err());
        assert!(attachment_for(&PathBuf::from("/tmp/a\u{1}b.txt"), bytes).is_err());
        let long = format!("/tmp/{}.txt", "n".repeat(MAX_ATTACHMENT_NAME_BYTES));
        assert!(attachment_for(&PathBuf::from(long), bytes).is_err());
    }
}
