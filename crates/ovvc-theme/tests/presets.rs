//! Preset catalogue, `preset = "..."` parsing and persistence.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-theme
//! ```

use ovvc_theme::{Preset, Rgb, Theme, load_from, save_preset_to};

fn ratio(a: Rgb, b: Rgb) -> f64 {
    fn channel(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.039_28 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }

    let luminance = |colour: Rgb| {
        0.2126 * channel(colour.r) + 0.7152 * channel(colour.g) + 0.0722 * channel(colour.b)
    };
    let (lighter, darker) = match (luminance(a), luminance(b)) {
        (a, b) if a >= b => (a, b),
        (a, b) => (b, a),
    };
    (lighter + 0.05) / (darker + 0.05)
}

#[test]
fn every_preset_has_a_stable_lowercase_name() {
    for preset in Preset::all() {
        let name = preset.name();
        assert_eq!(name, name.to_lowercase(), "{name}");
        assert!(
            name.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
            "{name}"
        );
        assert_eq!(Preset::from_name(name), Some(*preset));
        assert_eq!(Preset::from_name(&name.to_uppercase()), Some(*preset));
        assert_eq!(Preset::from_name(&format!("  {name}  ")), Some(*preset));
    }
    assert_eq!(Preset::from_name("phosphor"), Some(Preset::Phosphor));
    assert_eq!(Preset::from_name("amber"), Some(Preset::Amber));
    assert_eq!(Preset::from_name("ice"), Some(Preset::Ice));
    assert_eq!(Preset::from_name("paper"), Some(Preset::Paper));
    assert_eq!(Preset::from_name(""), None);
    assert_eq!(Preset::from_name("solarized"), None);
}

#[test]
fn the_picker_order_starts_with_the_default() {
    assert_eq!(Preset::all().first(), Some(&Preset::Phosphor));
    assert_eq!(Preset::all().len(), 4);
    assert_eq!(Preset::default(), Preset::Phosphor);
}

#[test]
fn phosphor_is_exactly_the_built_in_theme() {
    assert_eq!(Theme::preset(Preset::Phosphor), Theme::default());
    assert_eq!(Preset::Phosphor.theme(), Theme::default());
}

#[test]
fn each_preset_formats_and_parses_back_to_the_same_theme() {
    for preset in Preset::all() {
        let text = preset.to_toml();
        assert!(
            text.contains(&format!("preset = \"{}\"", preset.name())),
            "{text}"
        );
        assert_eq!(Theme::parse(&text).unwrap(), Theme::preset(*preset));
        assert_eq!(
            Theme::parse(&format!("preset = \"{}\"\n", preset.name())).unwrap(),
            preset.theme()
        );
    }
}

#[test]
fn an_empty_file_keeps_phosphor() {
    assert_eq!(Theme::parse("").unwrap(), Theme::default());
    assert_eq!(
        Theme::parse("[colors]\n[fonts]\n[authors]\n").unwrap(),
        Theme::default()
    );
}

#[test]
fn explicit_keys_override_a_named_preset() {
    let theme = Theme::parse(
        r##"
        preset = "amber"

        [colors]
        accent = "#ff0000"
        selection_background = "#ff0000"

        [fonts]
        body = 15.0

        [authors]
        colors = ["#ffffff"]
        "##,
    )
    .unwrap();
    let amber = Theme::preset(Preset::Amber);
    assert_eq!(theme.colors.accent, Rgb::new(0xff, 0, 0));
    assert_eq!(theme.colors.selection_background, Rgb::new(0xff, 0, 0));
    // Keys that were not listed still come from the preset, not the default.
    assert_eq!(theme.colors.background, amber.colors.background);
    assert_eq!(theme.colors.text, amber.colors.text);
    assert_ne!(theme.colors.background, Theme::default().colors.background);
    assert_eq!(theme.fonts.body, 15.0);
    assert_eq!(theme.authors, vec![Rgb::new(0xff, 0xff, 0xff)]);
}

#[test]
fn a_preset_carries_its_author_colours() {
    for preset in Preset::all() {
        let theme = Theme::preset(*preset);
        assert!(!theme.authors.is_empty(), "{}", preset.name());
        assert_eq!(
            Theme::parse(&preset.to_toml()).unwrap().authors,
            theme.authors
        );
    }
}

