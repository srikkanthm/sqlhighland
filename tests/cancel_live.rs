//! Opt-in validation for the forked plain-TCP cancel API.
//!
//! Validates that `Connection::cancel_handle()` actually interrupts an
//! in-flight statement and that the connection survives it. Requires a real
//! Oracle database on **plain TCP** (no TCPS/TLS).
//!
//! Run:
//! ```sh
//! cargo test --test cancel_live -- --ignored --nocapture
//! ```
//!
//! Env overrides: `ORACLE_HOST` / `ORACLE_PORT` / `ORACLE_SERVICE` /
//! `ORACLE_USER` / `ORACLE_PWD`, and `CANCEL_SQL` for the statement to
//! interrupt (default `BEGIN dbms_lock.sleep(15); END;` — needs EXECUTE on
//! DBMS_LOCK, which `system` has).

use std::time::{Duration, Instant};

fn config() -> oracledb::Config {
    let user = std::env::var("ORACLE_USER").unwrap_or_else(|_| "system".to_string());
    let password = std::env::var("ORACLE_PWD").unwrap_or_else(|_| "test".to_string());
    let host = std::env::var("ORACLE_HOST").unwrap_or_else(|_| "localhost".to_string());
    let port = std::env::var("ORACLE_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(1521);
    let service = std::env::var("ORACLE_SERVICE").unwrap_or_else(|_| "highlandpdb".to_string());
    oracledb::Config::default()
        .set_credentials(&user, &password)
        .set_connect_string(&format!("{host}:{port}/{service}"))
        .expect("connect string")
}

#[test]
#[ignore = "requires Oracle (plain TCP); validates the forked cancel API"]
fn cancel_interrupts_and_connection_survives() {
    let conn = oracledb::connect(config()).expect("connect");

    // The handle only exists for plain TCP; a TLS connection should error here.
    let cancel = conn
        .cancel_handle()
        .expect("cancel handle should exist for a plain TCP connection");
    println!("server supports OOB break: {}", conn.supports_oob());

    // Baseline: connection usable, and the statement below is interruptible.
    conn.execute("SELECT 1 FROM dual", &[])
        .expect("baseline execute");

    let sql = std::env::var("CANCEL_SQL")
        .unwrap_or_else(|_| "BEGIN dbms_lock.sleep(15); END;".to_string());
    println!("cancellation target: {sql}");

    // Run the long statement on a worker that owns the connection and hands it
    // back, so we can test reuse afterwards.
    let worker = std::thread::spawn(move || {
        let started = Instant::now();
        let result = conn.execute(&sql, &[]);
        (conn, result, started.elapsed())
    });

    // Let the statement reach the server, then interrupt it.
    std::thread::sleep(Duration::from_secs(2));
    let cancel_started = Instant::now();
    cancel.cancel().expect("cancel request sent");
    let cancel_send = cancel_started.elapsed();

    let (conn, result, query_elapsed) = worker.join().expect("worker thread");
    println!("cancel() returned in {cancel_send:?}; statement returned in {query_elapsed:?}");

    assert!(
        query_elapsed < Duration::from_secs(6),
        "statement did not return promptly after cancel: {query_elapsed:?}"
    );

    match result {
        Ok(exec) => panic!(
            "statement unexpectedly succeeded ({} rows affected) — cancel did not interrupt",
            exec.rows_affected()
        ),
        Err(e) => {
            println!("error kind: {:?}", e.kind());
            println!("error: {e}");
            let kind = format!("{:?}", e.kind());
            let msg = e.to_string().to_lowercase();
            assert!(
                kind.contains("Cancelled") || msg.contains("ora-01013") || msg.contains("cancel"),
                "unexpected error after cancel: {e}"
            );
        }
    }

    // A cancelled statement must not poison the connection.
    conn.execute("SELECT 1 FROM dual", &[])
        .expect("connection should still be usable after cancel");
    println!("connection reusable after cancel: OK");
}
