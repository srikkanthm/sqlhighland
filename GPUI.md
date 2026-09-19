# Building Desktop Apps with GPUI

A portable field guide to the `gpui` / `gpui-kit` stack, distilled from the
SQLHighland codebase. Copy this file into a new repo and work from it.

**Scope:** practical, battle-tested patterns and the traps that actually bite.
Every claim below was verified in a shipping app; `path:line` citations point
at the source in SQLHighland (adjust or drop them when you copy it).

**Pinned stack (read the caveat first):**

| Crate | Version | Notes |
|---|---|---|
| `gpui` (`gpui-pre`) | `=0.3.4` | Zed snapshot published to crates.io |
| `gpui-kit` | `=0.6.1` + `tree-sitter-sql` | Component facade over `gpui`/`gpui-component`/`gpui-base` |
| `gpui-kit-assets` | `=0.6.1` | Full Lucide catalog; the default bundle is missing icons |
| Rust | ≥ 1.89 | macOS is the current target; Metal toolchain required |

> **API-churn warning.** `gpui-pre` is a moving snapshot of Zed's internal API
> and `gpui-kit` is a thin facade over it. Names and signatures below are true
> for the pinned versions. When you bump, expect breakage in `Window`,
> `Context`, the `test` module, and the editor/LSP traits. Keep those usages
> behind small helpers so a bump touches one file.

---

## 0. Mental model

Four ideas explain almost everything that follows:

1. **Retained, not immediate.** GPUI only repaints views whose entities were
   marked dirty (`cx.notify()`). A window repaint by itself reuses cached
   views. If something changed and the screen didn't, you forgot a `notify()`.
2. **Entities are `Rc<RefCell<…>>`-like.** You lease them with `read`/`update`.
   Leasing one entity while it is already leased on the same call stack
   **panics** or silently fails. This is the single most common bug.
3. **Actions, not raw key events.** Keybindings dispatch named actions to a
   focused node; they bubble to ancestors. Where you register an action and
   how many times matters.
4. **Async is explicit.** Background work runs on `cx.background_executor()`
   and marshals results back to the UI thread through a weak entity handle.

---

## 1. Bootstrap

`main.rs` should be a thin launcher. Put the view in a library so headless
tests can construct it without spawning a process (`src/main.rs:29`,
`src/lib.rs:17`).

The ordering in `app.run(|cx| { … })` is deliberate — each step depends on the
one before it:

```rust
fn main() {
    let app = gpui_kit::application()
        .with_assets(AppAssets)                       // layer app icons over the kit catalog
        .with_quit_mode(QuitMode::LastWindowClosed);  // else macOS stays windowless after close

    app.run(move |cx| {
        gpui_kit::init(cx);                           // required before any kit component
        guitheme::register_themes(cx);                // register theme families
        fonts::register_bundled_fonts(cx);            // before first text layout
        guitheme::capture_theme_mono_default(cx);     // before any user font choice

        // App-global actions: defer the body, re-take the window inside.
        cx.on_action(|_: &OpenSettings, cx| {
            cx.defer(|cx| {
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| { /* … */ });
                }
            });
        });
        cx.bind_keys([KeyBinding::new("cmd-,", OpenSettings, None)]);
        cx.set_menus(app_menus());

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(780.)), cx)),
            window_min_size: Some(size(px(640.), px(480.))),
            ..TitleBar::window_options()              // custom themed title bar
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                window.set_window_title("My App");
                // Apply theme here: "System" needs a live window/OS appearance.
                let view = cx.new(|cx| SqlHighlandView::new(window, cx));
                cx.set_global(AppView(view.downgrade()));  // window-level handlers have no view
                cx.new(|cx| Root::new(view, window, cx))   // Root MUST be the first level
            })
            .expect("Failed to open window");
        })
        .detach();
    });
}
```

Load-bearing details (`src/main.rs`):

- **Custom `AssetSource`.** `gpui-kit-assets`' `AllAssets` is complete, but if
  you ship your own icon, layer it and delegate the rest
  (`src/main.rs:12-27`). Missing icons render blank rather than error.
- **`QuitMode::LastWindowClosed`.** macOS defaults to `Explicit`, leaving a
  single-window app alive with no window (`src/main.rs:34-39`).
- **`Root` first.** GPUI requires it as the window's first element, and it
  hosts the dialog layer (`src/main.rs:181-182`).
