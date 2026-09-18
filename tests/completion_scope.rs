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
/// CASE expressions in the select list: `WHEN` after `CASE`, `THEN` after a
/// `WHEN` condition, the case keywords after a result, and the enclosing
/// select list after `END`.
#[gpui_kit::test]
async fn case_expression_completion(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("case");
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
        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_meta("c1", "SCOTT", "EMP", &["EMPNO", "ENAME", "SAL"]);
            let labels = |this: &mut SqlHighlandView, sql: &str| {
                this.debug_completion_labels(sql, sql.len())
            };
            let has = |l: &[String], want: &str| l.iter().any(|x| x.eq_ignore_ascii_case(want));

            let start = "SELECT CASE ";
            let l = labels(this, start);
            assert!(has(&l, "WHEN"), "CASE start missing WHEN: {l:?}");

            // A CASE in WHERE has EMP in scope, so the WHEN condition offers
            // its columns (and THEN), but no clause transitions.
            let cond = "SELECT * FROM EMP WHERE CASE WHEN ";
            let l = labels(this, cond);
            assert!(has(&l, "THEN"), "WHEN condition missing THEN: {l:?}");
            assert!(has(&l, "ENAME"), "WHEN condition missing column: {l:?}");
            assert!(
                !has(&l, "ORDER BY"),
                "WHEN condition should not offer clause transitions: {l:?}"
            );

            let res = "SELECT * FROM EMP WHERE CASE WHEN ENAME = 'x' THEN ";
            let l = labels(this, res);
            for want in ["END", "WHEN", "ELSE"] {
                assert!(has(&l, want), "THEN result missing {want}: {l:?}");
            }
            assert!(!has(&l, "FROM"), "THEN result should not offer FROM: {l:?}");

            // After the CASE's END in WHERE, the predicate resumes.
            let where_end = "SELECT * FROM EMP WHERE CASE WHEN ENAME = 'x' THEN 'a' END ";
            let l = labels(this, where_end);
            assert!(has(&l, "AND"), "after END in WHERE missing AND: {l:?}");
            assert!(
                !has(&l, "FROM"),
                "after END in WHERE should be a predicate, not a select list: {l:?}"
            );

            // In the select list, END returns to the select-list tail.
            let select_end = "SELECT CASE WHEN ENAME = 'x' THEN 'a' END ";
            let l = labels(this, select_end);
            assert!(has(&l, "FROM"), "after END missing FROM: {l:?}");
            assert!(
                !has(&l, "AND"),
                "after END should be the select list, not a predicate: {l:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
/// Subquery starts, CAST types, analytic windows, and USING columns.
#[gpui_kit::test]
async fn structural_expression_completion(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("structural");
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
        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_meta("c1", "SCOTT", "A", &["IDA", "NAME"]);
            this.debug_insert_meta("c1", "SCOTT", "B", &["IDB", "NAME"]);
            let labels = |this: &mut SqlHighlandView, sql: &str| {
                this.debug_completion_labels(sql, sql.len())
            };
            let has = |l: &[String], want: &str| l.iter().any(|x| x.eq_ignore_ascii_case(want));

            let sub = "SELECT * FROM (";
            let l = labels(this, sub);
            assert!(has(&l, "SELECT"), "subquery start missing SELECT: {l:?}");
            assert!(
                !has(&l, "INSERT INTO"),
                "subquery start should be query-only: {l:?}"
            );

            let union = "SELECT 1 UNION ALL ";
            let l = labels(this, union);
            assert!(has(&l, "SELECT"), "after UNION missing SELECT: {l:?}");

            let cast = "SELECT CAST(x AS ";
            let l = labels(this, cast);
            assert!(has(&l, "NUMBER"), "CAST missing types: {l:?}");
            assert!(has(&l, "VARCHAR2"), "CAST missing types: {l:?}");

            let win = "SELECT ROW_NUMBER() OVER (";
            let l = labels(this, win);
            assert!(has(&l, "PARTITION BY"), "OVER missing PARTITION BY: {l:?}");

            let using = "SELECT * FROM A JOIN B USING (";
            let l = labels(this, using);
            assert!(has(&l, "NAME"), "USING missing common column NAME: {l:?}");
            assert!(
                !has(&l, "IDA"),
                "USING should not offer non-common columns: {l:?}"
            );
            assert!(
                !has(&l, "IDB"),
                "USING should not offer non-common columns: {l:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
/// JOIN … ON keeps offering FK-derived conditions after the first one is
/// typed (the `AND` tail), and does not re-suggest what is already written.
#[gpui_kit::test]
async fn join_on_conditions_after_and(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("join-on");
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
        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_meta("c1", "SCOTT", "EMP", &["EMPNO", "ENAME", "DEPTNO"]);
            this.debug_insert_meta("c1", "SCOTT", "DEPT", &["DEPTNO", "DNAME"]);
            this.debug_insert_fk("c1", "EMP", "DEPTNO", "DEPT", "DEPTNO");
            let labels = |this: &mut SqlHighlandView, sql: &str| {
                this.debug_completion_labels(sql, sql.len())
            };
            let has = |l: &[String], want: &str| l.iter().any(|x| x.eq_ignore_ascii_case(want));

            let fresh = "SELECT * FROM EMP e JOIN DEPT d ON ";
            let l = labels(this, fresh);
            assert!(
                has(&l, "e.DEPTNO = d.DEPTNO"),
                "fresh ON missing FK condition: {l:?}"
            );

            // After a hand-written first condition, the FK link still shows.
            let and_tail = "SELECT * FROM EMP e JOIN DEPT d ON e.ENAME = d.ENAME AND ";
            let l = labels(this, and_tail);
            assert!(
                has(&l, "e.DEPTNO = d.DEPTNO"),
                "AND tail missing remaining FK condition: {l:?}"
            );

            // Once typed, it is not re-suggested (falls back to the predicate).
            let used = "SELECT * FROM EMP e JOIN DEPT d ON e.DEPTNO = d.DEPTNO AND ";
            let l = labels(this, used);
            assert!(
                !has(&l, "e.DEPTNO = d.DEPTNO"),
                "typed condition should not be re-suggested: {l:?}"
            );
            assert!(
                has(&l, "AND"),
                "after a typed condition the predicate resumes: {l:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
/// A synonym is suggested after FROM and resolves to its table's columns.
#[gpui_kit::test]
async fn synonym_completion_and_columns(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("synonym");
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
        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_meta("c1", "SCOTT", "EMP", &["EMPNO", "ENAME", "DEPTNO"]);
            this.debug_insert_synonym("c1", "SCOTT", "EMP_SYN", "SCOTT", "EMP");
            let labels = |this: &mut SqlHighlandView, sql: &str| {
                this.debug_completion_labels(sql, sql.len())
            };
            let has = |l: &[String], want: &str| l.iter().any(|x| x.eq_ignore_ascii_case(want));

            let from = "SELECT * FROM EMP_S";
            let l = labels(this, from);
            assert!(
                l.iter().any(|x| x.to_ascii_uppercase().contains("EMP_SYN")),
                "synonym not suggested after FROM: {l:?}"
            );

            let where_clause = "SELECT * FROM EMP_SYN WHERE ";
            let l = labels(this, where_clause);
            assert!(
                has(&l, "ENAME"),
                "synonym did not resolve to its table's columns: {l:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
/// `pkg.` completes the package's members as calls.
#[gpui_kit::test]
async fn package_member_completion(cx: &mut TestAppContext) {
    let _guard = env_guard();
    cx.update(gpui_kit::init);
    let dir = staged_dir("package");
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
        view.update(cx, |this, _| {
            this.debug_set_connection("c1");
            this.debug_insert_package_member("c1", "SCOTT", "UTIL", "ADD_ONE");
            this.debug_insert_package_member("c1", "SCOTT", "UTIL", "TO_UPPER");
            let sql = "SELECT UTIL.";
            let labels = this.debug_completion_labels(sql, sql.len());
            assert!(
                labels.iter().any(|l| l.eq_ignore_ascii_case("ADD_ONE()")),
                "package member missing: {labels:?}"
            );
            assert!(
                labels.iter().any(|l| l.eq_ignore_ascii_case("TO_UPPER()")),
                "package member missing: {labels:?}"
            );
        });
    })
    .unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
