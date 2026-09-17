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
/// Editors keep their size across theme switches; an explicit family stamps
/// over the theme, while an empty family restores the theme's own family.
pub fn apply_preferences(prefs: &Preferences, window: Option<&mut Window>, cx: &mut App) {
    capture_theme_mono_default(cx);
    apply_theme(&prefs.theme_name(), window, cx);
    apply_font_prefs(prefs, cx);
}

/// The platform's default monospace family, captured once. "Theme default"
/// restores it: `apply_config` leaves `mono_font_family` untouched when a
/// theme declares none, so re-applying a theme cannot clear an explicit pick.
#[derive(Clone)]
struct ThemeMonoDefault(SharedString);

impl Global for ThemeMonoDefault {}

/// Capture the default mono family. Idempotent, so it can be called on every
/// apply; the first call (at startup, before any user font is stamped) wins.
pub fn capture_theme_mono_default(cx: &mut App) {
    if cx.has_global::<ThemeMonoDefault>() {
        return;
    }
    let family = Theme::global(cx).mono_font_family.clone();
    cx.set_global(ThemeMonoDefault(family));
}

/// The mono family the selected theme declares itself, if any.
fn theme_mono_family(selection: &str, cx: &App) -> Option<SharedString> {
    ThemeRegistry::global(cx)
        .themes()
        .get(selection)
        .and_then(|config| config.mono_font_family.clone())
}

/// Stamp user font choices over the active theme. An empty family means
/// "follow the theme": restore the theme's own family (or the captured
/// platform default), since applying a theme does not reset it.
pub fn apply_font_prefs(prefs: &Preferences, cx: &mut App) {
    let family = if prefs.font_family.is_empty() {
        theme_mono_family(&prefs.theme_name(), cx)
            .or_else(|| cx.try_global::<ThemeMonoDefault>().map(|d| d.0.clone()))
    } else {
        Some(prefs.font_family.clone().into())
    };
    let theme = Theme::global_mut(cx);
    if let Some(family) = family {
        theme.mono_font_family = family;
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
