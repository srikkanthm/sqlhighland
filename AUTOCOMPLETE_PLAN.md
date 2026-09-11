# SQLHighland — Autocomplete Plan (approved 2026-09-12)

The most critical feature for this project. This doc is the build contract:
v1 scope, phased follow-ups, and verification.

## 1. Decisions (locked)

- **Scope v1:** full smart complete — tables, columns (with alias resolution),
  sequences, keywords.
- **Source:** cached per-connection dictionary data (prefetch on connect,
  refresh manually/TTL). Never query on keystroke; complete from snapshot.
- **Trigger:** auto popup by default (2+ word chars; `.` forces column list),
  plus manual trigger via shortcut; configurable Auto/Manual in Settings.
- **Parser:** hand scanner + tree-sitter-sequel context. `datafusion-sqlparser-rs`
  explicitly rejected: heavy dep, hard-fails on PL/SQL blocks, multi-statement
  scripts, and `&`/`:` variables — exactly our bread-and-butter.

## 2. Editor hook (kit API, verified in source)

- `EditorState::new(window, cx).language("sql")` runs in `CodeEditor` mode.
- `editor.update(cx, |e, _| e.lsp_mut().completion_provider = Some(Rc::new(p)))`
  installs a `CompletionProvider` (`gpui-base/.../input/editor/lsp/completions.rs:40-103`).
- Required methods: `completions(text: &Rope, offset, trigger, window, cx) -> Task<Result<CompletionResponse>>`
  and `is_completion_trigger(offset, new_text, cx) -> bool` (main-thread gate).
- Popup/keyboard/ghost rendering is automatic once installed. No new editor crate.
- `CompletionMenuOptions.max_width` tunable via `lsp_mut()` for long `OWNER.TABLE` labels.

## 3. v1 behavior (strict gating since 2026-09-12)

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

## 4. v1 extras (folded in per agreement)

- **Recency/frequency boost:** per-connection usage counts (persisted lightweight JSON).
- **Type icons + detail:** table/view/column/sequence/keyword kinds; `OWNER · TYPE` detail.
- **Hide system schemas** (`SYS/SYSTEM/XDB`) by default + toggle.
- **Debounce (~150ms)** + stale-task cancel (run-token pattern) against popup flicker.

## 5. Metadata cache

- New `src/metadata.rs`: `MetadataCache { tables, columns_by_table, sequences,
  fetched_at, loading }`, stored `HashMap<connection_id, Arc<Mutex<MetadataCache>>>`
  beside `SessionPool` (sessions are per-connection; caches follow).
- Background fetch only (`bg.spawn + session.lock()`), three dictionary queries
  via existing `start_query`/`fetch_more`:
  `all_tables ∪ all_views`, `all_tab_columns`, `all_sequences`.
- Triggers: lazy on first completion request + eager after connect/lazy-connect;
  invalidate on disconnect/remove; manual Refresh + 15-min TTL.
- While loading: keywords + cached subset, never block.

## 6. Files

| File | Change |
|---|---|
| `src/complete.rs` (new) | Pure: prefix, context classify, alias map, rank/filter + tests. No GPUI/driver |
| `src/metadata.rs` (new) | Cache + blocking fetchers + fake-result tests |
| `src/app.rs` | `OracleCompleter` provider per tab, `TriggerComplete` action + binding, Settings toggle, loading status |
| `src/config.rs` | `completion` pref (Auto/Manual, default Auto) |
| `tests/live.rs` | Dictionary smoke (counts>0, known-table columns) |

## 7. Follow-ups (queued, not v1)

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

## 8. Verify

- `clippy`, lib tests (`complete.rs` matrix: prefix, alias, dot-context, ranking),
  live dictionary smoke, release smoke, headless UI (popup appears, Enter accepts),
  manual matrix both themes (bare/`FROM`/`alias.`/sequence/2-char gate/shortcut/
  Manual silence/huge schema).
