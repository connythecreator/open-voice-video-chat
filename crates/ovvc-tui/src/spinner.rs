//! A bracketed eight-step terminal spinner.
//!
//! The GUI plays `loading.gif`, a 3x3 glyph loop. A terminal cannot draw that
//! raster, so this maps the same cadence onto the eight braille cells the
//! phosphor layout uses. The glyph is painted with `palette.accent` and the
//! brackets with `palette.faint`, so a theme file restyles the spinner with the
//! rest of the interface instead of hard-coding the GIF's green.

use ratatui::style::Style;
use ratatui::text::Span;

use crate::theme::Palette;

/// The eight positions, in cadence order.
///
/// Braille cells are single-width in a monospace terminal, which keeps the
/// bracketed marker from shifting the layout as it advances, unlike a spinner
/// made of different-width ASCII punctuation.
pub const FRAMES: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// The current position in the eight-step loop.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Spinner {
    frame: usize,
}

impl Spinner {
    /// The glyph for the current position.
    pub fn glyph(&self) -> &'static str {
        FRAMES[self.frame % FRAMES.len()]
    }

    /// Advance one step, wrapping after the eighth.
    pub fn tick(&mut self) {
        self.frame = (self.frame + 1) % FRAMES.len();
    }
}

/// The bracketed marker as spans: faint `[`, accent glyph, faint `]`.
pub fn bracket(palette: &Palette, spinner: &Spinner) -> Vec<Span<'static>> {
    vec![
        Span::styled("[", Style::new().fg(palette.faint)),
        Span::styled(spinner.glyph(), Style::new().fg(palette.accent)),
        Span::styled("]", Style::new().fg(palette.faint)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn the_spinner_has_eight_distinct_single_cell_frames() {
        assert_eq!(FRAMES.len(), 8);
        for frame in FRAMES {
            assert_eq!(frame.chars().count(), 1, "{frame:?} is not one cell");
        }
        let distinct: std::collections::HashSet<&str> = FRAMES.into_iter().collect();
        assert_eq!(distinct.len(), 8, "the cadence repeats a frame");
    }

    #[test]
    fn ticking_advances_in_cadence_order_and_wraps() {
        let mut spinner = Spinner::default();
        assert_eq!(spinner.glyph(), FRAMES[0]);
        for step in 1..=8 {
            spinner.tick();
            assert_eq!(spinner.glyph(), FRAMES[step % FRAMES.len()]);
        }
        assert_eq!(spinner, Spinner::default(), "a full turn comes back");
    }

    #[test]
    fn the_glyph_is_accent_and_the_brackets_are_faint() {
        let palette = Palette::default();
        let spans = bracket(&palette, &Spinner::default());
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[0].content.as_ref(), "[");
        assert_eq!(spans[1].content.as_ref(), FRAMES[0]);
        assert_eq!(spans[2].content.as_ref(), "]");
        assert_eq!(spans[0].style.fg, Some(palette.faint));
        assert_eq!(spans[1].style.fg, Some(palette.accent));
        assert_eq!(spans[2].style.fg, Some(palette.faint));
        assert_ne!(
            spans[1].style.fg,
            Some(Color::Rgb(0, 255, 0)),
            "the glyph must follow the palette, not the GIF green"
        );
    }
}
