# Autocomplete engine plan

Goal: make completion **scope-correct**, **broader in coverage**, **deeper in
catalog**, **better ranked**, and **engine-agnostic** so a second database
(Postgres next) is additive rather than a rewrite.

Status legend: `[ ]` planned, `[~]` in progress, `[x]` done.

## Guiding decisions

- **Incremental, not a rewrite.** No from-scratch autocomplete library and no
  new hard-failing parser. `datafusion-sqlparser-rs` stays rejected (hard-fails
  on incomplete/PL/SQL/`&`-var input — the live-typing case).
- **tree-sitter as structure, dictionary as names.** `tree-sitter-sequel`
  supplies nesting/scoping; it never supplies identifiers. It is a
  generic/Postgres-flavored grammar (no `CONNECT BY`/`START WITH`, no `MERGE`
  statement node, partial buffers yield ERROR nodes), so every structural use
  has a **lexical fallback** — the same posture `sqlparse.rs` already takes.
- **Scope from the debounced pass.** The existing debounced background parse
  (`src/app/tabs.rs` `schedule_diagnostics`) also produces a distilled,
  `Send`, byte-span-indexed scope forest stored on the tab. Completion reads it
  at the cursor and falls back to the lexical engine when absent/stale. No
  parse per keystroke.
- **Dialect seam early.** Even with one engine, Oracle-specific catalog/rules
  move behind a `Dialect` trait so Postgres is additive.
- **Pure core stays pure.** `src/complete` stays GUI-free and unit-tested; the
  parser-dependent code is GUI-gated (like `src/sqlparse.rs`).

## Current architecture (as-is)

- Pure engine: `src/complete/` — `word_prefix`/`word_at`, `qualifier_before`,
  `classify_context` (hand-rolled clause scanner), `build_alias_map`,
  `join_condition_candidates`, `rank_candidates`, keyword/function catalogs.
- Compose + LSP mapping: `src/providers.rs` `completion_items_for`.
- Metadata cache: `src/metadata.rs` (`MetadataCache`, `columns_for`, `fks`,
  `sequences`), fetched through `MetadataProvider`/`provider_for`.
- Engine seams: `schema.rs` (`DbEngine`, `SchemaProvider`), `session.rs`,
  `db.rs` (`DbClient`).
- Structure: `src/sqlparse.rs` (diagnostics only today), GUI-gated.

### Known limits (all confirmed)

- `build_alias_map` is statement-wide and flat: subquery aliases leak across
  scopes; same-name shadowing is wrong.
- No CTEs (`WITH x AS (…) … FROM x` treats `x` as an unknown table).
- DML positions work only incidentally (`INTO`/`UPDATE` happen to hit
  `FROM`/`Predicate`); `MERGE` falls through to `BareWord`.
- No projection aliases in `ORDER BY`/`GROUP BY`/`HAVING`.
- Catalog is small (~40 functions, ~80 keywords); no data types.
- No snippet/tab-stops or signature help (kit-blocked; see below).

## Phases

### Phase 0 — Dialect seam (no behavior change) `[x]`

Introduce `src/complete/dialect.rs`:

- `trait Dialect` exposing catalogs and rules:
  - identifier fold (`Upper`/`Lower`/`Preserve`), identifier quote char,
    dollar-quoting support
  - statement starters, expression/predicate keywords, clause-follow sets
  - keyword/function/data-type catalogs
  - system schemas + prefixes + `is_system_schema`
  - **preferred schemas** (generalizes "own schema" to a set: Oracle → the
    connected user; Postgres → `search_path`)
  - sequence-member syntax (`NEXTVAL`/`CURRVAL` vs `nextval('seq')`) and
    whether sequence pseudo-columns apply
  - grammar/structural hook (which grammar the Phase A extractor uses)
- `OracleDialect` returns today's constants unchanged.
- `DbEngine::dialect()` maps engine → dialect (`src/schema.rs`).
- Completion path (`src/providers.rs`) reads through the dialect.

