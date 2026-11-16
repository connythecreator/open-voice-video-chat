//! The `theme.toml` palette shared by the ovvc frontends.
//!
//! Colours are kept as plain RGB triples so this crate has no window-toolkit
//! or terminal dependency. The GUI maps them to `egui::Color32` and the TUI to
//! `ratatui::style::Color`, which means both frontends read the same file and
//! start from the same built-in phosphor palette.
//!
//! The file lives in the per-user config directory (usually
//! `~/.config/ovvc/theme.toml`). `OVVC_THEME` points at a different file. A
//! missing file means the built-in palette, and a malformed one is reported by
//! the frontend without stopping it.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

pub const THEME_FILE: &str = "theme.toml";
pub const THEME_ENV: &str = "OVVC_THEME";

/// One colour in the palette, with no alpha and no colour space attached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Parse `#rgb`, `#rgba`, `#rrggbb` or `#rrggbbaa`. The alpha channel is
    /// accepted for compatibility with web palettes and then ignored.
    pub fn from_hex(text: &str) -> Result<Self> {
        let hex = text.trim();
        let digits = hex.strip_prefix('#').unwrap_or(hex);
        let mut nibbles = [0u8; 8];
        let count = digits.len();
        if !matches!(count, 3 | 4 | 6 | 8) {
            return Err(anyhow!("expected 3, 4, 6 or 8 hex digits"));
        }
        for (index, character) in digits.chars().enumerate() {
            nibbles[index] = character
                .to_digit(16)
                .map(|value| value as u8)
                .ok_or_else(|| anyhow!("{character:?} is not a hex digit"))?;
        }
        let pairs = match count {
            3 | 4 => [nibbles[0] * 17, nibbles[1] * 17, nibbles[2] * 17, 0],
            6 | 8 => [
                nibbles[0] * 16 + nibbles[1],
                nibbles[2] * 16 + nibbles[3],
                nibbles[4] * 16 + nibbles[5],
                0,
            ],
            _ => unreachable!(),
        };
        Ok(Self::new(pairs[0], pairs[1], pairs[2]))
    }
}

/// Every colour the interface paints.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Colors {
    pub background: Rgb,
    pub panel: Rgb,
    pub chat: Rgb,
    pub input: Rgb,
    pub hover: Rgb,
    pub border: Rgb,
    pub accent: Rgb,
    pub accent_dim: Rgb,
    pub text: Rgb,
    pub bright: Rgb,
    pub muted: Rgb,
    pub faint: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub red: Rgb,
    pub cyan: Rgb,
    pub magenta: Rgb,
    pub blue: Rgb,
    pub code: Rgb,
    pub selection_background: Rgb,
    pub selection_foreground: Rgb,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            background: Rgb::new(0x08, 0x0c, 0x09),
            panel: Rgb::new(0x0c, 0x11, 0x0d),
            chat: Rgb::new(0x09, 0x0e, 0x0a),
            input: Rgb::new(0x05, 0x09, 0x06),
            hover: Rgb::new(0x15, 0x24, 0x19),
            border: Rgb::new(0x22, 0x3a, 0x28),
            accent: Rgb::new(0x5c, 0xff, 0x8a),
            accent_dim: Rgb::new(0x1c, 0x4d, 0x2d),
            text: Rgb::new(0xb8, 0xd4, 0xbc),
            bright: Rgb::new(0xe0, 0xff, 0xe6),
            muted: Rgb::new(0x74, 0x8f, 0x7a),
            faint: Rgb::new(0x4b, 0x63, 0x50),
            green: Rgb::new(0x5c, 0xff, 0x8a),
            yellow: Rgb::new(0xff, 0xc2, 0x57),
            red: Rgb::new(0xff, 0x6b, 0x6b),
            cyan: Rgb::new(0x6c, 0xe6, 0xf2),
            magenta: Rgb::new(0xd8, 0x8b, 0xe8),
            blue: Rgb::new(0x7c, 0xb8, 0xff),
            code: Rgb::new(0x11, 0x1c, 0x14),
            selection_background: Rgb::new(0x5c, 0xff, 0x8a),
            selection_foreground: Rgb::new(0x08, 0x0c, 0x09),
        }
    }
}

/// A built-in palette the frontends can switch between at runtime. `Phosphor`
/// is the original green-on-black look and remains the default, so an empty or
/// missing theme file changes nothing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Preset {
    #[default]
    Phosphor,
    Amber,
    Ice,
    Paper,
}

