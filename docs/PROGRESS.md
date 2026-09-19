# SQLHighland — Build Log

Cross-platform SQL GUI client in Rust + GPUI (Oracle is the first engine;
macOS is the current target). Started 2026-09-08. Older entries below say
"Oracle-only"/"macOS-only", reflecting the original v1 scope.
Plan: `HISTORY.md` (Part 1). Status: working MVP — connect, edit, run, page
through results, export, schema browser, scoped autocomplete, live diagnostics,
and safe exit (transaction/unsaved-file guards). This file is a chronological
build log: the newest entries are at the bottom, and older entries reflect the
state at the time they were written.

## Stack (all pinned)

| Crate | Version | Notes |
|---|---|---|
| `oracledb` | `26.0.0-beta.4` (fork) | Pure-Rust thin driver, no Instant Client. Pinned via `[patch.crates-io]` to `srikkanthm/rust-oracledb@528a79a` for real query cancellation (see `CANCELLATION.md`). Beta — API churn expected, isolated in `db.rs` |
| `gpui` (`gpui-pre`) | `=0.3.4` | Zed snapshot from crates.io |
| `gpui-kit` | `=0.6.1` + `tree-sitter-sql` | Component library (sidebar/dialog/table/editor). Grammar feature required or the editor is plain text |
| `gpui-kit-assets` | `=0.6.1` | `AllAssets` bundle registered at startup; the default bundle lacks Database/Plug/etc. icons |
| `tree-sitter` + `tree-sitter-sequel` | `0.26` / `0.3` | Live diagnostics + structural scope for completion (gui-only) |
| `sqlformat` | `=0.5.0` | Query formatting (uppercase keywords, 2-space indent) |
| `rust_xlsxwriter` | `=0.99.0` | Native `.xlsx` export (`constant_memory` streaming) |
| `keyring` | `=4.2.0` | OS secret storage (Keychain/Credential Manager/Secret Service) |
| `zeroize` | `1` | Zeroize in-memory secrets on drop |
| `serde_json` + `flate2` | `1` | On-disk suggestions cache (gzipped JSON) |
| `uuid` v4 | `1` | Stable ids for saved connections |
| `anyhow`, `serde`, `toml` | — | Errors, saved-connections file |

Rust ≥ 1.89 (toolchain: 1.98). macOS-only. Full Xcode + Metal toolchain required
(`sudo xcode-select --switch /Applications/Xcode.app/...`, `xcodebuild
-downloadComponent MetalToolchain`).

## Layout

`src/lib.rs` is the module map. The GUI-free core (`config`, `db`, `model`,
`session`, `metadata`, `schema`, `complete`, `sql`, `export`, `filetab`,
`fsutil`, `keychain`, `logging`) builds and tests without the platform
toolchain; GPUI (`app/`, dialogs, `run/`, `providers`, `browser`, `sidebar`,
`sqlparse`, `sqlscope`, `fonts`, `guitheme`) is gated behind the `gui` feature.
`src/main.rs` is a thin launcher over `app::SqlHighlandView`.

See [`ARCHITECTURE.md`](../ARCHITECTURE.md) for the full module map, the
code-split pattern, and the engine/platform seams. `cargo run --features gui`
builds/runs; always use the **release** binary for real use
(`./target/release/sqlhighland`) — debug GPUI-on-Metal is sluggish (hover lag,
stuttering dividers).

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
  normally with the fresh token. *(Superseded 2026-09-13 by real server-side
  cancellation on plain TCP — see the cancellation section below; the abandon
  path remains as the fallback.)*
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

## Code folding (2026-09-11)

- No new parser dep: `datafusion-sqlparser-rs` rejected (heavy, hard-fails on
  PL/SQL blocks/scripts/`&` vars). The kit already parses `tree-sitter-sequel`
  in the background and folds every named node spanning 2+ lines (outermost
  per start line); gutter chevrons need no app code.
- Probe (throwaway test, since removed) confirmed sane regions: subquery,
  per-CTE bodies, `CASE..END`, `BEGIN..END` block, `IN (...)` list, block
  comments; single-line statements yield no folds. No Phase-2 custom
  highlighter needed.
- Folding is gutter-only by user call: a `cmd-alt-right` unfold shortcut was
  added then removed (it no-oped unless the cursor sat on a hidden line, and
  keyboard fold has no kit API — `display_map` is crate-private).
- Verified: clean `clippy`, 43 lib tests, release smoke. Visual pass
  (chevron rendering in both themes) is manual.

## Export CSV/XLSX (2026-09-11)

- Uncapped by design: drains the tab's shared cursor past the grid cap to
  true exhaustion (grid fills with everything too, so paging stays
  consistent — no second execution/snapshot skew).
- Entry points: grid right-click context menu + header Export dropdown
  (`Export as CSV…` / `Export as Excel (.xlsx)…`).
- Flow: native save dialog first (`<tab>-<timestamp>.<ext>`, HOME dir),
  then background drain (1000-row pages) with `Exporting… N rows` status +
  Cancel; writes to a `.part` sibling, renames on success, deletes on
  cancel/failure. Scroll-fetching pauses via held `loading` flag.
- CSV streams line by line through tested RFC-4180 `csv_row`; XLSX via
  pinned `rust_xlsxwriter =0.99.0` (`constant_memory` worksheets, bold
  header, all cells strings, data sheet named after tab) plus a second
  `query` sheet holding the exported SQL (one line per row) as the audit
  trail. Headers included, NULL → empty in both.
- Finish: `Exported N rows (…) to <file>` status; focus stays on the
  results tab (no extra tabs created).
  New runs are blocked while exporting (message, not silent).
- Verified: clean `clippy`, 50 lib + 8 live tests, 2505-row/3-page live
  drain composition check (CSV + XLSX byte-exact), release smoke. Manual
  pass (save dialog, progress, cancel, open .xlsx in Excel) outstanding.

## Picker scroll + UI tests (2026-09-11)

