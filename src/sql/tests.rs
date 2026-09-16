//! Unit tests for statement splitting, classification, and scripts.

use super::*;

#[test]
fn formats_basic_select() {
    let out = format_sql("select a, b from t where a = 1;");
    assert!(out.contains("SELECT"), "keywords uppercased:\n{out}");
    assert!(out.contains('\n'), "multiline:\n{out}");
}

#[test]
fn empty_input_stays_empty() {
    assert_eq!(format_sql("").trim(), "");
}

#[test]
fn splits_on_top_level_semicolons() {
    let stmts = split_statements("SELECT 1;\nSELECT 2;");
    assert_eq!(stmts.len(), 2);
    assert_eq!(stmts[0].text, "SELECT 1;");
    assert_eq!(stmts[1].text, "SELECT 2;");
}

#[test]
fn ignores_semicolons_in_strings_and_comments() {
    let sql = "SELECT ';' AS a, \"b;c\" FROM t; -- trailing;\n/* block; */\nSELECT 2;";
    let stmts = split_statements(sql);
    assert_eq!(stmts.len(), 2, "{stmts:?}");
    assert!(stmts[0].text.starts_with("SELECT ';'"));
    // Trailing comments attach to the following statement; crucially the
    // `;` inside strings/comments did not split further.
    assert!(stmts[1].text.ends_with("SELECT 2;"), "{stmts:?}");
}

#[test]
fn handles_escaped_quotes() {
    let stmts = split_statements("SELECT 'it''s; fine';\nSELECT 2;");
    assert_eq!(stmts.len(), 2, "{stmts:?}");
}

#[test]
fn slash_line_terminates_like_sql_developer() {
    // `/` alone on a line terminates, even without `;`. The slash is
    // excluded from the statement text — nothing to strip server-side.
    let stmts = split_statements("SELECT 1\n/\nSELECT 2;");
    assert_eq!(stmts.len(), 2, "{stmts:?}");
    assert_eq!(stmts[0].text, "SELECT 1");
    assert_eq!(stmts[1].text, "SELECT 2;");

    // Classic SQL Developer shape: PL/SQL block closed by `/`.
    let stmts = split_statements("BEGIN NULL; END;\n/\nSELECT 2;");
    assert_eq!(stmts.len(), 2, "{stmts:?}");
    assert!(stmts[0].text.ends_with("END;"));

    // Caret on the `/` line itself runs the statement it terminates.
    let sql = "SELECT 1\n/\nSELECT 2;";
    let slash = sql.find('/').unwrap();
    assert_eq!(statement_at(sql, slash).as_deref(), Some("SELECT 1"));

    // Leading / consecutive slash lines vanish, not error.
    let stmts = split_statements("/\n/\nSELECT 1;");
    assert_eq!(stmts.len(), 1, "{stmts:?}");

    // `/` mid-line is ordinary text, not a terminator.
    let stmts = split_statements("SELECT 1/2 FROM dual;");
    assert_eq!(stmts.len(), 1, "{stmts:?}");
}

#[test]
fn statement_at_after_semicolon_runs_current_line() {
    // Caret right after `;` (offset 9) belongs to statement 1, even
    // though statement 2 starts at the same offset.
    let sql = "SELECT 1;\nSELECT 2;";
    assert_eq!(statement_at(sql, 9).as_deref(), Some("SELECT 1;"));
    // Interior positions are unaffected.
    assert_eq!(statement_at(sql, 2).as_deref(), Some("SELECT 1;"));
    assert_eq!(statement_at(sql, 12).as_deref(), Some("SELECT 2;"));
}

#[test]
fn statement_at_trailing_space_runs_current_line() {
    // End of line 1 with trailing spaces still belongs to statement 1.
    let sql = "SELECT 1;  \nSELECT 2;";
    let eol = sql.find('\n').unwrap();
    assert_eq!(statement_at(sql, eol).as_deref(), Some("SELECT 1;"));
    // Same line, two statements: the gap after `;` runs the first.
    let sql = "SELECT 1; SELECT 2;";
    assert_eq!(statement_at(sql, 9).as_deref(), Some("SELECT 1;"));
    assert_eq!(statement_at(sql, 10).as_deref(), Some("SELECT 1;"));
    assert_eq!(statement_at(sql, 11).as_deref(), Some("SELECT 2;"));
}

