//! Drag-to-reorder support for the tab strip.
//!
//! The tab strip renders each tab as a `gpui_base::Tab` (role + selection
//! accessibility, which the tests read back). Dragging one live-reorders the
//! strip: the dragged tab carries a [`TabDrag`] payload, and each tab's
//! `on_drag_move` moves it to its own slot once the pointer is over it. The
//! ghost is just what follows the cursor.

use super::*;

/// Payload carried while dragging a tab. The id is stable across reorders, so
/// the drop handler and the live-reorder hit tests can find the tab by id
/// rather than by the (moving) positional element id.
#[derive(Clone)]
pub(crate) struct TabDrag {
    pub(crate) id: String,
    pub(crate) label: SharedString,
}

/// The floating pill drawn under the cursor while a tab is dragged. It mirrors
/// the active tab's capsule so the thing following the cursor reads as the tab.
pub(crate) struct TabDragGhost {
    label: SharedString,
    height: Pixels,
}

impl TabDragGhost {
    pub(crate) fn new(label: SharedString, height: Pixels) -> Self {
        Self { label, height }
    }
}

impl Render for TabDragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h(self.height)
            .px(px(12.))
            .flex()
            .items_center()
            .rounded_full()
            .bg(cx.theme().primary)
            .text_color(cx.theme().primary_foreground)
            .text_sm()
            .font_weight(FontWeight::MEDIUM)
            .shadow_lg()
            .opacity(0.92)
            .child(self.label.clone())
    }
}
