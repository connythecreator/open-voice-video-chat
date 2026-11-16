//! The terminal half of the shared phosphor theme.
//!
//! Colours and the author list come from `ovvc-theme`, the same crate the GUI
//! reads, so `theme.toml` and `OVVC_THEME` restyle both frontends. A terminal
//! has one cell size, so the font scale is parsed but not used here.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use ovvc_theme::{Preset, Rgb, Theme};
use ratatui::style::Color;

#[cfg(test)]
pub use ovvc_theme::THEME_FILE;

/// A palette already mapped to terminal colours.
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg: Color,
    pub panel: Color,
    pub chat: Color,
    pub input: Color,
    pub hover: Color,
    pub border: Color,
    pub accent: Color,
    pub accent_dim: Color,
    pub text: Color,
    pub bright: Color,
    pub muted: Color,
    pub faint: Color,
    pub green: Color,
    pub yellow: Color,
    pub red: Color,
    pub cyan: Color,
    pub magenta: Color,
    pub blue: Color,
    pub code_bg: Color,
    pub select_bg: Color,
    pub select_fg: Color,
    pub authors: Vec<Color>,
}

impl From<&Theme> for Palette {
    fn from(theme: &Theme) -> Self {
        let c = &theme.colors;
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
        }
    }
}

fn rgb(color: Rgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

impl Default for Palette {
    fn default() -> Self {
        Self::from(&Theme::default())
    }
}

impl Palette {
    /// The same stable author colour the GUI paints for this identity.
    pub fn author_color(&self, seed: &[u8]) -> Color {
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

    pub fn presence_color(&self, status: ovvc_control::PresenceStatus) -> Color {
        match status {
            ovvc_control::PresenceStatus::Online => self.green,
            ovvc_control::PresenceStatus::Away => self.yellow,
            ovvc_control::PresenceStatus::DoNotDisturb => self.red,
            ovvc_control::PresenceStatus::Offline => self.faint,
        }
    }
}

static ACTIVE: OnceLock<Palette> = OnceLock::new();

/// Install the palette loaded at startup. Later calls are ignored, so a test
/// that draws before `main` cannot fight the running terminal.
pub fn install(palette: Palette) {
    let _ = ACTIVE.set(palette);
}

/// The palette the drawing code uses. Before [`install`] this is the built-in
/// theme, which keeps the widget tests independent of the user's config.
pub fn palette() -> &'static Palette {
    ACTIVE.get_or_init(Palette::default)
}

/// Where the theme file lives unless `OVVC_THEME` overrides it.
pub fn theme_path() -> Option<PathBuf> {
    ovvc_theme::theme_path()
}

/// Load the configured theme, falling back to the built-in palette on any
/// problem. The terminal must still start when the file is missing or broken.
pub fn load() -> Palette {
    let Some(path) = theme_path() else {
        return Palette::default();
    };
    match load_from(&path) {
        Ok(Some(palette)) => palette,
        Ok(None) => Palette::default(),
        Err(error) => {
            eprintln!("ovvc-tui: {error:#} (using the built-in theme)");
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
            eprintln!("ovvc-tui: unknown client.theme {name:?} (using theme.toml or the built-in theme)");
            None
        }
    }
}

/// Load the client theme named by the layered configuration.
///
/// `client.theme_file` is layered on top of `client.theme`: a file that sets
/// only the accent colour keeps the preset's other colours. When the file is
/// missing or broken the configured preset is used, rather than the legacy
/// `theme.toml`, so a config typo does not silently pick a different palette.
/// With neither key set the existing `theme.toml`/`OVVC_THEME` path applies.
pub fn load_configured(client: Option<&ovvc_config::ClientConfig>) -> Palette {
    let preset = configured_preset(client);
    if let Some(path) = client.and_then(|client| client.theme_file.as_deref()) {
        let base = preset.map(|preset| preset.theme()).unwrap_or_default();
        match ovvc_theme::load_from_over(path, base) {
            Ok(Some(theme)) => return Palette::from(&theme),
            Ok(None) => {
                eprintln!(
                    "ovvc-tui: client.theme_file {} does not exist (using the configured fallback)",
                    path.display()
                );
            }
            Err(error) => {
                eprintln!(
                    "ovvc-tui: client.theme_file: {error:#} (using the configured fallback)"
                );
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
    fn the_default_palette_is_the_shared_phosphor_theme() {
        let palette = Palette::default();
        let theme = Theme::default();
        assert_eq!(palette.bg, rgb(theme.colors.background));
        assert_eq!(palette.accent, rgb(theme.colors.accent));
        assert_eq!(palette.select_fg, rgb(theme.colors.selection_foreground));
        assert_eq!(palette.authors.len(), theme.authors.len());
    }

    #[test]
    fn a_theme_file_restyles_the_terminal_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(THEME_FILE);
        std::fs::write(&path, "[colors]\naccent = \"#ffb000\"\n").unwrap();
        let palette = load_from(&path).unwrap().unwrap();
        assert_eq!(palette.accent, Color::Rgb(0xff, 0xb0, 0x00));
        assert_eq!(palette.text, Palette::default().text);
    }

    #[test]
    fn a_theme_file_layers_over_the_configured_preset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom.toml");
        std::fs::write(&path, "[colors]\naccent = \"#ffb000\"\n").unwrap();
        let client = ovvc_config::ClientConfig {
            theme: Some("ice".to_string()),
            theme_file: Some(path),
            keybindings: Default::default(),
        };

        let palette = load_configured(Some(&client));
        let ice = Palette::from(&Preset::Ice.theme());
        assert_eq!(palette.accent, Color::Rgb(0xff, 0xb0, 0x00));
        assert_eq!(palette.bg, ice.bg, "unset colours stay from the preset");
        assert_eq!(palette.text, ice.text);
    }

    #[test]
    fn a_missing_theme_file_falls_back_to_the_configured_preset() {
        let client = ovvc_config::ClientConfig {
            theme: Some("amber".to_string()),
            theme_file: Some(PathBuf::from("/definitely/missing/theme.toml")),
            keybindings: Default::default(),
        };

        assert_eq!(
            load_configured(Some(&client)),
            Palette::from(&Preset::Amber.theme())
        );
    }

    #[test]
    fn presence_glyphs_are_the_gui_glyphs() {
        use ovvc_control::PresenceStatus;
        assert_eq!(presence_glyph(PresenceStatus::Online), "*");
        assert_eq!(presence_glyph(PresenceStatus::Away), "~");
        assert_eq!(presence_glyph(PresenceStatus::DoNotDisturb), "!");
        assert_eq!(presence_glyph(PresenceStatus::Offline), "-");
    }
}
