//! Unit tests for the completion engine.

use super::*;
use crate::metadata::TableKind;
use crate::metadata::{ColumnMeta, MetadataCache};

#[test]
fn prefix_extracts_word_and_offset() {
    assert_eq!(word_prefix("SELECT em", 9), ("em".to_string(), 7));
    assert_eq!(word_prefix("SELECT ", 7), ("".to_string(), 7));
    assert_eq!(word_prefix("e.emp_", 6), ("emp_".to_string(), 2));
    assert_eq!(word_prefix("SELECT \"Mixed", 14), ("Mixed".to_string(), 8));
}

#[test]
fn prefix_ignores_closed_quotes_on_earlier_lines() {
    // @-script path on line 1 must not claim prefixes below it.
    let text = "@\"/Users/srikanth/test.sql\";\nSELECT * FROM em";
    assert_eq!(word_prefix(text, text.len()), ("em".to_string(), 43));
    // Same-line closed pair: the completed "B" is not an open quote.
    let text = "SELECT \"B\" FROM emp WHERE x = em";
    assert_eq!(word_prefix(text, text.len()), ("em".to_string(), 30));
    // Genuinely open quotes still win, on any line.
    let text = "@\"/x.sql\";\nSELECT \"Mixed";
    assert_eq!(word_prefix(text, text.len()), ("Mixed".to_string(), 19));
    // Escaped "" pairs don't confuse parity.
    assert_eq!(word_prefix("SELECT \"A\"\"B", 13), ("B".to_string(), 11));
    // Quotes inside comments/single-quoted strings never claim prefixes.
    let text = "/* say \"hi */ SELECT em";
    assert_eq!(word_prefix(text, text.len()), ("em".to_string(), 21));
    let text = "SELECT 'a\"b' || em";
    assert_eq!(word_prefix(text, text.len()), ("em".to_string(), 16));
}

#[test]
fn word_at_extends_past_cursor() {
    // Mid-word pointer (the hover case): full word, same start.
    assert_eq!(
        word_at("SELECT FIRST_NAME FROM SYSTEM.EMPLOYEES", 34),
        ("EMPLOYEES".to_string(), 30)
    );
    assert_eq!(
        word_at("SELECT FIRST_NAME FROM SYSTEM.EMPLOYEES", 30),
        ("EMPLOYEES".to_string(), 30)
    );
    // End-of-word: identical to the prefix.
    let sql = "SELECT FIRST_NAME FROM SYSTEM.EMPLOYEES";
    assert_eq!(word_at(sql, 39), ("EMPLOYEES".to_string(), 30));
    // Non-word positions stay empty; on `.` the word before holds
    // (unchanged legacy behavior — resolves to no card downstream).
    assert_eq!(word_at("SELECT a.b", 8), ("a".to_string(), 7));
    assert_eq!(word_at("SELECT ", 7), ("".to_string(), 7));
    // Quoted: inner name, no quotes.
    assert_eq!(
        word_at("SELECT \"MixedCase\" FROM t", 12),
        ("MixedCase".to_string(), 8)
    );
}

#[test]
fn qualifier_detects_dotted() {
    assert_eq!(qualifier_before("SELECT e.", 9), Some("e".to_string()));
    let sql = "SELECT scott.emp.";
    assert_eq!(
        qualifier_before(sql, sql.len()),
        Some("scott.emp".to_string())
    );
    assert_eq!(qualifier_before("SELECT em", 9), None);
    assert_eq!(qualifier_before("SELECT e. ", 10), Some("e".to_string()));
}

#[test]
fn context_after_from_and_dot() {
    let no_seq = |_: &str| false;
    assert_eq!(
        classify_context("SELECT * FROM ", 14, &no_seq),
        CompleteContext::AfterFrom
    );
    assert_eq!(
        classify_context("SELECT e.", 9, &no_seq),
        CompleteContext::ColumnOf("e".to_string())
    );
    assert_eq!(
        classify_context("SELECT em", 9, &no_seq),
        CompleteContext::SelectList
    );
    let is_seq = |n: &str| n == "MYSEQ";
    assert_eq!(
        classify_context("SELECT myseq.", 13, &is_seq),
        CompleteContext::SequenceMember("myseq".to_string())
    );
}

