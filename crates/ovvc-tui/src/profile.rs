//! Local display-name storage.
//!
//! The name is deliberately tiny and human-editable: a single line in
//! `<data_dir>/profile.txt`. It is not part of the signed event log because
//! profile events (MemberJoined) are not propagated yet; see the roadmap.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub const FILE_NAME: &str = "profile.txt";

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn load(data_dir: &Path) -> Option<String> {
    let text = fs::read_to_string(data_dir.join(FILE_NAME)).ok()?;
    let name = text.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

pub fn save(data_dir: &Path, name: &str) -> std::io::Result<()> {
    write_private(&data_dir.join(FILE_NAME), name.trim())
}

/// Replace `path` with `contents` without following a symlink and without
/// leaving the file world-readable.
///
/// Writing to a fresh temporary file and renaming it into place makes the
/// operation atomic and, on Unix, replaces a symlink at `path` rather than
/// writing through it to whatever it points at.
pub(crate) fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut name = std::ffi::OsString::from(".");
    name.push(
        path.file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("file")),
    );
    name.push(format!(
        ".tmp.{}.{}",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = parent.join(name);

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let result = (|| {
        let mut file = options.open(&tmp)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_replaces_a_symlink_instead_of_writing_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        fs::write(&target, b"do not touch").unwrap();
        let link = dir.path().join(FILE_NAME);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();

        save(dir.path(), "alice").unwrap();

        assert_eq!(fs::read_to_string(&target).unwrap(), "do not touch");
        assert_eq!(load(dir.path()).as_deref(), Some("alice"));
        assert!(
            !fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }
}