- **`set_global` for window-level handlers.** A global `cx.on_action` has a
  window but no view handle; stash a `WeakEntity` and retrieve it
  (`src/main.rs:178-180`, `src/app.rs:508-519`).
- **`cx.set_menus` twice.** AppKit resolves key equivalents from the keymap
  *snapshot at `set_menus` time*. `main.rs` runs before the view registers its
  keys, so the view re-calls `cx.set_menus(app_menus())` after its
  `bind_keys` (`src/app.rs:1177-1182`).

---

## 2. Entities, handles, and callbacks

### Weak handles

Any callback, task, or dialog builder that outlives the current borrow should
capture `cx.entity().downgrade()`, never a strong `Entity` (`src/app.rs:806`,
`src/app/render.rs:27`). Upgrade-and-update is the standard marshalling shape:

```rust
let view = cx.entity().downgrade();
cx.spawn(async move |cx| {
    let rows = cx.background_executor().spawn(async move { fetch_rows() }).await;
    view.update(cx, |this, cx| { this.set_rows(rows); cx.notify(); }).ok();
})
.detach();
```

`src/run/query.rs:275-445` is the full version: outer `cx.spawn`, inner
`cx.background_executor().spawn`, then a weak `view.update`.

### Listener vs subscribe

- `cx.listener(|this, event, window, cx| …)` — element event handlers that
  need `&mut Self` (`src/app/render.rs:285`).
- `cx.subscribe_in(&entity, window, |this, _, ev, window, cx| …)` — observe
  another entity when you need a window (`InputEvent`, `SelectEvent`;
  `src/app/tabs.rs:117`).
- `cx.subscribe(&entity, |this, _, ev, cx| …)` — same, no window needed.

**Subscriptions must be stored.** A dropped `Subscription` stops firing. Keep
them in a `Vec<Subscription>` field (`src/app.rs:289`, `src/app/tabs.rs:115`).

### `cx.spawn` vs `cx.spawn_in`

- `cx.spawn(async move |cx| …)` — no window required (`src/run/query.rs:275`).
- `cx.spawn_in(window, async move |cx, window| …)` — native file pickers and
  anything needing the window (`src/app/tabs.rs:1059`).

### `cx.defer`: the lease escape hatch

If you're currently inside an operation that leases an entity, mutating that
same entity again (or calling into it) **fails silently or panics**. Defer the
mutation to run after the current event batch:

```rust
// Inside the editor's own change dispatch — read inline, mutate deferred.
let stale = { let editor = editor_sub.read(cx); /* … */ };
if stale {
    let editor = editor_sub.clone();
    cx.defer(move |cx| {
        editor.update(cx, |editor, cx| { /* safe now */ });
    });
}
```

`src/app/tabs.rs:141-157` (sticky completion offset) and `:1261-1265` (cursor
placement) are the canonical examples. App-global action handlers defer for a
related reason: keypress dispatch holds the window take (`src/main.rs:52-58`).

### `window.on_next_frame`

Some layout values simply don't exist until after the first frame. Schedule a
follow-up with `window.on_next_frame(move |_, cx| …)` (`src/app/tabs.rs:302`).

---

## 3. Rendering and layout

### The `Render` impl

```rust
impl Render for SqlHighlandView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .child(self.render_main(window, cx))
            // Last: the dialog layer, once per window.
            .child(Root::render_dialog_layer(window, cx))
    }
}
```

`src/app/render.rs:1456-1577`. Split large layouts into `render_*` builder
methods that each re-open `impl SqlHighlandView`.

### Element essentials

- `.id("name")` — required for stateful/scrollable elements and test lookup
  (`src/app/render.rs:351`, `:367`).
- `.test_support()` — registers the element so tests can `try_find` it
  (`src/app/render.rs:302`, `:741`).
- `on_click` / `on_mouse_up` / `on_drop` / `on_hover` / `on_action`.
- `h_flex()` / `v_flex()` are the bread and butter; use `.gap()`, `.flex_1()`,
  `.min_w_0()`, `.min_h_0()`.

### Min-size rules for virtualized content

Flex children **refuse to shrink below their content** unless you say so. A
virtualized table then sees an unbounded viewport and silently loses
virtualization and horizontal scroll. Add `.min_w_0().min_h_0()` plus
`.overflow_hidden()` on the container (`src/app/render.rs:755-761`).