#[test]
fn context_gates_by_grammar_position() {
    use CompleteContext::*;
    let no_seq = |_: &str| false;
    let at = |sql: &str| classify_context(sql, sql.len(), &no_seq);
    // Statement start: starters only.
    assert_eq!(at(""), StatementStart);
    assert_eq!(at("SEL"), StatementStart);
    // Select list: never tables.
    assert_eq!(at("SELECT "), SelectList);
    assert_eq!(at("SELECT a, "), SelectList);
    assert_eq!(at("SELECT COUNT("), SelectList);
    // After FROM/JOIN: tables.
    assert_eq!(at("SELECT * FROM emp, "), AfterFrom);
    assert_eq!(at("DELETE FROM "), AfterFrom);
    assert_eq!(at("UPDATE "), AfterFrom);
    // Predicates: never tables.
    assert_eq!(at("SELECT * FROM emp WHERE "), Predicate);
    assert_eq!(at("SELECT * FROM emp WHERE deptno = "), Predicate);
    assert_eq!(at("SELECT * FROM emp ORDER BY "), Predicate);
    assert_eq!(at("SELECT * FROM emp GROUP BY d, "), Predicate);
    assert_eq!(at("UPDATE emp SET "), Predicate);
    // Ambiguous: keywords only.
    assert_eq!(at("SELECT emp "), BareWord);
    assert_eq!(at("SELECT * FROM emp "), BareWord);
    // Subqueries scope inward.
    assert_eq!(at("SELECT * FROM (SELECT "), SelectList);
    // New statement after terminator starts over.
    assert_eq!(at("SELECT 1; "), StatementStart);
    assert_eq!(at("SELECT 1; SEL"), StatementStart);
    // Owner qualifier after FROM completes tables, not columns.
    assert_eq!(at("SELECT * FROM scott."), OwnerTables("scott".to_string()));
    // Fresh ON conditions route through detect_join_on in the provider;
    // classify itself sees predicate scope (columns for manual typing).
    assert_eq!(at("SELECT * FROM emp e JOIN dept d ON "), Predicate);
}

#[test]
fn empty_prefix_allowed_only_after_operand_keywords() {
    let at = |sql: &str| allows_empty_prefix(sql, sql.len());
    assert!(at("SELECT * FROM "));
    assert!(at("SELECT "));
    assert!(at("SELECT * FROM emp WHERE x=1 AND "));
    assert!(!at("SELECT * FROM emp "));
    assert!(!at("SELECT emp "));
    assert!(!at(""));
    // Finished conditions stay in predicate scope (AND/OR offered).
    assert!(at("SELECT * FROM emp e JOIN dept d ON e.x = 1 "));
}

#[test]
fn follow_sets_cover_transitions() {
    // The reported gaps: FROM after a select list, ORDER after predicates.
    assert!(SELECT_FOLLOW.contains(&"FROM"));
    assert!(SELECT_FOLLOW.contains(&"WHERE"));
    assert!(PRED_FOLLOW.contains(&"ORDER"));
    assert!(PRED_FOLLOW.contains(&"GROUP"));
}

#[test]
fn keyword_subsets_are_sane() {
    // Every subset item is a known keyword (no typos silently dropping).
    for kw in STMT_KEYWORDS
        .iter()
        .chain(EXPR_KEYWORDS)
        .chain(PRED_KEYWORDS)
        .chain(SELECT_FOLLOW)
        .chain(PRED_FOLLOW)
    {
        assert!(ORACLE_KEYWORDS.contains(kw), "{kw} unknown");
    }
    assert!(STMT_KEYWORDS.contains(&"SELECT"));
    assert!(EXPR_KEYWORDS.contains(&"DISTINCT"));
    assert!(PRED_KEYWORDS.contains(&"AND"));
}