#[test]
fn statement_at_next_line_indent_runs_next() {
    // Leading indentation of the next line belongs to the next statement.
    let sql = "SELECT 1;\n   SELECT 2;";
    let indent = sql.find("SELECT 2").unwrap() - 1;
    assert_eq!(statement_at(sql, indent).as_deref(), Some("SELECT 2;"));
}

#[test]
fn statement_at_picks_statement_under_caret() {
    let sql = "SELECT 1;\nSELECT 2;\nSELECT 3;";
    assert_eq!(statement_at(sql, 2).as_deref(), Some("SELECT 1;"));
    assert_eq!(statement_at(sql, 12).as_deref(), Some("SELECT 2;"));
    assert_eq!(statement_at(sql, 22).as_deref(), Some("SELECT 3;"));
}

#[test]
fn statement_at_range_matches_splice() {
    // Ranges cover the raw span (leading whitespace included); the
    // text is trimmed. Format must splice on the range while keeping
    // the surrounding whitespace so statements never join or drift.
    let sql = "select 1;\nselect 2;";
    let (text, start, end) = statement_at_range(sql, 12).unwrap();
    assert_eq!(text, "select 2;");
    let span = &sql[start..end];
    assert_eq!(span, "\nselect 2;");
    let lead = span.len() - span.trim_start().len();
    let trail = span.len() - span.trim_end().len();
    let formatted = format_sql(span.trim()).trim().to_string();
    let rebuilt = format!(
        "{}{}{}{}{}",
        &sql[..start],
        &span[..lead],
        formatted,
        &span[span.len() - trail..],
        &sql[end..]
    );
    assert_eq!(rebuilt, format!("select 1;\n{formatted}"));
    assert!(rebuilt.contains('\n'));
}

#[test]
fn statement_at_past_end_reruns_last() {
    let sql = "SELECT 1;\nSELECT 2;   ";
    assert_eq!(statement_at(sql, sql.len()).as_deref(), Some("SELECT 2;"));
}

#[test]
fn statement_at_blank_line_prefers_next() {
    let sql = "SELECT 1;\n\n\nSELECT 2;";
    let blank = sql.find("\n\n").unwrap() + 1;
    assert_eq!(statement_at(sql, blank).as_deref(), Some("SELECT 2;"));
}

#[test]
fn statement_at_empty_is_none() {
    assert_eq!(statement_at("", 0), None);
    assert_eq!(statement_at("  \n;  ", 3), None);
}

#[test]
fn single_statement_ignores_caret() {
    assert_eq!(statement_at("SELECT 1;", 99).as_deref(), Some("SELECT 1;"));
}

#[test]
fn statement_kind_routes_queries_and_commands() {
    use StatementKind::*;
    assert_eq!(statement_kind("SELECT 1 FROM dual"), Query);
    assert_eq!(statement_kind("  -- comment\nselect a from t"), Query);
    assert_eq!(
        statement_kind("WITH x AS (SELECT 1 FROM dual) SELECT * FROM x"),
        Query
    );
    assert_eq!(
        statement_kind("WITH x AS (SELECT 1 FROM dual) UPDATE t SET a = 1"),
        Execute
    );
    assert_eq!(statement_kind("INSERT INTO t VALUES (1)"), Execute);
    assert_eq!(statement_kind("UPDATE t SET a = 1"), Execute);
    assert_eq!(statement_kind("DELETE FROM t"), Execute);
    assert_eq!(statement_kind("CREATE TABLE t (a NUMBER)"), Execute);
    assert_eq!(statement_kind("BEGIN NULL; END;"), Execute);
    assert_eq!(
        statement_kind("DECLARE x NUMBER; BEGIN x := 1; END;"),
        Execute
    );
    assert_eq!(statement_kind("COMMIT"), Execute);
    assert_eq!(statement_kind(""), Execute);
    // DESCRIBE rides the query path (emulated via ALL_TAB_COLUMNS).
    assert_eq!(statement_kind("DESCRIBE emp"), Query);
    assert_eq!(statement_kind("  desc scott.emp;"), Query);
}

#[test]
fn plsql_blocks_stay_whole() {
    let stmts = split_statements("BEGIN NULL; DBMS_OUTPUT.PUT_LINE('x;'); END;");
    assert_eq!(stmts.len(), 1, "{stmts:?}");
    assert!(stmts[0].text.ends_with("END;"));

    let stmts = split_statements("DECLARE x NUMBER; BEGIN x := 1; END;\nSELECT 2;");
    assert_eq!(stmts.len(), 2, "{stmts:?}");

    // Nested blocks and END IF / END LOOP don't confuse the depth.
    let sql = "BEGIN IF x THEN y := 1; END IF; BEGIN z := 2; END; END;";
    let stmts = split_statements(sql);
    assert_eq!(stmts.len(), 1, "{stmts:?}");

    // CASE...END in plain SQL still splits at the real terminator.
    let stmts = split_statements("SELECT CASE WHEN a THEN 1 END FROM t;");
    assert_eq!(stmts.len(), 1, "{stmts:?}");
}

