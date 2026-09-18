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

/// A connection opted into disk caching loads its persisted dictionary without
/// touching the database (no session is ever created here).
#[gpui_kit::test]
async fn disk_cached_metadata_loads_without_a_database(cx: &mut TestAppContext) {
    use sqlhighland::metadata::{
        save_cache, CacheFingerprint, MetadataCache, MetadataCacheDisk, TableId, TableKind,
    };
    use sqlhighland::model::ConnectionConfig;

    cx.update(gpui_kit::init);
    let dir = std::env::temp_dir().join(format!(
        "sqlhighland-metadisk-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A connection with disk caching enabled.
    let toml = "[[connections]]\nid = \"c0\"\nname = \"db-00\"\nhost = \"h\"\nport = 1521\nservice_name = \"s\"\nuser = \"u\"\npassword = \"p\"\nenvironment = \"Dev\"\ncache_metadata_to_disk = true\n";
    std::fs::write(dir.join("connections.toml"), toml).unwrap();
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);

    // Stage a persisted cache matching that connection's fingerprint.
    let cfg = ConnectionConfig {
        id: "c0".to_string(),
        host: "h".to_string(),
        port: 1521,
        service_name: "s".to_string(),
        user: "u".to_string(),
        ..Default::default()
    };
    let fp = CacheFingerprint::of(&cfg, false);
    let cache = MetadataCache {
        tables: vec![TableId {
            owner: "SCOTT".to_string(),
            name: "EMP".to_string(),
            kind: TableKind::Table,
        }],
        fetched_at: Some(std::time::Instant::now()),
        ..Default::default()
    };
    save_cache("c0", &MetadataCacheDisk::capture(&cache, fp), None).unwrap();

    let slot: std::rc::Rc<std::cell::RefCell<Option<gpui_kit::Entity<SqlHighlandView>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let for_window = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = slot.borrow().clone().expect("view captured");

    // Warm-up should install the disk copy; no DB is available, so a fetch
    // would leave the cache empty (and the status on "Loading…").
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        view.update(cx, |this, cx| this.debug_ensure_meta("c0", cx));
        assert_eq!(
            view.read(cx).debug_cached_tables("c0"),
            ["SCOTT.EMP"],
            "the persisted dictionary is loaded from disk"
        );
    })
    .unwrap();
    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}