#[test]
fn alias_map_from_join_and_commas() {
    let m = build_alias_map("SELECT * FROM scott.emp e JOIN dept d ON e.deptno = d.deptno");
    assert_eq!(m["e"].name, "emp");
    assert_eq!(m["e"].owner.as_deref(), Some("scott"));
    assert_eq!(m["d"].name, "dept");
    let m2 = build_alias_map("SELECT * FROM emp, dept WHERE emp.deptno = dept.deptno");
    assert_eq!(m2["emp"].name, "emp");
    assert_eq!(m2["dept"].name, "dept");
    let m3 = build_alias_map("SELECT * FROM employees AS e");
    assert_eq!(m3["e"].name, "employees");
}

#[test]
fn resolve_qualifier_prefers_alias_then_bare() {
    let m = build_alias_map("SELECT * FROM scott.emp e");
    let r = resolve_qualifier("e", &m).unwrap();
    assert_eq!(
        (r.owner.as_deref(), r.name.as_str()),
        (Some("scott"), "emp")
    );
    let r = resolve_qualifier("scott.emp", &m).unwrap();
    assert_eq!(
        (r.owner.as_deref(), r.name.as_str()),
        (Some("scott"), "emp")
    );
    let r = resolve_qualifier("dept", &m).unwrap();
    assert_eq!(r.name, "dept");
}

#[test]
fn ranking_prefers_scope_then_prefix_then_usage() {
    let cand = |label: &str, kind: CandidateKind, usage: u64| Candidate {
        label: label.into(),
        detail: "".into(),
        kind,
        owner: None,
        usage,
    };
    let cands = vec![
        cand("EMPLOYEE_AUDIT", CandidateKind::Table, 99),
        cand("EMP", CandidateKind::Table, 0),
        cand("EMP_ID", CandidateKind::ColumnInScope, 0),
        cand("SELECT", CandidateKind::Keyword, 0),
    ];
    let out = rank_candidates("emp", cands, "", 10);
    assert_eq!(out[0].label, "EMP_ID");
    assert_eq!(out[1].label, "EMP");
    // Usage breaks ties between equal-quality prefix matches.
    let tied = vec![
        cand("DEPT", CandidateKind::Table, 1),
        cand("DEPTNO", CandidateKind::Table, 50),
    ];
    let out = rank_candidates("dep", tied, "", 10);
    assert_eq!(out[0].label, "DEPTNO");
}

#[test]
fn function_table_is_consistent() {
    // No duplicates with keywords (would double-list), signatures present.
    for (name, sig) in ORACLE_FUNCTIONS {
        assert!(
            !ORACLE_KEYWORDS.contains(name),
            "{name} in both functions and keywords"
        );
        assert!(!sig.is_empty(), "{name} needs a signature");
        assert_eq!(function_insert(name), format!("{name}()"));
    }
    // Sorted for stable popup order among equals.
    let names: Vec<_> = ORACLE_FUNCTIONS.iter().map(|(n, _)| n).collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(names, sorted);
}

#[test]
fn ranking_prefers_own_schema() {
    let cand = |label: &str, owner: &str| Candidate {
        label: label.into(),
        detail: "".into(),
        kind: CandidateKind::Table,
        owner: Some(owner.into()),
        usage: 0,
    };
    let cands = vec![cand("DVSYS.DBA_X", "DVSYS"), cand("SCOTT.DEPT", "SCOTT")];
    let out = rank_candidates("d", cands, "SCOTT", 10);
    assert_eq!(out[0].label, "SCOTT.DEPT");
}

#[test]
fn ranking_prefers_functions_over_keywords() {
    let cand = |label: &str, kind: CandidateKind| Candidate {
        label: label.into(),
        detail: "".into(),
        kind,
        owner: None,
        usage: 0,
    };
    let cands = vec![
        cand("CASE", CandidateKind::Keyword),
        cand("COUNT()", CandidateKind::Function),
        cand("CREATE", CandidateKind::Keyword),
    ];
    let out = rank_candidates("c", cands, "", 10);
    assert_eq!(out[0].label, "COUNT()");
}

#[test]
fn dotted_labels_score_on_object_part() {
    let cand = |label: &str| Candidate {
        label: label.into(),
        detail: "".into(),
        kind: CandidateKind::Table,
        owner: None,
        usage: 0,
    };
    // `emp` must prefer the table named EMP… over *TEMP* substring noise.
    let cands = vec![cand("SYS.MVIEW$_ADV_TEMP"), cand("SYSTEM.EMPLOYEES")];
    let out = rank_candidates("emp", cands, "", 10);
    assert_eq!(out[0].label, "SYSTEM.EMPLOYEES");
}