#[test]
fn is_plsql_block_detects_anonymous_blocks() {
    assert!(is_plsql_block("BEGIN NULL; END;"));
    assert!(is_plsql_block("  declare x number; begin null; end;"));
    assert!(!is_plsql_block("SELECT 1 FROM dual"));
    assert!(!is_plsql_block("CREATE TABLE t (a NUMBER)"));
}

#[test]
fn is_plsql_covers_blocks_and_object_bodies() {
    // Anonymous blocks.
    assert!(is_plsql("BEGIN NULL; END;"));
    assert!(is_plsql("  -- note\nDECLARE x NUMBER; BEGIN NULL; END;"));
    // CREATE ... PL/SQL object bodies, with modifiers.
    assert!(is_plsql(
        "CREATE OR REPLACE PROCEDURE p IS BEGIN NULL; END;"
    ));
    assert!(is_plsql("create procedure p as begin null; end;"));
    assert!(is_plsql(
        "CREATE OR REPLACE FUNCTION f RETURN NUMBER IS BEGIN RETURN 1; END;"
    ));
    assert!(is_plsql("CREATE PACKAGE pkg AS END;"));
    assert!(is_plsql("CREATE OR REPLACE PACKAGE BODY pkg AS END;"));
    assert!(is_plsql(
        "CREATE EDITIONABLE OR REPLACE TRIGGER trg BEFORE INSERT ON t BEGIN NULL; END;"
    ));
    assert!(is_plsql("CREATE TYPE t AS OBJECT (x NUMBER);"));
    // Plain SQL and non-PL/SQL DDL.
    assert!(!is_plsql("SELECT 1 FROM dual"));
    assert!(!is_plsql("CREATE TABLE t (a NUMBER)"));
    assert!(!is_plsql("CREATE OR REPLACE VIEW v AS SELECT 1 FROM dual"));
    assert!(!is_plsql("CREATE INDEX i ON t (a)"));
    assert!(!is_plsql("ALTER TABLE t ADD (b NUMBER)"));
    assert!(!is_plsql("DROP PROCEDURE p"));
    assert!(!is_plsql(""));
}

#[test]
fn is_plsql_fragment_detects_body_pieces() {
    assert!(is_plsql_fragment("PROCEDURE p IS BEGIN NULL; END;"));
    assert!(is_plsql_fragment("FUNCTION f RETURN NUMBER;"));
    assert!(is_plsql_fragment("PACKAGE pkg AS END;"));
    assert!(is_plsql_fragment("TRIGGER trg BEFORE INSERT ON t"));
    assert!(is_plsql_fragment("END;"));
    assert!(is_plsql_fragment("  end pkg;"));
    assert!(!is_plsql_fragment("SELECT 1 FROM dual"));
    assert!(!is_plsql_fragment("INSERT INTO t VALUES (1)"));
    assert!(!is_plsql_fragment(""));
}

#[test]
fn exec_summary_uses_action_verbs() {
    assert_eq!(
        exec_summary("INSERT INTO t VALUES (1)", 1),
        "1 row inserted"
    );
    assert_eq!(
        exec_summary("insert into t select * from s", 5),
        "5 rows inserted"
    );
    assert_eq!(exec_summary("UPDATE t SET a = 1", 0), "0 rows updated");
    assert_eq!(exec_summary("-- gone\nDELETE FROM t", 2), "2 rows deleted");
    assert_eq!(
        exec_summary(
            "MERGE INTO t USING s ON (1=1) WHEN MATCHED THEN UPDATE SET a=1",
            1
        ),
        "1 row merged"
    );
    assert_eq!(exec_summary("BEGIN NULL; END;", 1), "PL/SQL block executed");
    assert_eq!(exec_summary("COMMIT", 0), "Committed");
    assert_eq!(exec_summary("rollback", 0), "Rolled back");
    assert_eq!(exec_summary("GRANT SELECT ON t TO r", 0), "0 rows affected");
}