- Root cause of dead picker scrolling: `overflow_y_scrollbar()` keys state
  by caller location and re-ids the inner div, so it misbehaves for dialog
  content rebuilt every render. Replaced with an owned `ScrollHandle` +
  `track_scroll` + `overflow_y_scroll` (the kit's own proven pattern).
- Tracked focus handles need explicit `.tab_stop(true).tab_index(i)` —
  fresh handles default to non-stops and the element's settings only apply
  to auto-created handles. This was the Tab-cycle fix.
- `tests/ui_picker.rs` (gui-gated, `test-support` dev-dep): stages 60
  connections via `SQLHIGHLAND_CONFIG_DIR`, drives the REAL view headless —
  unbound run opens picker, far rows scroll into view, typing filters,
  Enter picks the first match and closes. Plus bare scrollable + editor
  typing regression tests.

## Autocomplete v1 (2026-09-12)

- Plan: `HISTORY.md` (Part 2; locked decisions + queued follow-ups).
- `src/complete.rs` (pure): word prefix + quoted identifiers, qualifier
  detection (`e.` / `scott.emp.`), context classify
  (BareWord/AfterFrom/ColumnOf/SequenceMember/JoinOn), `FROM`/`JOIN` alias
  map, qualifier resolution, ranking (kind → exact → own-schema → prefix →
  usage → length), `is_trivia_position`, `SYSTEM_SCHEMAS` blocklist,
  Oracle keywords, `byte_to_lsp_pos` (UTF-16).
- `src/metadata.rs`: per-connection `MetadataCache` (tables/views, columns
  by `(OWNER,TABLE)`, sequences, FKs, 15-min TTL), blocking dictionary
  fetchers with SQL-level system filter + connected-user exemption.
- `src/app.rs`: `OracleCompleter` installed per tab; snapshot-only
  `completions()` (never touches the leased editor entity);
  `is_completion_trigger` (Auto: 2+ chars or `.`); `TriggerComplete` on
  `ctrl-space`; explicit per-item `textEdit` (kit's sticky fallback range
  replaced whole buffers); `ensure_meta` background refresh (connect/run/
  manual); usage learning; Settings → Editor page (Auto/Manual + system
  schemas toggle, persisted).
- Join suggestions: `JOIN dept d ON |` offers FK-derived
  `e.DEPTNO = d.DEPTNO` (aliases as written, both directions, composites
  via `AND`); first-condition-only, no FK → no popup.
- Strict context gating (2026-09-12): each position offers only what SQL
  allows (start → starters; select list → columns/functions, never tables;
  `FROM` → tables only; predicates → columns; `owner.` after `FROM` →
  that owner's tables). Statement-scoped keyword scan with list/paren/
  qualifier awareness; no new dependencies (tree-sitter-sequel evaluated
  and rejected for detection — ERROR soup on partial input).
- Scope transitions (2026-09-12): clause-following keywords re-added per
  scope (`FROM` after select lists, `ORDER`/`GROUP` after predicates —
  prefix filtering keeps them invisible until typed); empty-prefix popup
  right after operand-expecting keywords (`FROM |` lists tables); JoinOn
  without FK falls back to Predicate columns.
- Hover cards (2026-09-12): `HoverProvider` per tab; table cards (columns
  with types/comments, 30-cap), qualified + unique bare column cards;
  trivia-guarded, silent on unknown/ambiguous.
- Hover owner fix (2026-09-12): `SYSTEM.|EMPLOYEES` showed nothing — the
  qualifier misread as a table name via the alias map, and the system
  filter killed explicitly-written names. Direct owner.table resolution
  with filter bypass for qualified hovers (bare words keep the filter).
- Definition jump (2026-09-12): Cmd-hover underlines table words,
  Cmd-click runs `DESCRIBE owner.table` in the clicked tab (results grid,
  editor untouched). `describe_target` in `complete.rs` (table-only v1;
  columns never jump) + `OracleDefiner` + `show_document` hook on
  `oracle-describe:/` URIs, all installed per tab in `make_tab`.
  Clippy clean, 88 lib + 10 live green, release smoke ALIVE (`cmd_w` UI
  flake pre-existing). Headless harness can't Cmd-click (no
  modifier-click), so this one needs a live click to confirm.
- Schema browser v1 (2026-09-12): per-connection trees under sidebar
  rows (own schema only: Tables/Views/Sequences → objects → columns),
  per-tree filter, auto-connect on expand, release-on-object opens an
  ephemeral viewer tab (DESCRIBE grid for tables/views, catalog row for
  sequences). `schema.rs` model + `SchemaProvider` trait +
  `OracleProvider` over the existing cache (zero new queries);
  `TableId.kind` split (fetch already unioned both);
  `ConnectionConfig.engine` (serde-default Oracle); `TabKind::Query |
  Viewer` (editor kept unrendered, reversible). Debugging notes:
  virtualized tree needed a bounded 320px viewport (size_full in
  auto-height collapses invisible); kit mousedown rebuilds swallow
  `on_click`, so rows trigger on mouse-up; viewer grid needed the same
  resizable-panel sizing as query tabs (flex-only collapses body rows).
  Temp logging stripped. Clippy clean, 94 lib + 10 live green, release
  smoke ALIVE (`cmd_w` UI flake pre-existing).
- Browser auto-reveal on expand (2026-09-12): expanding pins the node
  to top when capped so fresh children show. Two kit ordering traps:
  `Expanded` fires before entries rebuild (defer past it or counts go
  stale), and `scroll_to_item` is non-strict (no-op when the node is
  already visible — exactly the expand case), so strict positioning is
  required. Temp logging stripped.
- Borderless results grid (2026-09-12): `table.row.border` :=
  `table.background` in all 13 variants — rejected in review (too naked).
- Zebra results grid (2026-09-12): `table.even.background` filled in all
  13 (midpoint of table/head tones) + `.stripe(true)` on the grid;
  hairlines stay dissolved for now (one variable at a time — restore if
  zebra alone feels loose).
- Theme surfaces differentiated (2026-09-12): the kit already had
  Zed-style surface slots — the gap was our chrome painting everything
  in base `background` (sidebar/status/headers were transparent). JSONs
  gained `input` everywhere, `table.head.background` on dark variants,
  and subtly off-base `editor.background` in all 8 variants; app chrome
  now reads `sidebar`/`status_bar`/`tab_bar` tokens. New
  `tests/themes.rs` guards all 8 variants (parse + register + apply).
  Clippy clean, 95 lib + 10 live green, release smoke ALIVE.
- Gruvbox + Ayu themes (2026-09-12): converted Zed's official files
  (not from memory) via a slot-mapping script — Gruvbox Dark/Light,
  Ayu Dark/Light/Mirage, 13 variants total. Surface slots, syntax
  (+ float/conditional/storageclass/parameter extras), translucent
  `players[0]` selections, and our 70%-toward-sidebar editor rule all
  carried over; `THEME_LIST` + themes test extended. Clippy clean,
  95 lib + 10 live + themes green, release smoke ALIVE.
- Selection readability + editor distinction (2026-09-12): the kit
  clamps selection alpha to 0.3, so muted slate selections melted away
  app-wide — all 8 variants now use saturated theme blues that survive
  the wash. Editor backgrounds pushed ~70% toward sidebar tones so the
  query window reads as its own region.
- Button + row-state slots (2026-09-12): buttons rendered in kit
  defaults in every theme (no `button_*` slots anywhere) — all 13
  variants now derive button_primary/secondary/danger/info/success/
  warning families (+hover/active 8% steps) from their own kind colors;
  table/list hover + selected-row washes reuse the selection wash;
  state hover/active steps + switch thumbs filled the same way.
  Clippy clean, themes + 95 lib + 10 live green, release smoke ALIVE.
- Editor backgrounds lifted (2026-09-12): per Zed's own files the
  editor anchors the central surface (== active tab == toolbar) while
  chrome steps away in tiers — our 70%-toward-sidebar rule was too timid
  and muddy. Darks now sit at 85% toward panel tone, lights lift toward
  near-white. All 13 variants green.
- Syntax highlighting gaps (2026-09-12): the palettes themselves are
  stock upstream (well-designed) — the gap was 4 captures the SQL
  grammar emits with no theme key (`parameter` binds, `float`,
  `conditional` THEN/ELSE, `storageclass`), which fell back to plain
  foreground. Added per variant mirroring each palette's own families
  (parameter→constant, float→number, conditional/storageclass→keyword);
  identifiers (`variable`/`field`) and operators stay foreground by
  design. Themes test still green.
- Browser UX round 2 (2026-09-12): single-click now only selects —
  double-click (release with `click_count >= 2`) opens the viewer.
  Cmd+W/⌘T/ctrl-tab fixed from sidebar focus: tab actions bubble from
  the focused element up through ancestors only, so the render_main
  listeners never fired with focus in the tree — the four tab actions
  are duplicated on both sidebar roots (dialog paths unaffected).
  Cmd+Q duplicated the same way (quit-from-sidebar).
- Browser UX round 3 (2026-09-12): Other Users folder (own groups at
  root, other schemas nested, zero new queries); per-row type icons
  (Users/User/Table/Eye/Hash/Dot + chevrons); dynamic tree height from
  visible rows (capped 320); per-connection filter fields (18px);
  row context menus (Open description + Copy name; columns Copy name).
  Nesting bug fixed: tree rendered inside the row div shared its hitbox
  and fired both menus — now true siblings (verified in probe paths).
- Scoped Format (2026-09-12): Format button/shortcut now formats only
  the statement at the cursor (same scope as Run) via new
  `statement_at_range` (shared selection logic, plus byte range);
  surrounding whitespace preserved so statements never join, caret
  tracked, already-formatted is a no-op. Viewer tabs guarded.
- Settings UI round (2026-09-12): Cmd+, toggles (second press closes
  the top dialog); search fixed — every item was `SettingItem::render`
  with empty keywords so all filtered out, now all 7 carry keywords;
  About reworded off Oracle-only; completion + system-schemas rows use
  pill toggles (`setting_pill`, env_tag pattern) instead of checks.
- Segmented Suggestions toggle (2026-09-12): Automatic/Manual now one
  on-off pill row (active segment primary, other ghost) instead of two
  rows — single click target, same prefs path.
- Connection settings sextet (2026-09-12): Role pills (SYSDEFAULT/
  SYSDBA/SYSOPER via driver `set_auth_mode`; XA has no driver auth mode
  — open question), Service/SID toggle (colon EZCONNECT form), SSL
  switch (`tcps://` scheme; wallet mTLS later), password modes
  (File/Keychain via login keychain/Ask with session unlock map +
  prompt dialog; keychain entries cleaned on mode-leave/delete),
  Database row (Oracle, forward-compat), delete confirmation dialog.
  Old files migrate via serde defaults. Clippy clean, 101 lib + 10
  live green, release smoke ALIVE (`cmd_w` flake pre-existing).
- Settings trio (2026-09-12): grid row-cap presets (10k–1M, default
  100k, clamped, future runs; exports stay uncapped); CSV delimiter
  presets (comma/semicolon/tab/pipe) + header toggle (new `csv_*_with`
  plumbing, quoting follows the delimiter); editor font family presets
  + size stepper (10–24, stamped over the theme so size survives theme
  switches). New Results page + Editor Font group, all keyworded.
- Native Switch toggles (2026-09-12): per kit Switch component docs —
  Suggestions collapsed to one "Automatic suggestions" switch row and
  system schemas to a Switch (custom pills/segments removed); same
  prefs writes + cache invalidation, checked state follows live prefs.
- Own-schema names go bare (2026-09-12): connected as SYSTEM, `FROM `
   suggests `EMPLOYEES` (not `SYSTEM.EMPLOYEES`) and inserts it bare —
  Oracle resolves unqualified names to the connected schema first, so the
  prefix was noise. New `display_name` helper applied to AfterFrom labels,
  hover card titles/column lines, and the Cmd-click DESCRIBE statement;
  other schemas stay qualified. Clippy clean, 89 lib + 10 live green,
  release smoke ALIVE (`cmd_w` UI flake pre-existing).
- Hover mid-word fix (2026-09-12): diagnostic log showed `word="EMPL"`
  with `md=none` — hover reused completion's `word_prefix` (text *before*
  the cursor only), so any mid-word pointer looked up a truncated name.
  New `word_at` extends forward to word end (hover semantics; completion
  untouched). Confirmed by user in a real session; temp log instrumentation
  removed. Clippy clean, 87 lib + 10 live green, release smoke ALIVE
  (`cmd_w` UI flake pre-existing, reproduces on clean tree).
- Qualify-on-collision + column comments (2026-09-12): columns shared by
  two scope tables complete qualified (`e.DEPTNO`, alias preferred);
  popup detail appends the one-line `ALL_COL_COMMENTS` text (joined fetch,
  capped at 80 chars). Scope resolution factored once per request.
- Function snippets (2026-09-12): 32 built-ins complete as `NAME()` with
  signatures in detail; true tab-stops impossible (no kit snippet engine,
  no post-accept hook) — upstream gap #2, cursor lands after `)`.
- Fixes along the way: typing-crash (entity double-lease), whole-buffer
  replace on second accept, tab-loss hardening (atomic writes, corrupt
  backup, orphan adoption), stuck "Loading suggestions…" (unbound tabs no
  longer mark loading; runs warm the cache), refresh hardening (filtered
  FK query composed a double WHERE → ORA-00933, and all-or-nothing install
  let it empty the cache; now predicates compose with AND and each
  dictionary installs independently).
