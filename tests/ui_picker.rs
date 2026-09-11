//! Headless drive of the real connection pick dialog: with many staged
//! connections, an unbound run must open a scrollable, filterable picker:
//! far rows scroll into view, typing narrows the list, Enter picks the
//! first match (dialog closes). Uses `SQLHIGHLAND_CONFIG_DIR` to stage an
//! isolated config; never touches a database (no run reaches connect).

#![recursion_limit = "256"]

use std::time::Duration;

use gpui_kit::component::Root;
use gpui_kit::test::{TestAppContextExt, TestWindowExt};
use gpui_kit::{
    div, px, size, AppContext, InteractiveElement, IntoElement, ParentElement,
    StatefulInteractiveElement, Styled, TestAppContext,
};
use sqlhighland::app::SqlHighlandView;

fn staged_config_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut toml = String::from("[[connections]]\n");
    // details irrelevant; only count + names matter here.
    for ix in 0..60 {
        toml.push_str(&format!(
            "id = \"c{ix}\"\nname = \"db-{ix:02}\"\nhost = \"h\"\nport = 1521\nservice_name = \"s\"\nuser = \"u\"\npassword = \"p\"\nenvironment = \"Dev\"\n[[connections]]\n"
        ));
    }
    // Trim the trailing empty table header the loop leaves behind.
    let toml = toml
        .trim_end_matches("[[connections]]\n")
        .trim_end()
        .to_string()
        + "\n";
    std::fs::write(dir.join("connections.toml"), toml).unwrap();
    dir
}

#[gpui_kit::test]
async fn pick_dialog_scrolls_tabs_and_picks(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("pick");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    // Fresh tab is unbound with editor focused: Cmd+Enter opens the picker.
    // Retry the keypress: headless windows can swallow the first frames'
    // input before focus settles.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for _ in 0..20 {
            window.press("cmd-enter", cx);
            window.render_frame(cx);
            if window.try_find("dialog").is_some() {
                break;
            }
        }
        assert!(
            window.try_find("dialog").is_some(),
            "picker dialog should open on run"
        );
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, _| {
        window.try_find("conn-pick-scroll").is_some() || window.try_find("pick-cancel").is_some()
    })
    .await;
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        for _ in 0..20 {
            window.press("cmd-enter", cx);
            window.render_frame(cx);
            if window.try_find("dialog").is_some() {
                break;
            }
        }
        assert!(
            window.try_find("dialog").is_some(),
            "picker dialog should open on run"
        );
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, _| {
        window.try_find(("conn-pick", 0usize)).is_some()
    })
    .await;
    // Far rows start out of the 400px-capped list: wheel over a visible row.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        let last = window.try_find(("conn-pick", 59usize));
        assert!(
            last.as_ref().is_none_or(|s| !s.visible()),
            "row 59 should start out of view"
        );
        window.scroll(
            ("conn-pick", 5usize),
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-3000.))),
            cx,
        );
        window.render_frame(cx);
        let first = window.try_find(("conn-pick", 0usize));
        assert!(
            first.as_ref().is_none_or(|s| !s.visible()),
            "row 0 should scroll out of view"
        );
        window.scroll(
            ("conn-pick", 55usize),
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-3000.))),
            cx,
        );
        window.render_frame(cx);
        let last = window.try_find(("conn-pick", 59usize));
        assert!(
            last.as_ref().is_some_and(|s| s.visible()),
            "row 59 should scroll into view"
        );
    })
    .unwrap();
    // …typing filters to the db-5* matches, Enter picks the first one
    // (dialog closes and the run proceeds).
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("db-5", cx);
        window.render_frame(cx);
        window.render_frame(cx);
        // 10 matches (db-50..db-59) at filtered indices 0..10; the first
        // sits at the rewound top.
        assert!(
            window
                .try_find(("conn-pick", 0usize))
                .as_ref()
                .is_some_and(|s| s.visible()),
            "first match should be visible at the rewound top"
        );
        assert!(
            window.try_find(("conn-pick", 10usize)).is_none(),
            "only 10 rows should match db-5"
        );
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(2), |window, _| {
        window.try_find("dialog").is_none()
    })
    .await;
    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui_kit::test]