A grid that must be a definite pixel size (because a flex chain collapses the
virtualized body to zero rows) should mirror an explicit panel size rather
than rely on flex (`src/app/render.rs:1174-1209`).

### Responsive sizing via `on_prepaint`

Measure every frame, but only `notify()` when a derived value actually
changes. Keep the derived value a pure function of the measurement so it can't
oscillate:

```rust
.on_prepaint({
    let view = cx.entity().downgrade();
    move |bounds, _, cx| {
        let width = bounds.size.width.as_f32();
        view.update(cx, |this, cx| {
            let before = this.toolbar_size();
            this.main_width.set(width);
            if this.toolbar_size() != before { cx.notify(); }  // only on threshold flip
        }).ok();
    }
})
```

`src/app/render.rs:1261-1278`. Use the same hook to capture geometry you need
later, e.g. per-tab bounds (`src/app/render.rs:280-284`).

### Scrolling

Prefer an **owned `ScrollHandle`** with `track_scroll` + `overflow_x_scroll` /
`overflow_y_scroll`:

```rust
let handle = Rc::new(ScrollHandle::new());
div()
    .id("list-scroll")
    .test_support()
    .overflow_y_scroll()
    .track_scroll(&handle)
```

`src/conn_picker.rs:296-312`, `src/app/render.rs:366-373`.

To reveal an item, compute its content-space bounds and clamp the new offset:

```rust
fn scroll_item_into_view(&self, ix: usize) {
    let Some(item) = self.tab_scroll.bounds_for_item(ix) else {
        self.tab_scroll.scroll_to_item(ix);   // not laid out yet; kit applies later
        return;
    };
    let viewport = self.tab_scroll.bounds();
    let offset = self.tab_scroll.offset();
    // bounds_for_item is content-space; on-screen = content + offset.
    let left = item.left() + offset.x;
    if left >= viewport.left() && item.right() + offset.x <= viewport.right() {
        return;
    }
    let max = self.tab_scroll.max_offset();
    let mut next = offset;
    next.x = (viewport.left() - item.left()).clamp(-max.x, px(0.));
    self.tab_scroll.set_offset(next);
}
```

`src/app/tabs.rs:787-807`.

---

## 4. Actions and keybindings

### Declare once

```rust
gpui_kit::actions!(myapp, [RunQuery, CloseTab, OpenSettings, Quit, /* … */]);
```

`src/app.rs:61-96`.

### Register exactly once

**Duplicate registration fires the action twice.** Keep each action in exactly
one place. App-global actions (`Quit`, `Settings`, `NewTab`, `CloseTab`) belong
*only* in `main.rs`, because dialogs render outside every element root and
would otherwise miss dialog-focused keypresses (`src/main.rs:52-54`,
`src/app/render.rs:1338-1340`).

The exception: actions must be duplicated on **each independent root an action
can bubble through**, because bubbling only reaches ancestors. If you have two
sibling sidebar roots, register the tab/file actions on both
(`src/sidebar.rs:303-307`).

### App-global handler shape

Defer and re-take the window; grab the view from the stored global:

```rust
cx.on_action(|_: &NewTab, cx| {
    cx.defer(|cx| {
        if let Some(handle) = cx.windows().into_iter().next() {
            let _ = handle.update(cx, |_, window, cx| {
                if let Some(view) = app_view(cx) {
                    view.update(cx, |this, cx| this.new_tab_command(window, cx));
                }
            });
        }
    });
});
```

`src/main.rs:107-119`. Without the defer, the nested update fails silently
because dispatch already holds the window take.

### Scope bindings to key contexts

```rust
cx.bind_keys([KeyBinding::new("cmd-enter", RunQuery, Some("Input"))]);   // editor only
cx.bind_keys([KeyBinding::new("cmd-c", CopySelection, Some("DataTable"))]); // grid only
cx.bind_keys([KeyBinding::new("cmd-t", NewTab, None)]);                  // global
```

`src/app.rs:1113-1176`. Check the kit's own `Input`/`DataTable` bindings before
claiming a chord (comments throughout `src/app.rs` note which are taken).

### Element-level handlers

```rust
.on_action(cx.listener(|this, _: &RunQuery, window, cx| {
    if this.editor_focused(window, cx) { this.run(window, cx); }
}))
```

