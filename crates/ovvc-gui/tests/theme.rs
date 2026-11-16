//! Hot-swapping presets on a live egui context.
//!
//! Run with:
//!
//! ```text
//! cargo test -p ovvc-gui --test theme
//! ```

#![cfg(feature = "ui")]

use egui::{Color32, Context};
use ovvc_gui::theme::{self, Palette};
use ovvc_theme::{Preset, Theme};

fn expected(preset: Preset) -> Palette {
    Palette::from(&Theme::preset(preset))
}

#[test]
fn a_fresh_context_is_phosphor() {
    let ctx = Context::default();
    assert_eq!(theme::active(&ctx), Preset::Phosphor);
    assert_eq!(theme::palette(&ctx), Palette::default());
}

#[test]
fn apply_hot_swaps_every_preset() {
    let ctx = Context::default();
    for preset in Preset::all() {
        theme::apply(&ctx, *preset);
        assert_eq!(theme::active(&ctx), *preset, "{}", preset.name());
        assert_eq!(theme::palette(&ctx), expected(*preset));
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.panel_fill,
            expected(*preset).bg
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Light).visuals.panel_fill,
            expected(*preset).bg
        );
        assert_eq!(
            ctx.style_of(egui::Theme::Dark).visuals.selection.bg_fill,
            expected(*preset).select_bg
        );
    }
}

#[test]
fn apply_asks_for_a_repaint() {
    let ctx = Context::default();
    theme::apply(&ctx, Preset::Amber);
    assert!(ctx.has_requested_repaint());
}

#[test]
fn switching_back_and_forth_keeps_the_selected_preset() {
    let ctx = Context::default();
    theme::apply(&ctx, Preset::Amber);
    let amber = theme::palette(&ctx);
    theme::apply(&ctx, Preset::Paper);
    let paper = theme::palette(&ctx);
    assert_ne!(amber.bg, paper.bg);
    assert_eq!(theme::active(&ctx), Preset::Paper);
    theme::apply(&ctx, Preset::Amber);
    assert_eq!(theme::active(&ctx), Preset::Amber);
    assert_eq!(theme::palette(&ctx), amber);
}

#[test]
fn a_configured_theme_file_layers_over_its_preset() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("custom.toml");
    std::fs::write(&path, "[colors]\naccent = \"#ffb000\"\n").unwrap();
    let client = ovvc_config::ClientConfig {
        theme: Some("ice".to_string()),
        theme_file: Some(path),
        keybindings: Default::default(),
    };

    let palette = theme::load_configured(Some(&client));
    let ice = Palette::from(&Theme::preset(Preset::Ice));
    assert_eq!(palette.accent, Color32::from_rgb(0xff, 0xb0, 0x00));
    assert_eq!(palette.bg, ice.bg, "unset colours stay from the preset");
    assert_eq!(palette.text, ice.text);
}

#[test]
fn a_missing_configured_theme_file_falls_back_to_the_preset() {
    let client = ovvc_config::ClientConfig {
        theme: Some("amber".to_string()),
        theme_file: Some(std::path::PathBuf::from("/definitely/missing/theme.toml")),
        keybindings: Default::default(),
    };

    assert_eq!(
        theme::load_configured(Some(&client)),
        expected(Preset::Amber)
    );
}

#[test]
fn a_palette_installed_from_the_file_maps_back_to_its_preset() {
    let ctx = Context::default();
    for preset in Preset::all() {
        let mut palette = expected(*preset);
        // A font-size override on top of `preset = "..."` must not hide the
        // preset from the picker.
        palette.body = 17.0;
        theme::install_with(&ctx, palette);
        assert_eq!(theme::active(&ctx), *preset, "{}", preset.name());
    }
}

#[test]
fn a_fully_custom_palette_reports_the_default() {
    let ctx = Context::default();
    theme::install_with(
        &ctx,
        Palette {
            accent: Color32::RED,
            ..Palette::default()
        },
    );
    assert_eq!(theme::active(&ctx), Preset::Phosphor);
}