#[test]
fn exec_summary_names_ddl_objects() {
    assert_eq!(
        exec_summary("CREATE TABLE emp (a NUMBER)", 0),
        "Table emp created"
    );
    assert_eq!(
        exec_summary("create or replace view v as select 1 from dual", 0),
        "View v created"
    );
    assert_eq!(
        exec_summary("DROP INDEX \"My Index\"", 0),
        "Index My Index dropped"
    );
    assert_eq!(
        exec_summary("ALTER TABLE scott.emp ADD (b NUMBER)", 0),
        "Table scott.emp altered"
    );
    assert_eq!(exec_summary("TRUNCATE TABLE t", 0), "Table t truncated");
    // Unparseable DDL falls back to generic text, never panics.
    assert_eq!(exec_summary("CREATE", 0), "0 rows affected");
}

#[test]
fn txn_end_detects_commit_rollback() {
    assert_eq!(txn_end("COMMIT"), Some(true));
    assert_eq!(txn_end("  rollback ;"), Some(false));
    assert_eq!(txn_end("SELECT 1 FROM dual"), None);
    assert_eq!(txn_end("COMMITMENT ISSUES"), None);
}

#[test]
fn is_dml_flags_transactional_statements() {
    assert!(is_dml("INSERT INTO t VALUES (1)"));
    assert!(is_dml("  update t set a = 1"));
    assert!(is_dml("-- fix\nDELETE FROM t"));
    assert!(is_dml(
        "MERGE INTO t USING s ON (t.a = s.a) WHEN MATCHED THEN UPDATE SET a = 1"
    ));
    assert!(!is_dml("SELECT 1 FROM dual"));
    assert!(!is_dml("CREATE TABLE t (a NUMBER)"));
    assert!(!is_dml("BEGIN NULL; END;"));
}

#[test]
fn sub_vars_detect_named_positional_and_double() {
    let vars = find_substitution_vars("SELECT * FROM &tab WHERE a = &1 AND b = &&tab");
    assert_eq!(
        vars,
        vec![
            SubVar {
                name: "tab".into(),
                double: true
            },
            SubVar {
                name: "1".into(),
                double: false
            },
        ]
    );
}

#[test]
fn sub_vars_include_strings_skip_comments_and_escape() {
    // Inside '...' still prompts (SQL*Plus parity); comments don't; \& doesn't.
    let vars = find_substitution_vars(
        "SELECT '&dept' FROM t; -- &ignored\n/* &also_ignored */ SELECT \\&lit, &real",
    );
    assert_eq!(
        vars,
        vec![
            SubVar {
                name: "dept".into(),
                double: false
            },
            SubVar {
                name: "real".into(),
                double: false
            },
        ]
    );
}

#[test]
fn sub_vars_trailing_dot_is_separator() {
    let vars = find_substitution_vars("SELECT * FROM &schema.emp");
    assert_eq!(
        vars,
        vec![SubVar {
            name: "schema".into(),
            double: false
        }]
    );
    let mut map = std::collections::HashMap::new();
    map.insert("schema".to_string(), "scott".to_string());
    assert_eq!(
        apply_substitutions("SELECT * FROM &schema.emp", &map),
        "SELECT * FROM scottemp"
    );
}

#[test]
fn sub_apply_is_raw_and_non_recursive() {
    let mut map = std::collections::HashMap::new();
    map.insert("d".to_string(), "&e".to_string());
    // Value containing `&` is NOT re-expanded.
    assert_eq!(
        apply_substitutions("SELECT &d FROM dual", &map),
        "SELECT &e FROM dual"
    );
    // Missing names stay in place; escapes unescape.
    assert_eq!(
        apply_substitutions("SELECT &missing, \\&lit FROM dual", &map),
        "SELECT &missing, &lit FROM dual"
    );
}

#[test]
fn bind_vars_detect_and_skip_pseudo() {
    assert_eq!(
        find_bind_vars("SELECT * FROM t WHERE a = :id AND b = :1"),
        vec!["id", "1"]
    );
    // Dedup, `:=` skipped, trigger pseudo-binds skipped.
    assert_eq!(
        find_bind_vars("BEGIN x := :v; IF :v > 0 THEN :NEW.x := 1; END; -- :c\n/* :d */"),
        vec!["v"]
    );
    assert!(find_bind_vars("SELECT ':not_a_bind', \":neither\" FROM dual").is_empty());
    assert_eq!(
        find_bind_vars("SELECT :OLD, :old, :Parent FROM dual"),
        Vec::<String>::new()
    );
}