`src/app/render.rs:615-648`. Always double-check focus, because a context
binding can still reach you when focus is elsewhere.

### Focus at startup

Without an initial focus target, GPUI has no focused dispatch node and **global
bindings never translate into actions** (`src/app.rs:1632-1638`). Focus your
primary input in the view constructor.

---

## 5. Async, threads, and cancellation

### Run blocking work off the UI thread

```rust
let view = cx.entity().downgrade();
cx.spawn(async move |cx| {
    let result = cx.background_executor().spawn(async move {
        do_blocking_io()             // never on the UI thread
    }).await;
    view.update(cx, |this, cx| { this.apply(result); cx.notify(); }).ok();
})
.detach();
```

`src/run/query.rs:275-445`. Clone the executor handle (`cx.background_executor()`)
into worker closures (`src/browser.rs:339`).

### Cancel by dropping the `Task`

Store a `Task` in a field; assigning a new one drops (cancels) the old. Use
this for debounces and superseding work (`src/app/tabs.rs:1278`, `:1311`).
Fire-and-forget work uses `.detach()` (`src/run/query.rs:445`).

### Cancel results without driver support: run tokens

Bump a generation counter on start/cancel; discard a result whose token no
longer matches (`src/app.rs:235-239`, `src/run/query.rs:186`, `:332`).

### Don't hold locks across the UI path

Never lock a mutex on a render/UI path. Keep a separate, render-visible
liveness flag (`src/app.rs:611-614`). Make lock helpers poison-tolerant
(`unwrap_or_else(|e| e.into_inner())`) so one panic degrades to stale data
instead of a crash loop (`src/session.rs:17-23`).

### The editor lease rule

While the editor entity is leased for an event (e.g. a completion trigger),
**do not read the editor entity again** — you'll double-lease. Providers
receive the `&Rope`; use that plus view-owned state instead
(`src/app/lsp.rs:184-191`). Reads during the entity's own change dispatch are
fine; only mutations must be deferred (`src/app/tabs.rs:1242-1265`).

---

## 6. Dialogs and overlays

### Open / close

```rust
window.open_dialog(cx, move |dialog, window, cx| { /* builder */ });
window.open_alert_dialog(cx, move |dialog, window, cx| { /* … */ });
window.close_dialog(cx);
```

`src/conn_picker.rs:231`, `src/app/tabs.rs:505`. Close the current dialog
before opening the next so the stack stays clean (`src/conn_picker.rs:59`).

### The dialog builder must never touch the view

Builders are `Fn` and **re-run every render**. Reading or updating the view
inside one double-leases and panics. Keep all builder-local state in
`Rc<RefCell<…>>` / `Rc<Cell<…>>` cells (`src/conn_picker.rs:211-228`,
`src/connection_dialog.rs:187-202`).

```rust
let filter = Rc::new(RefCell::new(String::new()));
let scroll = Rc::new(ScrollHandle::new());
window.open_dialog(cx, move |dialog, window, cx| {
    // use filter/scroll here; never `view.read(cx)`
});
```

### The `Scrollable` wrapper trap

`overflow_y_scrollbar()` keys its state by caller location and re-ids the
inner div. For dialog content that rebuilds every render, it misbehaves. Use
an owned `ScrollHandle` + `track_scroll` + `overflow_y_scroll` instead
(`src/conn_picker.rs:208-211`, `:296-312`).

### Modal bookkeeping

- A monotonically increasing counter tracks "is a dialog open" so a global
  shortcut can tell whether Settings is already the top dialog
  (`src/app.rs:749-751`, `src/settings_dialog.rs:86-95`).
- `window.has_active_dialog(cx)` is the guard against stacking modals
  (`src/app.rs:1647-1652`).
- Focus on open: focus the target field one-shot on the first build (a
  pre-mount focus call alone may not stick), and re-apply after `open_dialog`
  focuses the dialog layer (`src/conn_picker.rs:227-237`).

---

## 7. Editor and LSP integration

Create an editor and install providers per tab:

```rust
let editor = cx.new(|cx| {
    EditorState::new(window, cx).language("sql").default_value(text)
});
editor.lsp_mut().completion_provider(Some(Arc::new(CompletionProvider { … })));
editor.lsp_mut().hover_provider(Some(Arc::new(HoverProvider { … })));
```