- Verified: clean `clippy`, 72 lib + 10 live (incl. dictionary + composite
  FK smoke) + 3 UI tests, release smoke. Live probe as SYSTEM: 9779→146
  tables, columns complete, `SYSTEM.EMPLOYEES` #1 for `emp`.

## Dialog scrollbars + keychain password UX (2026-09-12)

- Visible scrollbars: the kit theme default is hover-only, which hides
  overflow in short dialogs. Connection dialog + connection picker now
  overlay `Scrollbar::vertical` (mode Always) bound to the same owned
  `ScrollHandle` the scroll area tracks — no caller-id `Scrollable`
  wrapper (see "Picker scroll" above for why that misbehaves on
  rebuild-every-render dialog content).
- Keychain password UX: editing a Keychain-mode connection showed a blank
  password field (reads as "no password"), and saving it untouched
  the stored entry. Now `fill_form` shows a "Saved in keychain"
  placeholder when an entry exists,
  snapshots the field at open (`password_snapshot`, Keychain-opens only),
  and `save_from_dialog` leaves the entry alone when untouched. Switching
  File→Keychain with a visible password still stores it (snapshot is
  None outside Keychain opens).
- Verified: clean `clippy`, 101 lib + themes + browser_tree green,
  debug GUI build.

## Dialog scrollbars, Cmd+Q, Cmd+, (2026-09-12)

- Scrollbar thumb now driven by the tracked scroll handle's own viewport
  (`viewport_from_layout` dropped — it tied thumb math to the overlay's
  auto-layout box instead of the exact scroll area). Headless geometry
  probe confirmed the overlay box == scroll-area box and stable across a
  3000px scroll. Scroll areas/wraps carry `.test_support()` (as picker
  rows already do) for future layout assertions.
- Cmd+Q with a popup open: verified every link headless — the Quit action
  dispatches with a modal focused, the unsaved-changes alert stacks visibly
  over the picker, its buttons work, and Cancel returns to the popup.
  Clean tabs quit via unconditional `platform.quit()`. No code change
  needed; production symptom unreproduced — exact user flow still open.
- Cmd+, with another popup open used to CLOSE that popup (the toggle
  guard only knew *whether* a dialog was open). Now a `dialog_seq` /
  `settings_seq` pair (Cell, every view-level open site bumps it) detects
  Settings-on-top: second press dismisses just Settings, otherwise
  Settings stacks above the popup. View-level entry mutates directly
  (it runs under the action listener's lease — Entity::update there
  panics); the main.rs global mirrors it inside view.update. Headless
  probes: baseline open/toggle, stack-over-picker, untoggle-keeps-picker.
- Verified: clean `clippy`, 101 lib + themes + browser_tree + ui_picker
  green except pre-existing `cmd_w` flake (fails on clean tree too).

## Single-path Quit/Settings + deferred global handlers (2026-09-12)

- Root cause of dead Cmd+Q/Cmd+, found by probe: app-global handlers
  called `handle.update()` while event dispatch holds the window take
  (`update_window_id` takes the window out of the map), so the nested
  re-take failed and `let _ =` swallowed it — silently doing nothing.
  Editor-focused worked only because view-level listeners (handed
  `&mut Window` directly) fired there. Headless `window.press`
  dispatches without the take, which is why probes passed while
  production died; nesting press inside `update_window` reproduces it.
- Fix: Quit/Settings live ONLY in main.rs (removed 3 view-level Quit
  listeners + root OpenSettings listener — the duplication also
  double-fired), and both global bodies run inside `cx.defer`, i.e.
  after the event batch releases the take. Also fixed a real toggle
  bug the probe caught: main.rs never closed Settings on toggle-off.
- Verified headless: baseline open/toggle, stack-over-picker +
  untoggle-keeps-picker, quit dispatch with dialog open. Clean `clippy`,
  101 lib + themes + browser_tree + ui_picker green except
  pre-existing `cmd_w` flake (fails on clean tree).

## Database type row in connection dialog (2026-09-12)

- The dialog now shows Database type as its first row: a single selected
  Oracle pill (only engine today). `DbEngine::label()` carries the text;
  a second engine adds one pill next to Oracle's. `pending_engine`
  (dialog-owned, like role/kind) replaces the preserve-stored hack in
  `form_config`. Pills gained `.test_support()` for layout assertions.
- Verified headless (Oracle pill visible in Add dialog), clean `clippy`,
  101 lib + themes + browser_tree green.

## Sidebar rail, editor env ring, live-row signal (2026-09-12)

- Collapsed rail is expand + settings only: rail-add and the per-icon
  connection list (with its menus) removed; connections/adding live in
  the expanded pane. Probed headless (collapse hides rail entries).
- Query editor wears a subtle 1px rounded ring in the bound
  connection's environment color (45% opacity, same hue as its badge);
  none when untagged/unbound. `tab_environment` helper shared with the
  connection picker badge.
- Live connection no longer washes the whole row green: the Database
  icon tints success + the 3px status bar goes green (muted icon +
  border-tone bar when idle). The status bar had in fact never painted
  (row-level child of a vertical div = zero height); moved into the
  horizontal row body where it stretches to full row height — probed
  3x24px visible.
- Verified: clean `clippy`, 101 lib + themes + browser_tree green.

## App-global New/Close Tab (2026-09-12, uncommitted)

- Cmd+T/W had element-level listeners only (sidebar roots + main),
  dead with any dialog focused. Now single app-global deferred
  handlers in main.rs (`new_tab_command` /
  `close_active_tab_command` pub entry points; active tab snapshotted
  at execution); six element listeners removed (double-fire hazard).
- Caught by probe: main.rs `view.update` takes a 2-arg closure
  (window comes from the outer handle.update) — plus a reminder that
  `grep "^a|^b"` without -E matches nothing (earlier "clean" checks).
- Verified headless: baseline single-tab creation, dialog-focus
  Cmd+T (once) + Cmd+W closes newest. Clean `clippy`, 101 lib +
  themes + browser_tree green.

## Revert schema-tree auto-reveal (2026-09-12)

- Removed the scroll-on-expand (deferred strict Top-pin); expansion
  leaves scroll alone. Kept expansion-state tracking + repaint.
  Reason: any scroll between double-click presses moves the row,
  press #2 lands elsewhere, viewer never opens — timing-dependent,
  unfixable by reducing movement alone.
- Solution preserved in `HISTORY.md` (Part 4; final code,
  variants tried, kit facts) for possible restoration.
- Verified: clean `clippy`, 101 lib + browser_tree green.

## Cmd+K connection picker -> new bound tab (2026-09-12)

- Cmd+K (`PickConnection`, context-free, root-div listener so it fires
  with any focus) opens the same picker as unbound runs. `PendingPick`
  gains `new_tab`: picking opens a fresh blank tab bound to the pick,
  selects + focuses it, and connects eagerly (same session/password/
  failure paths as sidebar Connect) — never runs anything. Classic
  bind-and-run flow untouched; both pick sites (click, Enter) branch.
  No-op while a pick is pending; cancel/Esc refocuses as before.
- Verified headless (picker, single new tab, picker gone), clean
  `clippy`, 101 lib + themes + browser_tree green.

## Picker hover-follows-selection (2026-09-12)

- First-match wash now follows the mouse: hover drives an active index
  into the filtered list (repaint only on change), Enter confirms the
  highlighted row in both Cmd+K and run modes, typing resets to the top
  alongside the scroll rewind. Click picks directly as before.
- Verified headless (hover + Enter opens exactly one tab), clean
  `clippy`, 101 lib + picker suite green (two known flakes excluded).

## Shortcut review fixes (2026-09-12)

- Full audit of all 17 registered shortcuts (native/kit conflicts,
  context scoping, handler placement, menu coverage). Two fixes:
  (a) RunQuery handler gains the siblings' `editor_focused` guard —
  Cmd+Enter in dialog fields/picker search no longer runs the active
  tab's query behind the dialog; (b) Open/Save/SaveAs listeners
  duplicated onto both sidebar roots (bubble-path pattern) so Cmd+S/O
  work with sidebar focused.
- Verified headless (dialog Cmd+Enter ignored, sidebar-focus save
  writes the file; serial threads — config-dir env is process-global),
  clean `clippy`, 101 lib + menus/themes/browser_tree green.

## Shift+Cmd+K rebind active tab (2026-09-12)

- `PendingPick` bool became three-way `PickAfter` (Run/NewTab/Rebind).
  New `RebindConnection` action on cmd-shift-k (root listener, free
  in kit): picker picks rebind the ACTIVE tab, connect eagerly, no
  new tab, no run. Both pick sites + hint text branch; File menu +
  coverage test updated. Fixed a latent lease panic en route
  (focus helper must run outside view.update).
- Verified headless (no new tab, picker closes), clean `clippy`,
  101 lib + menus green.

## Query timeout, 60s default (2026-09-12)

- `query_timeout_secs` pref (default 60, 0 = unlimited) + Settings →
  Results picker (30s/1min/2min/5min/Unlimited). Applied per call on
  the live connection at connect + every query/exec (cheap setter, no
  reconnect needed); per-round-trip semantics documented. Timeout maps
  centrally to "Query timed out after Ns"; excluded from poisoning so
  the session survives. Cancel unchanged.
- Verified live against local Oracle: 10s sleep trips at staged 2s
  with friendly message, next query succeeds. Clean `clippy`, 101
  lib, 11 live, menus/themes/browser_tree green, GUI builds.

## @-script execution (2026-09-12)

