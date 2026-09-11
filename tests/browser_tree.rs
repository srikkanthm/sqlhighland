//! Schema-browser expand wiring: clicking a connection's chevron
//! materializes a tree (showing the loading placeholder with no database).
//! Uses `SQLHIGHLAND_CONFIG_DIR` to stage an isolated config.

#![recursion_limit = "256"]

use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{px, size, AppContext as _, TestAppContext};
use sqlhighland::app::SqlHighlandView;

fn staged_config_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let toml = "[[connections]]\nid = \"c0\"\nname = \"db-00\"\nhost = \"h\"\nport = 1521\nservice_name = \"s\"\nuser = \"u\"\npassword = \"p\"\nenvironment = \"Dev\"\n";
    std::fs::write(dir.join("connections.toml"), toml).unwrap();
    dir
}

#[gpui_kit::test]
async fn browser_expand_shows_tree(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("browsertree");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    let tree_present = cx
        .update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            window.click(("conn-expand", 0usize), cx);
            window.render_frame(cx);
            window.render_frame(cx);
            gpui_kit::base::test_support::registered_paths(window).contains("tree-")
        })
        .unwrap();
    assert!(tree_present, "expanding a connection should show its tree");
    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}
