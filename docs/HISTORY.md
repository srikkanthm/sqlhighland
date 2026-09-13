# SQLHighland — History & Design Notes

Consolidated record of the project's original plan, shipped feature designs,
and the completed `app.rs` refactor. This is historical context, not current
documentation:

- **Current architecture:** [`../ARCHITECTURE.md`](../ARCHITECTURE.md)
- **Chronological build log:** [`PROGRESS.md`](PROGRESS.md)
- **Code & security review:** [`REVIEW.md`](REVIEW.md)
- **User-facing overview:** [`../README.md`](../README.md)

> **Direction update:** SQLHighland is engine-first (Oracle is the first
> backend) and cross-platform (macOS is the current target; Windows/Linux
> planned). Earlier sections below predate that wording and say
> "Oracle-only" / "macOS-only"; read them with that in mind.

---


## Part 1 — Original plan (2026-09-09)

SQL GUI client in Rust + Zed GPUI, Oracle DB only for v1.

> **Update (later direction):** SQLHighland is engine-first, not Oracle-only —
> Oracle is the first backend and the driver/schema layers are seams for more.
> Likewise it is cross-platform: macOS is the current target, with
> Windows/Linux planned. The v1 scope below reflects the original decision.

Approved 2026-09-09. Scope: **macOS-only, official `oracledb` 26.0.0-beta.x
pure-Rust thin driver, minimal MVP (connect + query + results),
basic host:port/service + user/pass.**

### 1. Goals / non-goals

Goals (v1):
- Connect to Oracle (19c / 21c / 26ai, on-prem or cloud, TCP, no wallet).
- Run ad-hoc SQL, show tabular results + errors + elapsed/rowcount.
- Native-feeling macOS window, GPU-accelerated via GPUI.

Non-goals (deferred to v2+):
- Schema browser tree, multi-tab / multi-connection, explain plan.
- CSV export, query history persistence, syntax highlighting / tree-sitter editor.
- Wallet / TCPS / EZCONNECT extras, OS keychain (passwords in local file in v1 — known debt).
- Linux / Windows ports.

### 2. Tech choices

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

### 3. Architecture

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

### 4. Milestones

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

### 5. Risks / debt

1. GPUI + `oracledb` both pre-stable → pin `rev`/version, budget time per bump.
2. Beta type gaps → `DbValue` preview fallback.
3. Plaintext password storage in v1 → warn in UI/docs, keychain in v2.
4. GPUI docs are thin → Zed repo `crates/gpui` + `gpui-component` docs are canon.


---


## Part 2 — Autocomplete design (2026-09-12, shipped)

The most critical feature for this project. This doc is the build contract:
v1 scope, phased follow-ups, and verification.

### 1. Decisions (locked)

- **Scope v1:** full smart complete — tables, columns (with alias resolution),
  sequences, keywords.
- **Source:** cached per-connection dictionary data (prefetch on connect,
  refresh manually/TTL). Never query on keystroke; complete from snapshot.
- **Trigger:** auto popup by default (2+ word chars; `.` forces column list),
  plus manual trigger via shortcut; configurable Auto/Manual in Settings.
- **Parser:** hand scanner + tree-sitter-sequel context. `datafusion-sqlparser-rs`
  explicitly rejected: heavy dep, hard-fails on PL/SQL blocks, multi-statement
  scripts, and `&`/`:` variables — exactly our bread-and-butter.

### 2. Editor hook (kit API, verified in source)

- `EditorState::new(window, cx).language("sql")` runs in `CodeEditor` mode.
- `editor.update(cx, |e, _| e.lsp_mut().completion_provider = Some(Rc::new(p)))`
  installs a `CompletionProvider` (`gpui-base/.../input/editor/lsp/completions.rs:40-103`).
- Required methods: `completions(text: &Rope, offset, trigger, window, cx) -> Task<Result<CompletionResponse>>`
  and `is_completion_trigger(offset, new_text, cx) -> bool` (main-thread gate).
