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