impl Preset {
    /// Every preset in picker order, default first.
    pub fn all() -> &'static [Preset] {
        &[Preset::Phosphor, Preset::Amber, Preset::Ice, Preset::Paper]
    }

    /// The stable lowercase name written to `theme.toml` as `preset = "..."`.
    pub const fn name(self) -> &'static str {
        match self {
            Preset::Phosphor => "phosphor",
            Preset::Amber => "amber",
            Preset::Ice => "ice",
            Preset::Paper => "paper",
        }
    }

    /// A short label for a picker in the terminal interface.
    pub const fn label(self) -> &'static str {
        match self {
            Preset::Phosphor => "Phosphor",
            Preset::Amber => "Amber",
            Preset::Ice => "Ice",
            Preset::Paper => "Paper",
        }
    }

    /// One line describing the palette, for a tooltip or wizard caption.
    pub const fn description(self) -> &'static str {
        match self {
            Preset::Phosphor => "Green phosphor on black, the original palette",
            Preset::Amber => "Amber and cream on near-black, a warm CRT terminal",
            Preset::Ice => "Cool cyan and blue on near-black",
            Preset::Paper => "Dark ink on warm paper, a light terminal",
        }
    }

    /// Parse a preset name from `theme.toml`. Names are matched without
    /// regard to case so hand-edited files with `Preset = "Amber"` still work.
    pub fn from_name(name: &str) -> Option<Self> {
        let name = name.trim();
        Self::all()
            .iter()
            .copied()
            .find(|preset| preset.name().eq_ignore_ascii_case(name))
    }

    /// The built-in theme for this preset. Equivalent to [`Theme::preset`].
    pub fn theme(self) -> Theme {
        Theme::preset(self)
    }

    /// The `theme.toml` text that selects this preset. Explicit colour, font
    /// and author keys may be added below it and still override the preset.
    pub fn to_toml(self) -> String {
        format!("preset = \"{}\"\n", self.name())
    }
}

const AMBER_AUTHORS: [Rgb; 6] = [
    Rgb::new(0xff, 0xb0, 0x00),
    Rgb::new(0xff, 0xd1, 0x66),
    Rgb::new(0xe8, 0xc8, 0x8a),
    Rgb::new(0x9c, 0xcc, 0x65),
    Rgb::new(0x7f, 0xd4, 0xc1),
    Rgb::new(0xff, 0x8f, 0x6b),
];

const ICE_AUTHORS: [Rgb; 6] = [
    Rgb::new(0x4f, 0xd6, 0xff),
    Rgb::new(0x6a, 0xa8, 0xff),
    Rgb::new(0x59, 0xe6, 0xa8),
    Rgb::new(0xb1, 0x8c, 0xff),
    Rgb::new(0xe6, 0xf7, 0xff),
    Rgb::new(0xff, 0xd1, 0x66),
];

const PAPER_AUTHORS: [Rgb; 6] = [
    Rgb::new(0x19, 0x6b, 0x32),
    Rgb::new(0x1a, 0x4f, 0x9c),
    Rgb::new(0x7a, 0x59, 0x00),
    Rgb::new(0x8a, 0x2f, 0x7a),
    Rgb::new(0x00, 0x6b, 0x7a),
    Rgb::new(0x20, 0x26, 0x1f),
];

fn amber_colors() -> Colors {
    Colors {
        background: Rgb::new(0x0a, 0x08, 0x06),
        panel: Rgb::new(0x10, 0x0d, 0x09),
        chat: Rgb::new(0x0c, 0x09, 0x06),
        input: Rgb::new(0x06, 0x05, 0x04),
        hover: Rgb::new(0x24, 0x1a, 0x0e),
        border: Rgb::new(0x3d, 0x2b, 0x14),
        accent: Rgb::new(0xff, 0xb0, 0x00),
        accent_dim: Rgb::new(0x4d, 0x34, 0x08),
        text: Rgb::new(0xe8, 0xc8, 0x8a),
        bright: Rgb::new(0xff, 0xe3, 0xb0),
        muted: Rgb::new(0x9a, 0x7c, 0x4a),
        faint: Rgb::new(0x7a, 0x64, 0x40),
        green: Rgb::new(0x9c, 0xcc, 0x65),
        yellow: Rgb::new(0xff, 0xd1, 0x66),
        red: Rgb::new(0xff, 0x6b, 0x5e),
        cyan: Rgb::new(0x7f, 0xd4, 0xc1),
        magenta: Rgb::new(0xe3, 0x9a, 0xd6),
        blue: Rgb::new(0x86, 0xad, 0xff),
        code: Rgb::new(0x16, 0x10, 0x0a),
        selection_background: Rgb::new(0xff, 0xb0, 0x00),
        selection_foreground: Rgb::new(0x0a, 0x08, 0x06),
    }
}

