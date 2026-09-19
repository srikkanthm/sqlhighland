# SQLHighland

A native, cross-platform SQL client built with Rust + GPUI. Connect to a
database, write queries in a syntax-highlighted editor, and page through
results that fetch on demand as you scroll.

macOS is the current target (GPUI renders via Metal there); Windows and Linux
are planned. Oracle is the first supported engine — the driver (`DbClient`)
and schema (`SchemaProvider`) layers are built as engine seams, so more can
plug in without reshaping the app. Currently a working MVP under active
development — see [`docs/PROGRESS.md`](docs/PROGRESS.md) for the build log and
[`docs/HISTORY.md`](docs/HISTORY.md) for the original plan and design notes.

## Features

- **Connections sidebar** — collapsible, resizable, persisted list. Add/Edit
  via dialog; per-row right-click menu (Connect/Disconnect, Refresh
  suggestions, Edit…, Delete). Live connection marked with a green dot.
  Environment tags (Prod/Dev/QA/UAT) plus per-engine connection options (Oracle
  role / service-vs-SID, TLS). The dialog has a **Test connection** button that
  connects with the current form values (throwaway session) before you save.
  Disconnect the active tab's connection with `Shift+Cmd+D`.
- **Query editor** — tree-sitter SQL highlighting, code folding, **live syntax
  and structure diagnostics** (squiggles with hover messages), **Format**
  (`sqlformat`), and **Cmd+Enter** to run either the selection (when one exists)
  or the statement under the cursor (multi-statement scripts supported
  client-side).
- **Tabs** — one editor per tab with neutral `Untitled N` titles. **Drag a tab**
  to reorder it (or Move Tab Left/Right, `Cmd+Alt+←/→`), close with the `×` (an
  in-memory tab with text prompts Save As / Discard / Cancel), and `+` opens a
  new one. Order, the active tab, and drafts survive relaunch.
- **Autocomplete & hover** — scope-aware, dialect-ready dictionary completion:
  tables, columns, sequences, synonyms, materialized views, package members,
  CTE/subquery columns, JOIN…ON from foreign keys, and expression contexts
  (`CAST(x AS |)`, `OVER (|)`, `USING (|)`, subqueries, join tails). An empty
  `SELECT` list offers tables and inserts `* FROM t `. Table/column hover cards
  are **opt-in** (Settings → Editor → Hover); Cmd-click a table for its
  `DESCRIBE`. A connection's dictionary can optionally be cached on disk so
  suggestions survive a restart (off by default).
- **Schema browser** — per-connection tree of schemas, tables, views, and
  sequences, with a client-side filter; can load from the on-disk cache.
- **Results grid** — virtualized table with `NULL` styling, an error banner,
  bold headers, **cell/row/column selection** with native copy, and
  **server-side column sorting** (double-click a header: Asc → Desc → clear).
  Right-click for CSV/Excel export and **Count rows**. Row height is a slider in
  Settings → Results.
- **Incremental fetching** — rows load in pages as you scroll near the bottom;
  the page size is configurable (Settings → Results → Rows → Fetch size, default
  50) and the server-side cursor stays open (no re-execution). The grid row cap
  is configurable (blank/0 = unlimited).
- **Export** — CSV and native `.xlsx` (with the exported SQL on a `query`
  sheet), always the **full** result set, streamed with constant memory. Uses
  the grid buffer when it already holds every row; otherwise it re-executes the
  query on its **own session**, so the grid and other tabs stay usable. Export
  has its own fetch size (default 1000).
- **Count rows** — right-click the grid to run `SELECT COUNT(*)` over the
  current query on a **separate session**, so the grid and its open cursor are
  left untouched; the result appears in a popup.
- **Safe exit** — closing a tab, disconnecting, or quitting with uncommitted DML
  prompts to commit or roll back; the red window button follows the same path.
  Unsaved external SQL files are guarded too (in-memory drafts still autosave).
- **Query cancellation** — Cancel on a running query/script/export sends a
  real server-side interrupt on plain-TCP Oracle sessions (forked driver, see
  [`docs/CANCELLATION.md`](docs/CANCELLATION.md)); it falls back to
  client-side abandon when no interrupt is available (TLS, other engines).
