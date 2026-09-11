# Schema Browser — Plan (v1)

Agreed with user 2026-09-12. Oracle now; portable core for later engines.
Implemented + live-confirmed 2026-09-12.

## 1. Goals
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

## 2. Non-goals (v1)
Packages/procedures/indexes/triggers, DDL actions, drag-into-editor,
viewer persistence, context menus, second-engine implementation.

## 3. Architecture (portability-first)

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

## 4. Data changes
- `TableId` gains `kind: TableKind { Table, View }`. The fetch already
  unions `ALL_TABLES` + `ALL_VIEWS` — add the object-type column and
  populate `kind`. Update struct literals (metadata.rs, tests).
- No new dictionary queries: tables/views/columns/sequences all come from
  the warmed cache. Grouping: by owner (= schema), own-schema-first,
  honoring the existing show-system toggle.

## 5. Sidebar UI
- Disclosure chevron per connection row → `schemas → Tables/Views/
  Sequences → object → columns`, backed by kit `TreeState`.
- Expansion state per connection in the view; lazy per level (expand
  warms `ensure_meta` first; columns are cache hits).
- Single filter field; narrows cached names across expanded trees.
- Expand on a dead connection: connect in background, then populate
  (error surfaces inline on failure, tree stays collapsed).

## 6. Tabs: viewer kind
- `TabKind::Query | Viewer { owner, name, kind }`; editor entity kept but
  unrendered for viewers (reversible: kind checks mark every branch point
  for a future `Option<editor>` refactor — mechanical, compiler-guided).
- Viewer header: title via `display_name` (bare for own schema), Refresh
  (re-runs DESCRIBE), Close. Grid + Cancel reuse the run pipeline with a
  generated `DESCRIBE` statement; `last_sql` audit kept.
- Draft persist + tab-manifest skip viewer tabs (ephemeral).
- Editor-gated actions (Run/Format/Commit/…) no-op or hidden on viewers.

## 7. Verify
- `clippy`, lib tests (model grouping, `display_name` titles, kind
  split), live dictionary smoke (table vs view counts), headless UI
  (expand/click where the harness allows), release smoke, manual matrix
  (expand/collapse, filter, click→viewer, refresh, restart drops viewers).
