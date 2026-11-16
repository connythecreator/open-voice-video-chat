//! Local daemon/client protocol.
//!
//! One daemon per user/data directory owns the redb store, the identity and
//! every libp2p node. Frontends (CLI, TUI, GUI, web shell) attach over a Unix
//! domain socket. The daemon checks the peer's OS uid, so another user on the
//! same host cannot attach, and it is the only process that opens the store.
//!
//! Frames are length-prefixed postcard with an explicit protocol version. The
//! socket is a local trust boundary, but every frame is still parsed with a
//! maximum size and the handshake refuses a version it does not understand.

#[cfg(unix)]
pub mod client;
pub mod codec;
pub mod paths;
#[cfg(unix)]
pub mod server;
pub mod types;

#[cfg(unix)]
pub use client::DaemonClient;
pub use codec::{
    FrameError, encode_frame, encode_frame_limited, read_frame, read_frame_limited, write_frame,
    write_frame_limited,
};
pub use paths::{
    DAEMON_INFO_FILE, DAEMON_LOG_FILE, DAEMON_LOG_PREVIOUS, DaemonInfo, daemon_log_path,
    default_data_dir, default_socket_path, read_daemon_info, remove_daemon_info, write_daemon_info,
};
pub use types::*;
