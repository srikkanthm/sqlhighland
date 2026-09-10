# SQLHighland

A native Oracle SQL client for macOS, built with Rust + GPUI. Connect to an
Oracle database, write queries in a syntax-highlighted editor, and page
through results that fetch on demand as you scroll.

Oracle-only by design. Currently a working MVP under active development —
see `PROGRESS.md` for the build log and `PLAN.md` for the original plan.

## Features

- **Connections sidebar** — collapsible, resizable, persisted list. Add/Edit
  via dialog; per-row right-click menu (Connect/Disconnect, Edit…, Delete).
  Live connection marked with a green dot.
- **Query editor** — tree-sitter SQL highlighting, **Format** button
  (`sqlformat`), **Cmd+Enter** runs the statement under the cursor
  (multi-statement scripts supported client-side).
- **Results grid** — virtualized table, `NULL` styling, error banner.
- **Incremental fetching** — first 1000 rows load immediately, then 1000-row
  pages append as you scroll near the bottom. Server-side cursor stays open
  (no re-execution); 100,000-row memory cap with notice.
- **Status bar** — action status left (`Running…` / `N rows · M ms` /
  `Fetching more…`), connection status right.

## Prerequisites

- macOS with full Xcode (App Store) — GPUI renders via Metal:
  ```sh
  sudo xcode-select --switch /Applications/Xcode.app/Contents/Developer
  xcodebuild -downloadComponent MetalToolchain
  ```
- Rust ≥ 1.89 via rustup (`rustc 1.98` used here).
- An Oracle database (19c+). Tested against a local container:
  `localhost:1521/highlandpdb`, user `system`.

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
Connections persist to `~/.config/sqlhighland/connections.toml`
(plaintext passwords — keychain integration is planned).

Try: `SELECT level AS n FROM dual CONNECT BY level <= 5000;` then scroll
to watch on-demand fetching kick in.

## Developing

```sh
cargo check                     # lib only, no Metal toolchain needed
cargo test --lib                # 17 unit tests (no DB required)
cargo test --test live          # 5 integration tests, needs Oracle up
cargo clippy --features gui --all-targets   # must stay clean
```

GUI code lives behind the `gui` feature so DB logic and tests build without
Xcode/Metal. Pinned dependency highlights: `oracledb 26.0.0-beta.3` (official
thin driver, no Instant Client), `gpui-pre 0.3.4`, `gpui-kit 0.6.1`.

## Troubleshooting

- `xcrun: error: unable to find utility "metal"` → Xcode isn't selected
  (see Prerequisites).
- `cannot execute tool 'metal' … MetalToolchain` → run the
  `xcodebuild -downloadComponent` step above.
- `failed to select a version for 'cc'` → the SQL grammar pin needs the
  older line: `cargo update -p cc --precise 1.2.67`.

## Roadmap

Single statement execution is statement-at-cursor (scripts with multiple
statements run one at a time); multi-session connections, query history,
CSV export, schema browser, persisted pane sizes, and keychain secret
storage are all future work.
