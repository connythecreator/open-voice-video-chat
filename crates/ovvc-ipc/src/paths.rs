use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DAEMON_INFO_FILE: &str = "daemon.json";
/// The daemon's append-only diagnostic log, in the data directory.
pub const DAEMON_LOG_FILE: &str = "daemon.log";
/// The single rotated predecessor of [`DAEMON_LOG_FILE`].
pub const DAEMON_LOG_PREVIOUS: &str = "daemon.log.1";
const UNIX_SOCKET_PATH_LIMIT: usize = 100;

/// The default per-user data directory.
pub fn default_data_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "ovvc", "ovvc").map(|dirs| dirs.data_dir().to_path_buf())
}

/// Where the daemon writes its diagnostic log.
///
/// The log lives beside the identity and store so a bug report can name one
/// per-user path, and so `doctor` can find it without attaching to the daemon.
pub fn daemon_log_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DAEMON_LOG_FILE)
}

/// Where the daemon listens for clients.
///
/// The socket normally lives in `$XDG_RUNTIME_DIR/ovvc`, which is per-user and
/// cleaned up on logout. A data directory on a filesystem with a long path can
/// exceed the Unix socket limit, so both fallbacks keep the path short.
pub fn default_socket_path(data_dir: &Path) -> PathBuf {
    let hash = short_hash(data_dir);
    let uid = current_uid();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        let candidate = PathBuf::from(runtime)
            .join("ovvc")
            .join(format!("daemon-{hash}.sock"));
        if candidate.as_os_str().len() <= UNIX_SOCKET_PATH_LIMIT {
            return candidate;
        }
    }
    let fallback = PathBuf::from("/tmp").join(format!("ovvc-{uid}-{hash}.sock"));
    if fallback.as_os_str().len() <= UNIX_SOCKET_PATH_LIMIT {
        return fallback;
    }
    data_dir.join("daemon.sock")
}

fn short_hash(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().hash(&mut hasher);
    format!("{:08x}", hasher.finish() & 0xffff_ffff)
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // Safety: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

/// Discovery record written next to the store.
///
/// It is advisory: a client should still try the socket directly, and a stale
/// record is removed when the daemon starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub version: String,
    pub pid: u32,
    pub data_dir: String,
    pub socket: String,
    pub started_at: u64,
}

pub fn daemon_info_path(data_dir: &Path) -> PathBuf {
    data_dir.join(DAEMON_INFO_FILE)
}

pub fn write_daemon_info(data_dir: &Path, info: &DaemonInfo) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = daemon_info_path(data_dir);
    let bytes = serde_json::to_vec_pretty(info)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&temporary, &path)?;
    Ok(())
}

pub fn read_daemon_info(data_dir: &Path) -> std::io::Result<Option<DaemonInfo>> {
    let path = daemon_info_path(data_dir);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let info = serde_json::from_slice(&bytes)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
            Ok(Some(info))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn remove_daemon_info(data_dir: &Path) {
    let _ = std::fs::remove_file(daemon_info_path(data_dir));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_paths_are_short_and_distinct() {
        let one = default_socket_path(Path::new("/home/alice/.local/share/ovvc"));
        let two = default_socket_path(Path::new("/home/alice/other/ovvc"));
        assert!(one.as_os_str().len() <= UNIX_SOCKET_PATH_LIMIT);
        assert!(two.as_os_str().len() <= UNIX_SOCKET_PATH_LIMIT);
        assert_ne!(one, two);
    }

    #[test]
    fn daemon_info_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let info = DaemonInfo {
            version: "0.1.0".to_string(),
            pid: 42,
            data_dir: dir.path().display().to_string(),
            socket: "/tmp/test.sock".to_string(),
            started_at: 7,
        };
        write_daemon_info(dir.path(), &info).unwrap();
        assert_eq!(read_daemon_info(dir.path()).unwrap(), Some(info));
        remove_daemon_info(dir.path());
        assert_eq!(read_daemon_info(dir.path()).unwrap(), None);
    }
}