- Popup/keyboard/ghost rendering is automatic once installed. No new editor crate.
- `CompletionMenuOptions.max_width` tunable via `lsp_mut()` for long `OWNER.TABLE` labels.

### 3. v1 behavior (strict gating since 2026-09-12)

| Context | Candidates |
|---|---|
| Statement start | Statement starters only |
| Select list | In-scope columns, functions, sequences, expr keywords — never tables |
| After `FROM`/`JOIN`/`INTO`/`UPDATE` | Tables only |
| Predicates | In-scope columns, functions, sequences, predicate keywords — never tables |
| `owner.` after `FROM`/`JOIN` | That owner's tables (bare names) |
| `alias.` / sequences / fresh `ON` | Unchanged (columns / `NEXTVAL` / join conditions) |
| Ambiguous (post-identifier etc.) | Keywords + functions, never tables/columns |

- Alias map from statement `FROM`/`JOIN` clauses (`e → scott.emp`).
  Unresolvable alias → all cached columns (never empty).
- Quoted identifiers complete with case preserved; bare folds uppercase like Oracle.
- Never trigger in strings/comments/`&`/`:` vars (reuse `sql.rs` scanners).
- Ranking: in-scope columns > tables > sequences > keywords; recency/frequency
  boost; scope boost; case-insensitive fuzzy; de-dup.

### 4. v1 extras (folded in per agreement)

- **Recency/frequency boost:** per-connection usage counts (persisted lightweight JSON).
- **Type icons + detail:** table/view/column/sequence/keyword kinds; `OWNER · TYPE` detail.
- **Hide system schemas** (`SYS/SYSTEM/XDB`) by default + toggle.
- **Debounce (~150ms)** + stale-task cancel (run-token pattern) against popup flicker.

### 5. Metadata cache

- New `src/metadata.rs`: `MetadataCache { tables, columns_by_table, sequences,
  fetched_at, loading }`, stored `HashMap<connection_id, Arc<Mutex<MetadataCache>>>`
  beside `SessionPool` (sessions are per-connection; caches follow).
- Background fetch only (`bg.spawn + session.lock()`), three dictionary queries
  via existing `start_query`/`fetch_more`:
  `all_tables ∪ all_views`, `all_tab_columns`, `all_sequences`.
- Triggers: lazy on first completion request + eager after connect/lazy-connect;
  invalidate on disconnect/remove; manual Refresh + 15-min TTL.
- While loading: keywords + cached subset, never block.

### 6. Files

| File | Change |
|---|---|
| `src/complete.rs` (new) | Pure: prefix, context classify, alias map, rank/filter + tests. No GPUI/driver |
| `src/metadata.rs` (new) | Cache + blocking fetchers + fake-result tests |
| `src/app.rs` | `OracleCompleter` provider per tab, `TriggerComplete` action + binding, Settings toggle, loading status |
| `src/config.rs` | `completion` pref (Auto/Manual, default Auto) |
| `tests/live.rs` | Dictionary smoke (counts>0, known-table columns) |

### 7. Follow-ups (queued, not v1)

