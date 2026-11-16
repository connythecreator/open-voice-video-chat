//! Re-export of the chat Markdown parser in [`ovvc_core::markdown`].
//!
//! The parser is shared with the terminal frontend, which is the only way the
//! two can agree about bold, code blocks, links and mentions on the same
//! message. The GUI renderer is the part that is egui-specific, and it lives
//! in `ui::render_body`.

pub use ovvc_core::markdown::*;
