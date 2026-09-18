//! Headless check that live SQL diagnostics reach the editor: typing an
//! unterminated quote / unbalanced paren (lexical, immediate) and a broken
//! statement (tree-sitter, debounced) marks the editor's diagnostic set.
//! Uses an isolated config dir; no database.

#![recursion_limit = "256"]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::Root;
use gpui_kit::test::{TestAppContextExt, TestWindowExt};
use gpui_kit::{px, size, AppContext, Entity, TestAppContext};
use sqlhighland::app::SqlHighlandView;

/// Stage an isolated config dir and return it.
fn staged_config_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-diag-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    dir
}

#[gpui_kit::test]
async fn lexical_issue_marks_editor(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("lex");
    let slot: Rc<RefCell<Option<Entity<SqlHighlandView>>>> = Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *slot2.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        // Unbalanced paren — caught lexically and pushed synchronously. The
        // editor auto-closes `(`, so delete the inserted closer first.
        window.input("SELECT (1 + 2", cx);
        window.press("delete", cx);
        window.render_frame(cx);
    })
    .unwrap();
    // The push runs in the editor's Change subscription (a deferred effect);
    // wait until it lands.
    let view = slot.borrow().clone().unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(2), move |_, cx| {
        view.read(cx).debug_active_diagnostic_count(cx) > 0
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

#[gpui_kit::test]
async fn structural_issue_marks_editor_after_debounce(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir("struct");
    let slot: Rc<RefCell<Option<Entity<SqlHighlandView>>>> = Rc::new(RefCell::new(None));
    let slot2 = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *slot2.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("SELECT * FROM", cx);
        window.render_frame(cx);
    })
    .unwrap();
    // The tree-sitter pass is debounced (~300ms); wait until it lands.
    let view = slot.borrow().clone().unwrap();
    cx.wait_for(handle.into(), Duration::from_secs(3), move |_, cx| {
        view.read(cx).debug_active_diagnostic_count(cx) > 0
    })
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}
