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

/// Index of the selected tab, or panics if none is marked selected.
fn active_tab(window: &Window) -> usize {
    (0..64usize)
        .find(|ix| window.try_find(*ix).and_then(|s| s.selected()) == Some(true))
        .expect("exactly one tab should be selected")
}

#[gpui_kit::test]
async fn tab_nav_moves_adjacent(cx: &mut TestAppContext) {
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

/// With more tabs than fit, selecting a far tab must scroll the strip so the
/// active tab is on screen (the kit's Underline bar renders its indicator as
/// the scroll container's first child, so the app's child index is offset).
#[gpui_kit::test]
async fn tab_nav_scrolls_active_into_view(cx: &mut TestAppContext) {
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
