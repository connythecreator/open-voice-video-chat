//! The theme-mapped loading grid.
//!
//! A 3x3 grid of rounded cells that cycles through the eight masks used by
//! `assets/loading.gif`, one every 140 ms. Only the motion and geometry come
//! from the GIF: every colour is read from the installed [`Palette`] on each
//! frame, so a live theme change restyles the widget instead of leaving a
//! hardcoded green behind.
//!
//! The grid is painted without a background, so it sits on any panel colour.
//! The leading active cell uses `palette.accent`, the cells trailing behind it
//! use a trail tone blended from `palette.accent_dim` toward the accent until
//! it is at least as legible as the resting cells, and inactive cells use
//! `palette.faint` with a contrast fallback on the light Paper palette. The
//! widget only asks for a repaint while it is on screen.

use std::time::Duration;

use egui::{Color32, Sense};

use crate::theme::Palette;

/// How long each mask is held, matching the GIF's frame delay.
pub const FRAME_MS: u64 = 140;

/// Row-major 3x3 masks, one bit per cell, bit 0 is the top-left cell.
///
/// These are the eight masks transcribed from `assets/loading.gif`:
/// `100000000`, `110000000`, `110100000`, `011011000`, `100100100`,
/// `000000111`, `000001011`, `001001000`.
pub const STATES: [u16; 8] = [
    0b000_000_001,
    0b000_000_011,
    0b000_001_011,
    0b000_110_110,
    0b001_001_001,
    0b111_000_000,
    0b110_100_000,
    0b000_100_100,
];

/// The leading cell of each mask, as a row-major index. The other active
/// cells trail it and are painted with `palette.accent_dim`.
const HEADS: [usize; 8] = [0, 1, 3, 5, 6, 8, 5, 2];

/// Number of cells in the grid.
pub const CELLS: usize = 9;

/// The state shown after `elapsed_ms` of animation.
pub fn state_at_ms(elapsed_ms: u64) -> usize {
    ((elapsed_ms / FRAME_MS) % STATES.len() as u64) as usize
}

/// The mask for `state`, wrapping around if a caller passes a larger index.
pub fn mask(state: usize) -> u16 {
    STATES[state % STATES.len()]
}

/// The leading cell for `state`.
pub fn head(state: usize) -> usize {
    HEADS[state % HEADS.len()]
}

/// Whether `cell` is lit in `masked`.
pub fn is_active(masked: u16, cell: usize) -> bool {
    masked & (1 << cell) != 0
}

/// The row-major indices lit in `masked`, in ascending order.
pub fn active_cells(masked: u16) -> Vec<usize> {
    (0..CELLS).filter(|cell| is_active(masked, *cell)).collect()
}

