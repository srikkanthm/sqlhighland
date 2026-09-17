//! Headless check of the tab-strip back/forward buttons: they move to the
//! previous/next tab in display order (independent of visit history), are
//! no-ops at the ends, and stay correct after jumping around or adding tabs.
//! The active tab is asserted via the kit Tab's accessibility `selected` flag.
//! Uses an isolated config dir; no database.

#![recursion_limit = "256"]

use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{px, size, AppContext, TestAppContext, Window};
use sqlhighland::app::SqlHighlandView;

fn staged_tabs_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-tabnav-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let toml = "\
[[tabs]]
id = \"t0\"
name = \"T0\"

[[tabs]]
id = \"t1\"
name = \"T1\"

[[tabs]]
id = \"t2\"
name = \"T2\"

[[tabs]]
id = \"t3\"
name = \"T3\"

[[tabs]]
id = \"t4\"
name = \"T4\"
";
    std::fs::write(dir.join("tabs.toml"), toml).unwrap();
    dir
}

/// `SQLHIGHLAND_CONFIG_DIR` is process-global and cargo runs the tests in this
/// binary on parallel threads, so serialize them: otherwise one test can
/// redirect another's config lookups mid-run (and, for `persist_tabs`, its
/// writes).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Index of the selected tab, or panics if none is marked selected.
fn active_tab(window: &Window) -> usize {
    (0..64usize)
        .find(|ix| window.try_find(*ix).and_then(|s| s.selected()) == Some(true))
        .expect("exactly one tab should be selected")
}

/// Labels of the tabs in display order (indices are dense from zero).
fn tab_labels(window: &Window) -> Vec<String> {
    (0..64usize)
        .map_while(|ix| {
            window
                .try_find(ix)
                .and_then(|s| s.label().map(str::to_string))
        })
        .collect()
}