fn ice_colors() -> Colors {
    Colors {
        background: Rgb::new(0x05, 0x08, 0x0d),
        panel: Rgb::new(0x08, 0x0d, 0x15),
        chat: Rgb::new(0x06, 0x0a, 0x11),
        input: Rgb::new(0x03, 0x06, 0x0b),
        hover: Rgb::new(0x10, 0x20, 0x2f),
        border: Rgb::new(0x1e, 0x3a, 0x52),
        accent: Rgb::new(0x4f, 0xd6, 0xff),
        accent_dim: Rgb::new(0x10, 0x38, 0x4d),
        text: Rgb::new(0xb8, 0xdc, 0xea),
        bright: Rgb::new(0xe6, 0xf7, 0xff),
        muted: Rgb::new(0x6f, 0x93, 0xa6),
        faint: Rgb::new(0x47, 0x67, 0x7a),
        green: Rgb::new(0x59, 0xe6, 0xa8),
        yellow: Rgb::new(0xff, 0xd1, 0x66),
        red: Rgb::new(0xff, 0x6b, 0x7a),
        cyan: Rgb::new(0x4f, 0xd6, 0xff),
        magenta: Rgb::new(0xb1, 0x8c, 0xff),
        blue: Rgb::new(0x6a, 0xa8, 0xff),
        code: Rgb::new(0x0b, 0x16, 0x22),
        selection_background: Rgb::new(0x4f, 0xd6, 0xff),
        selection_foreground: Rgb::new(0x05, 0x08, 0x0d),
    }
}

fn paper_colors() -> Colors {
    Colors {
        background: Rgb::new(0xf4, 0xf0, 0xe4),
        panel: Rgb::new(0xeb, 0xe6, 0xd8),
        chat: Rgb::new(0xf0, 0xec, 0xdf),
        input: Rgb::new(0xfa, 0xf8, 0xf0),
        hover: Rgb::new(0xdd, 0xd6, 0xc2),
        border: Rgb::new(0xb3, 0xa9, 0x8f),
        accent: Rgb::new(0x0b, 0x6b, 0x5a),
        accent_dim: Rgb::new(0xb9, 0xd8, 0xcf),
        text: Rgb::new(0x20, 0x26, 0x1f),
        bright: Rgb::new(0x00, 0x00, 0x00),
        muted: Rgb::new(0x55, 0x60, 0x4f),
        faint: Rgb::new(0x77, 0x7c, 0x6b),
        green: Rgb::new(0x19, 0x6b, 0x32),
        yellow: Rgb::new(0x7a, 0x59, 0x00),
        red: Rgb::new(0xb3, 0x26, 0x1e),
        cyan: Rgb::new(0x00, 0x6b, 0x7a),
        magenta: Rgb::new(0x8a, 0x2f, 0x7a),
        blue: Rgb::new(0x1a, 0x4f, 0x9c),
        code: Rgb::new(0xe6, 0xe0, 0xd0),
        // A light terminal keeps dark text in the selection: the fill is a
        // teal tint rather than a reverse-video accent, so selected rows and
        // text stay readable while still being clearly selected.
        selection_background: Rgb::new(0xb9, 0xd8, 0xcf),
        selection_foreground: Rgb::new(0x08, 0x11, 0x0d),
    }
}

/// The type scale. A terminal has one cell size, so the TUI ignores these,
/// but it parses them so the same file works in both frontends.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fonts {
    pub heading: f32,
    pub body: f32,
    pub monospace: f32,
    pub button: f32,
    pub small: f32,
    pub avatar: f32,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            heading: 16.0,
            body: 14.5,
            monospace: 14.0,
            button: 13.0,
            small: 11.5,
            avatar: 40.0,
        }
    }
}