- **Secret storage** — per-connection password mode: plaintext `File` (legacy),
  OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret
  Service), or **Ask every time**. App
  state is written owner-only (`0600` files in a `0700` dir) and fsynced.
- **Settings** (`⌘,`) — interface density (Compact/Comfortable); theme and
  editor font are **filterable dropdowns**; the results row cap, query timeout,
  fetch sizes, and suggestions refresh are free-form fields (blank/0 = never /
  unlimited); the CSV delimiter is a field with quick presets; row height,
  syntax checking, hover, and the on-disk cache cap all live here. JetBrains
  Mono, Fira Code, Cascadia Code, and Hack are embedded in the app; the
  remaining families are offered only when installed, so a pick never renders in
  a substitute font.
- **Status bar** — action status left (`Running…` / `N rows · M ms` /
  `Fetching more…`) with Cancel while a run/export is in flight, connection
  status right.

## Prerequisites

- An **Apple Silicon** Mac (arm64). Intel Macs are not supported.
- The current **macOS** build needs full Xcode (App Store) — GPUI renders via
  Metal:
  ```sh
  sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
  xcodebuild -downloadComponent MetalToolchain
  ```
  Windows/Linux GUI builds are planned, not yet wired up.
- Rust ≥ 1.89 via rustup (`rustc 1.98` used here).
- A database. Oracle 19c+ is supported today; tested against a local
  container: `localhost:1521/highlandpdb`, user `system`.

## Quick start

```sh
cargo run --features gui        # debug build (fine for iterating)
```

For real use, always run the release binary — debug GPUI-on-Metal is
noticeably sluggish:

```sh
cargo build --release --features gui
./target/release/sqlhighland
```

On first launch add a connection with **+** in the sidebar
(e.g. host `localhost`, port `1521`, service `highlandpdb`, user `system`).
Connections persist to `~/.config/sqlhighland/connections.toml`. Passwords can
be kept in the OS keychain or prompted per run; the legacy plaintext
`File` mode stores the secret owner-only (`0600`).

Try: `SELECT level AS n FROM dual CONNECT BY level <= 5000;` then scroll
to watch on-demand fetching kick in.

## Configuration files

All app state lives in the platform config dir — `~/.config/sqlhighland/` on
macOS/Linux, `%APPDATA%\sqlhighland\` on Windows:

| File | Contents |
|---|---|
| `connections.toml` | saved connections: host/port/service, role, TLS, password mode, per-connection options |
| `preferences.toml` | theme, density, editor font/syntax, completion, fetch sizes, timeout, cache caps |
| `tabs.toml` + `tabs/<id>.sql` | open-tabs manifest (order + active tab) and per-tab autosaved in-memory drafts |
| `metadata/<conn_id>.json.gz` | opt-in cached dictionary per connection (only when "Cache suggestions on disk" is on) |
| `usage.toml` | suggestion usage counts, so frequency ranking survives relaunch |

Passwords are kept out of these files by default (OS keychain or per-run
prompt); only the legacy plaintext `File` mode stores one. Everything is
written owner-only (`0600` files in a `0700` directory).

## Developing

```sh
cargo check                     # lib only, no Metal toolchain needed
cargo test --lib                # 160 unit tests (no DB required)
cargo test --test live          # 12 integration tests, needs Oracle up
cargo test --test cancel_live -- --ignored   # interrupt test, needs plain-TCP Oracle
cargo test --features gui-test --test menus --test browser_tree --test themes \
    --test fonts --test tab_nav --test quit_guard --test completion_scope \
    --test sql_diagnostics --test ui_picker   # headless UI tests (53)
