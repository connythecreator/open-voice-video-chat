//! A phosphor terminal theme: monospace everywhere, reverse-video selections,
//! hairline borders and a green-on-black palette. The hierarchy comes from
//! brightness and box-drawing rules, not from cards and rounded corners.
//!
//! The palette is defined once in `ovvc-theme`, read from `theme.toml` in the
//! per-user config directory (usually `~/.config/ovvc/theme.toml`), and shared
//! with the terminal frontend. `OVVC_THEME` points at a different file. A
//! missing file means the built-in palette, and a malformed one is reported on
//! stderr without stopping the GUI.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use egui::{Color32, FontFamily, FontId, Stroke, TextStyle};
use ovvc_theme::{Fonts, Theme};

pub use ovvc_theme::{Preset, THEME_ENV, THEME_FILE};

/// Runtime palette and type scale. Every colour the UI paints comes from here,
/// so a theme file can restyle the whole client without touching the layout.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg: Color32,
    pub panel: Color32,
    pub chat: Color32,
    pub input: Color32,
    pub hover: Color32,
    pub border: Color32,
    pub accent: Color32,
    pub accent_dim: Color32,
    pub text: Color32,
    pub bright: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub green: Color32,
    pub yellow: Color32,
    pub red: Color32,
    pub cyan: Color32,
    pub magenta: Color32,
    pub blue: Color32,
    pub code_bg: Color32,
    pub select_bg: Color32,
    pub select_fg: Color32,
    /// ANSI-ish author colours. A terminal distinguishes speakers by hue, so
    /// keep a small readable set rather than a full rainbow.
    pub authors: Vec<Color32>,
    pub heading: f32,
    pub body: f32,
    pub monospace: f32,
    pub button: f32,
    pub small: f32,
    /// Edge length of a message avatar tile, in points.
    pub avatar: f32,
}

impl From<&Theme> for Palette {
    fn from(theme: &Theme) -> Self {
        let c = &theme.colors;
        let Fonts {
            heading,
            body,
            monospace,
            button,
            small,
            avatar,
        } = theme.fonts;
        Self {
            bg: rgb(c.background),
            panel: rgb(c.panel),
            chat: rgb(c.chat),
            input: rgb(c.input),
            hover: rgb(c.hover),
            border: rgb(c.border),
            accent: rgb(c.accent),
            accent_dim: rgb(c.accent_dim),
            text: rgb(c.text),
            bright: rgb(c.bright),
            muted: rgb(c.muted),
            faint: rgb(c.faint),
            green: rgb(c.green),
            yellow: rgb(c.yellow),
            red: rgb(c.red),
            cyan: rgb(c.cyan),
            magenta: rgb(c.magenta),
            blue: rgb(c.blue),
            code_bg: rgb(c.code),
            select_bg: rgb(c.selection_background),
            select_fg: rgb(c.selection_foreground),
            authors: theme.authors.iter().copied().map(rgb).collect(),
            heading,
            body,
            monospace,
            button,
            small,
            avatar,
        }
    }
}

fn rgb(color: ovvc_theme::Rgb) -> Color32 {
    Color32::from_rgb(color.r, color.g, color.b)
}

impl Default for Palette {
    fn default() -> Self {
        Self::from(&Theme::default())
    }
}