`src/app/tabs.rs:25-29`, `:93-100`. Providers hold only a
`WeakEntity<View>` + id and resolve live state at call time
(`src/app/lsp.rs:20-23`).

- **Accept** goes through `editor.insert_completion(&item, range, window, cx)`
  (`src/app/tabs.rs:1570`).
- **Manual trigger / clear** uses
  `editor.present_completion_items(start, prefix, items, cx)`
  (`src/providers.rs:697`).
- **There is no "completion accepted" event.** The only signal is a generic
  `InputEvent::Change`, so if you need post-accept behavior, set a flag when
  you serve the item and inspect it from the change handler
  (`src/app/tabs.rs:117-159`). Clear the flag only when the shape is actually
  confirmed, or unrelated keystrokes will consume it.
- **Emit an explicit per-item `textEdit`.** The kit's sticky
  `trigger_start_offset` can otherwise replace the whole buffer on a second
  accept (`src/providers.rs:630-638`).
- **Self-heal a stale trigger offset.** If the cursor ends up before
  `trigger_start_offset`, auto-triggering is blocked until Ctrl+Space; reset it
  when you detect the backward move (`src/app/tabs.rs:141-157`).

---

## 8. Headless testing

Gate GUI tests behind a feature so plain `cargo test` never builds the UI
stack:

```toml
[features]
gui-test = ["gui", "gpui-kit/test-support"]

[[test]]
name = "my_ui"
required-features = ["gui-test"]
```

`Cargo.toml:67`, `:100-144`.

Test skeleton:

```rust
#![recursion_limit = "256"]   // deep render types need this

#[gpui_kit::test]
async fn dialog_scrolls_and_picks(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let handle = cx.open_window(size(px(1100.), px(780.)), |window, cx| {
        let view = cx.new(|cx| SqlHighlandView::new(window, cx));
        Root::new(view, window, cx)
    });
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("SELECT 1 FROM dual", cx);
        for _ in 0..20 {                 // headless can swallow early input
            window.press("cmd-enter", cx);
            window.render_frame(cx);
            if window.try_find("dialog").is_some() { break; }
        }
    });
    cx.run_until_parked();               // let detached background work settle
}
```

`tests/ui_picker.rs:40-60`. Useful primitives:

| Need | Use |
|---|---|
| Force a frame | `window.render_frame(cx)` (repeat once or twice) |
| Deliver a scheduled next-frame callback | `window.simulate_next_frame(cx)` |
| Drive input | `window.click` / `.input` / `.press` / `.scroll` |
| Find/assert elements | `window.try_find("id")`, `try_find(("id", ix))`, `.visible()`, `.selected()` |
| Advance debounce timers | `cx.executor().advance_clock(Duration)`, `cx.background_executor.advance_clock(...)` |
| Await async landing | `cx.wait_for(handle.into(), timeout, \|window, cx\| cond).await` |
| Let background work settle | `cx.run_until_parked()` |
| Test window close guard | `VisualTestContext::from_window(...).simulate_close()` |

`tests/tab_nav.rs:653-709`, `tests/quit_guard.rs:77-113`,
`tests/sql_diagnostics.rs:49-78`.

**Process-global environment must be serialized.** Cargo runs tests in one
binary on parallel threads; if a test sets an env var, take a static mutex
first (`tests/tab_nav.rs:111-119`, `src/testenv.rs:1-17`). Give each test a
unique temp dir keyed by pid (`tests/browser_tree.rs:12-19`).

**Headless caveat:** `window.press` dispatches *without* holding the window
take, so a probe can pass while production would deadlock or no-op. For
take-sensitive paths, nest the press inside `cx.update_window`
(`docs/HISTORY.md:532-538`).

---

## 9. Pitfalls checklist

Run through this when something behaves impossibly.

