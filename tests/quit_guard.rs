//! Headless checks of the quit / window-close guard. The native close (red
//! traffic light) is routed through the same guard as Cmd+Q: uncommitted
//! transactions are settled first, then unsaved external SQL files are
//! prompted for, while in-memory tabs (auto-saved drafts) are ignored.
//! Uses an isolated config dir and a fake session; no database.

#![recursion_limit = "256"]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::Root;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{px, size, AnyWindowHandle, AppContext, Entity, TestAppContext, VisualTestContext};
use sqlhighland::app::SqlHighlandView;
use sqlhighland::db::{BindParam, DbClient, DbError, SharedSession};
use sqlhighland::model::ConnectionConfig;

/// A session whose commit/rollback always succeed, so the guard's async
/// settle path can run headlessly.
struct OkDb;

impl DbClient for OkDb {
    fn connect(&mut self, _: &ConnectionConfig) -> Result<(), DbError> {
        Ok(())
    }
    fn is_connected(&self) -> bool {
        true
    }
    fn disconnect(&mut self) {}
    fn exec(&mut self, _: &str, _: &[BindParam]) -> Result<(u64, u128), DbError> {
        Ok((0, 0))
    }
    fn commit(&mut self) -> Result<(), DbError> {
        Ok(())
    }
    fn rollback(&mut self) -> Result<(), DbError> {
        Ok(())
    }
}

fn staged_config_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-quit-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::env::set_var("SQLHIGHLAND_CONFIG_DIR", &dir);
    dir
}

fn setup(
    cx: &mut TestAppContext,
    tag: &str,
) -> (AnyWindowHandle, Entity<SqlHighlandView>, std::path::PathBuf) {
    cx.update(gpui_kit::init);
    let dir = staged_config_dir(tag);
    let slot: Rc<RefCell<Option<Entity<SqlHighlandView>>>> = Rc::new(RefCell::new(None));
    let slot_for_window = slot.clone();
    let handle = cx.open_window(size(px(1100.), px(780.)), move |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        *slot_for_window.borrow_mut() = Some(view.clone());
        Root::new(view, window, cx)
    });
    let view = slot.borrow().clone().expect("view captured at window open");
    (handle.into(), view, dir)
}

fn cleanup(dir: &std::path::Path) {
    std::env::remove_var("SQLHIGHLAND_CONFIG_DIR");
    let _ = std::fs::remove_dir_all(dir);
}

#[gpui_kit::test]
async fn clean_window_close_is_allowed(cx: &mut TestAppContext) {
    let (handle, _view, dir) = setup(cx, "clean");
    let mut vtc = VisualTestContext::from_window(handle, cx);
    assert!(
        vtc.simulate_close(),
        "a clean window should be allowed to close"
    );
    cleanup(&dir);
}

/// In-memory tabs are auto-saved as drafts, so they must NOT guard the quit.
#[gpui_kit::test]
async fn in_memory_dirty_tab_does_not_guard(cx: &mut TestAppContext) {
    let (handle, view, dir) = setup(cx, "memory");
    cx.update(|cx| {
        view.update(cx, |this, _| this.debug_mark_memory_dirty());
        assert!(
            !view.read(cx).debug_quit_needs_guard(),
            "in-memory tabs must not count as a guard"
        );
    });
    let mut vtc = VisualTestContext::from_window(handle, cx);
    assert!(
        vtc.simulate_close(),
        "in-memory dirty tabs should not veto the close"
    );
    cleanup(&dir);
}

/// A dirty external SQL file must veto the close and show the save prompt.
#[gpui_kit::test]
async fn dirty_external_file_guards_close(cx: &mut TestAppContext) {
    let (handle, view, dir) = setup(cx, "file");
    let path = dir.join("query.sql");
    cx.update(|cx| view.update(cx, |this, _| this.debug_mark_file_dirty(path)));
    let mut vtc = VisualTestContext::from_window(handle, cx);
    assert!(
        !vtc.simulate_close(),
        "a dirty external file should veto the close"
    );
    vtc.update(|window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("quit-save").is_some(),
            "the unsaved-files prompt should be showing"
        );
    });
    cleanup(&dir);
}

