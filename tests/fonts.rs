//! Editor font preferences: "Theme default" must genuinely restore the
//! theme's mono family after an explicit pick, and the availability check
//! must reject families that would resolve to a substitute.
//!
//! Registration of the embedded faces is covered by `src/fonts.rs` unit tests:
//! the headless text system (`NoopTextSystem`) ignores `add_fonts`, so
//! `all_font_names` cannot observe embedded fonts here.

#![recursion_limit = "256"]

use gpui_kit::component::Theme;
use gpui_kit::TestAppContext;
use sqlhighland::config::{font_family_available, Preferences};
use sqlhighland::guitheme;

fn prefs(theme: &str, family: &str) -> Preferences {
    let mut prefs = Preferences::default();
    prefs.theme = theme.to_string();
    prefs.font_family = family.to_string();
    prefs
}

#[gpui_kit::test]
async fn theme_default_pick_restores_theme_family(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        guitheme::register_themes(cx);
        // Baseline: theme with the empty "Theme default" family.
        guitheme::apply_preferences(&prefs("Nord Dark", ""), None, cx);
        let baseline = Theme::global(cx).mono_font_family.clone();

        // An explicit pick stamps over the theme.
        guitheme::apply_preferences(&prefs("Nord Dark", "Hack"), None, cx);
        assert_eq!(Theme::global(cx).mono_font_family.clone(), "Hack");

        // Re-selecting "Theme default" must genuinely revert, not stay on Hack.
        guitheme::apply_preferences(&prefs("Nord Dark", ""), None, cx);
        assert_eq!(Theme::global(cx).mono_font_family.clone(), baseline);
    });
}

#[gpui_kit::test]
async fn explicit_family_survives_theme_switch(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        guitheme::register_themes(cx);
        // A chosen family follows the user across theme changes.
        guitheme::apply_preferences(&prefs("Nord Dark", "Hack"), None, cx);
        guitheme::apply_preferences(&prefs("Gruvbox Dark", "Hack"), None, cx);
        assert_eq!(Theme::global(cx).mono_font_family.clone(), "Hack");
    });
}

#[test]
fn availability_predicate() {
    let installed = vec!["Menlo".to_string(), "Hack".to_string()];
    assert!(font_family_available("", &installed)); // Theme default
    assert!(font_family_available("Hack", &installed));
    assert!(!font_family_available("JetBrains Mono", &installed));
}
