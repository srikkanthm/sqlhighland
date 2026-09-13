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
  via dialog; per-row right-click menu (Connect/Disconnect, Edit…, Delete).
  Live connection marked with a green dot. Environment tags (Prod/Dev/QA/UAT)
  plus per-engine connection options (Oracle role / service-vs-SID, TLS).
- **Query editor** — tree-sitter SQL highlighting, code folding, **Format**
  button (`sqlformat`), **Cmd+Enter** runs the statement under the cursor
  (multi-statement scripts supported client-side).
- **Autocomplete & hover** — dictionary-backed table/column/sequence
  suggestions, JOIN…ON completion from foreign keys, and table/column hover
  cards. Also Cmd-click a table for its `DESCRIBE`.
- **Schema browser** — per-connection tree of schemas, tables, views, and
  sequences, with a client-side filter.
- **Results grid** — virtualized table, `NULL` styling, error banner.
- **Incremental fetching** — first 1000 rows load immediately, then 1000-row
  pages append as you scroll near the bottom. Server-side cursor stays open
  (no re-execution); 100,000-row memory cap with notice.
- **Export** — CSV and native `.xlsx` (with the exported SQL on a `query`
  sheet), uncapped, streamed with constant memory.
- **Secret storage** — per-connection password mode: plaintext `File` (legacy),
  OS keychain (macOS today; Windows/Linux planned), or **Ask every time**. App
  state is written owner-only (`0600` files in a `0700` dir) and fsynced.
- **Status bar** — action status left (`Running…` / `N rows · M ms` /
  `Fetching more…`), connection status right.

## Prerequisites

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
be kept in the macOS login keychain or prompted per run; the legacy plaintext
`File` mode stores the secret owner-only (`0600`).

Try: `SELECT level AS n FROM dual CONNECT BY level <= 5000;` then scroll
to watch on-demand fetching kick in.

## Developing

```sh
cargo check                     # lib only, no Metal toolchain needed
cargo test --lib                # 116 unit tests (no DB required)
cargo test --test live          # 11 integration tests, needs Oracle up
cargo test --features gui --test menus --test ui_picker \
    --test browser_tree --test themes   # headless UI tests
cargo clippy --features gui --all-targets   # must stay clean
```

GUI code lives behind the `gui` feature so DB logic and tests build without
Xcode/Metal. The headless UI tests also require `--features gui`, but no
window server. Pinned dependency highlights: `oracledb 26.0.0-beta.3` (official
thin driver, no Instant Client), `gpui-pre 0.3.4`, `gpui-kit 0.6.1`.

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

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — module map, layering, and the code-split pattern.
- [`docs/PROGRESS.md`](docs/PROGRESS.md) — chronological build log.
- [`docs/HISTORY.md`](docs/HISTORY.md) — original plan, shipped feature designs, completed refactor.
- [`docs/REVIEW.md`](docs/REVIEW.md) — code, security, and organization review with fix status.

## Roadmap

Single statement execution is statement-at-cursor (scripts with multiple
statements run one at a time). Still open: multi-session connections, query
history, and persisted pane sizes.