/// A parsed palette: colours, type scale and per-author accents.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub colors: Colors,
    pub fonts: Fonts,
    /// ANSI-ish author colours. A terminal distinguishes speakers by hue, so
    /// keep a small readable set rather than a full rainbow.
    pub authors: Vec<Rgb>,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            colors: Colors::default(),
            fonts: Fonts::default(),
            authors: vec![
                Rgb::new(0x5c, 0xff, 0x8a),
                Rgb::new(0x6c, 0xe6, 0xf2),
                Rgb::new(0xff, 0xc2, 0x57),
                Rgb::new(0xd8, 0x8b, 0xe8),
                Rgb::new(0x7c, 0xb8, 0xff),
                Rgb::new(0xb8, 0xd4, 0xbc),
            ],
        }
    }
}

impl Theme {
    /// Build the built-in theme for `preset`. `Phosphor` is exactly
    /// [`Theme::default`], so leaving the file out keeps the original look.
    pub fn preset(preset: Preset) -> Self {
        match preset {
            Preset::Phosphor => Self::default(),
            Preset::Amber => Self::from_colors(amber_colors(), &AMBER_AUTHORS),
            Preset::Ice => Self::from_colors(ice_colors(), &ICE_AUTHORS),
            Preset::Paper => Self::from_colors(paper_colors(), &PAPER_AUTHORS),
        }
    }

    fn from_colors(colors: Colors, authors: &[Rgb]) -> Self {
        Self {
            colors,
            fonts: Fonts::default(),
            authors: authors.to_vec(),
        }
    }

    /// A stable author colour: the same person reads the same in every
    /// channel and every frontend.
    pub fn author_color(&self, seed: &[u8]) -> Rgb {
        let mut hash = 0x811c_9dc5u32;
        for byte in seed {
            hash ^= u32::from(*byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        if self.authors.is_empty() {
            return self.colors.text;
        }
        self.authors[(hash as usize) % self.authors.len()]
    }

    /// Parse a theme file. Unknown keys and bad colours are errors so a typo
    /// does not silently produce a half-themed window.
    pub fn parse(text: &str) -> Result<Self> {
        let file: ThemeFile = toml::from_str(text).context("invalid theme TOML")?;
        file.into_theme()
    }

    /// Parse a theme file on top of `base`.
    ///
    /// Every explicit colour, font and author list overrides `base`; a file
    /// that names its own `preset` uses that instead. This is what
    /// `client.theme_file` means: a file layered over the configured preset,
    /// not a replacement that silently drops the preset's other colours.
    pub fn parse_over(text: &str, base: Theme) -> Result<Self> {
        let file: ThemeFile = toml::from_str(text).context("invalid theme TOML")?;
        file.overlay(base)
    }
}

/// Where the theme file lives unless `OVVC_THEME` overrides it.
pub fn default_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("org", "ovvc", "ovvc")
        .map(|dirs| dirs.config_dir().join(THEME_FILE))
}

pub fn theme_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(THEME_ENV) {
        return Some(PathBuf::from(path));
    }
    default_path()
}

/// A missing file returns `Ok(None)`; anything else that goes wrong is an
/// error the caller can report.
pub fn load_from(path: &Path) -> Result<Option<Theme>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let theme =
                Theme::parse(&text).with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(theme))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Load `path` as an overlay on `base`. A missing file returns `Ok(None)`.
pub fn load_from_over(path: &Path, base: Theme) -> Result<Option<Theme>> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let theme = Theme::parse_over(&text, base)
                .with_context(|| format!("parsing {}", path.display()))?;
            Ok(Some(theme))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// Write `preset` to `path`, replacing the file and creating its parent
/// directory. Only the `preset` key is written: any previous explicit colour
/// overrides are deliberately discarded, because the caller chose a preset.
pub fn save_preset_to(path: &Path, preset: Preset) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, preset.to_toml()).with_context(|| format!("writing {}", path.display()))
}