| # | Trap | Fix |
|---|---|---|
| 1 | Dialog builder reads/updates the view → double-lease panic | Use `Rc<RefCell>` builder-local state (`src/connection_dialog.rs:187`) |
| 2 | `cx.notify()` in `render`/`on_prepaint` → repaint loop | Only notify when a measured value changes (`src/app/render.rs:1261`) |
| 3 | Theme change doesn't repaint | GPUI only repaints dirty views — notify after switch; keep an `AppView` global (`src/app.rs:508`) |
| 4 | Fixed-name temp file clobbered by concurrent writer | `<path>.<pid>.<seq>.tmp` (`src/fsutil.rs:151-162`) |
| 5 | Initial scroll consumed before `overflow` is known | Re-issue via `window.on_next_frame` (`src/app/tabs.rs:297-309`) |
| 6 | Virtualized row `on_click` swallowed | Use `on_mouse_up` + `event.click_count >= 2` (`src/app/render.rs:110-124`) |
| 7 | Nested hitboxes → both context menus fire | Make sibling, not child (`src/sidebar.rs:35-38`) |
| 8 | `overflow_y_scrollbar()` misbehaves for per-render content | Owned `ScrollHandle` + `overflow_y_scroll` (`src/conn_picker.rs:208`) |
| 9 | Menu items show no shortcuts | Keymap snapshot at `set_menus`; re-call after `bind_keys` (`src/app.rs:1177`) |
| 10 | Flex row child collapses to zero height | Move it into the horizontal row + `items_stretch` (`src/sidebar.rs:44-60`) |
| 11 | Duplicate action registration fires twice | Register once; duplicate only across independent roots (`src/main.rs:52`) |
| 12 | App-global handler no-ops | Dispatch holds the window take — defer + re-take (`src/main.rs:52-58`) |
| 13 | Virtualized table shows all rows / loses h-scroll | `min_w_0`/`min_h_0`/`overflow_hidden` (`src/app/render.rs:755-761`) |
| 14 | Duplicate column names collide element ids | Key grid columns positionally (`src/app/results.rs:297-307`) |
| 15 | Global shortcuts never fire | Set an initial focus target at startup (`src/app.rs:1632-1638`) |
| 16 | Stacked modals | Guard with `window.has_active_dialog(cx)` (`src/app.rs:1647-1652`) |
| 17 | Closing/quit loses state | Stage it: transactions first, then files; withhold follow-up on failure (`src/app/actions.rs:25-36`) |
| 18 | Editor leases twice during a provider/change | Read `&Rope` + view state inline; defer mutations (`src/app/lsp.rs:184-191`) |
| 19 | Optimistic/liveness UI blocks on a lock | Separate render-visible flag; never lock on the UI path (`src/app.rs:611-614`) |

---

## 10. Quick reference

| Primitive | Where to look |
|---|---|
| Bootstrap ordering | `src/main.rs:41-187` |
| Weak handle in a long-lived closure | `src/app.rs:806`; `src/app/lsp.rs:34` |
| Blocking work + marshal back | `src/run/query.rs:275-445` |
| Store a `Task` to cancel on replace | `src/app/tabs.rs:1278`, `:1311` |
| `cx.defer` under a lease | `src/app/tabs.rs:1261-1265` |
| App-global action + defer + window take | `src/main.rs:107-132` |
| Context-scoped keybinding | `src/app.rs:1113-1176` |
| Sole-registration action on a root | `src/app/render.rs:1503-1537` |
| `open_dialog` with local `Rc<RefCell>` state | `src/conn_picker.rs:211-231` |
| Alert dialog from a background completion | `src/run/count.rs:105-131` |
| Owned scroll handle + `track_scroll` | `src/conn_picker.rs:296-312` |
| Responsive width via `on_prepaint` | `src/app/render.rs:1261-1278` |
| Reveal item in a scroll strip | `src/app/tabs.rs:787-807` |
| Headless drive + wait | `tests/ui_picker.rs:40-155` |
| Env-var test serialization | `tests/tab_nav.rs:111-119` |
| Deterministic debounce advance | `tests/quit_guard.rs:192-193` |

---

## 11. Starting a new app — order of operations

1. Pin `gpui-pre`, `gpui-kit`, `gpui-kit-assets`; add the `tree-sitter-sql`
   feature if you want a real editor.
2. Put the view in a lib (`src/lib.rs`), launcher in `src/main.rs`.
3. Copy the bootstrap skeleton from §1; verify `Root` and
   `QuitMode::LastWindowClosed`.
4. Build the entity model: one view entity, weak handles, stored
   subscriptions.
5. Add actions and keybindings; keep each action registered once.
6. Add `AppView` global early — theme switches will need it.
7. Set up the `gui-test` feature and one smoke test before the UI grows.
8. Use `on_prepaint` for any measurement-driven layout; never `notify()`
   unconditionally.
9. When tempted to read an entity from inside its own callback — defer.