- Run `@path` / `@@path` / `START path` from the caret's line: expands
  nested includes then executes sequentially. `@@` resolves against the
  includer's dir (CWD-independent nesting); `@` against the tab file's
  dir else CWD (SQL Developer worksheet-first parity); absolute
  verbatim, `~` → HOME, `+.sql` fallback, quoted paths, full-line
  directives only (comments left alone), depth cap 10 + cycle errors.
- One variables dialog for the whole expanded script; sequential
  runner under the run_token umbrella (Cancel aborts between
  statements); per-statement bind partitioning; last SELECT lands in
  the grid, otherwise a summary (`@seed: N statements, 0 errors · T
  ms`); first error stops with `name: statement i/N failed`.
  Resumes (picker/password) funnel through start_run detection.
- Verified: 7 new sql.rs unit tests; live headless UI probe (since
  removed) ran a real 4-statement DDL/DML script + a failing script
  against local Oracle via real picker clicks — summary and
  `statement 2/3 failed: ORA-00942` asserted from the output pane.
  Clean `clippy`, 108 lib, 11 live, menus/themes/browser_tree green.

## Scripts render text-only, no grid (2026-09-12)

- SQL Developer parity (Run Script/F5 -> Script Output tab, never a
  grid): run_script completion always sets the info pane with a
  per-statement trail (capped at 50 + "+N more") plus the summary, and
  never builds a grid fetch. Error path shows the trail + `✗ #i/N`.
  Empty fetch retained for post-Dismiss/export plumbing; last SELECT
  still recorded in last_sql.
- Verified live (probe since removed): real 4-statement script ->
  trail + `@probe_seed.sql: 4 statements, 0 errors`, failing script
  -> trail + `✗ #2/3 — ORA-00942`, asserted from the pane via
  clipboard. Clean `clippy`, 108 lib, 11 live, menus/themes/
  browser_tree green, GUI builds.

## Run Script button + Shift+Cmd+Enter (2026-09-12)

- Whole buffer through the script pipeline (SQL Developer F5): new
  "Script" header button (FileTerminal icon) + `RunScript` action on
  shift-cmd-enter (Input-scoped, editor-gated) + Query-menu item
  ("Run as Script", menus test updated). `PickAfter::ScriptBuffer`
  re-enters via live buffer re-read on picker/password resume;
  summaries use the display verbatim (`@file` vs tab name).
- Verified live (probe since removed): button click on a plain buffer
  ran 3 DDL/DML statements via real picker pick — trail + `seed.sql:
  3 statements, 0 errors`. Clean `clippy`, 108 lib, 11 live,
  menus/themes/browser_tree green, GUI builds.

## Ergonomics batch (2026-09-12)

- New shortcuts (all context-free, kit/macOS-conflict checked):
  Cmd+B sidebar, Shift+Cmd+N new connection, Cmd+=/-/0 font zoom
  (10..24, persist + live apply), Ctrl+Cmd+↑/↓ editor height via the
  owned ResizableState (`.size()` is initial-only — an `editor_h`
  field could never work), Cmd+J dismiss results.
- Native copy everywhere: grid headers render as SelectableText with
  column-select mode off (no more whole-column selects); output pane
  message is a read-only Textarea (caret, select, native Cmd+C) synced
  only on text change; shared copy handler prefers live window
  selection, falls back to cell/row. Output Copy button removed.
- Dismiss means editor-only: hide_results flag, × on output pane
  (icon-only, consistent) and grid export row; new runs reopen.
- Menus cover all 25 actions (View: sidebar/zoom/editor/dismiss;
  File: new connection). Verified headless where observable
  (toggle, dialog open, zoom steps, height bounds +48, pane copy
  exact-match, dismiss states); clean `clippy`, 108 lib +
  menus/themes/browser_tree green.

## Autocomplete scope-nesting fixes (2026-09-12)

- `@`-line with a quoted path killed every popup below it: word_prefix
  treated any earlier `"` as opening a quoted identifier, swallowing
  the whole buffer tail as the "prefix" (reproduced as
  `(";\nSELECT * FROM em", 27)`). Fixed with line-scoped,
  escape-aware parity.
- Same family in is_trivia_position (gates completion+hover+define):
  apostrophes in `--` comments, quotes in closed `/* */` blocks, and
  `--`/`/*`/`"` inside string literals each vetoed all later popups.
  Both now share one scope-aware lexer (scan_head); genuinely open
  quotes behave exactly as before.
- Verified: 7 new unit tests (fail-before/pass-after), 110 lib +
  menus/themes/browser_tree green, clean clippy, GUI builds.

## Refactor Phase 0 — robustness + hygiene (2026-09-12, uncommitted)

- Clippy fully clean (was 6 warnings): NewTabSpec params struct for
  make_tab, ShowDocumentHook/DialogPick type aliases, then_some,
  keychain unit delete, struct-update init in 3 test helpers, one
  needless borrow.
- Poison-tolerant locks: session::lock() helper (+ unit test) replaces
  16 production lock().expect sites; db.rs pull_locked rewritten on
  disjoint field borrows — zero-limit and missing-cursor are DbErrors
  (+ unit test), poison path preserved.
- Crash vector closed: grid column() falls back to `col{ix}` instead
  of expect on DB-shaped data.
- Visible persistence: persist_tabs/persist/write_draft and all 12
  prefs.save sites (via save_prefs_status + AppView global) report
  failures on the status bar.
- Release signal for unparsed browser rows (eprintln; view updates
  are lease-illegal mid-render).
- Verified: clean clippy, 112 lib + menus/themes/browser_tree green,
  GUI builds. See `HISTORY.md` (Part 5) for Phases 1–3.

## Refactor Phase 1 — dialogs extracted (2026-09-12, uncommitted)

- settings_dialog.rs (~765 lines) + connection_dialog.rs (~586):
  mechanical moves, no behavior change. app.rs 8,019 → ~6,830.
  Cross-module access via pub(crate) fields/methods; main.rs paths
  unchanged. Two tab-command fns swept up in the cut moved back.
- Verified: temp headless probe for both dialogs (since removed),
  clean clippy, 112 lib + menus/themes/browser_tree green.

## Refactor Phase 2a — picker/bind dialogs extracted (2026-09-12, uncommitted)

- Unified the 4 pick_connection_* helpers into one parameterized
  pick_connection_resume (verified in place first): same per-mode
  behavior, both call sites collapsed to one call each.
- conn_picker.rs (~490): PendingPick/PickAfter, resume, focus helper,
  open_pick_for_*, open_conn_pick_dialog. bind_dialog.rs (~310):
  PendingBind/BindField, submit fns, open/submit bind dialog.
  dialog_footer helper (connection + password-prompt footers; alerts
  and custom footers keep theirs). pub(crate) for cross-module use.
- Verified: temp headless probes for connection dialog (pills, cancel)
  and bind dialog (opens on pick, Enter submits) — since removed.
  Clean clippy, 112 lib + menus/themes/browser_tree green.

## Refactor Phase 2b — run executors extracted (2026-09-13, uncommitted)

- run.rs (~1,295 lines): start_run gate, script_base_dir,
  start_script_run, run_buffer_as_script, start_script_common,
  run_sql, run_script, cancel_run/export, start_export,
  begin_export_drain, finish_export, mark_siblings_exhausted,
  clear_pending — plus ExportOutcome, file_stem, unix_timestamp,
  export_drain_blocking moved in (executor-side only).
  app.rs 6,0xx → 4,732. All impl methods + shared struct fields
  (QueryTab, FetchState, ResultData, SqlHighlandView) + helpers
  (Output::info, ExportFormat::ext, ResultsDelegate::set_fetch,
  with_password, effective_password, ensure_meta, bump_usage,
  PendingPassword, TabKind, CopySel, ResultsDelegate) pub(crate);
  with_password/effective_password/bump_usage/ensure_meta move
  to final homes in Phase 3d. Unused-import trim + 6 pre-existing
  needless_borrow cleanups (lock(&cache) → lock(cache)).
- Verified: clean clippy --all-targets, 112 lib +
  menus/themes/browser_tree green, GUI builds, live serial 11/11.
  NOTE: parallel live suite flakes live_call_timeout_trips_and_survives
  ("connection has been closed" instead of timeout) on clean 20164c9
  too (2/4 base runs fail identically) — pre-existing timing flake
  under parallel DB load, not from this refactor.

## Refactor Phase 3a — autocomplete provider extracted (2026-09-13, uncommitted)

- providers.rs (~417 lines): completion_items_for, to_items,
  trigger_complete (mechanical move). app.rs 4,732 → 4,331.
  own_schema_of stays (shared by browser/hover/completion) as
  pub(crate); editor_focused + COMPLETE_LIMIT pub(crate).
  Lexer-level items (CompleteContext, ScopeTable, resolve_qualifier,
  keyword tables) already lived in complete.rs — no move needed.
- Verified: clean clippy --all-targets, 112 lib +
  menus/themes/browser_tree green, GUI builds, live serial 11/11.

## Refactor Phase 3b — schema browser extracted (2026-09-13, uncommitted)

- browser.rs (~237 lines): browser_tree_items, browser_group_items,
  refresh_browser, toggle_browser (mechanical move). app.rs 4,331 →
  4,110. render_browser_tree stays (render code). ConnCache struct
  introduction deferred to Phase 3d (touches field layout + all users).
- Verified: clean clippy --all-targets, 112 lib +
  menus/themes/browser_tree green, GUI builds, live serial 11/11.

## Refactor Phase 3c — sidebar extracted (2026-09-13, uncommitted)

- sidebar.rs (~426 lines): render_connection_row, render_sidebar,
  toggle_sidebar, set_sidebar_collapsed (mechanical move). app.rs
  4,110 → 3,708. ConnMenuOp/conn_menu_item pub(crate); sidebar
  render calls cycle_tab/dismiss_results/open_sql_file/
  save_active_tab(_as)/render_browser_tree pub(crate) in place.
