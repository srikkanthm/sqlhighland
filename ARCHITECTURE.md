# Architecture & Code Organization

How SQLHighland is laid out and how to keep files small as it grows. See
`README.md` for features, `docs/PROGRESS.md` for the build log, and
`docs/HISTORY.md` for the original plan and completed design notes.

## Layering

The crate is split so the database/completion logic builds and tests without
the GUI's platform toolchain (Xcode/Metal on the current macOS target).
`src/lib.rs` is the map:

- **GUI-free core** (no `gui` feature needed): `db`, `config`, `model`,
  `session`, `metadata`, `schema`, `complete`, `sql`, `export`, `filetab`,
  `fsutil`, `keychain`.
- **GUI** (`#[cfg(feature = "gui")]`): `app`, `guitheme`, dialogs, `run`,
  `providers`, `browser`, `sidebar`.

The binary (`main.rs`) is a thin launcher over `app::SqlHighlandView`.

## Engine & platform seams

A second database engine plugs in without touching the UI/run plumbing:

- `DbClient` (`db.rs`) — the driver: connect/exec/txn plus the incremental
  cursor API (`start_query`/`fetch_more`/`close_cursor`, default `run_query`).
  Sessions are `SharedSession = Arc<Mutex<Box<dyn DbClient>>>`.
- `CancelToken` + `DbClient::cancel_token()` (`db.rs`) — optional per-session
  interrupt (default `None`); `SessionPool` holds one token per connection and
  the run/export paths fire it. The Oracle backend wraps the forked driver's
  `CancelHandle` — see [`docs/CANCELLATION.md`](docs/CANCELLATION.md) for the
  fork and the upstream revert guide.
- `SessionPool::get_or_create(id, engine)` (`session.rs`) — constructs the
  concrete session per `DbEngine` via `new_session` (one match arm per engine).
- `MetadataProvider` / `provider_for(engine)` (`metadata.rs`) — dictionary
  fetching; `browser.rs` names only the provider.
- `SchemaProvider` (`schema.rs`) — tree shaping + describe SQL.

Platform seams: `keychain` (OS keychain via the `keyring` crate) and `fsutil`
(owner-only perms: Unix mode bits, Windows `icacls`).

## Module map

```
src/
  app/            the main view (was one 3,491-line app.rs)
    mod (app.rs)  shared view types + struct + core methods + re-exports
    tabs.rs       tab lifecycle, SQL files, autosave drafts
    connections.rs connection CRUD, sessions, connect/disconnect
    actions.rs    run/commit/format/copy/zoom/dismiss + suggestions
    lsp.rs        editor definition/hover/completion providers
    results.rs    grid data, held cursor, table delegate
    render.rs     all render_* builders + Render impl + env tags
  sql/            statement text helpers (was one 1,721-line sql.rs)
    mod (sql.rs)  format_sql + shared lexers + re-exports
    split.rs      statement splitting / caret selection
    classify.rs   statement kind, DML detection, exec summaries
    substitute.rs &name substitution and :name binds
    script.rs     @/@@/START directives + include expansion
    tests.rs      unit tests
  complete/       completion engine (types + re-exported submodules)
    mod (complete.rs) shared types + re-exports + tests decl
    context.rs    caret-context classification
    catalog.rs    scope tables + hover/describe cards
    aliases.rs    dotted names, alias maps, JOIN…ON detection
    ranking.rs    keyword/function tables + candidate ranking
    tests.rs      unit tests
  run/            query/script/export pipeline
    mod (run.rs)  shared imports + re-exports
    query.rs      run entry gates, SQL executor, cancellation
    script.rs     `@` gates, buffer-as-script, sequential runner
    export.rs     streaming CSV/XLSX drain
  ...             one module per remaining concern (db, config, ...)
```

## The split pattern

1. **A file can be its own parent module.** With both `src/app.rs` and a
   `src/app/` directory, `src/app/tabs.rs` is the `app::tabs` submodule — no
   rename to `mod.rs` needed.
2. **Inherent impls may live in any module.** GUI code is all
   `impl SqlHighlandView { … }`; split it by writing another `impl` block in a
   child module. Behavior is unchanged.
3. **Children see the parent.** Every submodule starts with `use super::*;`,
   which imports the parent's imports and private items (private items are
   visible to descendant modules).
4. **Siblings need visibility.** A method defined in `app::tabs` and called
   from `app::render` or `app.rs` must be at least `pub(super)`. Items used
   by modules *outside* `app` (e.g. `run.rs`) stay `pub(crate)`.
5. **Preserve public paths with re-exports.** When moving `pub` items into a
   child, add `pub use child::*;` (or a named `pub use`) in the parent so
   `crate::app::Foo` / `crate::sql::foo` keep working.
6. **Keep tests in a sibling `tests.rs`.** Declare `#[cfg(test)]
   mod tests;` and put the suite in `<module>/tests.rs`. `use super::*` still
   resolves because `tests` is a child of the same parent.

## Guidelines

- **Target < ~800 lines per file.** When a file crosses that, look for a
  responsibility seam and extract a child module using the pattern above.
- Prefer splitting by *responsibility* (what the code is for) over splitting
  by *type* (one file per struct). `render.rs` is still ~1,000 lines but is a
  single cohesive concern.
- GUI modules should not be reached from the GUI-free core.
- Keep the public API at the parent module: children define, parents
  re-export.
- **Keep `SqlHighlandView` shallow.** Cohesive field clusters live in their own
  structs in `app.rs` — `ConnectionDialogState` (dialog form + option pills),
  `PendingOps` (bind/picker/password resume), `BrowserState` (dictionary
  caches, usage, schema-browser). Add a feature's state to the right struct
  rather than growing the view.

## Verify a split

```sh
cargo clippy --features gui-test --all-targets   # must stay clean
cargo test --lib                            # core unit tests
cargo test --features gui-test --test menus --test browser_tree --test themes
```

## Remaining large files

Everything is now under ~1,000 lines. The largest are `app/render.rs`
(~1,000) and `app/tabs.rs` (~830), each a single cohesive concern; `db.rs`
(~830) is one driver adapter. Split further only if a responsibility seam
appears (e.g. `render.rs` could become `app/render/{mod,editor,grid}.rs`).
