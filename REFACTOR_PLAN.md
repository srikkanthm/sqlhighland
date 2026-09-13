# Refactor plan: decomposing `app.rs` (and hardening first)

Status: **in progress** (2026-09-12). `src/app.rs` is 8,019 lines —
52% of `src/` — with a 45-field view struct and 300–600-line dialog
builders. Every feature lands there. This doc is the map; `PROGRESS.md`
keeps the per-step log.

## Ground rules (from the quality analysis)

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

## Phase 0 — robustness + hygiene (hours, first)

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

## Phase 1 — dialogs out (~1,100 lines)

- `settings_dialog.rs` (~700): the giant settings builder + pick
  rows + prefs mapping. Needs `Preferences`, view flags, toggle.
- `connection_dialog.rs` (~400): form state + `save_from_dialog` +
  `dialog_field`/`dialog_pills` helpers. The 7 `pending_*` fields
  move into a `ConnectionDialogState` struct.

## Phase 2 — flows out (~1,100 lines)

- `conn_picker.rs` + `bind_dialog.rs`: collapse the 4×
  `pick_connection_*` fns and twin open-pick fns into one
  parameterized resume; one shared dialog-footer helper (kills the
  7× footer duplication).
- `run.rs`: `start_run`/script gates, `run_sql`/`run_script`,
  export-drain UI, local `Outcome`/`StmtOutcome` enums.

## Phase 3 — surfaces out (~800 lines)

- `providers.rs` (definer/hover/completer + one shared snapshot
  helper — kills the ~30-line duplication), `browser.rs`,
  `sidebar.rs`. Browser caches move into a `ConnCache` struct;
  dialog form state into `ConnectionDialogState`; pending ops into
  `PendingOps`.

## Explicitly out of scope

- `sql.rs` / `complete.rs` untouched (exemplary as-is).
- Oracle `q'[…]'` alternative quoting (nothing handles it; consistent).
- `tokenize` string-content awareness (wrong-list, never dead-popup).
- Fixing/quarantining the flaky `cmd_w`/`cmd_t` headless tests
  (tracked separately; failures reproduce on the clean tree).

## Methodology note (learned the hard way)

Headless `window.press` dispatches *without* holding the window take,
so probes can pass while production dies. Reproduce take-sensitive
paths by nesting the press inside `update_window`; prefer pure-logic
unit tests where possible. Probes are temporary: write → verify →
delete.