cargo clippy --features gui-test --all-targets   # must stay clean
```

Most GUI-test suites serialize access to the process-global config dir with an
in-process lock. `tests/browser_tree.rs` does not yet, so run that binary with
`--test-threads=1` until the race is fixed — see
[`docs/TODO.md`](docs/TODO.md) (which also tracks wiring the newer suites into
CI).

GUI code lives behind the `gui` feature so DB logic and tests build without
Xcode/Metal. The headless UI tests use `gui-test` (gui + the kit's
`test-support`) but need no window server. Pinned dependency highlights: `oracledb 26.0.0-beta.4` via a
pinned git patch to `srikkanthm/rust-oracledb` (adds plain-TCP cancellation;
see [`docs/CANCELLATION.md`](docs/CANCELLATION.md)), `gpui-pre 0.3.4`,
`gpui-kit 0.6.1`.

## Packaging (macOS)

Bundle a native `.app` + `.dmg` with
[cargo-packager](https://github.com/crabnebula-dev/cargo-packager):

```sh
cargo install cargo-packager --locked
cargo packager --release
```

Artifacts land in `dist/` (git-ignored): `SQLHighland.app` and
`SQLHighland_<version>_<arch>.dmg`. Builds target **Apple Silicon
(arm64) only** — no Intel or universal binary. Config is in
`[package.metadata.packager]` (Cargo.toml); the app icon source is
`assets/icon/icon.svg`. Output is intentionally **unsigned** — see
[`docs/PACKAGING.md`](docs/PACKAGING.md) for the Gatekeeper/`xattr` note, the
icon pipeline, and signing steps if ever needed.

Releases are automated: push a `v*` tag (matching `Cargo.toml`'s version) and
[`.github/workflows/release.yml`](.github/workflows/release.yml) builds the DMG
on an arm64 macOS runner and attaches it (plus `SHA256SUMS.txt`) to the GitHub
Release.

## Troubleshooting

- `xcrun: error: unable to find utility "metal"` → Xcode isn't selected
  (see Prerequisites).
- `cannot execute tool 'metal' … MetalToolchain` → run the
  `xcodebuild -downloadComponent` step above.
- `failed to select a version for 'cc'` → the SQL grammar pin needs the
  older line: `cargo update -p cc --precise 1.2.67`.
- `stripping debug info with 'rust-objcopy' failed … Library not loaded:
  @rpath/libLLVM.dylib` → the `llvm-tools` rustup component is half
  installed (stale `rust-objcopy`, missing library). The build itself
  succeeds — only the strip step is skipped, leaving a larger binary.
  Repair with:
  ```sh
  rustup component add llvm-tools
  touch src/main.rs && cargo build --features gui --release
  ```

## Documentation

- [`GPUI.md`](GPUI.md) — portable guide to building GPUI apps (patterns + pitfalls), reusable for new projects.
- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, layering, and the code-split pattern.
- [`docs/PROGRESS.md`](docs/PROGRESS.md) — chronological build log.
- [`docs/HISTORY.md`](docs/HISTORY.md) — original plan, shipped feature designs, completed refactor.
- [`docs/REVIEW.md`](docs/REVIEW.md) — code, security, and organization review with fix status.
- [`docs/CANCELLATION.md`](docs/CANCELLATION.md) — forked-driver query cancellation and upstream revert guide.
- [`docs/PACKAGING.md`](docs/PACKAGING.md) — macOS `.app`/`.dmg` bundling, icon pipeline, signing notes.
- [`docs/AUTOCOMPLETE.md`](docs/AUTOCOMPLETE.md) — completion engine design, scope/context reference, and remaining gaps.
- [`docs/AUTOCOMPLETE_ISSUE.md`](docs/AUTOCOMPLETE_ISSUE.md) — open: autocomplete intermittently dies (diagnosis + proposed fix; distinct from the stale-offset wedge fixed in 2026-09-17).
- [`docs/TODO.md`](docs/TODO.md) — backlog specs: connection keep-alive, TNS aliases/connect descriptors/properties.
- [`docs/TNS.md`](docs/TNS.md) — research and plan for connection-string modes and Oracle Net/session properties.
- [`docs/IDLE_CPU.md`](docs/IDLE_CPU.md) — idle-CPU investigation (no app-side spin; toolkit frame pump/animation notes).
- [`docs/TTC_DESYNC.md`](docs/TTC_DESYNC.md) — intermittent ANO response-stream desync: analysis and diagnostics.

## Roadmap

Execution is statement-at-cursor, or the selection when one exists; scripts run
one statement at a time. Sidebar width persists; the editor/results split size
does not yet. Still open: multi-session connections, query history, persisted
pane sizes, connection keep-alive, and connection-string modes (TNS aliases /
connect descriptors / properties) — see [`docs/TODO.md`](docs/TODO.md).