#[test]
fn system_schemas_flagged() {
    assert!(is_system_schema("SYS"));
    assert!(is_system_schema("sys"));
    assert!(is_system_schema("DVSYS"));
    assert!(is_system_schema("AUDSYS"));
    assert!(is_system_schema("APEX_240200"));
    assert!(is_system_schema("FLOWS_300100"));
    assert!(is_system_schema("GSMADMIN_INTERNAL"));
    assert!(!is_system_schema("SCOTT"));
    assert!(!is_system_schema("HR"));
}

#[test]
fn trivia_positions_detected() {
    assert!(is_trivia_position("SELECT '--x", 11));
    assert!(is_trivia_position("SELECT 1 -- foo", 15));
    assert!(is_trivia_position("SELECT /* open", 14));
    assert!(!is_trivia_position("SELECT /* shut */ 1", 18));
    assert!(!is_trivia_position("SELECT emp", 10));
    assert!(is_trivia_position("SELECT \"AB", 10));
}

#[test]
fn trivia_respects_scope_nesting() {
    // Apostrophe in a line comment must not poison later lines.
    let text = "-- don't do this\nSELECT em";
    assert!(!is_trivia_position(text, text.len()));
    // Quotes inside a closed block comment never count.
    let text = "/* it's \"quoted\" */\nSELECT em";
    assert!(!is_trivia_position(text, text.len()));
    // Comment markers inside strings never count.
    let text = "SELECT '--' || em";
    assert!(!is_trivia_position(text, text.len()));
    let text = "SELECT '/*' || em";
    assert!(!is_trivia_position(text, text.len()));
    // A quote inside a single-quoted string is content, not scope.
    let text = "SELECT 'a\"b' || em";
    assert!(!is_trivia_position(text, text.len()));
    // ...but real scopes still gate.
    let text = "-- don't\nSELECT 'open";
    assert!(is_trivia_position(text, text.len()));
    let text = "SELECT 'it''s' || 'open";
    assert!(is_trivia_position(text, text.len()));
}

#[test]
fn detect_join_on_finds_fresh_condition() {
    let aliases = build_alias_map("SELECT * FROM emp e JOIN dept d ON ");
    let (alias, tref) = detect_join_on("SELECT * FROM emp e JOIN dept d ON ", &aliases).unwrap();
    assert_eq!(alias, "d");
    assert_eq!(tref.name, "dept");
    // AS alias + owner-qualified.
    let aliases = build_alias_map("SELECT * FROM scott.emp JOIN scott.dept AS dd ON ");
    let (alias, tref) = detect_join_on(
        "SELECT * FROM scott.emp JOIN scott.dept AS dd ON ",
        &aliases,
    )
    .unwrap();
    assert_eq!(alias, "dd");
    assert_eq!(tref.owner.as_deref(), Some("scott"));
    // No ON yet → None.
    assert!(detect_join_on("SELECT * FROM emp e JOIN dept d", &aliases).is_none());
    // Condition already started → None (v1: first condition only).
    assert!(detect_join_on("SELECT * FROM emp e JOIN dept d ON e.x = 1", &aliases).is_none());
    assert!(detect_join_on("SELECT * FROM emp e JOIN dept d ON e.x = 1 AND ", &aliases).is_none());
    // Quoted JOIN prose doesn't fool it.
    assert!(detect_join_on("SELECT 'join dept on ' FROM emp e", &aliases).is_none());
}