- Verified: clean clippy --all-targets, 112 lib +
  menus/themes/browser_tree green, GUI builds, live serial 11/11
  (one transient serial failure mid-phase, green on immediate
  rerun x2 — live tests don't touch GUI code; environmental).

## Refactor Phase 3d — final homes + close-out (2026-09-13, uncommitted)

- ensure_meta → browser.rs, bump_usage → providers.rs,
  with_password + submit_password → connection_dialog.rs
  (mechanical moves). app.rs 3,708 → 3,491. PendingPassword
  fields pub(crate). apply_font_prefs already in guitheme.rs.
- Declined: ConnCache / ConnectionDialogState / PendingOps
  struct introductions — field-layout restructures, risk exceeds
  benefit; modules are cohesive without them.
- Verified: clean clippy --all-targets, 112 lib +
  menus/themes/browser_tree green, GUI builds, live serial 11/11.
- Refactor complete: app.rs 8,019 → 3,491 across
  settings_dialog / connection_dialog / conn_picker /
  bind_dialog / run / providers / browser / sidebar.

## Real query cancellation, forked driver (2026-09-13)

- Upstream `oracledb` still has no break API, so Cancel was abandon-only.
  Added a plain-TCP interrupt in a fork of the driver
  (`srikkanthm/rust-oracledb`, pinned by commit in `[patch.crates-io]`):
  `Connection::cancel_handle()` / `cancel()` / `supports_oob()`, a
  `CancelHandle`, and `ErrorKind::Cancelled` for ORA-01013.
- App keeps a driver-agnostic seam: `CancelToken` trait +
  `DbClient::cancel_token()` (`db.rs`), a per-connection token registry on
  `SessionPool`, and calls in `run/query.rs` (`cancel_run`), `run/script.rs`,
  `run/export.rs` (`cancel_export`) and `app/connections.rs` (capture on
  connect). The handle owns an independent socket clone, so cancel never
  needs the session mutex; `ErrorKind::Cancelled` maps to "Query cancelled".
- The fork's real bug was recovery: `Client::reset()` waited for a second
  reset marker the caller had already consumed, hanging after every
  interrupt. Returning the first non-marker data packet fixed it. The local
  server reports `supports_oob() == false`, so the in-band INTERRUPT marker
  is the working path (OOB plumbing stays for servers that accept it).
- Fallback preserved: TLS/other engines, or a tab with no captured token,
  still use the `run_token` abandon.
- TCPS/TLS is **not** covered (deferred; no TLS Oracle to test against):
  Oracle disables OOB for TLS and the in-band marker can't be injected from
  another thread. Candidate designs — cooperative in-band cancel (fork) and
  server-side `ALTER SYSTEM CANCEL SQL` — are recorded in `CANCELLATION.md` §7.
- New opt-in `tests/cancel_live.rs` (ignored; needs plain-TCP Oracle):
  15 s `dbms_lock.sleep` returned in ~3 s with `Cancelled`, connection
  reusable. Full details + upstream revert checklist: `CANCELLATION.md`.
- Verified: clean `clippy --all-targets`, fmt, 122 lib + menus/themes/
  browser_tree green, live cancel test green, fork pushed + re-pinned by
  commit.

## macOS packaging with cargo-packager (2026-09-13)

- Bundle a native `.app` + `.dmg` via CrabNebula's `cargo-packager`
  (installed with `cargo install cargo-packager --locked`; not a repo dep).
  Config in `[package.metadata.packager]` (Cargo.toml):
  `product-name`, `identifier = com.srikkanthm.sqlhighland`,
  `category = DeveloperTool`, `formats = ["app", "dmg"]`, `out-dir = dist`,
  `icons`, and `before-packaging-command = "cargo build --release --features
  gui"` (the binary is gui-gated). `macos.minimum-system-version = 11.0`.
- App icon derived from a hand-authored `assets/icon/icon.svg` (scenic
  mountain + database + code window): rasterized with `rsvg-convert` to a
  1024² PNG, converted to `icon.icns` via an `icon.iconset` + `iconutil`;
  generated PNG/ICNS checked in so packaging needs no librsvg.
- Apple Silicon (arm64) **only** by design — no Intel or universal build, so
  no `target-triple` override is needed.
- Verified: `cargo packager --release` produced `dist/SQLHighland.app` (37 MB)
  and `dist/SQLHighland_0.1.0_aarch64.dmg` (13 MB); `plutil` shows the
  expected Info.plist keys (bundle id, executable `sqlhighland`,
  `LSMinimumSystemVersion`, `LSApplicationCategoryType`, icon), the bundle
  launches, and the DMG mounts with the app + `Applications` symlink.
- Output is **unsigned by design** — signing/notarization is intentionally out
  of scope. Distribution to other Macs needs a Gatekeeper unlock
  (right-click → Open, or `xattr -dr com.apple.quarantine`). Signing steps (if
  ever needed) + the icon/verify recipes: `docs/PACKAGING.md`.

## Automated GitHub Releases (2026-09-13)

- `.github/workflows/release.yml`: on pushed `v*` tags (and manual
  `workflow_dispatch`, which only uploads a workflow artifact), a `macos-15`
  **arm64** runner verifies the tag matches `Cargo.toml`, installs
  cargo-packager (`--locked --version '^0.11'`) and ensures the Metal compiler
  (present on the runner; downloads it only if missing), runs
  `cargo packager --release`, writes `dist/SHA256SUMS.txt`, and attaches the
  DMG + checksums to the GitHub Release via `softprops/action-gh-release`
  (auto-generated notes; no secrets, uses `GITHUB_TOKEN`).
- `ci.yml` GUI job bumped `macos-14` → `macos-15` (arm64; `macos-14` is
  deprecated, retires 2026-11, and the `-intel`/`-large` labels are x86_64).
- First release **v0.1.0** published: run green in ~9 min, assets
  `SQLHighland_0.1.0_aarch64.dmg` + `SHA256SUMS.txt`; checksum verified by
  re-download. Two CI-only fixes en route: `cargo install --version` needs a
  range qualifier (`^0.11`), and the image's default `xcodebuild` rejects
  `-downloadComponent` while `metal` is already installed (an Xcode probe
  workflow confirmed it; the step is now conditional).
- Docs: `PACKAGING.md` §8 (release flow) + README pointer.

## CI: GUI test deps gated behind `gui-test` (2026-09-13)

- Root cause of the long-red `Core (no gui)` and `MSRV (1.89)` jobs: the
  non-optional `[dev-dependencies] gpui-kit` was pulled into every
  test/`--all-targets` build, dragging in `gpui-pre-linux` — fontconfig on
  Ubuntu, and a rustc 1.90–1.92 requirement (`wgpu`→`ordered-float`,
  `gpui-pre-linux`→`oo7`, `gpui-component`→`tree-sitter-language`). The core
  library's own deps never needed any of it.
- Fix: dropped the dev-dependency and added
  `gui-test = ["gui", "gpui-kit/test-support"]`; the four headless UI test
  targets now declare `required-features = ["gui-test"]`. `core`/`msrv` keep
  their `--all-targets` sweep and never build GUI crates. The CI `gui` job
  lints/tests with `--features gui-test`; README/ARCHITECTURE commands match.
- Verified: default linux dep graph is GUI-free; `cargo check --all-targets`
  and `cargo +1.89.0 check --all-targets` clean (no rust-version violations);
  all 4 UI suites pass, 122 lib tests pass, `clippy --features gui-test
  --all-targets -D warnings` clean.
- Also bumped CI actions to Node 24 runtimes: `actions/checkout@v7` (both
  workflows), `actions/upload-artifact@v7`, `softprops/action-gh-release@v3`.
  `rustsec/audit-check@v2` has no Node 24 release, so its warning remains
  (non-blocking).
- Audit job gained `checks: write`: it was erroring ("Resource not accessible
  by integration") because it couldn't create its result check run even on a
  clean scan. RustSec reports **no vulnerabilities**; 5 informational
  "unmaintained" warnings remain (`instant`, `paste`, `rustls-pemfile`,
  `rustybuzz`, `ttf-parser` — all transitive, not actionable locally).

## About: app icon + menu link (2026-09-13)

- Settings → About now renders the app icon (embedded 256px PNG) beside the
  title/version. `main.rs` wraps `AllAssets` in an `AppAssets` source that
  serves `sqlhighland-icon.png` from `assets/icon/icon-256.png`; the About row
  draws it with `img(...)`.
- Native app menu gains **"About SQLHighland"** (new `OpenAbout` action,
  registered globally in `main.rs`) which opens Settings focused on its About
  page. The kit's `default_selected_index` only applies when the keyed state is
  first created, so the targeted open uses a unique Settings element id; normal
  Cmd+, opens keep the persistent page/search.
- Verified: menu coverage test extended with `OpenAbout`; `clippy --features
  gui-test --all-targets -D warnings` clean, 4 UI suites + 122 lib tests green,
  debug launch smoke OK. Visual confirmation of the icon/menu is manual.
- About content reworked: name and "Version x" on separate lines, an Author
  row, and a richer description. The old single "… — SQL database client" line
  was clipped at the content pane's edge because the header text column had no
  flex constraint; it now uses `flex_1()` (and `w_full()` on the description).
  Description/author come from the manifest (`CARGO_PKG_DESCRIPTION` /
  `CARGO_PKG_AUTHORS`; `authors` added to Cargo.toml), so About stays in sync.
- About trims to the essentials: dropped the duplicate group subheader and the
  Connections/Preferences/Tabs path list. Those locations now live in the
  README "Configuration files" section; `PACKAGING.md` §7 keeps the same list.

## Connect crash fix: no panics in the handshake (2026-09-14)

- A `SIGABRT` reported on another Oracle server traced to
  `ConnectMessage::deserialize` in the fork. The connect path had panics:
  `todo!()` on an unexpected response packet, `todo!()` when the server
  requires Native Network Encryption, and `parse::<usize>().unwrap()` on a
  malformed listener `(ERR=…)`. All three now return errors; the
  unexpected-packet arm logs and names the numeric packet type.
- Likely trigger: the fork advertised out-of-band attention
  (`GSO_CAN_RECV_ATTENTION` + `TNS_CHECK_OOB`) on every Unix socket, including
  `tcps://`. Oracle's own thin driver disables OOB for TLS. The advertisement
  is now **plain-TCP only** (`connect_oob_flags`, with a unit test).
- `Client::reset()` now also consumes `CONTROL` packets (it previously skipped
  only `MARKER`), so a reset can never hand a control packet to a message
  parser that doesn't expect it.
- Fork commit `52993b5`; re-pinned `[patch.crates-io]`. Verified: fork
  `cargo test --lib` (incl. the new per-protocol OOB test) + clippy clean;
  SQLHighland clippy/fmt, 122 lib tests, and the live plain-TCP cancel test
  green.

## Oracle ANO/NNE support (AES) (2026-09-14)

- The pure-Rust thin driver can't connect to servers with
  `SQLNET.ENCRYPTION_SERVER=REQUIRED` (Oracle's thin drivers lack native
  encryption; the JDBC thin driver implements it, which is why SQL Developer
  connects with no client config).
- Ported Oracle Advanced Networking Option from go-ora into the fork
  (`f1435e7`): advertise ANO at connect (instead of `NSI_DISABLE_NA`), run the
  post-ACCEPT service handshake, Diffie-Hellman key agreement, and install an
  AES-128/192/256 CBC cryptor that transparently wraps every DATA packet body.
  The former `NSI_NA_REQUIRED` panic-turned-error is now a targeted failure
  only when the server requires NA but doesn't offer ANO.
- Verified against a second Oracle 19c container with encryption required
  (`localhost:1522`): connect + query + live `cancel_live` all pass; the
  non-encrypted instance (`localhost:1521`) is unaffected.
- Integrity/checksum is not negotiated yet (the client offers "none"); details
  and the port map are in `docs/ANO.md`. SQLHighland re-pinned to the fork rev.

## Grid & editor polish: hover opt-in, Count rows, row-cap field, denser grid (2026-09-14)

- **Hover details are opt-in.** The editor table/column hover cards are gated by
  `Preferences.hover_details` (default **off**) and a Settings → Editor → Hover
  switch. The provider reads the live view flag, so toggling needs no per-tab
  reinstall; Cmd-hover → `DESCRIBE` is unchanged.
- **Count rows.** The results grid's right-click menu gained **Count rows**
  (then a separator, then the CSV/Excel items). It runs
  `SELECT COUNT(*) FROM (<query>)` for the tab's last query on a throwaway
  session — the grid's open cursor is never disturbed — and shows the
  thousands-separated result in a popup. Offered only for settled, wrapable
  query results (not DESCRIBE/object viewers); a query with `:binds` surfaces
  the server error.
- **Row cap is a free-form field.** Settings → Results → Maximum rows replaces
  the fixed picker. Blank or `0` means **unlimited** (the grid pages until the
  cursor is exhausted); any other integer is the cap.
- **Configurable grid density.** The results-grid row height is now a
  preference (`grid_row_height`, 18–34pt, default 22) with a slider in
  Settings → Results ("Row height"); dragging previews live and the value is
  persisted on release. Replaces the earlier fixed 22px compact size.
- Tests: new `format_count` unit tests + updated preference-defaults test;
  `cargo clippy --features gui --all-targets -- -D warnings` and the gui lib
  tests (124) are green.

## Settings inputs & filterable dropdowns (2026-09-14)

- **Query timeout** is now a free-form field (seconds; blank/0 = unlimited),
  replacing the fixed picker.
- **CSV delimiter** is a field (single character; `tab` / `\t` normalizes to a
  real tab) with quick preset buttons (Comma/Semicolon/Tab/Pipe) beside it.
- **Theme** and **editor font family** are searchable dropdowns (the kit's
  `Select` backed by `SearchableVec`), replacing the hand-rolled click-lists;
  the font list expanded to ~14 common monospace families.
- `settings_pick_row` removed (no callers left). New
  `export::csv_delim_from_input` / `csv_delim_display` helpers with unit tests.

## Fixes: About/gear crash + independent, uncapped export (2026-09-14)

- **Settings open crash.** `open_settings_dialog` read the view (`view.read`)
  to fetch the Settings control handles; when called from a path that already
  holds the view's mutable lease — the app-menu **About** (`view.update`) and
  the sidebar **gear** (`cx.listener`) — GPUI panicked on the nested read
  (Cmd+, was fine because `main.rs` opens it outside a lease). The handles are
  now gathered by direct field access (`SqlHighlandView::settings_controls`
  returning a `SettingsControls` struct) and passed in; the dialog never reads
  the view.
- **Export is uncapped and independent.** The grid cap previously set
  `exhausted = true`, which the export drain read as "cursor done", so exports
  stopped at the cap. `exhausted` (cursor exhausted) and `capped` (grid hit the
  cap) are now separate flags. Export is a hybrid: it writes the grid buffer
  when the grid already holds every row, otherwise it re-executes the query on
  a **throwaway session** (`last_sql` + retained `last_binds`) and streams to
  the file. Consequences: the full result set is always exported; another tab
  on the same connection can run queries during an export; the grid buffer is
  not grown (cap's memory guard respected). `FOR UPDATE` results that aren't
  complete export the buffered rows with a notice (re-running would re-lock).

### Export memory hardening (follow-up)

- The export drain no longer clones the whole grid buffer up front: the
  buffered rows are read only on the buffered path, and even then in
  `FETCH_CHUNK`-sized batches under short locks (constant-ish memory instead
  of a 2× spike).
- The re-run path releases its first page before paging the rest, and
  explicitly disconnects the throwaway session on a connect/`start_query`
  failure.
- Retained bind values (`QueryTab.last_binds`, used for export re-execution)
  are zeroized on overwrite and on tab close.
- Throwaway sessions (Count rows, connection-dialog "Test connection") now
  call `disconnect()` on every path instead of leaving teardown to `Drop`,
  so the cursor is released before the socket closes.

## Known issue: intermittent TTC desync on ANO sessions (2026-09-14)

- Occasionally a run fails with `internal error: unknown TTC message type 42 …`
  in the output pane, then the next run works. It is a response-stream desync;
  the app already treats it as poisoning and reconnects. Root cause not yet
  localized — see [`docs/TTC_DESYNC.md`](TTC_DESYNC.md) for the analysis,
  diagnostics (`SQLHIGHLAND_ANO_TRACE`, `RSO_DEBUG_PACKETS`) and the planned
  on-desync dump / query-only auto-retry.
- **Resolved (later 2026-09-14).** Root cause was an upstream TTC bit-vector
  decode bug, not the ANO/marker paths: re-pinning the driver fork to `528a79a`
  picked up the per-row bit-vector reset and the correct bitmap length on the
  initial execute, and the failure stopped reproducing. The poisoning +
  reconnect handling remains as a safety net. See [`TTC_DESYNC.md`](TTC_DESYNC.md).

## Responsive toolbars/headers (2026-09-14)

- The main-window rows clipped on narrow windows because they were single
  non-wrapping `h_flex` rows of fixed-width buttons. They now collapse
  responsively: a content width measured via `on_prepaint` on the main root
  (`SqlHighlandView::main_width`) selects a `ToolbarSize` — **Full** (labels),
  **Compact** (action buttons become icon-only; connection name shortened),
  or **Minimal** (secondary actions move into a "⋯" overflow menu; the
  connection picker drops its label and env pill).
- Applied to the query toolbar, viewer header, results toolbar (Export),
  output-pane title, and status bar (text truncates instead of pushing).
- Connection names in the picker/status/viewer header are shortened with the
  full name kept in tooltips.
- A `window_min_size` floor (640×480) bounds the squeeze.

### Short-window vertical clipping

- The query split's editor panel was `flex_none()` (fixed 300px, shrink 0), so
  the split's minimum height was editor 300 + results min 100 = 400px. On a
  short window the split overflowed its slot and pushed the status bar off the
  bottom (and clipped the grid).
- The editor panel now uses `flex_grow_0().flex_shrink_1()`: it holds its size
  on tall windows but yields height to the grid when the window is short
  (down to its 160px minimum). Both splits are wrapped in a
  `flex_1().min_h_0().overflow_hidden()` container, `render_main`'s root is
  `min_h_0`, the status bar is `flex_none`, and the results grid container is
  `min_h_0` so the virtualized table shrinks and scrolls.

## Configurable suggestions cache + manual refresh (2026-09-14)

- The dictionary/suggestions cache TTL is now a preference
  (`metadata_ttl_secs`, **default 0 = never expire** while the connection is
  active) instead of a hard-coded 15 minutes. `MetadataCache::is_stale` takes
  `Option<Duration>`; the live value is mirrored on the view
  (`SqlHighlandView::metadata_ttl`) and updated from Settings.
- Settings → Editor → Suggestions gained a free-form **"Refresh suggestions
  (minutes)"** field (blank/0 = never), matching the row-cap/timeout fields.
- The connection context menu gained **"Refresh suggestions"**
  (`ConnMenuOp::RefreshMeta` → `refresh_meta`), which forces a fetch now,
  bypassing the TTL.
- Disconnecting marks the connection's cache stale, so a reconnect refetches
  the dictionary even when the TTL is "never".

## Interface density + compact dialogs (2026-09-14)

- **Global `UiDensity` preference** (Compact default / Comfortable) sizes tabs,
  toolbar buttons, sidebar rows, the status bar, pane padding, env badges, and
  dialog spacing; the picker lives under Settings → Themes → Appearance. Editor
  font size and grid row height keep their own dedicated controls.
- Dialogs follow the density (Small form controls/footers on Compact, Medium on
  Comfortable); the connection picker matches.
- Run/Script gained **per-button spinners** driven by a tab run-kind flag
  (Script no longer spins when a statement runs). Run-as-Script is info (cyan);
  Format is warning (amber).
- Suggestions are fetched on a **throwaway session** (connect → fetch →
  disconnect) so a dictionary refresh never blocks a run on the pooled session;
  the in-memory cache is unaffected.
- The results toolbar row was removed; **Export and Dismiss moved into the
  status bar** and show only when relevant (status-bar height 26/32).
- The query editor split now defaults to half the window height; the sidebar
  width is pinned on window resize (own `ResizableState` + remembered width),
  fixing the over-wide sidebar on first launch.

## Grid polish + tab underline + compact picker (2026-09-14)

- Body cells and headers are vertically centered (`h_full` + `flex` +
  `items_center`); the row-number header is right-aligned to match its column.
- The active tab uses the tab bar's **Underline** variant (the filled default
  resolved to `tab_active == background`, i.e. invisible); a density-aware left
  inset clears the split handle and lines up with the pane content.
- Resize dividers (sidebar and editor/results) paint a transparent ~6px strip
  with a small pill grip that brightens on hover and while dragging.
- The connections database icon is a plain indicator again (not focusable, not
  clickable, no hover surface).
- Connection-picker rows span the dialog's full width (the right gutter moved
  onto each row's content) so the hover/first-match wash reaches the edge while
  text stays clear of the scrollbar; the picker follows density.

## Configurable fetch sizes + tab navigation (2026-09-15)

- **Grid fetch size** is a preference (`Settings → Results → Rows → Fetch size`),
  default **50**, clamped 1..=10000; it replaces the hard-coded
  `FETCH_CHUNK = 1000` and applies to the initial load, each scroll fetch, and
  the export drain. The grid's prefetch look-ahead is now one page instead of a
  fixed 200-row threshold (which over-buffered ~300 rows on the first frame).
- **Export fetch size** is its own preference (default **1000**, clamped
  1..=10000), so a large export is not throttled to the grid page size.
- A re-run resets the results scroll to the top (vertical and horizontal).
- Title bar no longer shows the app name.

## Tab strip back/forward (2026-09-15)

- Previous/next tab in display order as the tab bar's prefix, enabled from
  adjacency (`active > 0` / `active + 1 < len`), so it works at launch.
- Selection scrolls the strip so the active tab is visible; the kit's tab
  scroll area tracks two leading children before the tabs, so the scroll child
  index is display index + 2.
- Adds headless `tests/tab_nav.rs` (adjacency, no-ops at the ends, add-tab, and
  Forward-step visibility).

## Live SQL diagnostics (2026-09-15)

- Underlines SQL problems as you type, with a hover message (SQL Developer
  style). Two passes:
  - **Lexical** (core, no deps): unterminated string / quoted identifier /
    block comment and unbalanced parentheses (`src/sql/diagnostics.rs`).
  - **Structural** (gui): tree-sitter + tree-sitter-sequel walk for `ERROR`
    nodes (Error) and `MISSING` nodes (Warning), with a parse budget and an
    issue cap (`src/sqlparse.rs`). Messages name the offending token
    ("Unexpected 'SELEC'"), read dangling clauses as "Incomplete statement",
    and prettify expected tokens.
- Wiring pushes lexical + last-structural issues immediately on edit (so
  squiggles don't flicker off), then runs the tree pass debounced (300ms) on the
  background executor.
- Settings → Editor → Syntax: enable toggle (default on) and scope picker
  (whole buffer default / current statement).
- Adds gui-gated tree-sitter deps and headless `tests/sql_diagnostics.rs`.

## Grid sorting + selection (2026-09-15 → 2026-09-16)

- **Server-side sorting.** Double-click a data column header re-runs the query
  wrapped in `ORDER BY` (Asc → Desc → clear), so Oracle sorts instead of the
  buffered page. `FOR UPDATE`/`DESCRIBE` results stay unsortable. Sort re-runs
  hide the toolbar Cancel and don't flash the Run spinner.
- **Cell/row/column selection with native copy.** Click a data cell to select
  it; click the left row strip to select the row (Shift extends a range, Cmd
  toggles rows); click a column header to select the column (double-click still
  sorts). Cmd+A selects every buffered row; Cmd+C copies the selection honoring
  the configured CSV delimiter (rows → CSV lines, column → one value per line,
  cell → raw value); clicking the grid background clears. Selection state lives
  in `ResultsDelegate`, and the kit's single selection is bypassed
  (`row_selectable(false)`).
- Refinements: rows paint their own full-width background band (hover no longer
  changes a row); the pointer cursor lives on the row (no cell/padding
  flicker), the inert `#` column shows the default cursor; clicking a cell
  defers to the library's current cell so arrow keys navigate from it; a
  selected column tints only its body cells, not the header; the column
  highlight is a negative-inset overlay that reaches the column edges.
- Header row is bold; columns size to fit their header name (≥180px, cap 800px)
  with a trailing gutter for the overlay scrollbar; body values still ellipsize.
- Results-grid scrollbars are drawn always-visible over the table's public
  scroll handles (the library auto-hides them).

## Run selection + toolbar/status polish (2026-09-16)

- **Run with a selection** (Cmd+Enter) executes exactly the selected text
  (trimmed) via the normal statement path, so a fragment picked out of a longer
  statement runs on its own. No selection keeps statement-at-cursor behavior;
  Script (Shift+Cmd+Enter) is unchanged.
- Toolbar Cancel removed (the status bar already offers Cancel during a
  run/export).
- Status-bar Cancel no longer shifts: the run clock is fixed-width
  `HH:MM:SS` and the running/export status label has a fixed width.

## PL/SQL diagnostics fix (2026-09-16)

- tree-sitter-sequel only accepts SQL statements inside `BEGIN...END` and its
  `CREATE FUNCTION` body is Postgres-shaped, so PL/SQL was reported as errors
  (e.g. "Unexpected 'DBMS_SESSION'"). Added `sql::is_plsql` (anonymous blocks
  and `CREATE PROCEDURE`/`FUNCTION`/`PACKAGE`/`TRIGGER` bodies) plus
  `is_plsql_fragment` for package-body pieces; they are skipped in the Statement
  scope and masked (byte-length preserving, with lone `/` terminators) in the
  WholeBuffer scope so offsets stay exact. Lexical checks still run.
- `db::sanitize_statement` now uses `is_plsql` so object bodies keep their
  `END;`.

## Transaction + close guards (2026-09-16)

- **Commit/rollback guard.** Closing a tab or quitting with uncommitted DML now
  prompts to commit or roll back. Transactions are session-scoped, so the
  prompt names the affected connection and, on a shared connection, warns that
  the choice affects every tab using it; a disconnected session has already
  rolled back and closes without prompting. Commit/rollback across connections
  runs sequentially on the background executor; a partly-succeeded batch clears
  the pending flag only for the connections that actually settled; a failed
  commit/rollback keeps the tab open (or aborts the quit) so the user can retry.
- **Window-close guard.** The native red-button close routes through the same
  predicate (`quit_needs_guard`) and orchestrator as Cmd+Q / menu Quit —
  uncommitted transactions are settled first, then unsaved external SQL files,
  and only then does the app quit. External SQL files no longer auto-save (the
  debounced flush writes only in-memory drafts), so the guard can actually see
  a dirty external edit. `QuitMode::LastWindowClosed` makes an unguarded close
  quit the single-window app. Adds headless `tests/quit_guard.rs`.
- **Disconnect active tab** (`Shift+Cmd+D`) disconnects the connection bound to
  the active tab (session-scoped) after a confirmation that names uncommitted
  work when present; silent no-op when nothing is bound/connected.
- `Return` is wired to the primary action on every confirm dialog that only had
  clickable footers (Delete Connection, Quit Save-and-Quit, Close Tab Save,
  File-changed-on-disk Reload), sharing `save_all_and_quit`,
  `save_tab_and_close`, and `reload_tab` helpers.

## Editor fonts + Theme default (2026-09-16)

- Bundled JetBrains Mono, Fira Code, Hack, and Cascadia Code (OFL/MIT) into the
  binary and registered them with the text system before the first window lays
  out text, so a picked family always resolves. The remaining families stay
  OS-provided and are offered only when installed, so a pick never silently
  renders in a substitute.
- Fixed Theme default not reverting: `apply_config` left `mono_font_family`
  untouched when a theme declares none, so re-applying a theme could not clear
  an explicit pick. The platform default is captured once and restored when the
  preference is empty, and the font dropdown routes through
  `apply_preferences`.
- Adds asset-parsing tests for the embedded faces and headless tests for the
  revert round-trip, explicit-family persistence, and the availability
  predicate.

## Tab strip rework: drag-reorder + Move Tab (2026-09-17)

- Replaced the kit `TabBar` with a custom strip of `gpui_base::Tabs` so tabs can
  be dragged. A drag never reorders the model: it moves a **drop indicator** to
  the insertion slot, computed from the stable pre-reorder tab bounds, and the
  reorder happens once on drop (live reordering mutated the layout the hit test
  read, which cascaded into full-strip flicker).
- **Move Tab Left/Right** actions (Cmd+Alt+←/→, alias Ctrl+Shift+PageUp/Down)
  wired through the main root and both sidebar roots, with View menu items.
  `move_tab` keeps the moved tab active, remaps the active slot, scrolls it into
  view, and persists the new order.
- The strip keeps per-tab selection accessibility, adds a static 2px primary
  underline for the active tab, and drops the kit's sliding indicator (also
  removing an idle-CPU animation source).
- Registering the Cmd+Alt chord first makes the menu show it (the menu shows the
  first binding for an action; the PageUp/Down alias still works).
- Tests cover keyboard move, drag reorder, persistence, the alias, nav, and
  scroll-into-view; a per-binary env lock serializes the config-dir-global
  tests.

## Tab pills + dirty dot (2026-09-17)

- The active tab is a filled primary capsule with primary-foreground text;
  inactive tabs are transparent and brighten on hover. Inactive tabs also fill
  with the sidebar's subtle accent (50%) on hover so the capsule affordance
  shows before selection; the drag ghost matches the pill.
- The `*` dirty marker is replaced by an **amber dot** (the accessible label
  carries an "(unsaved)" note; the visible name stays plain).
- The new-tab `+` is a filled circular bubble using a custom button variant:
  the ghost variant installs its own hover, and setting a second one trips a
  debug assertion.

## Autocomplete: scope-aware, dialect-ready engine (2026-09-17)

- Added a per-engine **`Dialect` seam** (catalogs, folding, system schemas,
  sequence members, preferred schemas) and routed the completion path through
  it, so a second database is additive.
- Added structural scope from tree-sitter-sequel: a distilled `ScopeForest`
  (relations, CTEs, subquery projection, DML anchors, ORDER BY spans) produced
  by the existing debounced pass and consumed by `providers.rs`, with the
  lexical scanner as an exact fallback. Delivers CTE/subquery columns,
  nearest-scope resolution (no alias bleed), INSERT/UPDATE target-only columns,
  and ORDER BY projection aliases.
- Replaced the bare escape with **origin-aware tail contexts**: after a table
  reference only the valid continuations are offered (phrases, not fragments),
  with JOIN adding ON/USING, UPDATE adding SET, INSERT INTO adding
  VALUES/SELECT, plus the full Oracle join/order/group/connect-by combinations
  in the catalog. A cursor in trailing whitespace is tolerated, so
  columns/continuations still resolve (fixes columns missing after WHERE).
- **Expression contexts**: subquery starts (FROM `/` IN `/` EXISTS `/` set
  operators) offer SELECT/WITH; `CAST(x AS |)` offers dialect data types;
  `OVER (|)` offers window clauses; `USING (|)` offers common columns; JOIN ON
  keeps offering FK conditions after AND/OR (filtering what is already typed).
- **Richer metadata**: synonyms from `ALL_SYNONYMS` (suggested as SYNONYM and
  resolved to their table's columns), materialized views from `ALL_MVIEWS`, and
  package members from `ALL_PROCEDURES` so `pkg.` completes its members. Usage
  counts persist to `usage.toml` so frequency ranking survives relaunch.
- **Table-first select list**: in an empty `SELECT` list with no FROM yet, lead
  with the dictionary's tables (and CTE names); accepting one inserts
  `* FROM t ` via a `Candidate.insert` override, so columns become available
  from the next completion. The trigger ignores whitespace, `--`/`/* */`
  comments, and DISTINCT/ALL.
- Tests: dialect/scope/extractor units, provider mapping, and end-to-end
  `tests/completion_scope.rs` (CTE, DML, ORDER BY alias, WHERE trailing space,
  FROM tail); serializes config-dir tests. `docs/AUTOCOMPLETE.md` records the
  remaining gaps (standalone routines at call sites, CTE explicit column lists,
  quoted identifiers on insert, grouping-set/nulls-ordering phrases,
  scope-driven hover/definition, fuzzy matching).

## Completion cursor + stale-offset fix (2026-09-17)

- Accepting a function candidate (LOWER, UPPER, TO_DATE, …) now leaves the
  cursor **between the parentheses**. The editor kit has no snippet support — it
  inserts `text_edit.new_text` verbatim and leaves the cursor after `()`, and
  ignores `InsertTextFormat`/Snippet/tabstops — so the app places the cursor
  itself. `QueryTab.pending_completion` is set on a non-empty popup (cleared on
  an empty result); the next editor Change ending in a known `NAME()` moves the
  cursor one byte left. The provider owns the flag lifecycle, because the editor
  emits Change for every typed character while the popup is open and the accept
  does not re-run the provider.
- Fixed auto-complete wedging after clearing or replacing the buffer: the kit
  keeps a sticky `CompletionMenuState::trigger_start_offset` and
  `handle_completion_trigger` bails whenever the cursor is before it, and the
  base offset is never reset. The editor Change subscription now reads the
  offset and cursor inline and, when `cursor < start`, schedules a deferred
  `present_completion_items(cursor, empty)` to reset it (the same call
  Ctrl+Space uses; deferred because the editor is leased during its own change
  dispatch). Ordinary forward typing does not hit the path. Root cause and the
  upstream suggestion are documented in `docs/AUTOCOMPLETE.md`.

## Tab lifecycle: neutral titles, restore, rogue drafts (2026-09-17)

- First launch opens one blank tab (no sample SQL); titles are never derived
  from buffer content — they use **`Untitled N`**. New tabs number from the open
  set (highest `Untitled N` + 1, else `Untitled 1`), replacing the monotonic
  counter, so closing the last tab spawns `Untitled 1` instead of a climbing
  number.
- The active tab id is recorded in `tabs.toml` and reselected on launch (the
  nearest query tab when a viewer was focused).
- Closing an in-memory tab with text prompts **Save As / Discard / Cancel**;
  blank tabs close silently; in-memory tabs always show the unsaved marker.
- **Rogue-tab fix.** Two sources of unwanted tabs were closed: orphan adoption
  no longer resurrects whitespace-only drafts (drafts with text still recover),
  and the debounced draft flush does its existence check and write together
  under the entity so a write in flight when a tab closes can't recreate its
  draft. `write_atomic` also uses a pid+sequence temp name, so two writers can't
  share a fixed `<path>.tmp` and rename a half-written manifest into place.
  Regression tests `closed_tab_leaves_no_orphan_draft` and
  `empty_orphan_drafts_are_not_adopted`.
- `restore_keeps_saved_tab_names` pins byte-identical restore across two
  launches; orphan adoption now logs its evidence (count, ids, byte size) —
  never query text.

## Tab strip scroll reveal (2026-09-18)

- `scroll_tab_into_view` now aligns the target to the **left edge** (clamped at
  the end) from the scroll handle's measured geometry, instead of the kit's
  minimal `ScrollStrategy`, which pinned an off-screen target flush-right and
  hid every tab after it. An already-visible tab is left alone, so stepping
  through nearby tabs doesn't jump the strip.
- The strip doesn't know it overflows until after the first frame, so restore
  also re-issues the scroll on the next frame.

## On-disk suggestions cache + dictionary cleanup (2026-09-18)

- **Opt-in per-connection on-disk cache** (`ConnectionConfig.cache_metadata_to_disk`,
  default off; "Cache suggestions on disk" switch in the connection dialog).
  Persists a connection's dictionary so a restart loads suggestions and the
  schema browser instead of refetching; turning it off deletes the file.
- `MetadataCacheDisk` (serde) + `CacheFingerprint` (connection params + system
  filter) + `version`, written as gzipped JSON to `metadata/<conn_id>.json.gz`
  with an atomic, owner-only write. Tuple-keyed maps become entry vectors (JSON
  keys must be strings); `fetched_at` is unix millis. `load_cache` treats
  missing/corrupt/version/fingerprint mismatch as a miss; `save_cache` skips a
  cache above `Preferences.metadata_disk_cap_mb` (default 20, 0 = uncapped,
  edited in Settings). A session loads the disk copy once on first use (skipped
  on a forced refresh); a successful fetch persists on the background executor.
  Disconnect keeps the disk copy; delete-connection removes it; the
  system-schema toggle invalidates it.
- **Non-identifier dictionary objects are dropped.** Oracle XML DB component
  synonyms owned by PUBLIC (e.g. `oracle/xml/xqxp/functions/builtIns/UpperCase`)
  no longer appear, since `PUBLIC` is not a system schema and their slash paths
  can never be typed unquoted. `is_usable_object_name` (leading letter/`_`, then
  only identifier chars) is applied in every dictionary fetcher — tables/views/
  mviews, columns, sequences, synonyms, package members — so the cache is clean
  at the source and completion, hover, and the browser all benefit. Mixed-case
  quoted names still pass.
- Adds `serde_json` + `flate2` (already in the lock), a shared crate-level test
  lock for the process-global config dir, and `tests/browser_tree` coverage that
  loads a persisted cache with no database.

## TNS / connect-descriptor plan (2026-09-18)

- Documented the research and plan for connection-string **modes** (Basic / TNS
  alias / raw Connect Descriptor) and a curated set of Oracle Net/session
  **properties** in [`TNS.md`](TNS.md), linked from [`TODO.md`](TODO.md) so the
  work can resume without re-investigating the driver.
- Key finding: the pinned oracledb fork does **not** support RADIUS, Kerberos,
  external, or token authentication — only password auth, TLS/mTLS/wallets,
  `tnsnames.ora`, and the descriptor options it recognizes (the parser drops
  unknown nodes). Open questions are listed in `TNS.md`.

## Releases in this stretch

v0.5.0, v0.5.1, v0.5.2, v0.5.5 (2026-09-14/15), v0.6.0, v0.6.3 (2026-09-16),
v0.10.0, v0.10.1 (2026-09-18). Release tags are pushed to trigger the DMG
workflow in `.github/workflows/release.yml`.