1. **Join suggestions:** `ALL_CONSTRAINTS`/`ALL_CONS_COLUMNS` → `ON emp.deptno = dept.deptno` after `JOIN … ON`. — SHIPPED 2026-09-12 (first-condition-only; multi-condition tails deferred).
2. **Function snippets:** 32 built-ins complete as `NAME()` with signatures in the detail pane. — SHIPPED 2026-09-12 (tab-stops impossible: no snippet engine, no post-accept hook, `resolve_completions` never invoked — upstream gap #2; cursor lands after `)`).
3. **Auto-qualify on collision / auto-alias suggestion.**
   — SHIPPED 2026-09-12 (qualify-on-collision; alias suggestion deferred).
4. **Column comments** (`ALL_COL_COMMENTS`) in popup detail.
   — SHIPPED 2026-09-12 (joined fetch, one-line detail).
5. **Hover provider** (table → columns; column → type/nullable/comment) — reuses cache.
   — SHIPPED 2026-09-12 (table cards with 30-col cap, qualified + unique
   bare column cards, Markdown popover, trivia-guarded, silent on unknown).
6. **Definition provider** (Cmd-click → DESCRIBE output).
   — SHIPPED 2026-09-12 (`OracleDefiner`: table-only resolution mirroring
   the hover table cards via `describe_target`; Cmd-hover underlines,
   Cmd-click runs `DESCRIBE owner.table` through the normal run path via
   an `oracle-describe:/OWNER/TABLE` `show_document` hook — editor text
   untouched, unbound tabs get the picker first, unknown schemes fall
   through to the kit default).7. **Package members** (`DBMS_*`) via `ALL_OBJECTS`.
   — TABLED 2026-09-12 (user call: revisit later).
8. Keyword-casing follows user/formatter setting.
   — TABLED 2026-09-12 (user call: revisit later).

### 8. Verify

- `clippy`, lib tests (`complete.rs` matrix: prefix, alias, dot-context, ranking),
  live dictionary smoke, release smoke, headless UI (popup appears, Enter accepts),
  manual matrix both themes (bare/`FROM`/`alias.`/sequence/2-char gate/shortcut/
  Manual silence/huge schema).


---


## Part 3 — Schema browser design (2026-09-12, shipped)

Agreed with user 2026-09-12. Oracle now; portable core for later engines.
Implemented + live-confirmed 2026-09-12.

### 1. Goals
- Browse any connection's schema **independent of the active query tab**,
  via trees stacked under each connection in the sidebar.
- v1 objects: **tables, views, sequences** (+ columns under tables/views).
- Single-click selects only; double-click opens (or focuses) an
  **object-viewer tab**: DESCRIBE grid for tables/views, catalog row for
  sequences; grid only, no editor; viewer tabs are **ephemeral** (never
  persisted). (Single release used to open — changed 2026-09-12; the
  kit swallows `on_click` in virtualized rows, so release-with-
  `click_count >= 2` is the trigger.)
- Client-side **filter box** over cached names (big-schema usability).
- Expanding a disconnected connection **auto-connects**, then loads.

### 2. Non-goals (v1)
Packages/procedures/indexes/triggers, DDL actions, drag-into-editor,
viewer persistence, context menus, second-engine implementation.

### 3. Architecture (portability-first)

```
sidebar (app.rs) ──renders──▶ SchemaNode tree (schema.rs, GUI-free)
                                  ▲
SchemaProvider trait ──implemented by── OracleProvider (schema_oracle.rs)
   list_schemas / objects / columns / describe_sql / display rules
```

- **UI renders only the model** — no SQL, no `ALL_*` views outside the
  Oracle impl.
- `ConnectionConfig.engine: DbEngine` (`Oracle` default via serde — zero
  migration). Session pool stays Oracle-concrete; it goes generic *with*
  the second engine, not before.
- v1 providers read the existing per-connection `MetadataCache` (same TTL
  + `ensure_meta` warming). Note: the cache type is still Oracle-shaped;
  generalizing the snapshot is an explicit second-engine milestone, not
  v1 scope.
- `describe_sql(owner, obj)` lives on the trait, so Postgres later brings
  its own equivalent without touching UI code.

### 4. Data changes
- `TableId` gains `kind: TableKind { Table, View }`. The fetch already
  unions `ALL_TABLES` + `ALL_VIEWS` — add the object-type column and
  populate `kind`. Update struct literals (metadata.rs, tests).
- No new dictionary queries: tables/views/columns/sequences all come from
  the warmed cache. Grouping: by owner (= schema), own-schema-first,
  honoring the existing show-system toggle.

### 5. Sidebar UI
- Disclosure chevron per connection row → `schemas → Tables/Views/
  Sequences → object → columns`, backed by kit `TreeState`.
- Expansion state per connection in the view; lazy per level (expand
  warms `ensure_meta` first; columns are cache hits).
- Single filter field; narrows cached names across expanded trees.
- Expand on a dead connection: connect in background, then populate
  (error surfaces inline on failure, tree stays collapsed).

### 6. Tabs: viewer kind
- `TabKind::Query | Viewer { owner, name, kind }`; editor entity kept but
  unrendered for viewers (reversible: kind checks mark every branch point
  for a future `Option<editor>` refactor — mechanical, compiler-guided).
- Viewer header: title via `display_name` (bare for own schema), Refresh
  (re-runs DESCRIBE), Close. Grid + Cancel reuse the run pipeline with a
  generated `DESCRIBE` statement; `last_sql` audit kept.
- Draft persist + tab-manifest skip viewer tabs (ephemeral).
- Editor-gated actions (Run/Format/Commit/…) no-op or hidden on viewers.

### 7. Verify
- `clippy`, lib tests (model grouping, `display_name` titles, kind
  split), live dictionary smoke (table vs view counts), headless UI
  (expand/click where the harness allows), release smoke, manual matrix
  (expand/collapse, filter, click→viewer, refresh, restart drops viewers).


---


## Part 4 — Schema-tree auto-reveal (2026-09-12, reverted)

Status: **reverted** (2026-09-12, commit `e80d481` era). Expansion no
longer scrolls the tree. This doc preserves the solution so it can be
restored if the manual-scrolling tradeoff ever hurts more than the
double-click conflict described below.

### Problem it solved

The schema-browser tree is capped at 320px (`height_px =
(44 + rows*30).min(320)`). Expanding a node near the bottom pushed its
fresh children below the fold with no indication — users had to scroll
manually to see what just opened.

### Final solution (as reverted)

In the `TreeState` subscription (`open_browser`, `src/app.rs`), on
`TreeEvent::Expanded(id)`:

1. Record the id in `browser_expanded` (persistence — kept).
2. `cx.spawn` a deferred task (the kit emits `Expanded` **before**
   rebuilding its entries, so scrolling synchronously clamps against
   stale bounds; after the rebuild both the count and `index_of` are
   exact).
3. Overflow test: `rows * 30.0 + 44.0 >= 320.0` (30px rows, 44px chrome,
   320px cap — same constants as the height formula).
4. Only when overflowing: `scroll_handle().scroll_to_item_strict(ix,
   ScrollStrategy::Top)` — strict, because the clicked node itself is
   usually already visible (non-strict would no-op) while only its
   children are below the fold.

Collapses never scrolled. Fitted trees (content under the cap) never
scrolled.

#### Exact removed code

```rust
let scrolled: Option<SharedString> = match event {
    TreeEvent::Expanded(id) => {
        this.browser_expanded
            .entry(sub_conn.clone())
            .or_default()
            .insert(id.to_string());
        Some(id.clone())
    }
    TreeEvent::Collapsed(id) => {
        if let Some(set) = this.browser_expanded.get_mut(&sub_conn) {
            set.remove(id.as_ref());
        }
        None
    }
};
if let Some(id) = scrolled {
    let conn = sub_conn.clone();
    cx.spawn(async move |view, cx| {
        view.update(cx, |this, cx| {
            let Some(tree) = this.browser_trees.get(&conn).cloned() else {
                return;
            };
            let (overflow, at) = tree.read_with(cx, |t, _| {
                let mut rows: usize = 0;
                while t.entry(rows).is_some() {
                    rows += 1;
                }
                let at = t.index_of(&id);
                (rows as f32 * 30.0 + 44.0 >= 320.0, at)
            });
            if overflow {
                tree.update(cx, |t, _| {
                    if let Some(ix) = at {
                        t.scroll_handle()
                            .scroll_to_item_strict(ix, gpui_kit::ScrollStrategy::Top);
                    }
                });
            }
        })
        .ok();
    })
    .detach();
}
```

To restore: put this back in place of the state-only `match` in the
`cx.subscribe` closure in `open_browser`, keeping the trailing
`cx.notify()` (container height derives from visible rows).

### Variants tried and also reverted

- **Minimal reveal**: non-strict `scroll_to_item(last_descendant,
  Bottom)` — scrolls only when children are actually cut off, and by
  the minimum. Better, but any scroll still moves the clicked row.
- **Double-click-press suppression**: `on_mouse_down` with
  `click_count >= 2` + `cx.stop_propagation()` (our row nests inside
  the kit's row div, so ours runs first) — stops press #2 collapsing
  what press #1 revealed.
- **Stationary-proximity intent**: record every tree press (time,
  position, target); a press within 500ms/8px of an object-row press
  opens press #1's target even when it lands on another row, with
  release-position ruling out drags. Deterministic on paper, still
  flaky in practice against real timing.

### Why it was reverted — the core conflict

Double-click-to-open and scroll-on-expand are structurally opposed
here: the kit toggles folders on mouse-*down*, and OS click counts
reset when press #2 lands on a different element. Any scroll between
the two presses moves the row, press #2 lands elsewhere (toggling the
wrong node as collateral), and the viewer never opens. The failure is
timing-dependent (deferred task vs. second press), hence
non-deterministic. Restoring any reveal must solve press-#2 landing,
not just reduce movement.

### Kit facts established along the way

- `TreeState::toggle_expand` is private; expansion is user-driven
  (`on_entry_click` on left mouse-down) or via `expand_ancestors` —
  no programmatic expand API from app code.
- `UniformListScrollHandle`: `scroll_to_item` (non-strict, minimum,
  no-op when visible) vs `scroll_to_item_strict` (always positions);
  `..._with_offset` variants shrink the effective viewport.
- `ScrollStrategy::{Top, Center, Bottom, Nearest}` live on gpui, used
  here via `gpui_kit::ScrollStrategy`.
- `MouseDownEvent.click_count` exists; `cx.stop_propagation()`
  (App method) works from row handlers because our content nests
  inside the kit's row div.


---


## Part 5 — app.rs refactor (2026-09-13, complete)

Status: **in progress** (2026-09-12). `src/app.rs` is 8,019 lines —
52% of `src/` — with a 45-field view struct and 300–600-line dialog
builders. Every feature lands there. This doc is the map; `PROGRESS.md`
keeps the per-step log.

### Ground rules (from the quality analysis)

- Robustness before extraction (a crash vector and a poison cascade
  ship today — moving code first would just relocate them).
- Each phase is shippable alone and suite-guarded: clean `clippy`,
  110 lib + live + menus/themes/browser_tree/ui_picker green (minus
  the two known pre-existing headless flakes), GUI builds.
- Preserve the lease-safety discipline: dialog builders never touch
  the view entity (`Rc<RefCell>` dialog-local cells); extraction
  should *enforce* it better via function boundaries, not weaken it.
- No behavior changes inside extraction phases (mechanical moves
  only); fixes ride in Phase 0 with their own tests.
- Never commit or push without explicit instruction (standing rule).

### Phase 0 — robustness + hygiene (hours, first)

1. **Clippy clean** (6 warnings): field-init-outside-`Default` ×3,
   `make_tab` 8/7 args, `let`-unit-value in `keychain.rs:50`, needless
   `bool::then` closure, 2× complex-type → named `type` aliases.
2. **Poison-tolerant locks**: one helper
   (`lock(m: &Mutex<T>) -> MutexGuard<T>` recovering via
   `into_inner()`) replacing ~15 production `lock().expect(...)` on
   session/data/meta locks. A panic while holding a lock must degrade
   to stale data, never a crash loop. Tests keep `expect`.
3. **Column crash vector**: `column()`'s `expect("column with no
   result")` → `col{ix}` placeholder fallback. DB-shaped data must
   never kill the app (and its unsaved drafts).
4. **Visible persistence failures**: `persist_tabs`, connection Store
   save, and the 12 `prefs.save()` sites surface `Err` in the status
   bar instead of `let _ =`-swallowing.
5. **Release signal for unparsed rows**: the browser's
   `debug_assert!(false, ...)` gains a status-bar/eprintln signal so
   release builds don't fail silently.

### Phase 1 — dialogs out (~1,100 lines)

Status: **done** (2026-09-12), uncommitted at time of writing.
`app.rs` 8,019 → ~6,830.

- `settings_dialog.rs` (~765 lines): `open_settings`,
  `take_settings_toggle`, `note_dialog_open_for_settings`,
  `save_prefs_status`, `open_settings_dialog`, `settings_pick_row`.
  New `pub(crate)`: `complete_auto`, `show_system`, `dialog_seq`,
  `settings_seq`, `status`, `meta` fields; `open_settings` method.
  `main.rs` untouched (paths resolve through the type).
- `connection_dialog.rs` (~586 lines): `form_config`, `fill_form`,
  `persist` (moved with its block), `start_add`, `start_edit`,
  `open_connection_dialog`, `save_from_dialog`, `dialog_field`,
  `DialogPick`, `dialog_pills`. New `pub(crate)`: `connections`,
  `editing`, all `pending_*`, `password_snapshot`, all form entities,
  `note_dialog_open`, `env_color`; `start_add`/`start_edit` methods.
  `dialog_field` shared with the password prompt left in `app.rs`.
- Two tab-command fns swept up in the cut were moved back to
  `app.rs`. Verified: temp headless probe (settings open/toggle,
  connection dialog + pills render — since removed), clean clippy,
  112 lib + menus/themes/browser_tree green (2 pre-existing
  headless flakes excluded).

### Phase 2 — flows out (in progress; picker/bind done, runner pending)

Status: **part-done** (2026-09-12), uncommitted at time of writing.
`app.rs` 6,831 → ~6,000.

- Done: 4× `pick_connection_*` unified into one parameterized
  `pick_connection_resume` (verified in place first); `conn_picker.rs`
  (~490: PendingPick/PickAfter, resume, focus helper, open fns,
  picker dialog); `bind_dialog.rs` (~310: PendingBind/BindField,
  submit fns, open/submit); `dialog_footer` helper converted at the
  connection + password-prompt footers (alerts/custom footers keep
  theirs). Verified: temp probes for both dialogs (since removed),
  clean clippy, 112 lib + menus/themes/browser_tree green.
- Remaining: `run.rs` executor (`start_run`/script gates,
  `run_sql`/`run_script`, export-drain UI, `Outcome`/`StmtOutcome`).

### Phase 3 — surfaces out (~800 lines)

- `providers.rs` (definer/hover/completer + one shared snapshot
  helper — kills the ~30-line duplication), `browser.rs`,
  `sidebar.rs`. Browser caches move into a `ConnCache` struct;
  dialog form state into `ConnectionDialogState`; pending ops into
  `PendingOps`.

### Explicitly out of scope

- `sql.rs` / `complete.rs` untouched (exemplary as-is).
- Oracle `q'[…]'` alternative quoting (nothing handles it; consistent).
- `tokenize` string-content awareness (wrong-list, never dead-popup).
- Fixing/quarantining the flaky `cmd_w`/`cmd_t` headless tests
  (tracked separately; failures reproduce on the clean tree).

### Methodology note (learned the hard way)

Headless `window.press` dispatches *without* holding the window take,
so probes can pass while production dies. Reproduce take-sensitive
paths by nesting the press inside `update_window`; prefer pure-logic
unit tests where possible. Probes are temporary: write → verify →
delete.


---