#[test]
fn join_conditions_render_alias_pairs_both_directions() {
    let aliases = build_alias_map("SELECT * FROM emp e JOIN dept d ON ");
    let fk = ForeignKey {
        name: "EMP_DEPT_FK".into(),
        from_owner: Some("SCOTT".into()),
        from_table: "EMP".into(),
        from_cols: vec!["DEPTNO".into()],
        to_owner: Some("SCOTT".into()),
        to_table: "DEPT".into(),
        to_cols: vec!["DEPTNO".into()],
    };
    let right = TableRef {
        owner: None,
        name: "dept".into(),
    };
    let out = join_condition_candidates("d", &right, &aliases, &[fk]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].label, "e.DEPTNO = d.DEPTNO");
    assert_eq!(out[0].kind, CandidateKind::JoinCondition);
    // Reversed FK direction renders left-first too.
    let fk_rev = ForeignKey {
        name: "DEPT_MGR_FK".into(),
        from_owner: None,
        from_table: "dept".into(),
        from_cols: vec!["MGR".into()],
        to_owner: None,
        to_table: "emp".into(),
        to_cols: vec!["EMPNO".into()],
    };
    let out = join_condition_candidates("d", &right, &aliases, &[fk_rev]);
    assert_eq!(out[0].label, "e.EMPNO = d.MGR");
    // Composite keys join with AND.
    let fk_multi = ForeignKey {
        name: "COMP_FK".into(),
        from_owner: None,
        from_table: "emp".into(),
        from_cols: vec!["A".into(), "B".into()],
        to_owner: None,
        to_table: "dept".into(),
        to_cols: vec!["A".into(), "B".into()],
    };
    let out = join_condition_candidates("d", &right, &aliases, &[fk_multi]);
    assert_eq!(out[0].label, "e.A = d.A AND e.B = d.B");
    // Unrelated tables → no candidates (caller shows no popup).
    let aliases2 = build_alias_map("SELECT * FROM emp e JOIN bonus b ON ");
    let right2 = TableRef {
        owner: None,
        name: "bonus".into(),
    };
    let fk = ForeignKey {
        name: "EMP_DEPT_FK".into(),
        from_owner: None,
        from_table: "EMP".into(),
        from_cols: vec!["DEPTNO".into()],
        to_owner: None,
        to_table: "DEPT".into(),
        to_cols: vec!["DEPTNO".into()],
    };
    assert!(join_condition_candidates("b", &right2, &aliases2, &[fk]).is_empty());
}

#[test]
fn ambiguous_columns_flags_shared_names() {
    let col = |n: &str| ColumnMeta {
        name: n.to_string(),
        data_type: String::new(),
        comments: String::new(),
    };
    let scope = vec![
        ScopeTable {
            owner: Some("SCOTT".to_string()),
            table: "EMP".to_string(),
            cols: vec![col("DEPTNO"), col("EMPNO")],
        },
        ScopeTable {
            owner: Some("SCOTT".to_string()),
            table: "DEPT".to_string(),
            cols: vec![col("DEPTNO")],
        },
    ];
    let amb = ambiguous_columns(&scope);
    assert!(amb.contains("DEPTNO"));
    assert!(!amb.contains("EMPNO"));
    assert!(ambiguous_columns(&[]).is_empty());
}

#[test]
fn scope_label_prefers_alias_then_table() {
    let m = build_alias_map("SELECT * FROM scott.emp e JOIN dept d ON e.deptno = d.deptno");
    assert_eq!(
        scope_label(&Some("scott".to_string()), "emp", &m).as_deref(),
        Some("e")
    );
    assert_eq!(
        scope_label(&Some("SCOTT".to_string()), "DEPT", &m).as_deref(),
        Some("d")
    );
    assert_eq!(
        scope_label(&None, "bonus", &m),
        None,
        "unknown tables fall back to the table name itself"
    );
    // Known owner never claims an unknown table.
    assert_eq!(scope_label(&Some("SCOTT".to_string()), "bonus", &m), None);
}

#[test]
fn short_comment_collapses_whitespace_and_caps() {
    assert_eq!(short_comment("  employee\n id  "), "employee id");
    assert_eq!(short_comment(""), "");
    let long = "x".repeat(100);
    let out = short_comment(&long);
    assert_eq!(out.chars().count(), 80);
    assert!(out.ends_with('…'));
}