async fn cmd_w_closes_active_query_tab(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("close-tab");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tab-add", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(("tab-close", 1usize))
                .as_ref()
                .is_some_and(|tab| tab.visible()),
            "new tab should be visible"
        );

        // The new tab is active, so Cmd+W removes it and leaves the original.
        window.press("cmd-w", cx);
        window.render_frame(cx);
        assert!(
            window.try_find(("tab-close", 1usize)).is_none(),
            "Cmd+W should close the active tab"
        );
        assert!(
            window
                .try_find(("tab-close", 0usize))
                .as_ref()
                .is_some_and(|tab| tab.visible()),
            "the original tab should remain"
        );

        // Closing the final tab preserves the app's existing one-blank-tab
        // invariant rather than leaving the editor without a tab.
        window.press("cmd-w", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(("tab-close", 0usize))
                .as_ref()
                .is_some_and(|tab| tab.visible()),
            "closing the final tab should create a replacement tab"
        );
        assert!(
            window.try_find(("tab-close", 1usize)).is_none(),
            "only one replacement tab should remain"
        );
    })
    .unwrap();

    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui_kit::test]
async fn cmd_t_opens_and_focuses_new_query_tab(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("new-tab");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });

    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.press("cmd-t", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(("tab-close", 1usize))
                .as_ref()
                .is_some_and(|tab| tab.visible()),
            "Cmd+T should open a second tab"
        );

        // Input reaches the newly created tab without clicking the editor.
        window.input("SELECT 1", cx);
        window.render_frame(cx);
        window.press("cmd-w", cx);
        window.render_frame(cx);
        assert!(
            window
                .try_find(("tab-close", 0usize))
                .as_ref()
                .is_some_and(|tab| tab.visible()),
            "the original tab should remain after closing the new tab"
        );
    })
    .unwrap();

    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}

struct BareList {
    scroll: gpui_kit::ScrollHandle,
}

impl gpui_kit::Render for BareList {
    fn render(
        &mut self,
        _window: &mut gpui_kit::Window,
        _cx: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::base::TestSupportExt as _;
        div()
            .id("bare-list")
            .test_support()
            .w_full()
            .h(px(400.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .flex()
            .flex_col()
            .children((0..60usize).map(|rix| {
                div()
                    .id(("bare-row", rix))
                    .test_support()
                    .h(px(20.))
                    .flex_shrink_0()
                    .child(format!("row {rix}"))
            }))
            .into_any_element()
    }
}

#[gpui_kit::test]
async fn bare_scrollable_list_wheels(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(800.), px(600.)), |_, _| BareList {
        scroll: gpui_kit::ScrollHandle::new(),
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.scroll(
            ("bare-row", 5usize),
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(px(0.), px(-200.))),
            cx,
        );
        window.render_frame(cx);
        assert!(
            window
                .try_find(("bare-row", 0usize))
                .as_ref()
                .is_none_or(|s| !s.visible()),
            "row 0 should scroll out of view"
        );
    })
    .unwrap();
}

#[gpui_kit::test]
async fn typing_in_editor_does_not_crash(cx: &mut TestAppContext) {
    // Regression: the completion provider aborted the app on first keystroke
    // (editor entity re-read while mutably leased). Staged config, no DB —
    // suggestions degrade to keywords; typing must simply not crash.
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("typing");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Editor holds focus at launch (view focuses the active tab).
        assert!(
            window.focused(cx).is_some(),
            "editor should hold focus so typing reaches the provider"
        );
        // Word chars (auto-trigger gate) + dot (column force).
        window.input("SEL", cx);
        window.render_frame(cx);
        window.input(".", cx);
        window.render_frame(cx);
        // Manual trigger path (present_completion_items) + accept the
        // first item (exercises insert_completion with explicit textEdit
        // ranges — a stale fallback range once replaced the whole buffer).
        window.press("ctrl-space", cx);
        window.render_frame(cx);
        window.press("enter", cx);
        window.render_frame(cx);
        // JOIN … ON with no cached FKs: must show no popup, never crash.
        window.input(" FROM e JOIN d ON ", cx);
        window.render_frame(cx);
        window.press("ctrl-space", cx);
        window.render_frame(cx);
        window.press("escape", cx);
        window.render_frame(cx);
    })
    .unwrap();
    cx.run_until_parked();
    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(&dir);
}
