# Schema-tree auto-reveal on expand (REVERTED — reference only)

Status: **reverted** (2026-09-12, commit `e80d481` era). Expansion no
longer scrolls the tree. This doc preserves the solution so it can be
restored if the manual-scrolling tradeoff ever hurts more than the
double-click conflict described below.

## Problem it solved

The schema-browser tree is capped at 320px (`height_px =
(44 + rows*30).min(320)`). Expanding a node near the bottom pushed its
fresh children below the fold with no indication — users had to scroll
manually to see what just opened.

## Final solution (as reverted)

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

### Exact removed code

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

## Variants tried and also reverted

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

## Why it was reverted — the core conflict

Double-click-to-open and scroll-on-expand are structurally opposed
here: the kit toggles folders on mouse-*down*, and OS click counts
reset when press #2 lands on a different element. Any scroll between
the two presses moves the row, press #2 lands elsewhere (toggling the
wrong node as collateral), and the viewer never opens. The failure is
timing-dependent (deferred task vs. second press), hence
non-deterministic. Restoring any reveal must solve press-#2 landing,
not just reduce movement.

## Kit facts established along the way

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