#[test]
fn hover_table_card_lists_columns() {
    let mut cache = MetadataCache {
        tables: vec![crate::metadata::TableId {
            owner: "SCOTT".into(),
            name: "EMP".into(),
            kind: TableKind::Table,
        }],
        ..Default::default()
    };
    cache.columns.insert(
        ("SCOTT".into(), "EMP".into()),
        vec![
            ColumnMeta {
                name: "EMPNO".into(),
                data_type: "NUMBER".into(),
                comments: "employee id".into(),
            },
            ColumnMeta {
                name: "ENAME".into(),
                data_type: "VARCHAR2".into(),
                comments: "".into(),
            },
        ],
    );
    let aliases = build_alias_map("SELECT * FROM scott.emp e");
    let md = hover_markdown("emp", Some("e"), &aliases, &cache, false, "SCOTT").unwrap();
    // Own-schema table: bare title, no owner prefix (as-written case).
    assert!(md.contains("**emp** — TABLE"), "{md}");
    assert!(!md.to_ascii_uppercase().contains("SCOTT.EMP"), "{md}");
    assert!(md.contains("EMPNO — NUMBER — employee id"), "{md}");
    // Unknown object → None, never a guess.
    assert!(hover_markdown("nope", None, &aliases, &cache, false, "SCOTT").is_none());
    // Bare unique column resolves with its table.
    let md = hover_markdown("empno", None, &aliases, &cache, false, "SCOTT").unwrap();
    assert!(md.contains("**EMPNO**"), "{md}");
    assert!(md.contains("EMP"), "{md}");
    // Own-schema column card drops the owner suffix.
    let md = hover_markdown("ename", Some("e"), &aliases, &cache, false, "SCOTT").unwrap();
    assert!(md.contains("**ENAME**"), "{md}");
    assert!(!md.contains("· SCOTT"), "{md}");
}

#[test]
fn display_name_bares_own_schema() {
    assert_eq!(
        display_name(Some("SYSTEM"), "EMPLOYEES", "system"),
        "EMPLOYEES"
    );
    assert_eq!(
        display_name(Some("SYSTEM"), "EMPLOYEES", "SYSTEM"),
        "EMPLOYEES"
    );
    assert_eq!(display_name(Some("SCOTT"), "EMP", "HR"), "SCOTT.EMP");
    assert_eq!(display_name(None, "DUAL", "HR"), "DUAL");
    assert_eq!(display_name(Some(""), "DUAL", "HR"), "DUAL");
}

#[test]
fn hover_owner_table_resolves_directly() {
    // `SYSTEM.|EMPLOYEES`: qualifier is an owner, not an alias — must
    // not misread as table SYSTEM, and bypasses the system filter.
    let mut cache = MetadataCache {
        tables: vec![crate::metadata::TableId {
            owner: "SYSTEM".into(),
            name: "EMPLOYEES".into(),
            kind: TableKind::Table,
        }],
        ..Default::default()
    };
    cache.columns.insert(
        ("SYSTEM".into(), "EMPLOYEES".into()),
        vec![ColumnMeta {
            name: "ID".into(),
            data_type: "NUMBER".into(),
            comments: "".into(),
        }],
    );
    let aliases = build_alias_map("SELECT first_name FROM system.employees");
    let md = hover_markdown("EMPLOYEES", Some("SYSTEM"), &aliases, &cache, false, "HR").unwrap();
    assert!(md.contains("**SYSTEM.EMPLOYEES**"), "{md}");
    assert!(md.contains("ID — NUMBER"), "{md}");
    // Dotted qualifier + column: `scott.emp.|ename`.
    cache.tables.push(crate::metadata::TableId {
        owner: "SCOTT".into(),
        name: "EMP".into(),
        kind: TableKind::Table,
    });
    cache.columns.insert(
        ("SCOTT".into(), "EMP".into()),
        vec![ColumnMeta {
            name: "ENAME".into(),
            data_type: "VARCHAR2".into(),
            comments: "".into(),
        }],
    );
    let md = hover_markdown("ENAME", Some("scott.emp"), &aliases, &cache, false, "HR").unwrap();
    assert!(md.contains("**ENAME**"), "{md}");
    assert!(md.contains("emp · scott"), "{md}");
}