// --- @-script directives ------------------------------------------------

fn script_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("sqlhighland-script-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn parse_at_directive_forms() {
    assert_eq!(
        parse_at_directive("@/tmp/seed.sql"),
        Some(AtDirective {
            double_at: false,
            path: "/tmp/seed.sql".into()
        })
    );
    assert_eq!(
        parse_at_directive("  @@schema/tables.sql  "),
        Some(AtDirective {
            double_at: true,
            path: "schema/tables.sql".into()
        })
    );
    assert_eq!(
        parse_at_directive("@\"my dir/seed.sql\";"),
        Some(AtDirective {
            double_at: false,
            path: "my dir/seed.sql".into()
        })
    );
    assert_eq!(
        parse_at_directive("@'my dir/seed.sql'"),
        Some(AtDirective {
            double_at: false,
            path: "my dir/seed.sql".into()
        })
    );
    assert_eq!(
        parse_at_directive("START deploy.sql"),
        Some(AtDirective {
            double_at: false,
            path: "deploy.sql".into()
        })
    );
    assert_eq!(
        parse_at_directive("start  ./a.sql"),
        Some(AtDirective {
            double_at: false,
            path: "./a.sql".into()
        })
    );
    // Non-directives.
    assert_eq!(parse_at_directive("SELECT 1 FROM dual"), None);
    assert_eq!(parse_at_directive("@"), None);
    assert_eq!(parse_at_directive("@@"), None);
    assert_eq!(parse_at_directive("START"), None);
    assert_eq!(parse_at_directive("STARTUP costs"), None);
    assert_eq!(parse_at_directive(""), None);
}

#[test]
fn line_at_picks_caret_line() {
    let text = "@a.sql\nSELECT 1;\n";
    assert_eq!(line_at(text, 0), "@a.sql");
    assert_eq!(line_at(text, 3), "@a.sql");
    assert_eq!(line_at(text, 7), "SELECT 1;");
    assert_eq!(line_at(text, 999), "SELECT 1;");
}

#[test]
fn expand_single_and_sql_fallback() {
    let dir = script_dir("single");
    std::fs::write(dir.join("seed.sql"), "SELECT 1 FROM dual;\n").unwrap();
    // Extensionless ref finds seed.sql.
    let out = expand_at_directives("@seed\nSELECT 2 FROM dual;", &dir).unwrap();
    assert!(out.text.contains("SELECT 1 FROM dual;"), "{}", out.text);
    assert!(out.text.contains("SELECT 2 FROM dual;"), "{}", out.text);
    assert_eq!(out.files.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_double_at_resolves_against_includer() {
    let dir = script_dir("nest");
    std::fs::create_dir_all(dir.join("schema")).unwrap();
    std::fs::write(
        dir.join("schema").join("t.sql"),
        "CREATE TABLE t (a NUMBER);\n",
    )
    .unwrap();
    std::fs::write(dir.join("deploy.sql"), "@@schema/t.sql\n").unwrap();
    // Launched from elsewhere (base = dir itself here, but the nested
    // ref resolves against deploy.sql's dir either way).
    let entry = std::fs::read_to_string(dir.join("deploy.sql")).unwrap();
    let child_dir = dir.clone();
    let out = expand_at_directives(&entry, &child_dir).unwrap();
    assert!(out.text.contains("CREATE TABLE t"), "{}", out.text);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_missing_file_errors() {
    let dir = script_dir("missing");
    let err = expand_at_directives("@nope.sql\n", &dir).unwrap_err();
    assert!(err.contains("Cannot open script file"), "{err}");
    assert!(err.contains("nope.sql"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_cycle_errors() {
    let dir = script_dir("cycle");
    std::fs::write(dir.join("a.sql"), "@b.sql\n").unwrap();
    std::fs::write(dir.join("b.sql"), "@a.sql\n").unwrap();
    let err = expand_at_directives("@a.sql\n", &dir).unwrap_err();
    assert!(err.contains("Cyclic"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_ignores_comments() {
    let dir = script_dir("comments");
    std::fs::write(dir.join("real.sql"), "SELECT 1 FROM dual;\n").unwrap();
    let entry = "-- @real.sql\n/* @real.sql */\nSELECT 2 FROM dual;\n";
    let out = expand_at_directives(entry, &dir).unwrap();
    assert!(!out.text.contains("SELECT 1 FROM dual"), "{}", out.text);
    assert!(out.files.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}
