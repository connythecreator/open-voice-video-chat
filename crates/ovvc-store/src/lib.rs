//! The default on-disk [`Storage`] backend for ovvc.
//!
//! This crate is deliberately one implementation among many: it owns nothing
//! but persistence. Frontends, transports, and the client facade talk to
//! [`ovvc_core::Storage`], so an application can swap this crate out for an
//! in-memory, SQLite, or browser-storage backend without changing anything
//! above it.
//!
//! ```
//! use ovvc_core::Storage;
//! use ovvc_store::Store;
//!
//! let dir = tempfile::tempdir().unwrap();
//! let mut store = Store::open(dir.path()).unwrap();
//! assert!(store.list_guilds().unwrap().is_empty());
//! ```

mod error;
mod fsutil;
mod log;
mod store;

pub use error::{Result, StoreError};
pub use store::Store;
