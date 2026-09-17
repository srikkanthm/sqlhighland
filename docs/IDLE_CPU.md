# Idle CPU usage

Status: **open — not reproduced / not profiled yet; app-side code ruled out**
Date: 2026-09-15
Area: UI frame/animation scheduling (app + `gpui`/`gpui-component`)

## Symptom

When the app is left alone (no typing, no queries), it is expected to sit near
**0–1% CPU**. It currently doesn't. The cost is continuous (not a one-off
spike), which points at a **repaint/frame loop** rather than a burst.

Not yet measured: we have not profiled an idle process, so the exact hot path
is unconfirmed.

## What was checked and ruled out

A static sweep of SQLHighland's own code found nothing that should spin at idle:

- **No unconditional loops/ticks.** Every `cx.spawn(...)` is event-driven
  (connect, run, export, fetch, save, metadata). The only repeating tasks are
  three 500 ms `Running…` tickers that self-exit when `!busy`:
  `src/run/query.rs:197`, `src/run/script.rs:276`, `src/run/export.rs:440`.
  The draft-save debounce is one-shot (`src/app/tabs.rs:775`, 1500 ms).
- **No unconditional `cx.notify()` in a render/prepaint path.** The two
  `on_prepaint`s only act when a measured value changes:
  - `src/app/render.rs:1042` (toolbar size) notifies only when `ToolbarSize`
    crosses a threshold.
  - `src/app/render.rs:1260` (sidebar re-pin) only `defer`s when the window
    width changes.
- **No `request_animation_frame` / `with_animation` / `Animation` in app code.**
- **No driver/session background thread.** The `rust-oracledb` fork spawns a
  thread only inside its *pool* (`src/pool/mod.rs:73`), which the app does not
  use — it holds `oracledb::Connection` directly. `CancelHandle` is a mutex,
  not a thread.
- **No LSP/provider polling** — providers are snapshot-only, triggered by
  input/hover (`src/app/lsp.rs`).
- **Theme/appearance is observer-driven**, not polled (`src/guitheme.rs`,
  `window.observe_window_appearance` in `src/app.rs:997`).

Conclusion: the idle cost is most likely in the UI toolkit's frame/animation
scheduling (or a platform frame pump), not in application logic.

## Remaining suspects (toolkit)

1. **GPUI's macOS frame pump presenting continuously** — the window never
   becoming fully idle, so a frame is drawn each display refresh.
2. **Tab-bar Underline sliding indicator** — *removed in the tab-strip rework*:
   the strip no longer uses the kit `TabBar` or its `spring()` indicator. Tabs
   are plain `gpui_base::Tab`s with a static 2px underline
   (`src/app/render.rs`), so this animation source is gone.
3. **Scrollbar fade/thumb animation** — `gpui-base/src/scrollbar.rs:1234`
   (visibility) and `:1365` (thumb width) request frames while animating.
4. **Focused text editor** — the query editor takes focus on launch
   (`src/app.rs`, end of `new`); a focused caret/text system could drive
   per-frame work on the platform.

## Diagnostics (do these first)

Non-invasive, fastest to run against a live app:

- `top -pid <pid> -l 3 -stats cpu,threads` while idle.
- **Unfocus the window** (click another app): does CPU drop? → focus/caret or
  an animation gated on window activity.
- **Minimize the window**: does CPU drop? → presentation/frame pump.
- Toggle **System Settings → Accessibility → Display → Reduce motion**. The
  toolkit collapses `spring`/`transition` animations to instant
  (`gpui-base/src/motion.rs:611`); if idle CPU drops to ~0, it is an animation
  loop, not a timer.
- Note whether a **connection is live** and whether a **grid result** is shown.

Profiling (decisive):

- `cargo build --release --features gui`, run, idle 30–60 s.
- `sample <pid> 5 -file /tmp/sqlhighland-idle.sample` — inspect the main
  thread. In `draw`/`layout` every frame ⇒ continuous repaint. Parked ⇒ the
  cost is elsewhere (thread/GPU).
- `powermetrics --samplers tasks -n 3` if per-thread attribution is needed.

## Bisect (no profiler needed)

Temporarily flip one thing at a time and watch idle CPU:

1. Tab bar back to `TabVariant::Tab` (drops the spring indicator).
2. Drop `with_handle_appearance(resize_grip())` on the splits
   (`src/app/render.rs`).
3. Open a fresh empty tab / close the results grid.
4. Unfocus the editor (click the sidebar).

Whichever stops the spin identifies the source.

## Candidate fixes (by finding)

- **Indicator spring:** render the active indicator statically (no `spring`),
  or gate it on `!cx.reduce_motion()`.
- **Scrollbar:** pin `ScrollbarMode::Always` (motionless) on the grid, or
  disable toolkit motion for the app theme.
- **Editor focus/caret:** don't auto-focus the editor at launch, or disable the
  caret animation.
- **GPUI frame pump:** add an app-side idle-repaint guard and/or fix in the
  fork (the driver fork is ours; GPUI comes from `gpui-kit`).

## Regression guard (optional)

A debug repaint/FPS counter that logs when frames render with no input, so a
future change can't silently reintroduce idle spin.

## Related

- `gpui-base/src/motion.rs`, `gpui-base/src/scrollbar.rs` — the toolkit
  animation paths above.
- [`docs/PROGRESS.md`](PROGRESS.md) — ongoing work log.
