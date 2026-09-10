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
- **Results grid** — virtualized `DataTable`, NULL styling, leading 1-based row-number column (muted, right-aligned; excluded from CSV copy).
- **Incremental fetching (core)** — initial 1000 rows, then on-demand 1000-row
  pages as scrolling nears the bottom (200-row trigger), appended in place.
  Server-side cursor held open (owned `Cursor`, no re-execution); one-row
  lookahead detects end-of-data; 100,000-row memory cap with notice; stale
  generations discarded via query ids; fetch errors banner + retry on scroll.
- **Status bar** — action status left (`Running…` / `N rows · M ms` /
  `Fetching more…`), single connection status right. (Connection text had two
  sources once — fixed by construction; `connected_to` field deleted.)
- **Client-command emulation** — `DESCRIBE`/`DESC` is a SQL*Plus command, not
  SQL (server rejects it with ORA-00900). The app rewrites it to an
  `ALL_TAB_COLUMNS` query (Name / Null? / Type, with length/precision
  decoration), supporting schema-qualified and quoted names.
- **Full statement support** — the driver has no SQL*Plus layer (verified:
  zero `sqlplus` references in its source) but full PL/SQL via `execute()`.
  Run routes by leading keyword (`SELECT`/`WITH…SELECT` → paged query path,
  everything else → execute with verb-specific summaries: "1 row inserted",
  "Table EMP created", "PL/SQL block executed"). The splitter is
  BEGIN/DECLARE/END depth-aware so PL/SQL blocks stay whole, and the
  sanitizer preserves the `;` blocks require. Manual-commit model (driver
  never autocommits): Commit/Rollback buttons in the query header plus an
  amber `● Uncommitted` indicator; transaction state is per-connection.
- **SQL Developer statement parity** — Cmd+Enter resolves like the worksheet:
  cursor anywhere on a statement (including just after its `;`) runs it,
  blank lines prefer the next statement, and a `/` alone on a line terminates
  the preceding statement (required after PL/SQL, allowed without `;`).
- **Query header** — icon buttons (Play/Format/Wand, Check/Commit,
  Undo2/Rollback) with tooltips and shortcuts: `⌘↵` run, `⇧⌥F` format
  (`⌘⇧F` is taken by the editor's Replace), `⇧⌘C` commit, `⇧⌘R` rollback.
  The tab's connection picker is an outlined Database button with a live
  dot — no longer mistaken for plain text.
- **Status bar: single source of truth** — connection *state* is derived live
  from the session pool every render (dot + per-tab text); the center slot
  holds only transient event notices, so contradictory "Disconnected" /
  "Connected to …" text is impossible by construction.
- **Output pane** — the results area shows the tab's latest action outcome:
  failures red ("Query failed"), DML/DDL confirmations neutral ("Statement
  executed", e.g. "1 row inserted · 3 ms"); successful SELECTs
  show the grid. A new run returns automatically; Dismiss reveals prior
  results without re-running. The status line stamps failures too (`Failed ·
  12 ms`, `Fetch failed — scroll to retry`) instead of showing the previous
  run's success summary.
- **Run lifecycle** — live `Running… Ns` status (500 ms ticker), and Cancel.
  The driver has no break API (verified in its source), so Cancel is
  client-side abandon: a per-tab run token is bumped, late completions are
  discarded, and the server finishes in the background. Same-connection
  re-runs queue behind the abandoned worker's session lock, then proceed
  normally with the fresh token.
- **Render path never blocks on sessions** — connection liveness is cached in
  view state (`live` set, updated on connect/disconnect/run outcomes) because
  locking a session mutex during render froze the whole UI behind in-flight
   queries (`std` Mutex blocks, never yields). `pool.remove` uses `try_lock`,
   and the execute path carries its generation id out of the bg task instead
   of locking on the UI thread.
- **Settings + themes** — `⌘,` (or the sidebar gear) opens a Settings dialog
  built on the kit's Settings shell: sidebar nav with **Themes** (flat list —
  System + Default/Nord/Catppuccin-Latte-Frappé-Macchiato-Mocha/Solarized
  light+dark) and **About** (version, driver note, config file locations)
  sections, ready for more pages. Picks apply immediately via the kit's own
  apply-then-switch order and persist to `preferences.toml`;
  System follows the OS via a window appearance observer. Legacy
  family+mode pref files migrate to their concrete variant. All hardcoded
  UI colors migrated to theme tokens, so every family renders correctly.
  Custom `TitleBar` (kit component + `TitleBar::window_options()`) replaces
  the native macOS bar, which follows the OS appearance instead of app
  themes; it sits atop the main column and re-renders with the view.
  Repaint root cause, found via the retained-render model: GPUI only
  re-renders dirty views, and the dialog refactor had dropped the only
  `notify()` on the main view — self-notifying entities (editor, grid)
  updated on interaction while everything else stayed stale. Theme picks now
  notify the view through a handle stashed in an App global (window-level
  handlers have no view otherwise). Cmd+, is handled at window level via a
  global action listener (element handlers miss modals, which live in a
  sibling layer); the dialog shows the live applied name so apply vs. repaint
  failures are distinguishable.

## Bugs fixed along the way

- Trailing `;` rejected by Oracle (ORA-00933/01003) → `sanitize_statement`.
- Blank icons → default asset bundle is a subset; registered `AllAssets`.
- Missing SQL highlighting → `tree-sitter-sql` feature was off; also resolved
  a `cc` version conflict via `cargo update -p cc` (`tree-sitter-sequel 0.3.11`).
- Resizable panels fought the layout → `flex_none()` + wider ranges.
- Switched zed-git deps to crates.io (`gpui-pre`/`gpui-kit`) — reproducible,
  no multi-GB clone. (`gpui-pre` is published by the kit maintainer, not Zed.)

## Tests — 21 unit + 5 live, all passing

- `cargo test --lib` — EZCONNECT builder, TOML round-trips (connections +
  tabs manifest/drafts), `Send` bounds for bg tasks, sanitizer cases,
  formatter + 9 splitter cases, id stability, result summaries, tab naming,
  session-pool sharing.
- `cargo test --test live` — needs `highlanddb` on `localhost:1521`
  (`system`/`test` @ `highlandpdb`): connect, type coverage, truncation,
  semicolon regression, **2500-row paging (1000/1000/500 + exhaustion +
  stale-id discard)**.
- GUI verified by compile + `clippy` (clean) + launch smoke tests
  (debug + release, zero panics). No GUI automation — visual pass is manual.

## Tabs + sessions + autosave (2026-09-10)

- One live session per saved connection (`session.rs` pool keyed by
  connection id), shared by tabs; each tab binds to a connection via a
  dropdown in the query header. Sidebar rows eager-connect/disconnect;
  Run auto-connects lazily. Same-connection tabs share one cursor —
  superseded tabs are marked exhausted via generation guards.
- Tab bar (`TabBar` + close `X` + `+`), per-tab editor/grid/meta/error/busy.
  Tab names auto-derive from first SQL line.
- Auto-save: debounced (1.5s) drafts to `~/.config/sqlhighland/tabs/<id>.sql`
  + `tabs.toml` manifest; relaunch restores tabs. No dirty prompts by design.

## Known limitations / next

- Single statement per run (multi-statement scripts fail server-side).
- Passwords in plaintext TOML (documented debt → OS keychain).
- Splitter doesn't understand Oracle `q'[...]'` quoting.
- Pane sizes are session-only (not persisted); no query history; no export;
  no schema browser; single active session.
