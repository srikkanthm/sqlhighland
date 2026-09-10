# SQLHighland — Build Log

Oracle-only SQL GUI client in Rust + GPUI (macOS v1). Started 2026-09-08.
Plan: `PLAN.md`. Status: working MVP — connect, edit, run, page through results.

## Stack (all pinned)

| Crate | Version | Notes |
|---|---|---|
| `oracledb` | `26.0.0-beta.3` | Official pure-Rust thin driver, no Instant Client. Beta — API churn expected, isolated in `db.rs` |
| `gpui` (`gpui-pre`) | `=0.3.4` | Zed snapshot from crates.io |
| `gpui-kit` | `=0.6.1` + `tree-sitter-sql` | Component library (sidebar/dialog/table/editor). Grammar feature required or the editor is plain text |
| `gpui-kit-assets` | `=0.6.1` | `AllAssets` bundle registered at startup; the default bundle lacks Database/Plug/etc. icons |
| `sqlformat` | `=0.5.0` | Query formatting (uppercase keywords, 2-space indent) |
| `uuid` v4 | `1` | Stable ids for saved connections |
| `anyhow`, `serde`, `toml` | — | Errors, saved-connections file |

Rust ≥ 1.89 (toolchain: 1.98). macOS-only. Full Xcode + Metal toolchain required
(`sudo xcode-select --switch /Applications/Xcode.app/...`, `xcodebuild
-downloadComponent MetalToolchain`).

## Layout

```
src/lib.rs      # library: config, db, model, sql (GUI-free, fully testable)
src/main.rs     # thin GPUI bootstrap (window + Root + dialog layer)
src/app.rs      # all UI: sidebar, editor, results, status bar, dialogs
src/db.rs       # DbClient trait + OracledbSession (blocking; bg executor only)
src/model.rs    # ConnectionConfig (+ stable id), ColumnInfo, QueryResult
src/sql.rs      # format_sql + statement splitter (pure, unit-tested)
src/config.rs   # SavedConfig TOML load/save (~/.config/sqlhighland/connections.toml)
tests/live.rs   # integration tests against a real Oracle DB
```

`gui` cargo feature gates all GPUI deps: plain `cargo test` never touches the
Metal toolchain; the app builds/runs with `cargo run --features gui`.
Always run the **release** binary for real use (`./target/release/sqlhighland`) —
debug GPUI-on-Metal is sluggish (hover lag, stuttering dividers).

## What was built

- **M0 toolchain** — rustup stable, Xcode CLT + Metal toolchain, pinned deps.
- **M1 shell** — GPUI window via `gpui-kit`, `Root` + dialog layer.
- **M2 DB layer** — `connect`/`query` over thin driver, per-type cell rendering
  (NUMBER, floats, boolean, timestamps, intervals, RAW hex, JSON/Vector debug
  previews, LOB size placeholders — beta.3 `Lob` has no read API, `Vector`
  exposes no accessors), EZCONNECT builder, row cap.
- **Connections sidebar** — collapsible, resizable (180–480px), persisted
  multi-connection list. Add/Edit via modal dialog (not inline). Per-row
  right-click menu: Connect/Disconnect, Edit…, Delete. Live row marked with a
  left green dot matched by stable connection **id** (not host:port, so
  duplicate targets don't double-light). Single session auto-switches; deleting
  the live entry disconnects it. `connected_id` is the seam for multi-session.
- **Query editor** — kit `Editor` with tree-sitter SQL highlighting, Format
  button, **Cmd+Enter runs the statement under the cursor** (focus-guarded so
  dialog fields don't trigger it). Statement splitter respects strings,
  quotes, `--`/`/* */` comments; caret past end re-runs last, blank line
  prefers next. Run button follows the same path.
- **Results grid** — virtualized `DataTable`, NULL styling, error banner.
- **Incremental fetching (core)** — initial 1000 rows, then on-demand 1000-row
  pages as scrolling nears the bottom (200-row trigger), appended in place.
  Server-side cursor held open (owned `Cursor`, no re-execution); one-row
  lookahead detects end-of-data; 100,000-row memory cap with notice; stale
  generations discarded via query ids; fetch errors banner + retry on scroll.
- **Status bar** — action status left (`Running…` / `N rows · M ms` /
  `Fetching more…`), single connection status right. (Connection text had two
  sources once — fixed by construction; `connected_to` field deleted.)

## Bugs fixed along the way

- Trailing `;` rejected by Oracle (ORA-00933/01003) → `sanitize_statement`.
- Blank icons → default asset bundle is a subset; registered `AllAssets`.
- Missing SQL highlighting → `tree-sitter-sql` feature was off; also resolved
  a `cc` version conflict via `cargo update -p cc` (`tree-sitter-sequel 0.3.11`).
- Resizable panels fought the layout → `flex_none()` + wider ranges.
- Switched zed-git deps to crates.io (`gpui-pre`/`gpui-kit`) — reproducible,
  no multi-GB clone. (`gpui-pre` is published by the kit maintainer, not Zed.)

## Tests — 17 unit + 5 live, all passing

- `cargo test --lib` — EZCONNECT builder, TOML round-trip, `Send` bounds for
  bg tasks, sanitizer cases, formatter + 9 splitter cases, id stability,
  result summaries.
- `cargo test --test live` — needs `highlanddb` on `localhost:1521`
  (`system`/`test` @ `highlandpdb`): connect, type coverage, truncation,
  semicolon regression, **2500-row paging (1000/1000/500 + exhaustion +
  stale-id discard)**.
- GUI verified by compile + `clippy` (clean) + launch smoke tests
  (debug + release, zero panics). No GUI automation — visual pass is manual.

## Known limitations / next

- Single statement per run (multi-statement scripts fail server-side).
- Passwords in plaintext TOML (documented debt → OS keychain).
- Splitter doesn't understand Oracle `q'[...]'` quoting.
- Pane sizes are session-only (not persisted); no query history; no export;
  no schema browser; single active session.