impl Palette {
    pub fn author_color(&self, seed: &[u8]) -> Color32 {
        let mut hash = 0x811c_9dc5u32;
        for byte in seed {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        if self.authors.is_empty() {
            return self.text;
        }
        self.authors[(hash as usize) % self.authors.len()]
    }

    pub fn presence_color(&self, status: ovvc_control::PresenceStatus) -> Color32 {
        match status {
            ovvc_control::PresenceStatus::Online => self.green,
            ovvc_control::PresenceStatus::Away => self.yellow,
            ovvc_control::PresenceStatus::DoNotDisturb => self.red,
            ovvc_control::PresenceStatus::Offline => self.faint,
        }
    }

    fn id() -> egui::Id {
        egui::Id::unique("ovvc.palette")
    }

    /// Build an egui style from this palette. Widgets stay square and mostly
    /// flat, so buttons read as bracketed text until hovered.
    fn style(&self, base: &egui::Style) -> egui::Style {
        let mut style = base.clone();
        let mut visuals = egui::Visuals::dark();
        visuals.panel_fill = self.bg;
        visuals.window_fill = self.panel;
        visuals.faint_bg_color = self.panel;
        visuals.extreme_bg_color = self.input;
        visuals.code_bg_color = self.code_bg;
        visuals.override_text_color = Some(self.text);
        // Draw text selection over the selection background: in the dark
        // presets that is the accent (reverse video), while paper uses a light
        // tint so its dark text stays readable when selected.
        visuals.selection.bg_fill = self.select_bg;
        visuals.selection.stroke = Stroke::new(1.0, self.bg);
        visuals.hyperlink_color = self.cyan;
        visuals.window_stroke = Stroke::new(1.0, self.border);
        visuals.window_corner_radius = egui::CornerRadius::ZERO;
        visuals.menu_corner_radius = egui::CornerRadius::ZERO;
        visuals.window_shadow = egui::Shadow::NONE;
        visuals.popup_shadow = egui::Shadow::NONE;
        visuals.striped = false;

        let widgets = &mut visuals.widgets;
        widgets.noninteractive.bg_fill = self.panel;
        widgets.noninteractive.weak_bg_fill = self.panel;
        widgets.noninteractive.bg_stroke = Stroke::NONE;
        widgets.noninteractive.fg_stroke = Stroke::new(1.0, self.text);
        widgets.inactive.bg_fill = self.input;
        widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        widgets.inactive.bg_stroke = Stroke::NONE;
        widgets.inactive.fg_stroke = Stroke::new(1.0, self.text);
        widgets.hovered.bg_fill = self.hover;
        widgets.hovered.weak_bg_fill = self.hover;
        widgets.hovered.bg_stroke = Stroke::new(1.0, self.border);
        widgets.hovered.fg_stroke = Stroke::new(1.0, self.bright);
        widgets.active.bg_fill = self.accent_dim;
        widgets.active.weak_bg_fill = self.accent_dim;
        widgets.active.bg_stroke = Stroke::new(1.0, self.accent);
        widgets.active.fg_stroke = Stroke::new(1.0, self.bright);
        widgets.open.bg_fill = self.hover;
        widgets.open.weak_bg_fill = self.hover;
        widgets.open.fg_stroke = Stroke::new(1.0, self.bright);
        for widget in [
            &mut widgets.noninteractive,
            &mut widgets.inactive,
            &mut widgets.hovered,
            &mut widgets.active,
            &mut widgets.open,
        ] {
            widget.corner_radius = egui::CornerRadius::ZERO;
            widget.expansion = 0.0;
        }

        style.visuals = visuals;
        style.spacing.item_spacing = egui::vec2(6.0, 3.0);
        style.spacing.button_padding = egui::vec2(5.0, 2.0);
        style.spacing.interact_size = egui::vec2(20.0, 18.0);
        style.text_styles = [
            (
                TextStyle::Heading,
                FontId::new(self.heading, FontFamily::Monospace),
            ),
            (
                TextStyle::Body,
                FontId::new(self.body, FontFamily::Monospace),
            ),
            (
                TextStyle::Monospace,
                FontId::new(self.monospace, FontFamily::Monospace),
            ),
            (
                TextStyle::Button,
                FontId::new(self.button, FontFamily::Monospace),
            ),
            (
                TextStyle::Small,
                FontId::new(self.small, FontFamily::Monospace),
            ),
        ]
        .into();
        style
    }

    /// Parse a theme file. Unknown keys and bad colours are errors so a typo
    /// does not silently produce a half-themed window.
    pub fn parse(text: &str) -> Result<Self> {
        Theme::parse(text).map(|theme| Self::from(&theme))
    }
}

pub fn install(ctx: &egui::Context) {
    install_with(ctx, Palette::default());
}

/// Install `palette` for this context and store it for the drawing code.
pub fn install_with(ctx: &egui::Context, palette: Palette) {
    let style = palette.style(&ctx.style_of(egui::Theme::Dark));
    ctx.data_mut(|data| data.insert_temp(Palette::id(), palette));
    ctx.set_style_of(egui::Theme::Dark, style.clone());
    ctx.set_style_of(egui::Theme::Light, style);
    ctx.set_theme(egui::Theme::Dark);
}

pub fn palette(ctx: &egui::Context) -> Palette {
    ctx.data(|data| data.get_temp::<Palette>(Palette::id()))
        .unwrap_or_default()
}

fn preset_id() -> egui::Id {
    egui::Id::unique("ovvc.preset")
}

/// Hot-swap the running window to `preset`: build its palette, install it and
/// ask for a repaint so the change shows without a restart.
pub fn apply(ctx: &egui::Context, preset: Preset) {
    install_with(ctx, Palette::from(&Theme::preset(preset)));
    ctx.data_mut(|data| data.insert_temp(preset_id(), preset));
    ctx.request_repaint();
}

/// The preset currently installed on `ctx`.
///
/// [`apply`] records the preset directly. For a palette installed through
/// [`install_with`] instead (the startup path reads the file into a palette),
/// the colours are compared against each preset, so `preset = "amber"` in
/// `theme.toml` still shows Amber as selected. A fully custom palette that
/// matches no preset reports `Phosphor`, the built-in default.
pub fn active(ctx: &egui::Context) -> Preset {
    if let Some(preset) = ctx.data(|data| data.get_temp::<Preset>(preset_id())) {
        return preset;
    }
    let palette = palette(ctx);
    Preset::all()
        .iter()
        .copied()
        .find(|preset| matches_preset(&palette, *preset))
        .unwrap_or_default()
}

fn matches_preset(palette: &Palette, preset: Preset) -> bool {
    let expected = Palette::from(&Theme::preset(preset));
    // A theme file may override the type scale on top of a preset; the picker
    // still wants to show that preset as selected, so compare colours only.
    let mut actual = palette.clone();
    actual.heading = expected.heading;
    actual.body = expected.body;
    actual.monospace = expected.monospace;
    actual.button = expected.button;
    actual.small = expected.small;
    actual.avatar = expected.avatar;
    actual == expected
}

/// Persist the chosen preset to the configured `theme.toml`, creating the
/// config directory if needed. The error is handed back for the caller to
/// render as a notice.
pub fn save_preset(preset: Preset) -> Result<()> {
    ovvc_theme::save_preset(preset)
        .with_context(|| format!("saving the {} theme preset", preset.name()))
        .map(|_path| ())
}

/// Where the theme file lives unless `OVVC_THEME` overrides it.
pub fn default_path() -> Option<PathBuf> {
    ovvc_theme::default_path()
}

pub fn theme_path() -> Option<PathBuf> {
    ovvc_theme::theme_path()
}

/// Load the configured theme, falling back to the built-in palette on any
/// problem. The GUI must still start when the file is missing or malformed.
pub fn load() -> Palette {
    let Some(path) = theme_path() else {
        return Palette::default();
    };
    match load_from(&path) {
        Ok(Some(palette)) => palette,
        Ok(None) => Palette::default(),
        Err(error) => {
            eprintln!("ovvc-gui: {error:#} (using the built-in theme)");
            Palette::default()
        }
    }
}

/// Parse a preset name, reporting an unknown one but still allowing the
/// caller to fall back to the legacy `theme.toml` path.
fn configured_preset(client: Option<&ovvc_config::ClientConfig>) -> Option<Preset> {
    let name = client?.theme.as_deref()?;
    match Preset::from_name(name) {
        Some(preset) => Some(preset),
        None => {
            eprintln!("ovvc-gui: unknown client.theme {name:?} (using theme.toml or the built-in theme)");
            None
        }
    }
}

/// Load the client theme named by the layered configuration.
///
/// `client.theme_file` is layered on top of `client.theme`, and a missing or
/// broken file falls back to the configured preset rather than silently using
/// the legacy `theme.toml`. With neither key set the `theme.toml`/`OVVC_THEME`
/// path applies, matching the terminal frontend.
pub fn load_configured(client: Option<&ovvc_config::ClientConfig>) -> Palette {
    let preset = configured_preset(client);
    if let Some(path) = client.and_then(|client| client.theme_file.as_deref()) {
        let base = preset.map(|preset| preset.theme()).unwrap_or_default();
        match ovvc_theme::load_from_over(path, base) {
            Ok(Some(theme)) => return Palette::from(&theme),
            Ok(None) => {
                eprintln!(
                    "ovvc-gui: client.theme_file {} does not exist (using the configured fallback)",
                    path.display()
                );
            }
            Err(error) => {
                eprintln!("ovvc-gui: client.theme_file: {error:#} (using the configured fallback)");
            }
        }
        if let Some(preset) = preset {
            return Palette::from(&preset.theme());
        }
        return load();
    }
    if let Some(preset) = preset {
        return Palette::from(&preset.theme());
    }
    load()
}

/// A missing file returns `Ok(None)`; anything else that goes wrong is an
/// error the caller can report.
pub fn load_from(path: &Path) -> Result<Option<Palette>> {
    ovvc_theme::load_from(path)
        .with_context(|| format!("loading {}", path.display()))
        .map(|theme| theme.map(|theme| Palette::from(&theme)))
}

pub fn presence_glyph(status: ovvc_control::PresenceStatus) -> &'static str {
    match status {
        ovvc_control::PresenceStatus::Online => "*",
        ovvc_control::PresenceStatus::Away => "~",
        ovvc_control::PresenceStatus::DoNotDisturb => "!",
        ovvc_control::PresenceStatus::Offline => "-",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_keeps_the_built_in_palette() {
        assert_eq!(Palette::parse("").unwrap(), Palette::default());
        assert_eq!(
            Palette::parse("[colors]\n[fonts]\n[authors]\n").unwrap(),
            Palette::default()
        );
    }

    #[test]
    fn colours_and_sizes_are_overridden() {
        let palette = Palette::parse(
            r##"
            [colors]
            background = "#101010"
            accent = "#ffb000"
            selection_background = "#ffb000"
            selection_foreground = "#101010"

            [fonts]
            body = 14.0
            small = 10.5

            [authors]
            colors = ["#ffb000", "#ffffff"]
            "##,
        )
        .unwrap();
        assert_eq!(palette.bg, Color32::from_rgb(0x10, 0x10, 0x10));
        assert_eq!(palette.accent, Color32::from_rgb(0xff, 0xb0, 0x00));
        assert_eq!(palette.select_bg, palette.accent);
        assert_eq!(palette.body, 14.0);
        assert_eq!(palette.small, 10.5);
        assert_eq!(palette.authors.len(), 2);
        assert_eq!(palette.authors[0], palette.accent);
        // Untouched keys still come from the default.
        assert_eq!(palette.chat, Palette::default().chat);
    }

    #[test]
    fn a_bad_colour_names_the_key() {
        let error = Palette::parse("[colors]\naccent = \"orange\"\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("colors.accent"), "{message}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = Palette::parse("[colors]\naccentt = \"#ffffff\"\n").unwrap_err();
        assert!(format!("{error:#}").contains("accentt"));
    }

    #[test]
    fn font_sizes_are_range_checked() {
        let error = Palette::parse("[fonts]\nbody = 400.0\n").unwrap_err();
        assert!(format!("{error:#}").contains("fonts.body"));
        let error = Palette::parse("[fonts]\nbody = 0.0\n").unwrap_err();
        assert!(format!("{error:#}").contains("fonts.body"));
        let error = Palette::parse("[fonts]\navatar = 8.0\n").unwrap_err();
        assert!(format!("{error:#}").contains("fonts.avatar"));
    }

    #[test]
    fn the_avatar_size_is_configurable() {
        let palette = Palette::parse("[fonts]\navatar = 56.0\n").unwrap();
        assert_eq!(palette.avatar, 56.0);
        assert_eq!(Palette::default().avatar, 40.0);
    }

    #[test]
    fn an_empty_author_list_is_rejected() {
        let error = Palette::parse("[authors]\ncolors = []\n").unwrap_err();
        assert!(format!("{error:#}").contains("authors.colors"));
    }

    #[test]
    fn author_colours_are_stable_for_a_palette() {
        let palette = Palette::default();
        let seed = [7u8; 32];
        assert_eq!(palette.author_color(&seed), palette.author_color(&seed));
    }

    #[test]
    fn a_missing_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load_from(&dir.path().join(THEME_FILE)).unwrap(), None);
    }

    #[test]
    fn load_from_reads_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(THEME_FILE);
        std::fs::write(&path, "[colors]\naccent = \"#ff0000\"\n").unwrap();
        let palette = load_from(&path).unwrap().unwrap();
        assert_eq!(palette.accent, Color32::RED);
    }

    #[test]
    fn the_palette_is_stored_per_context() {
        let ctx = egui::Context::default();
        let other = egui::Context::default();
        let custom = Palette {
            accent: Color32::RED,
            ..Palette::default()
        };
        install_with(&ctx, custom);
        assert_eq!(palette(&ctx).accent, Color32::RED);
        assert_eq!(palette(&other), Palette::default());
    }
}