#[test]
fn describe_target_resolves_tables_only() {
    let mut cache = MetadataCache {
        tables: vec![
            crate::metadata::TableId {
                owner: "SYSTEM".into(),
                name: "EMPLOYEES".into(),
                kind: TableKind::Table,
            },
            crate::metadata::TableId {
                owner: "SCOTT".into(),
                name: "EMP".into(),
                kind: TableKind::Table,
            },
        ],
        ..Default::default()
    };
    cache.columns.insert(
        ("SYSTEM".into(), "EMPLOYEES".into()),
        vec![ColumnMeta {
            name: "ID".into(),
            data_type: "NUMBER".into(),
            comments: "".into(),
        }],
    );
    cache.columns.insert(
        ("SCOTT".into(), "EMP".into()),
        vec![ColumnMeta {
            name: "ENAME".into(),
            data_type: "VARCHAR2".into(),
            comments: "".into(),
        }],
    );
    let aliases = build_alias_map("SELECT first_name FROM system.employees");
    // Qualified, cursor on the table: owner + table.
    assert_eq!(
        describe_target("EMPLOYEES", Some("SYSTEM"), &aliases, &cache, false, "HR"),
        Some((Some("SYSTEM".to_string()), "EMPLOYEES".to_string()))
    );
    // Alias-qualified table part (`e.` + `emp` not a column of it).
    // Owner/name pass through as written (DESCRIBE folds case).
    let aliases = build_alias_map("SELECT * FROM scott.emp e");
    assert_eq!(
        describe_target("emp", Some("e"), &aliases, &cache, false, "SCOTT"),
        Some((Some("scott".to_string()), "emp".to_string()))
    );
    // Column words are not jumps (v1 table-only).
    assert_eq!(
        describe_target("ENAME", Some("scott.emp"), &aliases, &cache, false, "HR"),
        None
    );
    assert_eq!(
        describe_target("ENAME", Some("e"), &aliases, &cache, false, "SCOTT"),
        None
    );
    // Bare unique visible table.
    assert_eq!(
        describe_target("EMP", None, &aliases, &cache, false, "SCOTT"),
        Some((Some("SCOTT".to_string()), "EMP".to_string()))
    );
    // Bare system-owned table stays filtered for other users.
    assert_eq!(
        describe_target("EMPLOYEES", None, &aliases, &cache, false, "HR"),
        None
    );
    // Unknown words never resolve.
    assert_eq!(
        describe_target("nope", None, &aliases, &cache, false, "SCOTT"),
        None
    );
    assert_eq!(
        describe_target("", None, &aliases, &cache, false, "SCOTT"),
        None
    );
}

#[test]
fn insert_text_appends_space_for_keywords_only() {
    assert_eq!(insert_text_for(CandidateKind::Keyword, "SELECT"), "SELECT ");
    assert_eq!(
        insert_text_for(CandidateKind::Keyword, "ORDER BY"),
        "ORDER BY "
    );
    assert_eq!(insert_text_for(CandidateKind::Table, "EMP"), "EMP");
    assert_eq!(
        insert_text_for(CandidateKind::Function, "TO_DATE()"),
        "TO_DATE()"
    );
    assert_eq!(
        insert_text_for(CandidateKind::ColumnInScope, "EMPNO"),
        "EMPNO"
    );
}

#[test]
fn byte_to_lsp_pos_counts_utf16() {
    let text = "SELECT *\nFROM émp;";
    // Line 1 (0-based), after "FROM ".
    let off = text.find("émp").unwrap();
    assert_eq!(byte_to_lsp_pos(text, off), (1, 5));
    // `é` is 1 UTF-16 unit; mid-char offsets floor back.
    assert_eq!(byte_to_lsp_pos(text, off + 1), (1, 5));
    assert_eq!(byte_to_lsp_pos(text, off + 2), (1, 6));
    assert_eq!(byte_to_lsp_pos(text, 0), (0, 0));
    assert_eq!(
        byte_to_lsp_pos(text, 999),
        byte_to_lsp_pos(text, text.len())
    );
}

#[test]
fn split_dotted_handles_quotes() {
    assert_eq!(
        split_dotted("scott.emp"),
        (Some("scott".to_string()), "emp".to_string())
    );
    assert_eq!(
        split_dotted("\"MixedCase\""),
        (None, "MixedCase".to_string())
    );
    assert_eq!(split_dotted("emp"), (None, "emp".to_string()));
}
