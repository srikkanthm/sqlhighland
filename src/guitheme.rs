//! Flat theme selection, registration, and application.
//!
//! One list (`THEME_LIST`, mirrored from `config::THEME_LIST` by name):
//! `System` plus every concrete variant. `System` resolves to the stock
//! Default pair by OS appearance; anything else applies that exact config
//! directly, following the kit's own settings-story pattern
//! (`apply_config` then `change`), which keeps pair slots, tokens, and
//! highlight in agreement.

use crate::config::{Preferences, SYSTEM_THEME};
use gpui_kit::component::{Theme, ThemeMode, ThemeRegistry};
use gpui_kit::*;

const NORD_THEMES: &str = include_str!("../assets/themes/nord.json");
const CATPPUCCIN_THEMES: &str = include_str!("../assets/themes/catppuccin.json");
const SOLARIZED_THEMES: &str = include_str!("../assets/themes/solarized.json");
const GRUVBOX_THEMES: &str = include_str!("../assets/themes/gruvbox.json");
const AYU_THEMES: &str = include_str!("../assets/themes/ayu.json");

/// Register bundled theme families. Call once after `gpui_kit::init`.
pub fn register_themes(cx: &mut App) {
    let registry = ThemeRegistry::global_mut(cx);
    for content in [
        NORD_THEMES,
        CATPPUCCIN_THEMES,
        SOLARIZED_THEMES,
        GRUVBOX_THEMES,
        AYU_THEMES,
    ] {
        if let Err(e) = registry.load_themes_from_str(content) {
            crate::logging::error(format!("failed to load bundled themes: {e:#}"));
        }
    }
}

/// Resolve System mode against the OS appearance.
pub fn system_is_dark(window: &Window) -> bool {
    use gpui::WindowAppearance;
    matches!(
        window.appearance(),
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// Apply a theme selection: exact config name, or [`SYSTEM_THEME`].
/// Always repaints open windows.
pub fn apply_theme(selection: &str, window: Option<&mut Window>, cx: &mut App) {
    if selection == SYSTEM_THEME {
        let dark = window.as_ref().map(|w| system_is_dark(w)).unwrap_or(false);
        let (name, mode) = if dark {
            ("Default Dark", ThemeMode::Dark)
        } else {
            ("Default Light", ThemeMode::Light)
        };
        apply_config_by_name(name, mode, window, cx);
    } else if let Some(mode) = config_mode(selection, cx) {
        apply_config_by_name(selection, mode, window, cx);
    } else {
        crate::logging::warn(format!(
            "unknown theme {selection:?}; keeping current theme"
        ));
    }
}

/// Look up the selection's own mode from the registry (each config knows
/// whether it is light or dark).
fn config_mode(selection: &str, cx: &App) -> Option<ThemeMode> {
    ThemeRegistry::global(cx)
        .themes()
        .get(selection)
        .map(|config| config.mode)
}

/// Apply one registry config directly, then switch to its mode — the same
/// order the kit's settings story uses, so pair slots, tokens, and the
/// highlight theme stay in agreement.
fn apply_config_by_name(name: &str, mode: ThemeMode, window: Option<&mut Window>, cx: &mut App) {
    let config = ThemeRegistry::global(cx).themes().get(name).cloned();
    let Some(config) = config else {
        crate::logging::warn(format!(
            "theme {name:?} not registered; keeping current theme"
        ));
        return;
    };
    Theme::global_mut(cx).apply_config(&config);
    Theme::change(mode, window, cx);
    cx.refresh_windows();
}

/// Apply saved preferences (convenience over [`apply_theme`]).
/// User font overrides stamp over the active theme afterwards: editors
/// keep their size across theme switches, family follows the theme
/// unless explicitly chosen.
pub fn apply_preferences(prefs: &Preferences, window: Option<&mut Window>, cx: &mut App) {
    apply_theme(&prefs.theme_name(), window, cx);
    apply_font_prefs(prefs, cx);
}

/// Stamp user font choices over the active theme. Called on every apply
/// (theme switches reset these fields, so the override must re-apply).
pub fn apply_font_prefs(prefs: &Preferences, cx: &mut App) {
    let theme = Theme::global_mut(cx);
    if !prefs.font_family.is_empty() {
        theme.mono_font_family = prefs.font_family.clone().into();
    }
    theme.mono_font_size = px(prefs.font_size as f32);
    cx.refresh_windows();
}

/// Re-apply the saved selection for the OS appearance. Wired to the window's
/// appearance observer; a no-op unless the selection is System.
pub fn reapply_for_system_appearance(window: &mut Window, cx: &mut App) {
    let prefs = Preferences::load();
    if prefs.theme_name() == SYSTEM_THEME {
        apply_preferences(&prefs, Some(window), cx);
    }
}
