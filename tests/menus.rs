//! Native menu coverage: every shortcut-bearing action appears exactly
//! once, and grid-scoped CopySelection stays out (a menu item would
//! route AppKit's Cmd+C through global dispatch, breaking editor copy).

use gpui_kit::TestAppContext;
use sqlhighland::app;

#[gpui_kit::test]
async fn menu_coverage(_cx: &mut TestAppContext) {
    let mut names: Vec<String> = Vec::new();
    for menu in app::app_menus() {
        for item in &menu.items {
            if let gpui_kit::MenuItem::Action { action, .. } = item {
                let full = action.name().to_string();
                names.push(full.rsplit("::").next().unwrap_or("").to_string());
            }
        }
    }
    eprintln!("menu actions: {names:?}");
    // Every shortcut-bearing action exactly once — except CopySelection,
    // whose grid/output-scoped ⌘C twins must never route through the
    // menu (a menu item would hijack AppKit's Cmd+C globally, breaking
    // normal copy in the editor).
    for expected in [
        "OpenSettings",
        "OpenAbout",
        "Quit",
        "NewTab",
        "PickConnection",
        "RebindConnection",
        "CloseTab",
        "NewConnection",
        "OpenSql",
        "SaveSql",
        "SaveSqlAs",
        "RunQuery",
        "RunScript",
        "FormatQuery",
        "CommitTxn",
        "RollbackTxn",
        "TriggerComplete",
        "ToggleSidebar",
        "NextTab",
        "PrevTab",
        "DismissResults",
        "ZoomIn",
        "ZoomOut",
        "ZoomReset",
        "GrowEditor",
        "ShrinkEditor",
    ] {
        assert_eq!(
            names.iter().filter(|n| n.as_str() == expected).count(),
            1,
            "{expected} should appear exactly once"
        );
    }
    assert!(
        !names.iter().any(|n| n == "CopySelection"),
        "CopySelection must stay out of menus"
    );
}
