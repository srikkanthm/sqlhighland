# SQLHighland — Plan

SQL GUI client in Rust + Zed GPUI, Oracle DB only for v1.

Approved 2026-09-09. Scope: **macOS-only, official `oracledb` 26.0.0-beta.x
pure-Rust thin driver, minimal MVP (connect + query + results),
basic host:port/service + user/pass.**

## 1. Goals / non-goals

Goals (v1):
- Connect to Oracle (19c / 21c / 26ai, on-prem or cloud, TCP, no wallet).
- Run ad-hoc SQL, show tabular results + errors + elapsed/rowcount.
- Native-feeling macOS window, GPU-accelerated via GPUI.

Non-goals (deferred to v2+):
- Schema browser tree, multi-tab / multi-connection, explain plan.
- CSV export, query history persistence, syntax highlighting / tree-sitter editor.
- Wallet / TCPS / EZCONNECT extras, OS keychain (passwords in local file in v1 — known debt).
- Linux / Windows ports.

## 2. Tech choices

- **UI:** `gpui` + `gpui_platform` (same pinned git `rev`, `font-kit` feature on macOS),
  plus `longbridge/gpui-component` (`Root`, `Input`, `Button`, `Table`/`DataTable`).
  `Root` is required as first-level child of every window.
- **DB:** official `oracledb = "26.0.0-beta.x"` thin driver (no Instant Client).
  Isolate behind a `DbClient` trait in one module so beta churn stays contained
  and a thick driver could be added later.
- **Async:** `oracledb` is blocking → run on `cx.background_spawn` /
  background thread, marshal back via `weak_handle.upgrade()` + `update()` +
  `cx.notify()`. Never mutate `View` state off the main thread. Store `Task`
  handle so re-run / cancel drops the old task.
- **MSRV:** Rust 1.89+ (required by `oracledb` beta). Stable toolchain via rustup.
- **System prereqs (macOS):** Xcode + `xcode-select --install`, Metal-capable Mac.

## 3. Architecture

Single Cargo binary to start, layered internally:

```
SQLHighland/
  PLAN.md
  Cargo.toml
  src/
    main.rs      # gpui_platform::application().run(), open window, Root::new
    app.rs       # RootView: connection bar + editor + results + status
    db.rs        # DbClient trait + OracledbClient (blocking, pooled single conn)
    model.rs     # ConnectionConfig, QueryResult { columns, rows, elapsed }, DbValue
    views/
      connect.rs # host/port/service/user/password Inputs + Connect button
      editor.rs  # multi-line Input, Run (Cmd+Enter), cancel
      results.rs # DataTable delegate over QueryResult, error banner
    config.rs    # local TOML/JSON saved connections (plaintext pw in v1)
```

Data flow:

```
Run click → QueryState::Running → cx.background_spawn(blocking oracledb call, max_rows cap)
  → upgrade weak handle → update(Loaded|Error) → notify → DataTable re-render
```

- `DbValue`: `Null | String | I64 | F64 | Bool | Date(String) | LobPreview(String)`.
  LOB/VECTOR/JSON shown as preview strings in v1.
- Result cap: default 1000 rows (configurable later) to avoid OOM on `SELECT *`.
- Errors: map Oracle errors to status bar + banner, preserve ORA- code text.

## 4. Milestones

- **M0 — Env (0.5d):** Rust 1.89+, Xcode CLT, scaffold, pin GPUI rev, `cargo check`.
- **M1 — App shell:** window + `gpui_component::init`, `Root` wrapper, static
  3-pane layout, hello query box. Done when window renders on macOS.
- **M2 — DB core (no UI polish):** `ConnectionConfig`, `connect()`,
  `SELECT 1 FROM DUAL`, single-conn pool, integration test vs real Oracle,
  Oracle error mapping.
- **M3 — Wire-up:** connect form → status, `Cmd+Enter` runs query in background,
  `DataTable` results + elapsed/rowcount, error banner, cancel on re-run.
- **M4 — Package:** row cap + NULL display, in-memory history, saved-connections
  file, `.app` bundle, README matrix (19c/21c/26ai).

## 5. Risks / debt

1. GPUI + `oracledb` both pre-stable → pin `rev`/version, budget time per bump.
2. Beta type gaps → `DbValue` preview fallback.
3. Plaintext password storage in v1 → warn in UI/docs, keychain in v2.
4. GPUI docs are thin → Zed repo `crates/gpui` + `gpui-component` docs are canon.
