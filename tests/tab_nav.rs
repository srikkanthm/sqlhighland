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
use std::time::Duration;

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

fn staged_named_tabs_dir(tag: &str, tabs: &[(&str, &str, &str)]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-tabnav-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tabs_dir = dir.join("tabs");
    std::fs::create_dir_all(&tabs_dir).unwrap();
    let mut toml = String::new();
    for (id, name, text) in tabs {
        toml.push_str(&format!("[[tabs]]\nid = \"{id}\"\nname = \"{name}\"\n\n"));
        std::fs::write(tabs_dir.join(format!("{id}.sql")), text).unwrap();
    }
    std::fs::write(dir.join("tabs.toml"), toml).unwrap();
    dir
}

/// Config dir with a manifest whose saved active tab is `active`.
fn staged_active_tabs_dir(
    tag: &str,
    tabs: &[(&str, &str, &str)],
    active: Option<&str>,
) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-tabnav-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let tabs_dir = dir.join("tabs");
    std::fs::create_dir_all(&tabs_dir).unwrap();
    let mut toml = String::new();
    if let Some(id) = active {
        toml.push_str(&format!("active_tab = \"{id}\"\n"));
    }
    for (id, name, text) in tabs {
        toml.push_str(&format!("[[tabs]]\nid = \"{id}\"\nname = \"{name}\"\n\n"));
        std::fs::write(tabs_dir.join(format!("{id}.sql")), text).unwrap();
    }
    std::fs::write(dir.join("tabs.toml"), toml).unwrap();
    dir
}