/// Persist `preset` to the configured theme file (`OVVC_THEME`, else the
/// per-user config directory). Returns the path that was written so a caller
/// can show it in a notice.
pub fn save_preset(preset: Preset) -> Result<PathBuf> {
    let path = theme_path().ok_or_else(|| anyhow!("no config directory for {THEME_FILE}"))?;
    save_preset_to(&path, preset)?;
    Ok(path)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ThemeFile {
    preset: Option<String>,
    colors: ColorsFile,
    fonts: FontsFile,
    authors: AuthorsFile,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ColorsFile {
    background: Option<String>,
    panel: Option<String>,
    chat: Option<String>,
    input: Option<String>,
    hover: Option<String>,
    border: Option<String>,
    accent: Option<String>,
    accent_dim: Option<String>,
    text: Option<String>,
    bright: Option<String>,
    muted: Option<String>,
    faint: Option<String>,
    green: Option<String>,
    yellow: Option<String>,
    red: Option<String>,
    cyan: Option<String>,
    magenta: Option<String>,
    blue: Option<String>,
    code: Option<String>,
    selection_background: Option<String>,
    selection_foreground: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FontsFile {
    heading: Option<f32>,
    body: Option<f32>,
    monospace: Option<f32>,
    button: Option<f32>,
    small: Option<f32>,
    avatar: Option<f32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct AuthorsFile {
    colors: Option<Vec<String>>,
}

impl ThemeFile {
    fn into_theme(self) -> Result<Theme> {
        self.overlay(Theme::default())
    }

    /// Merge this file over `fallback` (or over its own `preset` key).
    fn overlay(self, fallback: Theme) -> Result<Theme> {
        let base = match self.preset.as_deref() {
            Some(name) => {
                let preset = Preset::from_name(name).ok_or_else(|| {
                    let known = Preset::all()
                        .iter()
                        .map(|preset| preset.name())
                        .collect::<Vec<_>>()
                        .join(", ");
                    anyhow!("preset: unknown preset {name:?}; expected one of {known}")
                })?;
                Theme::preset(preset)
            }
            None => fallback,
        };
        let c = self.colors;
        let d = base.colors;
        let colors = Colors {
            background: color(c.background, d.background, "colors.background")?,
            panel: color(c.panel, d.panel, "colors.panel")?,
            chat: color(c.chat, d.chat, "colors.chat")?,
            input: color(c.input, d.input, "colors.input")?,
            hover: color(c.hover, d.hover, "colors.hover")?,
            border: color(c.border, d.border, "colors.border")?,
            accent: color(c.accent, d.accent, "colors.accent")?,
            accent_dim: color(c.accent_dim, d.accent_dim, "colors.accent_dim")?,
            text: color(c.text, d.text, "colors.text")?,
            bright: color(c.bright, d.bright, "colors.bright")?,
            muted: color(c.muted, d.muted, "colors.muted")?,
            faint: color(c.faint, d.faint, "colors.faint")?,
            green: color(c.green, d.green, "colors.green")?,
            yellow: color(c.yellow, d.yellow, "colors.yellow")?,
            red: color(c.red, d.red, "colors.red")?,
            cyan: color(c.cyan, d.cyan, "colors.cyan")?,
            magenta: color(c.magenta, d.magenta, "colors.magenta")?,
            blue: color(c.blue, d.blue, "colors.blue")?,
            code: color(c.code, d.code, "colors.code")?,
            selection_background: color(
                c.selection_background,
                d.selection_background,
                "colors.selection_background",
            )?,
            selection_foreground: color(
                c.selection_foreground,
                d.selection_foreground,
                "colors.selection_foreground",
            )?,
        };
        let fonts = Fonts {
            heading: font_size(self.fonts.heading, base.fonts.heading, "fonts.heading")?,
            body: font_size(self.fonts.body, base.fonts.body, "fonts.body")?,
            monospace: font_size(
                self.fonts.monospace,
                base.fonts.monospace,
                "fonts.monospace",
            )?,
            button: font_size(self.fonts.button, base.fonts.button, "fonts.button")?,
            small: font_size(self.fonts.small, base.fonts.small, "fonts.small")?,
            avatar: match self.fonts.avatar {
                Some(size) if size.is_finite() && (16.0..=96.0).contains(&size) => size,
                Some(size) => bail!("fonts.avatar: {size} is outside the 16..=96 range"),
                None => base.fonts.avatar,
            },
        };
        let authors = match self.authors.colors {
            Some(values) => {
                if values.is_empty() {
                    bail!("authors.colors must contain at least one colour");
                }
                values
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        let key = format!("authors.colors[{index}]");
                        parse_hex(value, &key)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
            None => base.authors,
        };
        Ok(Theme {
            colors,
            fonts,
            authors,
        })
    }
}

fn color(value: Option<String>, default: Rgb, key: &str) -> Result<Rgb> {
    match value {
        Some(text) => parse_hex(&text, key),
        None => Ok(default),
    }
}

fn parse_hex(text: &str, key: &str) -> Result<Rgb> {
    Rgb::from_hex(text).map_err(|error| {
        anyhow!("{key}: expected #rgb, #rgba, #rrggbb or #rrggbbaa, got {text:?} ({error})")
    })
}

fn font_size(value: Option<f32>, default: f32, key: &str) -> Result<f32> {
    match value {
        Some(size) if size.is_finite() && (4.0..=48.0).contains(&size) => Ok(size),
        Some(size) => bail!("{key}: {size} is outside the 4..=48 range"),
        None => Ok(default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_keeps_the_built_in_palette() {
        assert_eq!(Theme::parse("").unwrap(), Theme::default());
        assert_eq!(
            Theme::parse("[colors]\n[fonts]\n[authors]\n").unwrap(),
            Theme::default()
        );
    }

    #[test]
    fn colours_and_sizes_are_overridden() {
        let theme = Theme::parse(
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
        assert_eq!(theme.colors.background, Rgb::new(0x10, 0x10, 0x10));
        assert_eq!(theme.colors.accent, Rgb::new(0xff, 0xb0, 0x00));
        assert_eq!(theme.colors.selection_background, theme.colors.accent);
        assert_eq!(theme.fonts.body, 14.0);
        assert_eq!(theme.fonts.small, 10.5);
        assert_eq!(theme.authors.len(), 2);
        assert_eq!(theme.authors[0], theme.colors.accent);
        // Untouched keys still come from the default.
        assert_eq!(theme.colors.chat, Colors::default().chat);
    }

    #[test]
    fn a_file_layers_over_the_given_base() {
        let ice = Theme::preset(Preset::Ice);
        let theme = Theme::parse_over("[colors]\naccent = \"#ffb000\"\n", ice.clone()).unwrap();
        assert_eq!(theme.colors.accent, Rgb::new(0xff, 0xb0, 0x00));
        assert_eq!(
            theme.colors.background, ice.colors.background,
            "unset colours keep the base preset"
        );
        assert_eq!(theme.colors.text, ice.colors.text);

        // A preset named inside the file still wins over the caller's base.
        let amber = Theme::parse_over("preset = \"amber\"\n", ice).unwrap();
        assert_eq!(amber, Theme::preset(Preset::Amber));
    }

    #[test]
    fn short_and_alpha_hex_forms_are_accepted() {
        assert_eq!(Rgb::from_hex("#fff").unwrap(), Rgb::new(255, 255, 255));
        assert_eq!(Rgb::from_hex("#f00a").unwrap(), Rgb::new(255, 0, 0));
        assert_eq!(
            Rgb::from_hex("#11223344").unwrap(),
            Rgb::new(0x11, 0x22, 0x33)
        );
        assert!(Rgb::from_hex("#12").is_err());
        assert!(Rgb::from_hex("nope").is_err());
    }

    #[test]
    fn a_bad_colour_names_the_key() {
        let error = Theme::parse("[colors]\naccent = \"orange\"\n").unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("colors.accent"), "{message}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = Theme::parse("[colors]\naccentt = \"#ffffff\"\n").unwrap_err();
        assert!(format!("{error:#}").contains("accentt"));
    }

    #[test]
    fn font_sizes_are_range_checked() {
        let error = Theme::parse("[fonts]\nbody = 400.0\n").unwrap_err();
        assert!(format!("{error:#}").contains("fonts.body"));
        let error = Theme::parse("[fonts]\nbody = 0.0\n").unwrap_err();
        assert!(format!("{error:#}").contains("fonts.body"));
        let error = Theme::parse("[fonts]\navatar = 8.0\n").unwrap_err();
        assert!(format!("{error:#}").contains("fonts.avatar"));
    }

    #[test]
    fn the_avatar_size_is_configurable() {
        let theme = Theme::parse("[fonts]\navatar = 56.0\n").unwrap();
        assert_eq!(theme.fonts.avatar, 56.0);
        assert_eq!(Theme::default().fonts.avatar, 40.0);
    }

    #[test]
    fn an_empty_author_list_is_rejected() {
        let error = Theme::parse("[authors]\ncolors = []\n").unwrap_err();
        assert!(format!("{error:#}").contains("authors.colors"));
    }

    #[test]
    fn author_colours_are_stable() {
        let theme = Theme::default();
        let seed = [7u8; 32];
        assert_eq!(theme.author_color(&seed), theme.author_color(&seed));
        assert!(theme.authors.contains(&theme.author_color(&seed)));
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
        let theme = load_from(&path).unwrap().unwrap();
        assert_eq!(theme.colors.accent, Rgb::new(0xff, 0, 0));
    }
}