/// Relative luminance of a colour, used only to recognise the light Paper
/// palette rather than to derive colours from it.
fn luminance(color: Color32) -> f32 {
    fn linear(channel: u8) -> f32 {
        let value = f32::from(channel) / 255.0;
        if value <= 0.040_45 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
}

fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let (lighter, darker) = {
        let (a, b) = (luminance(a), luminance(b));
        if a >= b { (a, b) } else { (b, a) }
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// Whether the palette is the light Paper style, where `faint` is too close to
/// the background to read as a resting cell.
fn is_light_paper(palette: &Palette) -> bool {
    luminance(palette.bg) > 0.5
}

/// The colour for a cell that is not part of the moving trail.
///
/// On the dark presets this is `faint`. Paper's `faint` only reaches about
/// 3.8:1 against its background, so the widget falls back to whichever of
/// `muted` or `border` contrasts best, which is `muted` in the shipped preset.
pub fn inactive_color(palette: &Palette) -> Color32 {
    if is_light_paper(palette) && contrast_ratio(palette.faint, palette.bg) < 4.5 {
        if contrast_ratio(palette.muted, palette.bg) >= contrast_ratio(palette.border, palette.bg) {
            palette.muted
        } else {
            palette.border
        }
    } else {
        palette.faint
    }
}

/// Blend two colours in sRGB channel space.
fn blend(from: Color32, to: Color32, amount: f32) -> Color32 {
    let channel = |a: u8, b: u8| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8
    };
    Color32::from_rgb(
        channel(from.r(), to.r()),
        channel(from.g(), to.g()),
        channel(from.b(), to.b()),
    )
}

/// Blend `from` toward `to` until it reaches `target` contrast against `bg`.
fn blend_to_contrast(from: Color32, to: Color32, bg: Color32, target: f32) -> Option<Color32> {
    for step in 1..=32 {
        let color = blend(from, to, step as f32 / 32.0);
        if contrast_ratio(color, bg) >= target {
            return Some(color);
        }
    }
    None
}

/// The colour for the cells trailing the leading active cell.
///
/// `accent_dim` is a quiet tone, but on every shipped preset it sits closer to
/// the background than the resting cell, so the moving trail would disappear
/// while the grid stayed bright. Blend it toward the accent until the trail is
/// at least as legible as the resting cells, and toward `text` on the light
/// Paper palette where even the accent does not reach the resting contrast.
pub fn trail_color(palette: &Palette) -> Color32 {
    let target = contrast_ratio(inactive_color(palette), palette.bg);
    if contrast_ratio(palette.accent_dim, palette.bg) >= target {
        return palette.accent_dim;
    }
    blend_to_contrast(palette.accent_dim, palette.accent, palette.bg, target)
        .or_else(|| blend_to_contrast(palette.accent_dim, palette.text, palette.bg, target))
        .unwrap_or(palette.text)
}

/// The nine cell colours for `state`, in row-major order.
///
/// The leading active cell is `accent`, any other active cells are
/// [`trail_color`], and resting cells use [`inactive_color`]. The palette is
/// taken by value at the call site so a repaint after a theme change picks up
/// the new colours without any cached state.
pub fn cell_colors(state: usize, palette: &Palette) -> [Color32; CELLS] {
    let masked = mask(state);
    let leading = head(state);
    let resting = inactive_color(palette);
    let trail = trail_color(palette);
    std::array::from_fn(|cell| {
        if is_active(masked, cell) {
            if cell == leading {
                palette.accent
            } else {
                trail
            }
        } else {
            resting
        }
    })
}

/// Draw the loading grid at the default size.
///
/// The returned response covers the whole square so callers can align or hover
/// it. The widget asks the context for a repaint at the next mask boundary
/// only when the square is inside the clip rect.
pub fn spinner(ui: &mut egui::Ui) -> egui::Response {
    spinner_sized(ui, 4.0)
}

/// Draw the loading grid with `cell` points per cell.
pub fn spinner_sized(ui: &mut egui::Ui, cell: f32) -> egui::Response {
    let cell = cell.max(2.0);
    let gap = (cell * 0.25).max(1.0);
    let side = cell * 3.0 + gap * 2.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(side, side), Sense::hover());

    // Read the palette on every frame, not once when the widget is created:
    // a live preset change must restyle the grid without a restart.
    let palette = crate::theme::palette(ui.ctx());
    let elapsed_ms = (ui.input(|input| input.time) * 1000.0) as u64;
    let colors = cell_colors(state_at_ms(elapsed_ms), &palette);

    let painter = ui.painter();
    let radius = egui::CornerRadius::same((cell * 0.3).round().clamp(1.0, 4.0) as u8);
    for (index, color) in colors.into_iter().enumerate() {
        let column = (index % 3) as f32;
        let row = (index / 3) as f32;
        let min = rect.min + egui::vec2(column * (cell + gap), row * (cell + gap));
        let cell_rect = egui::Rect::from_min_size(min, egui::vec2(cell, cell));
        painter.rect_filled(cell_rect, radius, color);
    }

    // A hidden widget costs nothing: only request the next frame while the
    // square can actually be seen. A visible one wakes at the next state.
    if ui.is_rect_visible(rect) {
        let remaining = FRAME_MS - (elapsed_ms % FRAME_MS);
        ui.ctx()
            .request_repaint_after(Duration::from_millis(remaining.max(1)));
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Preset;

    fn preset_palette(preset: Preset) -> Palette {
        Palette::from(&preset.theme())
    }

    fn all_presets() -> [Preset; 4] {
        [Preset::Phosphor, Preset::Amber, Preset::Ice, Preset::Paper]
    }

    #[test]
    fn the_gif_masks_are_transcribed_row_major() {
        let expected: [&[usize]; 8] = [
            &[0],
            &[0, 1],
            &[0, 1, 3],
            &[1, 2, 4, 5],
            &[0, 3, 6],
            &[6, 7, 8],
            &[5, 7, 8],
            &[2, 5],
        ];
        for (state, cells) in expected.into_iter().enumerate() {
            assert_eq!(active_cells(mask(state)), cells, "state {state}");
            assert!(is_active(mask(state), head(state)), "head of state {state}");
        }
    }

    #[test]
    fn the_state_cycles_every_140_ms() {
        assert_eq!(state_at_ms(0), 0);
        assert_eq!(state_at_ms(FRAME_MS - 1), 0);
        assert_eq!(state_at_ms(FRAME_MS), 1);
        assert_eq!(state_at_ms(FRAME_MS * 7), 7);
        assert_eq!(state_at_ms(FRAME_MS * 8), 0);
        assert_eq!(state_at_ms(FRAME_MS * 8 + FRAME_MS - 1), 0);
        assert_eq!(state_at_ms(1_000_000), ((1_000_000 / FRAME_MS) % 8) as usize);
    }

    #[test]
    fn a_single_active_cell_is_the_accent() {
        for preset in all_presets() {
            let palette = preset_palette(preset);
            let colors = cell_colors(0, &palette);
            assert_eq!(colors[0], palette.accent, "{preset:?}");
            for (cell, color) in colors.iter().enumerate().skip(1) {
                assert_eq!(*color, inactive_color(&palette), "{preset:?} cell {cell}");
            }
        }
    }

    #[test]
    fn trailing_cells_use_a_trail_at_least_as_legible_as_the_resting_cells() {
        // State 4 lights the left column; the head is the bottom cell.
        for preset in all_presets() {
            let palette = preset_palette(preset);
            let trail = trail_color(&palette);
            let colors = cell_colors(4, &palette);
            assert_eq!(colors[6], palette.accent, "{preset:?} head");
            assert_eq!(colors[0], trail, "{preset:?} trail 0");
            assert_eq!(colors[3], trail, "{preset:?} trail 3");
            let resting = inactive_color(&palette);
            for cell in [1, 2, 4, 5, 7, 8] {
                assert_eq!(colors[cell], resting, "{preset:?} cell {cell}");
            }

            // The trail must not vanish while the resting grid stays visible.
            // Paper's accent_dim is only about 1.34:1 against its background,
            // well below the muted resting cell at about 5.8:1.
            assert!(
                contrast_ratio(trail, palette.bg) >= contrast_ratio(resting, palette.bg),
                "{preset:?} trail {trail:?} contrasts less than resting {resting:?}"
            );

            // A one-cell mask never paints a trail.
            let single = cell_colors(0, &palette);
            assert!(!single.contains(&trail), "{preset:?}");
        }
    }

    #[test]
    fn paper_falls_back_from_faint_to_a_contrasting_inactive_colour() {
        let paper = preset_palette(Preset::Paper);
        let resting = inactive_color(&paper);
        assert_eq!(resting, paper.muted);
        assert_ne!(resting, paper.faint);
        assert_ne!(resting, paper.border);
        assert!(contrast_ratio(resting, paper.bg) > contrast_ratio(paper.faint, paper.bg));

        // The dark presets keep faint: it is a quiet but visible resting cell.
        for preset in [Preset::Phosphor, Preset::Amber, Preset::Ice] {
            let palette = preset_palette(preset);
            assert_eq!(inactive_color(&palette), palette.faint, "{preset:?}");
        }
    }

    fn painted_fills(palette: Palette, time_seconds: f64) -> Vec<Color32> {
        let context = egui::Context::default();
        crate::theme::install_with(&context, palette);
        let input = egui::RawInput {
            time: Some(time_seconds),
            ..Default::default()
        };
        let mut output = context.run_ui(input, |ui| {
            let _ = spinner(ui);
        });
        let fills = output
            .shapes
            .iter()
            .filter_map(|clipped| match &clipped.shape {
                egui::Shape::Rect(rect) => Some(rect.fill),
                _ => None,
            })
            .collect();
        // This test has no painter backend to consume the font atlas.
        output.textures_delta.clear();
        fills
    }

    #[test]
    fn the_widget_reads_the_installed_palette_on_each_frame() {
        let phosphor = preset_palette(Preset::Phosphor);
        let amber = preset_palette(Preset::Amber);

        let first = painted_fills(phosphor.clone(), 0.0);
        assert!(first.contains(&phosphor.accent), "phosphor accent missing");
        assert!(first.contains(&inactive_color(&phosphor)));

        // Re-installing a palette and running another frame must change the
        // painted cells, with no cached colour from the first frame.
        let second = painted_fills(amber.clone(), 0.0);
        assert!(second.contains(&amber.accent), "amber accent missing");
        assert!(!second.contains(&phosphor.accent));

        // State 4 (560 ms) has a trail, painted from the same palette.
        let trail = painted_fills(amber.clone(), 4.0 * FRAME_MS as f64 / 1000.0);
        assert!(trail.contains(&amber.accent));
        assert!(trail.contains(&trail_color(&amber)));
        assert!(
            !trail.contains(&amber.accent_dim),
            "the trail must be lifted off accent_dim to stay visible"
        );
    }
}
