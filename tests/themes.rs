//! Bundled theme JSONs must parse and register: a typo in a color slot
//! otherwise fails silently at startup (`register_themes` only eprintlns).

#![recursion_limit = "256"]

use gpui_kit::component::ThemeRegistry;
use gpui_kit::{AppContext as _, TestAppContext};

const EXPECTED: &[&str] = &[
    "Nord Light",
    "Nord Dark",
    "Catppuccin Latte",
    "Catppuccin Frappé",
    "Catppuccin Macchiato",
    "Catppuccin Mocha",
    "Solarized Light",
    "Solarized Dark",
    "Gruvbox Dark",
    "Gruvbox Light",
    "Ayu Dark",
    "Ayu Light",
    "Ayu Mirage",
];

#[gpui_kit::test]
async fn bundled_themes_register(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let files = [
            include_str!("../assets/themes/nord.json"),
            include_str!("../assets/themes/catppuccin.json"),
            include_str!("../assets/themes/solarized.json"),
            include_str!("../assets/themes/gruvbox.json"),
            include_str!("../assets/themes/ayu.json"),
        ];
        for content in files {
            ThemeRegistry::global_mut(cx)
                .load_themes_from_str(content)
                .expect("bundled theme json must parse");
        }
        // Every variant from the plan applies without falling back.
        for name in EXPECTED {
            sqlhighland::guitheme::apply_theme(name, None, cx);
        }
        let themes = ThemeRegistry::global(cx).themes();
        for name in EXPECTED {
            assert!(themes.contains_key(*name), "missing theme {name}");
        }
    });
}
