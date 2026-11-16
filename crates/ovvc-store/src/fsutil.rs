//! Filesystem helpers for keeping a store private.
//!
//! The store contains plaintext signed events and the guild index. On a
//! multi-user host the default umask would leave those world-readable, so the
//! store marks its directory and files private as it creates them.

use std::fs;
use std::path::Path;

/// Set `path` to `mode`, ignoring `mode` on platforms without Unix modes.
pub fn restrict(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

/// Create `path` (and parents) and make it owner-only.
pub fn create_dir_private(path: &Path) -> std::io::Result<()> {
    fs::create_dir_all(path)?;
    restrict(path, 0o700)
}
