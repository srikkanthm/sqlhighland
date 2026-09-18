//! Structural-scope completion: columns of a CTE come from the query scope
//! (`src/sqlscope.rs`), not the dictionary — so they work on an unbound tab
//! with no metadata cache. Uses an isolated config dir; no database.

#![recursion_limit = "256"]

use gpui_kit::component::Root;
use gpui_kit::{px, size, AppContext, TestAppContext};
use sqlhighland::app::SqlHighlandView;

fn staged_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sqlhighland-compscope-{tag}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Present but empty manifest: no restore, one starter tab.
    std::fs::write(dir.join("tabs.toml"), "").unwrap();
    dir
}

/// `SQLHIGHLAND_CONFIG_DIR` is process-global and cargo runs these tests on
/// parallel threads, so serialize them (same rule as `tests/tab_nav.rs`).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[gpui_kit::test]
async fn cte_columns_complete_from_scope(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("cte");
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
    cx.update_window(handle.into(), |_, _window, cx| {
        let sql = "WITH recent AS (SELECT id, total FROM orders) SELECT id FROM recent";
        // Cursor right after the main `SELECT ` (select-list position).
        let offset = sql.rfind("SELECT").unwrap() + "SELECT ".len();
        view.update(cx, |this, _| {
            // Lexical fallback (no scope): an unbound tab has no cache, so no
            // CTE columns are offered.
            let before = this.debug_completion_labels(sql, offset);
            assert!(
                !before.iter().any(|l| l.eq_ignore_ascii_case("total")),
                "no scope should mean no CTE columns: {before:?}"
            );

            // With the structural scope, the CTE's projection columns appear.
            this.debug_set_scope_from_sql(sql);
            let after = this.debug_completion_labels(sql, offset);
            assert!(
                after.iter().any(|l| l.eq_ignore_ascii_case("id")),
                "CTE column `id` missing: {after:?}"
            );
            assert!(
                after.iter().any(|l| l.eq_ignore_ascii_case("total")),
                "CTE column `total` missing: {after:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// DML positions use the structural anchor: an INSERT column list offers only
/// the target table's columns (no clause keywords), and UPDATE SET offers the
/// columns plus expression functions.
#[gpui_kit::test]
async fn dml_positions_offer_target_columns(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("dml");
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
    cx.update_window(handle.into(), |_, _window, cx| {
        let insert = "INSERT INTO emp (empno, ename) VALUES (1, 'x')";
        let insert_at = insert.find("ename").unwrap();
        let update = "UPDATE emp SET sal = 1 WHERE empno = 2";
        let update_at = update.find("sal").unwrap();

        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_meta("c1", "SCOTT", "EMP", &["EMPNO", "ENAME", "SAL"]);

            this.debug_set_scope_from_sql(insert);
            let labels = this.debug_completion_labels(insert, insert_at);
            for want in ["EMPNO", "ENAME", "SAL"] {
                assert!(
                    labels.iter().any(|l| l.eq_ignore_ascii_case(want)),
                    "INSERT list missing {want}: {labels:?}"
                );
            }
            // Target-only: no generic clause keywords in a column list.
            assert!(
                !labels.iter().any(|l| l.eq_ignore_ascii_case("from")),
                "INSERT list should not offer FROM: {labels:?}"
            );

            this.debug_set_scope_from_sql(update);
            let labels = this.debug_completion_labels(update, update_at);
            assert!(
                labels.iter().any(|l| l.eq_ignore_ascii_case("sal")),
                "UPDATE SET missing SAL: {labels:?}"
            );
            assert!(
                !labels.iter().any(|l| l.eq_ignore_ascii_case("from")),
                "UPDATE SET should not offer FROM: {labels:?}"
            );

            // The SET anchor stops at WHERE: the predicate keeps its keywords,
            // and a trailing space after `SET ` still offers the target's
            // columns (via the generic predicate path).
            let where_at = update.find("empno").unwrap();
            let labels = this.debug_completion_labels(update, where_at);
            assert!(
                labels.iter().any(|l| l.eq_ignore_ascii_case("and")),
                "UPDATE WHERE should offer predicate keywords: {labels:?}"
            );

            let set_trailing = "UPDATE emp SET ";
            this.debug_set_scope_from_sql(set_trailing);
            let labels = this.debug_completion_labels(set_trailing, set_trailing.len());
            assert!(
                labels.iter().any(|l| l.eq_ignore_ascii_case("sal")),
                "UPDATE SET with trailing space missing SAL: {labels:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reported bug: with the cursor after `WHERE ` (trailing space, past the
/// parsed span), columns must still be offered — not keyword noise.
#[gpui_kit::test]
async fn where_with_trailing_space_offers_columns(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("where-trailing");
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
    cx.update_window(handle.into(), |_, _window, cx| {
        // Cursor at end of text: after `WHERE ` (trailing space).
        let sql = "SELECT * FROM EMP WHERE ";
        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_meta("c1", "SCOTT", "EMP", &["EMPNO", "ENAME", "SAL"]);
            this.debug_set_scope_from_sql(sql);

            let labels = this.debug_completion_labels(sql, sql.len());
            for want in ["EMPNO", "ENAME", "SAL"] {
                assert!(
                    labels.iter().any(|l| l.eq_ignore_ascii_case(want)),
                    "after `WHERE ` missing column {want}: {labels:?}"
                );
            }
            // Columns rank first (kind order), so the head of the list is a
            // column, not a keyword.
            assert!(
                labels.first().is_some_and(|l| ["EMPNO", "ENAME", "SAL"]
                    .iter()
                    .any(|c| l.eq_ignore_ascii_case(c))),
                "first suggestion should be a column: {labels:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Projection aliases are offered in `ORDER BY`, which is a structural fact
/// the lexical scanner cannot see.
#[gpui_kit::test]
async fn order_by_offers_projection_alias(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("order-alias");
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
    cx.update_window(handle.into(), |_, _window, cx| {
        let sql = "SELECT ename AS n FROM emp ORDER BY n";
        let at = sql.rfind('n').unwrap();
        view.update(cx, |this, _| {
            this.debug_set_scope_from_sql(sql);
            let labels = this.debug_completion_labels(sql, at);
            assert!(
                labels.iter().any(|l| l == "n"),
                "ORDER BY should offer the projection alias `n`: {labels:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// After a table reference the popup is the set of valid clause
/// continuations — not statement starters, DDL, or functions.
#[gpui_kit::test]
async fn from_tail_offers_only_continuations(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("from-tail");
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
    cx.update_window(handle.into(), |_, _window, cx| {
        let sql = "SELECT * FROM EMP ";
        view.update(cx, |this, _| {
            let labels = this.debug_completion_labels(sql, sql.len());
            for want in [
                "WHERE",
                "JOIN",
                "ORDER BY",
                "GROUP BY",
                "LEFT JOIN",
                "LEFT OUTER JOIN",
                "NATURAL FULL JOIN",
                "CONNECT BY",
                "START WITH",
                "UNION ALL",
            ] {
                assert!(
                    labels.iter().any(|l| l.eq_ignore_ascii_case(want)),
                    "after a table, missing continuation {want}: {labels:?}"
                );
            }
            for forbidden in [
                "SELECT",
                "INSERT",
                "INSERT INTO",
                "CREATE",
                "DROP",
                "DELETE",
                "DELETE FROM",
            ] {
                assert!(
                    !labels.iter().any(|l| l.eq_ignore_ascii_case(forbidden)),
                    "after a table, {forbidden} should not be offered: {labels:?}"
                );
            }
            // Contracted phrases, never the fragments they are made of.
            for fragment in ["ORDER", "GROUP", "LEFT", "RIGHT", "INNER", "NATURAL", "BY"] {
                assert!(
                    !labels.iter().any(|l| l.eq_ignore_ascii_case(fragment)),
                    "after a table, bare {fragment} should not be offered: {labels:?}"
                );
            }
            assert!(
                !labels.iter().any(|l| l.ends_with("()")),
                "no functions should be offered after a table: {labels:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