**Done:** `src/complete/dialect.rs` (trait + `OracleDialect` + `oracle()`),
`DbEngine::dialect()` (`src/schema.rs`), `SqlHighlandView::engine_of`
(`src/app/actions.rs`), and `providers.rs` routes catalogs/system checks/
sequence members through the dialect. Tests: `oracle_dialect_exposes_the_catalog`,
`engine_maps_to_its_dialect`. Zero behavior diff (all suites green).

Deferred to when a driver lands (**0b**): thread the dialect into
`metadata.rs`/`schema.rs` fetchers (they take `&mut dyn DbClient` with no engine
today), and make cache key normalization dialect-aware (Oracle uppercases keys;
Postgres folds to lowercase).

Exit: zero behavior diff; existing `complete/tests.rs` + full suite green. **Met.**

### Phase A — Structural scope (biggest win) `[x]`

- Pure `src/complete/scope.rs`: `ScopeForest`/`Scope`/`Relation { Table | View |
  Cte | Subquery }` with alias + byte spans; `scope_at` / `visible_relations`
  (nearest-wins, correlated) / `projection_at` / `cte_at`.
- GUI-gated `src/sqlscope.rs`: `extract(text) -> ScopeForest` from the tree.
- `providers.rs` prefers the scope (`scope_from_forest`) and falls back to the
  lexical path (`lexical_scope_tables` + `build_alias_map`) when absent/empty.
  `ColumnOf` serves CTE/subquery columns from the scope, base tables from the
  cache.
- Wins delivered: CTE columns offered; nested subqueries resolve to the nearest
  scope (no alias bleed); subquery relations carry their projection columns.
- Storage: the scope rides the existing debounced diagnostics pass
  (`tabs.rs` `schedule_diagnostics`), stored on `QueryTab.scope` and
  invalidated on edit. Diagnostics disabled ⇒ no scope ⇒ lexical fallback.

**A.1 (model + extractor, no behavior change)** and **A.2 (wire into
completion)** both done. Grammar notes discovered while building:

- A `select` node spans **only** the select list; `from`/`where`/`group_by`/
  `order_by` are **siblings** under the same parent (`statement` or
  `subquery`). A scope is built from a `select` plus its siblings, spanning
  `select.start .. parent.end`.
- `select` nesting depth = number of ancestor `subquery` nodes (not `select`
  ancestors).
- Selects inside a `cte` are skipped (their columns feed the CTE relation);
  `WITH` target columns are read from the CTE body projection.
- A `select_expression` holds all comma-separated `term`s; a term's `AS alias`
  is a field on the **term**.

Tests: 3 pure + 7 gui-extractor (`src/complete/scope.rs`, `src/sqlscope.rs`),
2 provider-mapping (`src/providers.rs`), and the end-to-end
`tests/completion_scope.rs` (CTE columns appear only once a scope is
installed).

**Follow-ups** (folded into later phases): projection aliases in
ORDER BY/GROUP BY/HAVING (Phase C); DML positions and `set_operation`/`UNION`
scopes (Phase B).

### Phase B — DML positions `[x]`

Tree-driven detection of INSERT target + column list, UPDATE target/`SET`;
offer that table's columns with no keyword noise, falling back to today's
incidental handling when unparsed.

**Done:** `DmlAnchor { start, end, kind, target }` (`DmlKind::InsertColumns` /
`UpdateSet`) in `complete/scope.rs`; `sqlscope.rs` emits anchors for `insert`
(column-list span + target) and `update` (`SET` up to `WHERE`, else statement
end, + target). Providers short-circuit to the anchor's target columns (plus
functions for UPDATE SET) before the generic context. Tests: extractor
(INSERT/UPDATE anchors, the VALUES list is not an anchor, the WHERE clause is
not inside the SET anchor), pure `dml_at`, provider `relation_columns`, and the
end-to-end `dml_positions_offer_target_columns` (target columns present, no
`FROM` keyword, predicate keywords still present in WHERE).

**Follow-up:** `MERGE` (the grammar has no merge node) and `DELETE … USING`
stay on the lexical path; revisit with a per-dialect structural hook.

### Phase C — Projection aliases `[x]`