/// An uncommitted transaction must veto the close and show the transaction
/// prompt; its buttons no longer claim "and Quit" because the file stage
/// follows.
#[gpui_kit::test]
async fn pending_txn_guards_close(cx: &mut TestAppContext) {
    let (handle, view, dir) = setup(cx, "txn");
    cx.update(|cx| view.update(cx, |this, _| this.debug_mark_pending_txn("c1")));
    let mut vtc = VisualTestContext::from_window(handle, cx);
    assert!(
        !vtc.simulate_close(),
        "an uncommitted transaction should veto the close"
    );
    vtc.update(|window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("quit-txn-commit").is_some(),
            "the transaction prompt should be showing"
        );
        assert!(
            window.try_find("quit-save").is_none(),
            "the files prompt must wait for the transaction"
        );
    });
    cleanup(&dir);
}

/// Transactions are settled before files: with both unresolved, the
/// transaction prompt shows first and the files prompt is withheld.
#[gpui_kit::test]
async fn transactions_are_guarded_before_files(cx: &mut TestAppContext) {
    let (handle, view, dir) = setup(cx, "both");
    let path = dir.join("query.sql");
    cx.update(|cx| {
        view.update(cx, |this, _| {
            this.debug_mark_pending_txn("c1");
            this.debug_mark_file_dirty(path);
        })
    });
    let mut vtc = VisualTestContext::from_window(handle, cx);
    assert!(!vtc.simulate_close());
    vtc.update(|window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("quit-txn-commit").is_some(),
            "transactions come first"
        );
        assert!(window.try_find("quit-save").is_none(), "files come second");
    });
    cleanup(&dir);
}

/// Regression: the debounced flush must not auto-write an external file. An
/// edit leaves the tab dirty so close/quit can guard it.
#[gpui_kit::test]
async fn external_file_edit_is_not_auto_saved(cx: &mut TestAppContext) {
    let (_handle, view, dir) = setup(cx, "autosave-file");
    let path = dir.join("query.sql");
    std::fs::write(&path, "SELECT 1 FROM dual;").unwrap();

    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.debug_mark_file_dirty(path.clone());
            this.debug_schedule_draft_save(cx);
        });
    });
    // Give the debounce budget time to elapse (the in-memory test below
    // proves this drives the flush), so a regression would write the file.
    cx.background_executor.advance_clock(Duration::from_secs(3));
    cx.background_executor.run_until_parked();

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "SELECT 1 FROM dual;",
        "an external file must not be overwritten by the draft flush"
    );
    let dirty = cx.update(|cx| view.read(cx).debug_active_dirty());
    assert!(
        dirty,
        "an external edit must stay dirty for the close/quit guard"
    );
    cleanup(&dir);
}

/// In-memory tabs still auto-save a recovery draft (and clear dirty).
#[gpui_kit::test]
async fn in_memory_edit_is_auto_saved(cx: &mut TestAppContext) {
    let (_handle, view, dir) = setup(cx, "autosave-draft");
    cx.update(|cx| {
        view.update(cx, |this, cx| {
            this.debug_mark_memory_dirty();
            this.debug_schedule_draft_save(cx);
        });
    });
    cx.background_executor.advance_clock(Duration::from_secs(3));
    cx.background_executor.run_until_parked();
    let dirty = cx.update(|cx| view.read(cx).debug_active_dirty());
    assert!(!dirty, "an in-memory edit should be auto-saved as a draft");
    cleanup(&dir);
}

/// End to end: committing an uncommitted transaction lets the quit continue
/// to the files prompt, and saving there writes the file.
#[gpui_kit::test]
async fn settled_transaction_then_files_prompt(cx: &mut TestAppContext) {
    let (handle, view, dir) = setup(cx, "sequence");
    let path = dir.join("query.sql");
    let session: SharedSession = Arc::new(Mutex::new(Box::new(OkDb)));
    cx.update(|cx| {
        view.update(cx, |this, _| {
            this.debug_insert_session("c1", session);
            this.debug_mark_pending_txn("c1");
            this.debug_mark_file_dirty(path.clone());
        })
    });
    let mut vtc = VisualTestContext::from_window(handle, cx);
    assert!(!vtc.simulate_close());
    vtc.update(|window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("quit-txn-commit").is_some());
        window.click("quit-txn-commit", cx);
    });
    // Let the background commit settle and the files stage open.
    vtc.run_until_parked();
    vtc.update(|window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("quit-save").is_some(),
            "the files prompt should follow a settled transaction"
        );
        window.click("quit-save", cx);
    });
    vtc.run_until_parked();
    assert!(
        path.exists(),
        "Save and Quit should write the external file"
    );
    cleanup(&dir);
}
