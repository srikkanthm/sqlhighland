use sqlhighland::db::{DbClient, OracledbSession};
use sqlhighland::model::ConnectionConfig;

fn cfg() -> ConnectionConfig {
    ConnectionConfig {
        id: String::new(),
        name: "highland-local".to_string(),
        host: std::env::var("ORACLE_HOST").unwrap_or_else(|_| "localhost".to_string()),
        port: std::env::var("ORACLE_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .unwrap_or(1521),
        service_name: std::env::var("ORACLE_SERVICE").unwrap_or_else(|_| "highlandpdb".to_string()),
        user: std::env::var("ORACLE_USER").unwrap_or_else(|_| "system".to_string()),
        password: std::env::var("ORACLE_PWD").unwrap_or_else(|_| "test".to_string()),
    }
}

fn show(tag: &str, r: &sqlhighland::model::QueryResult) {
    println!("--- {tag} ({} rows, {}ms, truncated={})", r.row_count(), r.elapsed_ms, r.truncated);
    println!(
        "cols: {}",
        r.columns.iter().map(|c| format!("{}:{}", c.name, c.db_type)).collect::<Vec<_>>().join(", ")
    );
    for row in &r.rows {
        println!(
            "  {}",
            row.iter()
                .map(|c| c.as_deref().unwrap_or("NULL"))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
}

#[test]
fn live_connect_and_dual() {
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    assert!(s.is_connected());
    let r = s.run_query("select user from dual", 1000).expect("query");
    show("dual", &r);
    assert_eq!(r.row_count(), 1);
    assert_eq!(r.rows[0][0].as_deref(), Some("SYSTEM"));
}

#[test]
fn live_type_coverage() {
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    let r = s
        .run_query(
            "SELECT 42 AS num, \
                CAST(3.14 AS BINARY_FLOAT) AS bf, \
                CAST(2.718281828 AS BINARY_DOUBLE) AS bd, \
                'hello' AS txt, \
                SYSDATE AS dt, \
                SYSTIMESTAMP AS ts, \
                HEXTORAW('DEADBEEF') AS rawcol, \
                INTERVAL '1-2' YEAR TO MONTH AS ym, \
                INTERVAL '3 04:05:06' DAY TO SECOND AS ds, \
                NULL AS n \
             FROM DUAL",
            1000,
        )
        .expect("query");
    show("types", &r);
    assert_eq!(r.row_count(), 1);
    let row = &r.rows[0];
    assert_eq!(row[0].as_deref(), Some("42"));
    assert_eq!(row[3].as_deref(), Some("hello"));
    assert_eq!(row[6].as_deref(), Some("DEADBEEF"));
    assert_eq!(row[9], None); // NULL
}

#[test]
fn live_truncation() {
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    let r = s
        .run_query("SELECT level AS n FROM dual CONNECT BY level <= 5", 3)
        .expect("query");
    show("truncated", &r);
    assert_eq!(r.row_count(), 3);
    assert!(r.truncated);
}

#[test]
fn live_incremental_paging() {
    use sqlhighland::db::OracledbSession;
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    // 2500 rows, pulled in 1000-row pages: 1000 + 1000 + 500.
    let (columns, first, id) = s.start_query("SELECT level AS n FROM dual CONNECT BY level <= 2500", 1000).expect("start");
    assert_eq!(columns.len(), 1);
    assert_eq!(first.rows.len(), 1000);
    assert!(!first.exhausted);
    assert!(first.current);
    let p2 = s.fetch_more(id, 1000).expect("page 2");
    assert_eq!(p2.rows.len(), 1000);
    assert!(!p2.exhausted && p2.current);
    let p3 = s.fetch_more(id, 1000).expect("page 3");
    assert_eq!(p3.rows.len(), 500);
    assert!(p3.exhausted && p3.current);
    // Stale generation is discarded, never appended.
    let stale = s.fetch_more(id.wrapping_add(99), 1000).expect("stale");
    assert!(!stale.current);
    assert!(stale.rows.is_empty());
}

#[test]
fn live_trailing_semicolon_is_tolerated() {
    // Regression test: editors terminate statements with `;`, which Oracle
    // rejects in programmatic calls (ORA-00933 / ORA-01003).
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    let r = s
        .run_query("SELECT user, sysdate FROM dual;", 1000)
        .expect("query with trailing semicolon");
    show("semicolon", &r);
    assert_eq!(r.row_count(), 1);
}

#[test]
fn live_describe_emulated() {
    // DESCRIBE is a SQL*Plus client command; the app rewrites it to
    // ALL_TAB_COLUMNS. DUAL has one VARCHAR2(1) column, DUMMY.
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    for stmt in ["DESCRIBE dual", "desc sys.dual;"] {
        let r = s.run_query(stmt, 1000).expect("describe");
        show("describe", &r);
        assert_eq!(
            r.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["Name", "Null?", "Type"]
        );
        assert_eq!(r.row_count(), 1);
        assert_eq!(r.rows[0][0].as_deref(), Some("DUMMY"));
        assert_eq!(r.rows[0][2].as_deref(), Some("VARCHAR2(1)"));
    }
    // Unknown tables describe to zero rows, not an error.
    let r = s.run_query("DESCRIBE no_such_table_xyz", 1000).expect("describe");
    assert_eq!(r.row_count(), 0);
}

#[test]
fn live_ddl_dml_commit_rollback() {
    use sqlhighland::db::DbClient;
    let mut s = OracledbSession::new();
    s.connect(&cfg()).expect("connect");
    let table = "sh_test_txn";
    // Best-effort cleanup from any previous interrupted run.
    let _ = s.exec(&format!("DROP TABLE {table}"));
    s.exec(&format!("CREATE TABLE {table} (id NUMBER, name VARCHAR2(30))"))
        .expect("create");
    // DDL auto-commits; insert then roll back: row must vanish.
    let (affected, _) = s
        .exec(&format!("INSERT INTO {table} VALUES (1, 'a')"))
        .expect("insert");
    assert_eq!(affected, 1);
    s.rollback().expect("rollback");
    let r = s
        .run_query(&format!("SELECT COUNT(*) AS c FROM {table}"), 1000)
        .expect("count");
    assert_eq!(r.rows[0][0].as_deref(), Some("0"));
    // Insert then commit: row must persist across a fresh session.
    s.exec(&format!("INSERT INTO {table} VALUES (2, 'b')"))
        .expect("insert");
    s.commit().expect("commit");
    let mut s2 = OracledbSession::new();
    s2.connect(&cfg()).expect("connect");
    let r = s2
        .run_query(&format!("SELECT COUNT(*) AS c FROM {table}"), 1000)
        .expect("count");
    assert_eq!(r.rows[0][0].as_deref(), Some("1"));
    // PL/SQL block executes (success is what matters; Oracle reports a
    // driver-defined rowcount for blocks, so don't assert its value).
    s2.exec("BEGIN NULL; END;").expect("plsql");
    s2.exec(&format!("DROP TABLE {table}")).expect("drop");
}