#[gpui_kit::test]
async fn tab_nav_moves_adjacent(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("adjacent");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(active_tab(window), 0, "starts on the first tab");

        // Forward walks right: 0 -> 1 -> 2.
        window.click("tab-nav-forward", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 1);
        window.click("tab-nav-forward", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 2);

        // Back walks left: 2 -> 1 -> 0.
        window.click("tab-nav-back", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 1);
        window.click("tab-nav-back", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 0);

        // At the first tab, Back is a no-op.
        window.click("tab-nav-back", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 0, "back is a no-op at the first tab");

        // At the last tab, Forward is a no-op.
        window.click(4usize, cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 4);
        window.click("tab-nav-forward", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 4, "forward is a no-op at the last tab");

        // Adjacency after a direct jump: 4 -> 3 -> 2.
        window.click("tab-nav-back", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 3, "back from a jumped-to tab");
        window.click("tab-nav-back", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 2);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Adding a tab with "+" makes it the last tab, so Forward is a no-op there and
/// Back returns to the previous strip neighbour.
#[gpui_kit::test]
async fn tab_nav_after_adding_tab(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("add");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tab-add", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 5, "new tab is active and last");
        // Forward is a no-op at the end.
        window.click("tab-nav-forward", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 5);
        // Back returns to the previous strip neighbour (tab 4).
        window.click("tab-nav-back", cx);
        window.render_frame(cx);
        assert_eq!(
            active_tab(window),
            4,
            "back to the previous tab in the strip"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Move Tab Left/Right reorders the strip in place: the active tab keeps focus
/// and follows its slot, the ends are no-ops, and the new order is persisted to
/// `tabs.toml`.
#[gpui_kit::test]
async fn move_tab_reorders_and_persists(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("move");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T0", "T1", "T2", "T3", "T4"]);
        assert_eq!(active_tab(window), 0);

        // Move the active tab (T0) right: it follows and stays active.
        window.press("ctrl-shift-pagedown", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T1", "T0", "T2", "T3", "T4"]);
        assert_eq!(active_tab(window), 1, "moved tab stays active");

        // Right again, then left twice back to the start.
        window.press("ctrl-shift-pagedown", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T1", "T2", "T0", "T3", "T4"]);
        assert_eq!(active_tab(window), 2);
        window.press("ctrl-shift-pageup", cx);
        window.render_frame(cx);
        window.press("ctrl-shift-pageup", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T0", "T1", "T2", "T3", "T4"]);
        assert_eq!(active_tab(window), 0);

        // Left at the first tab is a no-op.
        window.press("ctrl-shift-pageup", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T0", "T1", "T2", "T3", "T4"]);
        assert_eq!(
            active_tab(window),
            0,
            "move left at the first tab is a no-op"
        );

        // The Cmd+Alt alias moves too (keyboard-native, no Fn).
        window.press("cmd-alt-right", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T1", "T0", "T2", "T3", "T4"]);
        window.press("cmd-alt-left", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T0", "T1", "T2", "T3", "T4"]);

        // The new order is persisted for the next launch.
        let manifest = std::fs::read_to_string(dir.join("tabs.toml")).unwrap();
        let names: Vec<&str> = manifest
            .lines()
            .filter_map(|l| l.trim().strip_prefix("name = \""))
            .filter_map(|l| l.strip_suffix('"'))
            .collect();
        assert_eq!(names, ["T0", "T1", "T2", "T3", "T4"]);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Dragging a tab along the strip live-reorders it; the dropped tab becomes
/// active and the order is persisted.
#[gpui_kit::test]
async fn drag_tab_reorders(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("drag");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T0", "T1", "T2", "T3", "T4"]);

        // Drag T0 past T2 (to T3's slot): T0 lands at index 2 and becomes
        // active. The drop slot is the count of tab centers left of the
        // pointer, so dropping on T3's center inserts after T2.
        window.drag_to(0usize, 3usize, cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T1", "T2", "T0", "T3", "T4"]);
        assert_eq!(active_tab(window), 2, "dropped tab becomes active");

        // Drag it back to the first slot.
        window.drag_to(2usize, 0usize, cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["T0", "T1", "T2", "T3", "T4"]);
        assert_eq!(active_tab(window), 0);

        // The reordered strip is persisted.
        let manifest = std::fs::read_to_string(dir.join("tabs.toml")).unwrap();
        let names: Vec<&str> = manifest
            .lines()
            .filter_map(|l| l.trim().strip_prefix("name = \""))
            .filter_map(|l| l.strip_suffix('"'))
            .collect();
        assert_eq!(names, ["T0", "T1", "T2", "T3", "T4"]);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// With more tabs than fit, selecting a far tab must scroll the strip so the
/// active tab is on screen (the strip's scroll container holds only the tabs,
/// so the scroll index is the tab index).
#[gpui_kit::test]
async fn tab_nav_scrolls_active_into_view(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("scroll");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Open enough tabs that the strip overflows the 1100px window.
        for _ in 0..15 {
            window.click("tab-add", cx);
            window.render_frame(cx);
        }
        let active = active_tab(window);
        assert_eq!(active, 19, "last created tab is active");
        let bounds = window.try_find(active).unwrap().bounds();
        assert!(
            bounds.left() >= px(0.) && bounds.right() <= px(1100.),
            "active tab should be scrolled into view, got {bounds:?}"
        );

        // Walk all the way back to the first tab (left scrolling already works),
        // then step Forward and require every step to keep the active tab fully
        // on screen.
        while active_tab(window) > 0 {
            window.click("tab-nav-back", cx);
            window.render_frame(cx);
        }
        assert_eq!(active_tab(window), 0);
        loop {
            let a = active_tab(window);
            let b = window.try_find(a).unwrap().bounds();
            assert!(
                b.left() >= px(0.) && b.right() <= px(1100.),
                "forward step to tab {a} left it off screen: {b:?}"
            );
            if a + 1 >= 20 {
                break;
            }
            window.click("tab-nav-forward", cx);
            window.render_frame(cx);
            assert_eq!(active_tab(window), a + 1);
        }
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
