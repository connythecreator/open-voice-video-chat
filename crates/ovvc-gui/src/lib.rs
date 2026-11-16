//! egui desktop client for ovvc.
//!
//! The crate is split so the interesting parts can be tested and fuzzed
//! without a display:
//!
//! * [`markdown`] parses the chat-flavoured Markdown subset.
//! * [`state`] holds all view state and reduces daemon replies and live events.
//! * the `ui` feature adds the egui rendering, the theme and the async bridge.
//! * the `desktop` feature (on by default) adds the eframe shell.

pub mod markdown;
pub mod state;

#[cfg(feature = "desktop")]
pub mod app;
/// Deterministic interaction fuzzer for the window, compiled only for tests.
#[cfg(all(test, feature = "desktop"))]
mod fuzz;
#[cfg(feature = "ui")]
pub mod bridge;
#[cfg(feature = "ui")]
pub mod loading;
#[cfg(feature = "ui")]
pub mod theme;
#[cfg(feature = "ui")]
pub mod ui;

pub use state::{Action, AppState, Dialog, Message, RequestTag};