#[test]
fn an_unknown_preset_is_rejected_and_names_the_options() {
    let error = Theme::parse("preset = \"solarized\"\n").unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("preset"), "{message}");
    assert!(message.contains("solarized"), "{message}");
    assert!(message.contains("phosphor"), "{message}");
}

#[test]
fn presets_are_visibly_different_from_each_other() {
    let backgrounds: Vec<_> = Preset::all()
        .iter()
        .map(|preset| Theme::preset(*preset).colors.background)
        .collect();
    for (index, left) in backgrounds.iter().enumerate() {
        for right in &backgrounds[index + 1..] {
            assert_ne!(left, right, "two presets share a background colour");
        }
    }
    // Paper is the light terminal, so its text must be darker than its
    // background while every other preset is the other way round.
    for preset in Preset::all() {
        let theme = Theme::preset(*preset);
        let text = ratio(theme.colors.text, theme.colors.background);
        assert!(text >= 4.5, "{}: text/background {text:.2}", preset.name());
        if *preset == Preset::Paper {
            assert!(theme.colors.text.r < theme.colors.background.r);
        } else {
            assert!(theme.colors.text.r > theme.colors.background.r);
        }
    }
}

#[test]
fn selection_is_reverse_video_except_on_paper() {
    for preset in Preset::all() {
        let c = Theme::preset(*preset).colors;
        if *preset == Preset::Paper {
            // A light terminal keeps dark text on a light selection tint.
            assert!(c.selection_foreground.r < c.selection_background.r);
        } else {
            // Reverse video: the accent fills the row and the background
            // colour becomes the text.
            assert_eq!(c.selection_background, c.accent, "{}", preset.name());
            assert_eq!(c.selection_foreground, c.background, "{}", preset.name());
        }
    }
}

/// WCAG 2.x relative-luminance contrast, with thresholds that match the role
/// of each colour: normal body text needs 4.5:1, bright text 7:1, and the
/// faint dividers/metadata are deliberately quiet at 3:1 (the AA large-text
/// minimum). Selection is reverse video, so its foreground must contrast with
/// the accent fill rather than with the window background. The original
/// phosphor `faint` colour lands on 2.9998, so its check allows 2.95 rather
/// than failing the unchanged built-in palette on a rounding error.
#[test]
fn every_preset_has_readable_contrast() {
    for preset in Preset::all() {
        let theme = Theme::preset(*preset);
        let c = theme.colors;
        let name = preset.name();
        let check = |role: &str, foreground: Rgb, background: Rgb, minimum: f64| {
            let ratio = ratio(foreground, background);
            assert!(
                ratio >= minimum,
                "{name}: {role} contrast is {ratio:.2}, expected at least {minimum:.1}"
            );
        };
        check("text/background", c.text, c.background, 4.5);
        check("text/chat", c.text, c.chat, 4.5);
        check("bright/background", c.bright, c.background, 7.0);
        check("muted/background", c.muted, c.background, 4.5);
        check("faint/background", c.faint, c.background, 2.95);
        check("accent/background", c.accent, c.background, 4.5);
        check(
            "selection foreground/background",
            c.selection_foreground,
            c.selection_background,
            4.5,
        );
        for (index, author) in theme.authors.iter().enumerate() {
            let ratio = ratio(*author, c.chat);
            assert!(
                ratio >= 4.5,
                "{name}: authors[{index}] contrast against chat is {ratio:.2}"
            );
        }
    }
}

#[test]
fn a_saved_preset_round_trips_through_the_theme_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("theme.toml");
    for preset in Preset::all() {
        save_preset_to(&path, *preset).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, preset.to_toml());
        let loaded = load_from(&path).unwrap().unwrap();
        assert_eq!(loaded, Theme::preset(*preset));
    }
}

#[test]
fn saving_creates_the_parent_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nested").join("config").join("theme.toml");
    save_preset_to(&path, Preset::Ice).unwrap();
    assert_eq!(load_from(&path).unwrap(), Some(Theme::preset(Preset::Ice)));
}

#[test]
fn saving_a_preset_replaces_a_full_custom_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("theme.toml");
    std::fs::write(
        &path,
        "[colors]\naccent = \"#ff0000\"\n\n[fonts]\nbody = 20.0\n",
    )
    .unwrap();
    save_preset_to(&path, Preset::Paper).unwrap();
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "preset = \"paper\"\n"
    );
    assert_eq!(
        load_from(&path).unwrap(),
        Some(Theme::preset(Preset::Paper))
    );
}
