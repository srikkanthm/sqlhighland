# Architecture & Code Organization

How SQLHighland is laid out and how to keep files small as it grows. See
`README.md` for features and `PROGRESS.md` for the build log.

## Layering

The crate is split so the database/completion logic builds and tests without
Xcode/Metal. `src/lib.rs` is the map:

- **GUI-free core** (no `gui` feature needed): `db`, `config`, `model`,
  `session`, `metadata`, `schema`, `complete`, `sql`, `export`, `filetab`,
  `fsutil`, `keychain`.
- **GUI** (`#[cfg(feature = "gui")]`): `app`, `guitheme`, dialogs, `run`,
  `providers`, `browser`, `sidebar`.

The binary (`main.rs`) is a thin launcher over `app::SqlHighlandView`.

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
  complete.rs     completion engine (types + re-exported submodules)
  complete/tests.rs
  ...             one module per remaining concern (db, config, run, ...)
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

## Verify a split

```sh
cargo clippy --features gui --all-targets   # must stay clean
cargo test --lib                            # core unit tests
cargo test --features gui --test menus --test browser_tree --test themes
```

## Remaining large files (next extraction targets)

- `complete.rs` (~1,500) — split into `complete/{context,cards,aliases,
  ranking}.rs`. Its items are interleaved, so plan ranges carefully.
- `run.rs` (~1,300) — split executors vs. export drain vs. script gates.
- `db.rs` (~830) / `settings_dialog.rs` — under the threshold; leave alone.
