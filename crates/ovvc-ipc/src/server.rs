//! Server-side socket helpers.
//!
//! Only Unix is implemented. The daemon owns the user's identity, so the socket
//! must not be reachable by another local user, and a plain loopback TCP port
//! with a bearer token would be a weaker default. A Windows named-pipe backend
//! can slot in behind the same helpers later.

#[cfg(unix)]
use std::path::Path;

#[cfg(unix)]
pub async fn bind_socket(path: &Path) -> std::io::Result<tokio::net::UnixListener> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        // The socket lives in a per-user directory whenever possible.
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    if path.exists() {
        // A live daemon keeps the socket connectable. A stale one does not, and
        // refusing to start because a previous crash left a file behind would
        // be worse than removing it.
        match tokio::net::UnixStream::connect(path).await {
            Ok(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AddrInUse,
                    format!("another ovvc daemon is listening on {}", path.display()),
                ));
            }
            Err(_) => {
                let _ = std::fs::remove_file(path);
            }
        }
    }
    let listener = tokio::net::UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// True when the connecting process runs as the same OS user as the daemon.
///
/// The kernel fills in the peer credentials, so the client cannot forge this.
#[cfg(unix)]
pub fn peer_uid_allowed(stream: &tokio::net::UnixStream) -> std::io::Result<bool> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let credentials = stream.peer_cred()?;
        Ok(credentials.uid() == current_uid())
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        // Other Unixes expose peer credentials through different APIs; until
        // that is wired up, the 0600 socket permissions remain the boundary.
        let _ = stream;
        Ok(true)
    }
}

#[cfg(unix)]
pub fn current_uid() -> u32 {
    // Safety: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(unix)]
pub fn remove_socket(path: &Path) {
    let _ = std::fs::remove_file(path);
}

#[cfg(not(unix))]
pub fn unsupported() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the ovvc daemon IPC transport is Unix-only",
    )
}
