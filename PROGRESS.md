# SQLHighland — Build Log

Cross-platform SQL GUI client in Rust + GPUI (Oracle is the first engine;
macOS is the current target). Started 2026-09-08. Older entries below say
"Oracle-only"/"macOS-only", reflecting the original v1 scope.
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

- Plan: `AUTOCOMPLETE_PLAN.md` (locked decisions + queued follow-ups).
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
- Solution preserved in SCHEMA_TREE_AUTOREVEAL.md (final code,
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
  GUI builds. See REFACTOR_PLAN.md for Phases 1–3.

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