/// Config dir with no manifest (first launch).
fn staged_empty_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-tabnav-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Config dir with a lone draft and no manifest (recovery path).
fn staged_orphan_dir(tag: &str, id: &str, text: &str) -> std::path::PathBuf {
    let dir = staged_empty_dir(tag);
    let tabs_dir = dir.join("tabs");
    std::fs::create_dir_all(&tabs_dir).unwrap();
    std::fs::write(tabs_dir.join(format!("{id}.sql")), text).unwrap();
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

/// Labels of the tabs in display order (indices are dense from zero). The
/// unsaved marker is a dot, surfaced in the accessible label as " (unsaved)";
/// strip it so callers compare plain names.
fn tab_labels(window: &Window) -> Vec<String> {
    (0..64usize)
        .map_while(|ix| {
            window.try_find(ix).and_then(|s| {
                s.label().map(|label| {
                    label
                        .strip_suffix(" (unsaved)")
                        .unwrap_or(label)
                        .to_string()
                })
            })
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

/// In-memory tabs (no external file) always carry the unsaved marker — a dot
/// plus "(unsaved)" to accessibility — while a clean external file does not,
/// and an edited external file does.
#[gpui_kit::test]
async fn dirty_tab_shows_dot(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("dirty");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let slot: std::rc::Rc<std::cell::RefCell<Option<gpui_kit::Entity<SqlHighlandView>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let for_window = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = slot.borrow().clone().expect("view captured at window open");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window)[0], "T0", "visible name stays plain");
        assert!(
            window.try_find(("tab-dirty", 0usize)).is_some(),
            "an in-memory tab is always unsaved"
        );
        assert_eq!(
            window.try_find(0usize).unwrap().label(),
            Some("T0 (unsaved)"),
            "accessibility carries the unsaved marker"
        );

        // A clean external file is not marked; editing it marks it dirty.
        view.update(cx, |this, _| {
            this.debug_mark_external_clean(dir.join("q.sql"))
        });
        window.render_frame(cx);
        assert!(
            window.try_find(("tab-dirty", 0usize)).is_none(),
            "a clean external tab has no dirty dot"
        );
        assert_eq!(
            window.try_find(0usize).unwrap().label(),
            Some("T0"),
            "a clean external tab reports no unsaved marker"
        );

        view.update(cx, |this, _| this.debug_mark_file_dirty(dir.join("q.sql")));
        window.render_frame(cx);
        assert!(
            window.try_find(("tab-dirty", 0usize)).is_some(),
            "a dirty external tab shows the dot"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Relaunching with an intact manifest must keep saved tab names byte-identical.
/// Drafts whose first lines look like titles must not rename `Untitled` tabs,
/// and no extra adopted tabs may appear on the next launch.
#[gpui_kit::test]
async fn restore_keeps_saved_tab_names(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_named_tabs_dir(
        "restore-names",
        &[
            ("u7", "Untitled 7", "SELECT alpha FROM dual;\n"),
            ("u8", "Untitled 8", "select beta from dual;\n"),
        ],
    );
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let first = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(first.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["Untitled 7", "Untitled 8"]);
        assert_eq!(active_tab(window), 0);
    })
    .unwrap();

    // A subsequent launch over the same config dir must restore the same tabs.
    let second = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(second.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["Untitled 7", "Untitled 8"]);
        assert_eq!(tab_labels(window).len(), 2, "no extra adopted tabs");
        assert_eq!(active_tab(window), 0);
    })
    .unwrap();

    let manifest = std::fs::read_to_string(dir.join("tabs.toml")).unwrap();
    let names: Vec<&str> = manifest
        .lines()
        .filter_map(|line| line.trim().strip_prefix("name = \""))
        .filter_map(|line| line.strip_suffix('"'))
        .collect();
    assert_eq!(names, ["Untitled 7", "Untitled 8"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// First launch (no manifest) starts with a single blank `Untitled 1` tab —
/// no sample SQL, and the name is never derived from content.
#[gpui_kit::test]
async fn first_launch_starts_blank(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_empty_dir("first");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let slot: std::rc::Rc<std::cell::RefCell<Option<gpui_kit::Entity<SqlHighlandView>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let for_window = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = slot.borrow().clone().expect("view captured at window open");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["Untitled 1"]);
        let editor = view.read(cx).debug_active_editor();
        assert_eq!(
            editor.read(cx).value().to_string(),
            "",
            "the starter tab is blank"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Closing the last tab spawns a fresh replacement — but it is numbered from
/// the open set, so it is always `Untitled 1`, never a climbing counter.
#[gpui_kit::test]
async fn close_last_tab_names_untitled_1(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("close-last");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window).len(), 5);
        // Close the first tab each time; the fifth close empties the strip and
        // spawns the replacement, the sixth closes that replacement too.
        for _ in 0..6 {
            window.click(("tab-close", 0usize), cx);
            window.render_frame(cx);
        }
        assert_eq!(tab_labels(window), ["Untitled 1"]);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A new tab is numbered one past the highest `Untitled N` currently open.
#[gpui_kit::test]
async fn new_tab_after_untitled_ones_picks_next(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_named_tabs_dir(
        "next-name",
        &[
            ("a", "Untitled 1", "SELECT 1;"),
            ("b", "Untitled 2", "SELECT 2;"),
        ],
    );
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tab-add", cx);
        window.render_frame(cx);
        assert_eq!(
            tab_labels(window),
            ["Untitled 1", "Untitled 2", "Untitled 3"]
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The manifest's saved `active_tab` decides which tab is selected on launch.
#[gpui_kit::test]
async fn restore_selects_saved_active_tab(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_active_tabs_dir(
        "active",
        &[("t0", "T0", ""), ("t1", "T1", ""), ("t2", "T2", "")],
        Some("t2"),
    );
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(active_tab(window), 2, "the saved active tab is selected");
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Selecting a tab is persisted immediately, so a relaunch reopens on it.
#[gpui_kit::test]
async fn active_tab_survives_relaunch(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("active-roundtrip");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let first = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(first.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("tab-nav-forward", cx);
        window.click("tab-nav-forward", cx);
        window.render_frame(cx);
        assert_eq!(active_tab(window), 2);
    })
    .unwrap();

    let second = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(second.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            active_tab(window),
            2,
            "relaunch reopens on the last active tab"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Closing an in-memory tab with text prompts to Save As or Discard; Cancel
/// keeps it, Discard closes it.
#[gpui_kit::test]
async fn closing_in_memory_tab_prompts(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_named_tabs_dir("close-memory", &[("u1", "Untitled 1", "SELECT 1;")]);
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("tab-close", 0usize), cx);
        window.render_frame(cx);
        assert!(
            window.try_find("close-memory-cancel").is_some()
                && window.try_find("close-memory-discard").is_some()
                && window.try_find("close-memory-save-as").is_some(),
            "the in-memory close prompt offers Cancel / Discard / Save As"
        );

        // Cancel keeps the tab (and its text).
        window.click("close-memory-cancel", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["Untitled 1"]);

        // Discard closes it; the empty strip spawns the `Untitled 1` replacement.
        window.click(("tab-close", 0usize), cx);
        window.render_frame(cx);
        window.click("close-memory-discard", cx);
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["Untitled 1"]);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// An empty in-memory tab closes without a prompt, so repeated Cmd+W on the
/// blank replacement does not nag.
#[gpui_kit::test]
async fn empty_in_memory_tab_closes_silently(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("close-empty");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(("tab-close", 0usize), cx);
        window.render_frame(cx);
        assert!(
            window.try_find("close-memory-discard").is_none(),
            "a blank in-memory tab closes without a prompt"
        );
        assert_eq!(tab_labels(window).len(), 4);
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A draft with no manifest entry is adopted, named neutrally (`Untitled N`,
/// never from its content), with its text restored.
#[gpui_kit::test]
async fn orphan_draft_adopts_with_neutral_name(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_orphan_dir("orphan", "u1", "SELECT 1 FROM dual;");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let slot: std::rc::Rc<std::cell::RefCell<Option<gpui_kit::Entity<SqlHighlandView>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let for_window = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = slot.borrow().clone().expect("view captured at window open");
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(tab_labels(window), ["Untitled 1"], "neutral, not content");
        let editor = view.read(cx).debug_active_editor();
        assert_eq!(
            editor.read(cx).value().to_string(),
            "SELECT 1 FROM dual;",
            "draft text is restored"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Empty orphan drafts (stray blank tabs) are dropped, not adopted; only drafts
/// with text recover. This is what kept blank tabs from piling up.
#[gpui_kit::test]
async fn empty_orphan_drafts_are_not_adopted(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_empty_dir("empty-orphan");
    let tabs_dir = dir.join("tabs");
    std::fs::create_dir_all(&tabs_dir).unwrap();
    std::fs::write(tabs_dir.join("blank1.sql"), "").unwrap();
    std::fs::write(tabs_dir.join("blank2.sql"), "   \n").unwrap();
    std::fs::write(tabs_dir.join("real.sql"), "SELECT 1 FROM dual;").unwrap();
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            tab_labels(window),
            ["Untitled 1"],
            "only the draft with text is adopted"
        );
        assert!(
            !tabs_dir.join("blank1.sql").exists() && !tabs_dir.join("blank2.sql").exists(),
            "empty orphan drafts are removed"
        );
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A closed tab's debounced draft flush must never (re)create its draft: a late
/// write would orphan the draft and adoption would resurrect it as a rogue tab.
#[gpui_kit::test]
async fn closed_tab_leaves_no_orphan_draft(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_tabs_dir("draft-orphan");
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    let draft = dir.join("tabs").join("t0.sql");
    let slot: std::rc::Rc<std::cell::RefCell<Option<gpui_kit::Entity<SqlHighlandView>>>> =
        std::rc::Rc::new(std::cell::RefCell::new(None));
    let for_window = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = slot.borrow().clone().expect("view captured at window open");

    // Schedule a flush, then let the debounce elapse: the draft appears.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        view.update(cx, |this, cx| this.debug_schedule_draft_save(cx));
    })
    .unwrap();
    cx.executor().advance_clock(Duration::from_millis(1600));
    cx.run_until_parked();
    assert!(draft.exists(), "the debounced flush wrote the draft");

    // Close the (blank) tab, then let more time pass: the draft stays gone.
    cx.update_window(handle.into(), |_, window, cx| {
        window.click(("tab-close", 0usize), cx);
        window.render_frame(cx);
    })
    .unwrap();
    assert!(!draft.exists(), "closing removes the draft");
    cx.executor().advance_clock(Duration::from_millis(1600));
    cx.run_until_parked();
    assert!(!draft.exists(), "a closed tab never recreates its draft");
    let _ = std::fs::remove_dir_all(&dir);
}