Capture select-list aliases; offer them in `ORDER BY`, ranked above keywords
(Oracle allows aliases only in `ORDER BY`, not `GROUP BY`/`HAVING`/`WHERE`).

**Done:** `Scope.order_group` records the `ORDER BY` span (running to the end
of its statement), and `ScopeForest::allows_projection_alias` gates a new
Predicate branch in `providers.rs` that adds the select-list aliases (skipping
names already offered as columns). Tests: pure
`projection_aliases_only_inside_order_or_group`, extractor
`order_by_span_is_recorded_for_projection_aliases`, and the end-to-end
`order_by_offers_projection_alias`.

### Bug fix — cursor in trailing whitespace `[x]`

Reported: `SELECT * FROM EMPLOYEES WHERE ␣` offered keyword noise instead of
columns. Cause: tree-sitter spans end at the last token, so the cursor after
the space was past `Scope.end`; `scope_at`/`visible_relations` returned nothing
and, because a (non-empty) forest suppressed the lexical fallback, only
keywords/functions remained.

**Fix:** `ScopeForest::enclosing(offset)` (innermost containing scope, else the
nearest preceding one — mirroring `sql::statement_at`'s trailing-space rule)
now backs `visible_relations`/`projection_at`/`allows_projection_alias`;
`ORDER BY` spans and the UPDATE `SET` anchor run to the end of the statement;
and `providers.rs` falls back to the lexical path if the structural branch
resolves **no** aliases/tables, so a structural gap can never hide columns.
Tests: pure `enclosing_tolerates_a_cursor_past_the_parsed_span`, the
trailing-whitespace assertion in the ORDER BY extractor test, and the
end-to-end `where_with_trailing_space_offers_columns`.

### Phase D — Catalog depth (per dialect) `[x]`

Expand Oracle functions/keywords/data types and analytic/window templates;
per-statement keyword sets (CREATE/ALTER/DROP/GRANT…). Catalog lives in the
dialect, so another engine is a new impl.

**Done:** `ORACLE_FUNCTIONS` grown (32 → ~90: math, string, `REGEXP_*`,
conversion, rolling-window/analytic like `LAG`/`LEAD`/`FIRST_VALUE`/
`PERCENTILE_*`/`NTILE`/`RATIO_TO_REPORT`/`STDDEV`/`VARIANCE`); `ORACLE_KEYWORDS`
extended with analytic (`OVER`, `PARTITION`, `ROWS`, `RANGE`, `PRECEDING`,
`FOLLOWING`, `UNBOUNDED`, `CURRENT`, `ROW`, `KEEP`), DDL (`CONSTRAINT`,
`PRIMARY`, `FOREIGN`, `REFERENCES`, `UNIQUE`, `CHECK`, `DEFAULT`, `USER`),
hierarchical (`LEVEL`, `CONNECT_BY_ROOT`, `CYCLE`, `NOCYCLE`), and more. New
`ORACLE_DATA_TYPES` exposed via `Dialect::data_types`. Invariants held:
functions sorted and disjoint from keywords; subsets ⊆ keywords. Tests:
existing `function_table_is_consistent` / `keyword_subsets_are_sane` plus the
data-type assertions in `oracle_dialect_exposes_the_catalog`.

Data types are cataloged for **cast contexts** (Postgres `x::t` / `CAST`);
wiring a cast completion context is deferred (Phase F).

### Tail contexts — origin-aware continuations `[x]`

`BareWord` used to be returned after any identifier, so `SELECT * FROM t `
offered the whole keyword catalog (statement starters, DDL, functions). The
token scan now distinguishes tails:

- select-list tail (`SELECT emp `) → `SelectList` (columns, functions, `FROM`);
- table tail (`FROM t ` / `DELETE FROM t ` / `JOIN t ` / `UPDATE t ` /
  `INSERT INTO t ` / `MERGE INTO t ` / `… USING t `) →
  `CompleteContext::FromTail(FromOrigin)`.

`FromOrigin` selects the continuations in `providers.rs`: `FROM_FOLLOW`
(`WHERE`, `GROUP BY`, `ORDER BY`, `ORDER SIBLINGS BY`, `HAVING`, the Oracle
join combinations `JOIN`/`INNER JOIN`/`LEFT [OUTER] JOIN`/`RIGHT [OUTER] JOIN`/
`FULL [OUTER] JOIN`/`CROSS JOIN`/`NATURAL [INNER|LEFT|RIGHT|FULL] JOIN`,
`CONNECT BY`, `START WITH`, `UNION [ALL]`/`INTERSECT`/`MINUS`,
`FETCH FIRST`/`FETCH NEXT`/`OFFSET`, `FOR UPDATE`), `JOIN_FOLLOW` (adds
`ON`/`USING`), `UPDATE_FOLLOW` (`SET`), `INTO_FOLLOW` (`VALUES`/`SELECT`),
`MERGE_FOLLOW` (`USING`), `USING_FOLLOW` (`ON`), and nothing for a DDL `TABLE`
tail. No functions or statement starters. `allows_empty_prefix` fires for these
tails, so the list appears right after the space. `BareWord` remains only for
genuinely ambiguous positions.

Clause keywords are offered as **phrases**, not fragments: the catalog holds
`ORDER BY`, `GROUP BY`, `CONNECT BY`, `START WITH`, `ORDER SIBLINGS BY`,
`PARTITION BY`, `INSERT INTO`, `DELETE FROM`, `MERGE INTO`, and the join
combinations, so accepting one inserts the whole phrase. `UNION ALL` already
worked this way.

**Known limitation:** `MERGE INTO` is detected from the preceding token; the
grammar has no `merge` node, so the rest of a `MERGE` stays on the lexical path.

### CASE expressions `[x]`

`scan_clause` tracks `CASE … END` nesting (with `case_depth`), so the case
keywords resolve and the cursor after `END` returns to the **enclosing**
clause:

- `CASE ␣` → `CaseStart` (`WHEN`);
- `WHEN ␣` / condition → `CaseCondition` (columns, functions, predicate
  operators, `THEN`; no clause transitions);
- `THEN ␣` / `ELSE ␣` → `CaseResult` (columns, functions, `CASE`, `WHEN`,
  `ELSE`, `END`);
- `END ␣` → the enclosing clause (select-list tail in a select item, predicate
  in `WHERE`, …), handling nested cases.

Functions are only pushed in the CASE contexts once a prefix is typed, so the
90-entry function catalog can't crowd out (`COMPLETE_LIMIT`) the small case
keyword set at an empty prefix. `allows_empty_prefix` fires for all three so
the popup appears right after the space.

Also fixed here: `tokenize` now skips single-quoted string literals (honoring
`''`), so a literal like `'from'`/`'where'` no longer switches the detected
clause and literals don't leak identifiers into the alias map.

**Known limits:** `END` also closes PL/SQL blocks — `BEGIN`/`DECLARE` now bail
clause scanning to `BareWord` rather than guess; `MERGE … WHEN MATCHED THEN`
reads as a case condition/result (MERGE is lexical/partial anyway).

### Structural expression contexts `[x]`

- **Subquery start**: `FROM (`, `IN (`, `EXISTS (`, `JOIN (`, and after a set
  operator (`UNION [ALL]`, `INTERSECT`, `MINUS`) → `SubqueryStart` offers
  `SELECT`/`WITH` (never DML/DDL). Determined from the token before the `(`.
- **CAST types**: `CAST(x AS |` → `CastType`, offering the dialect's
  `data_types()` (`ORACLE_DATA_TYPES`) as `TYPE` candidates.
- **Analytic window**: `OVER (|` → `WindowClause` (`PARTITION BY`, `ORDER BY`,
  `ROWS`, `RANGE`).
- **USING columns**: `USING (|` → `UsingColumns`, the intersection of the
  joined relations' column names.
- **JOIN … ON tails**: `detect_join_on` now also fires when conditions are
  already present, returning the tail offset; the provider filters out
  conditions already typed, so `… ON a = b AND |` offers the remaining FK
  links instead of nothing, and a fully-typed condition falls through to the
  predicate (so `AND` is still offered).

### Metadata expansion `[x]`

- **Synonyms** (`ALL_SYNONYMS`): fetched and offered after `FROM` as
  `SYNONYM`; `MetadataCache::columns_for` resolves a synonym name to its
  underlying table's columns.
- **Materialized views** (`ALL_MVIEWS`) added to the table/view fetch.
- **Package members** (`ALL_PROCEDURES`): `pkg.` now completes the package's
  members as call skeletons (`MEMBER()`), via the new
  `CompleteContext::PackageMember` (detected after `ColumnOf` when the
  qualifier names a known package).
- **Usage counts persist** (`~/.config/sqlhighland/usage.toml`): the frequency
  ranking now survives relaunch (`config::load_usage`/`save_usage`).

### Table-first select (Option A) `[x]`

In an **empty select list** with no `FROM`/`JOIN` yet, the popup leads with the
dictionary's tables (and CTE names), each inserting **`* FROM t `** via a new
per-candidate `Candidate.insert` override. The statement is populated and the
select list is then ready for column autocomplete — no column list is ever
inserted automatically.

- Trigger (`select_list_is_empty`): text between the nearest `SELECT` and the
  word start is empty modulo whitespace, `--`/`/* */` comments, and
  `DISTINCT`/`ALL`. `SELECT *` is therefore *not* empty (no `* * FROM t`), nor
  is `SELECT a,`. A typed prefix (`SELECT em`) still counts as empty — the
  prefix is replaced on accept.
- Name insertion is fold-safe: `qualified_ident`/`quote_ident` bare the
  connected user's schema and quote any segment that a bare identifier
  wouldn't preserve.
- While the select list is empty, `SELECT_FOLLOW` (`FROM`/`WHERE`/…) is
  omitted (invalid there), so the list is tables + functions + `EXPR_KEYWORDS`.
- Labels are deduped case-insensitively (a table and a same-named synonym
  don't both appear).

**Tabled:** an explicit "Expand Columns" action (`⌘⇧E`) to turn `*`/`alias.*`
into the column list, and any auto-expansion.

### Phase E — Ranking & UX `[x]`

Scope-proximity ranking (nearer scope higher), projection aliases top tier,
better fuzzy/contains scoring. Signature help / tab-stop snippets remain
**kit-blocked** (`$1` would insert literally, no post-accept hook); revisit if
the kit gains support.

**Done:** `Candidate.depth` (0 = innermost scope) added; `rank_candidates`
breaks ties by depth after prefix quality and before usage, so a correlated
column resolves to the nearest definition. `providers.rs` sets depth from
`scope_tables`' nearest-first order. Projection aliases already rank above
keywords via `CandidateKind::Column`. Test:
`ranking_prefers_the_nearest_scope`. Fuzzy scoring left as-is (contains +
prefix tiers) — revisit only if users report misses.

### Phase F — Postgres (when a driver lands) `[ ]`

Driver (`DbClient`) + `MetadataProvider` (pg_catalog) + `SchemaProvider` +
`Dialect` + grammar hook. Completion needs no structural change if Phase 0 was
done. Note: adding an engine is dominated by driver/metadata work, not
completion.

### Postgres readiness (shapes the Phase 0 surface)

| Concern | Oracle | Postgres |
| --- | --- | --- |
| Bare identifier fold | UPPER | lower |
| Quoting | `"` id, `'` str | `"` id, `'` str, `$tag$…$tag$` |
| Unqualified resolution | own user schema | `search_path` (a set) |
| System schemas | `SYSTEM_SCHEMAS` | `pg_catalog`, `information_schema`, `pg_toast` |
| Sequences | `seq.NEXTVAL/CURRVAL` | `nextval('seq')`, identity columns |
| Pseudo-objects | `DUAL`, `ROWNUM`, `SYSDATE` | none; no-FROM `SELECT` valid |
| DML extras | `MERGE` | `ON CONFLICT`, `RETURNING`, `UPDATE … FROM` |
| Casts | `CAST(x AS t)` | `x::t` and `CAST` (data-type completion) |

## Known issues & workarounds

### Auto-complete wedges after clearing the buffer (kit sticky offset) `[x]`

**Symptom:** after clearing (or replacing) the whole buffer, auto-complete
stops firing until `Ctrl+Space` is pressed. Typing normal text produces no
popup.

**Root cause (upstream, `gpui-base`):** the editor keeps a sticky
`CompletionMenuState::trigger_start_offset` and, on each change,
`handle_completion_trigger` bails when the cursor is before it
(`gpui-base/src/input/editor/lsp/completions.rs`):

```rust
let start_offset = ...completion.trigger_start_offset.unwrap_or(start);
if new_offset < start_offset { return; }   // no trigger, and the offset is kept
```

The offset is only cleared by `completion_menu.hide()` (via the component's
`sync_lsp` when `open` flips false) or reset by `present_completion_items`. The
early return above — and the spawned-task path that finds **zero** items (sets
`open = false` but leaves the offset) — both keep it. So a trigger at offset
`N` followed by clearing the buffer (cursor → 0) makes every later keystroke
satisfy `new_offset < N` and bail. `Ctrl+Space` works only because our
`trigger_complete` calls `present_completion_items`, which rewrites the offset.

**Local fix (`src/app/tabs.rs`, editor `Change` subscription):** after a text
change, read `trigger_start_offset` and `cursor()` inline; if the cursor is
before the offset (`cursor < start`), schedule a **deferred**
`present_completion_items(cursor, "", vec![])` to reset it. Deferred because
the editor is mutably leased during its own change dispatch (the reset mutates
it). This is the same call `Ctrl+Space` uses, so no new API, and the kit's
base offset is otherwise never reset.

**Performance:** the check runs on every change but only does two cheap reads
on an entity the same handler already reads (`schedule_diagnostics` clones the
full buffer text just before). The `cursor < start` test is false for ordinary
forward typing, and true only after a backward move (paste-over, clearing,
large deletions), where a reset is genuinely needed — so nothing is allocated,
deferred, or re-rendered on the common keystroke path.

**Upstream suggestion:** clear `trigger_start_offset` in the early-return and
zero-items paths (or store the last trigger offset and compare before bailing).
Until the pin is bumped, the local reset above is the workaround.

## Cross-cutting

- Pure logic + tests stay in `src/complete`; parser code GUI-gated.
- Every phase keeps the lexical path as an exact fallback (no regression when
  the grammar chokes on PL/SQL, hierarchical queries, or partial input).
- Each phase verified with `cargo fmt`, `cargo clippy --features gui-test
  --all-targets -- -D warnings`, and the full `gui-test` suite.

## Known follow-ups

- **Double parse per debounced pass**: `tabs.rs` runs `syntax_issues` and
  `sqlscope::extract` on the same text; each parses. `syntax_issues` also
  masks PL/SQL and can parse once per statement (`SqlCheckScope::Statement`),
  so an N-statement buffer costs N+1 parses. Background + debounced, so it
  only bounds worker tail latency; sharing a tree is possible but complicated
  by the masking. Revisit if profiling flags it.
- **Cast context**: `Dialect::data_types` (`ORACLE_DATA_TYPES`) is surfaced at
  `CAST(x AS |)`; Postgres `x::t` still needs the Postgres dialect (Phase F).
- **Remaining from the gap audit**: standalone procedures/functions at call
  sites (only package members are fetched); CTE explicit column lists
  (`WITH cte(a,b) AS`); quoting non-fold-safe identifiers on insert;
  `GROUPING SETS`/`ROLLUP`/`CUBE`/`NULLS FIRST`/`NULLS LAST`; hover/definition
  using the structural scope; fuzzy subsequence matching; window `ORDER BY`
  already offered but frame sub-clauses are minimal.

## Risks

- Generic grammar mis-parses Oracle-specific syntax → fallback must be correct
  and cheap to reach.
- Debounce latency: completion uses the lexical path until the scope pass
  lands; no popup regression, no perceived delay.
- Constraining the seam too narrowly now would leak Oracle assumptions later;
  Phase 0 deliberately covers the Postgres table above.
