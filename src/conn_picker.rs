//! Connection picker dialog: search/filter/scroll list, row click
//! and Enter handling, and the single parameterized pick resume shared
//! by every pick mode (bind+run, new tab, rebind, buffer script).
//!
//! Extracted from `app.rs` (refactor Phase 2); behavior unchanged.

use std::rc::Rc;

use gpui::{px, App, Context, ScrollHandle, Window};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use crate::app::{env_tag, Density, SqlHighlandView};
use crate::config::Preferences;
use crate::model::Environment;

/// A run deferred for connection choice: the statement waits while the user
/// picks the tab's connection. Only one pick dialog opens at a time.
/// What picking a connection does once chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PickAfter {
    /// Classic unbound-run mode: bind the tab and run its statement.
    Run,
    /// Cmd+K mode: open a NEW tab bound to the pick (no run).
    NewTab,
    /// Shift+Cmd+K mode: rebind the ACTIVE tab to the pick (no run).
    Rebind,
    /// Whole-buffer script mode: bind the tab and run its buffer as a
    /// script (fresh buffer text is re-read on resume, never stored).
    ScriptBuffer,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingPick {
    pub(crate) tab_id: String,
    pub(crate) sql: String,
    pub(crate) after: PickAfter,
}

/// One parameterized resume for every connection pick: both the row
/// click and Enter handlers share it. Closes the picker, binds per
/// `after`, then runs that mode's follow-up. Collapses the four
/// near-identical helpers it replaces; per-mode behavior is unchanged.
fn pick_connection_resume(
    view: &WeakEntity<SqlHighlandView>,
    after: PickAfter,
    tab_id: &str,
    sql: &str,
    conn_id: &str,
    window: &mut Window,
    cx: &mut App,
) {
    // Close the picker first so a variables dialog opened below lands on a
    // clean dialog stack, and focus lands cleanly for the no-dialog modes.
    window.close_dialog(cx);
    let focus_id: Option<String> = view
        .update(cx, |this, cx| {
            match after {
                PickAfter::Run => {
                    if let Some(t) = this.tab_by_id(tab_id) {
                        t.connection_id = Some(conn_id.to_string());
                    }
                    this.pending.pick = None;
                    this.persist_tabs(cx);
                    this.start_run(tab_id, sql.to_string(), window, cx);
                    // No focus call: the run manages it (variables dialog
                    // focuses itself; direct runs keep editor focus).
                    None
                }
                PickAfter::NewTab => {
                    let new_id = this.add_tab(Some(conn_id.to_string()), String::new(), window, cx);
                    if let Some(ix) = this.tab_index(&new_id) {
                        this.select_tab(ix, window, cx);
                    }
                    this.pending.pick = None;
                    this.persist_tabs(cx);
                    // Establish the session now so the tab is live before the
                    // first run (failures surface through the standard
                    // connect-failed path, same as sidebar Connect).
                    this.connect_connection(conn_id, window, cx);
                    Some(new_id)
                }
                PickAfter::Rebind => {
                    if let Some(t) = this.tab_by_id(tab_id) {
                        t.connection_id = Some(conn_id.to_string());
                    }
                    this.pending.pick = None;
                    this.persist_tabs(cx);
                    // Same eager session as a fresh Cmd+K tab (failures surface
                    // through the standard connect-failed path).
                    this.connect_connection(conn_id, window, cx);
                    Some(tab_id.to_string())
                }
                PickAfter::ScriptBuffer => {
                    if let Some(t) = this.tab_by_id(tab_id) {
                        t.connection_id = Some(conn_id.to_string());
                    }
                    this.pending.pick = None;
                    this.persist_tabs(cx);
                    this.run_buffer_as_script(tab_id, window, cx);
                    None
                }
            }
        })
        .ok()
        .flatten();
    // Outside the update above: reading the leased view here would panic.
    if let Some(id) = focus_id {
        focus_tab_editor(view, &id, window, cx);
    }
}

/// Return keyboard focus to the tab's editor so the next Cmd+Enter works
/// immediately after the dialog closes (no reliance on focus-restore alone).
pub(crate) fn focus_tab_editor(
    view: &WeakEntity<SqlHighlandView>,
    tab_id: &str,
    window: &mut Window,
    cx: &mut App,
) {
    let editor = view.upgrade().and_then(|v| {
        v.read(cx)
            .tabs
            .iter()
            .find(|t| t.id == tab_id)
            .map(|t| t.editor.clone())
    });
    if let Some(editor) = editor {
        let handle = editor.read(cx).focus_handle(cx);
        window.focus(&handle, cx);
    }
}

impl SqlHighlandView {
    /// Cmd+K: open the connection picker; choosing opens a NEW tab
    /// bound to the pick (no run). No-op while a pick is already pending
    /// (picker open) so the deferred run it carries is never clobbered.
    /// The active tab id is kept only to refocus it on cancel/Esc.
    pub(crate) fn open_pick_for_new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.pick.is_some() {
            return;
        }
        let tab_id = self.active_tab().id.clone();
        self.pending.pick = Some(PendingPick {
            tab_id,
            sql: String::new(),
            after: PickAfter::NewTab,
        });
        self.open_conn_pick_dialog(window, cx);
    }

    /// Shift+Cmd+K: open the connection picker; choosing rebinds the
    /// ACTIVE tab to the pick and connects (no new tab, no run). Same
    /// already-pending guard as Cmd+K.
    pub(crate) fn open_pick_for_rebind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending.pick.is_some() {
            return;
        }
        let tab_id = self.active_tab().id.clone();
        self.pending.pick = Some(PendingPick {
            tab_id,
            sql: String::new(),
            after: PickAfter::Rebind,
        });
        self.open_conn_pick_dialog(window, cx);
    }

    /// Connection picker for unbound runs: choosing binds the tab and the
    /// deferred statement runs immediately (variables dialog next, if needed).
    /// Search-first: type to filter, Enter runs on the first match, click
    /// picks any row. Builder-safe like the other dialogs: everything is
    /// cloned in, the builder never touches the view entity.
    pub(crate) fn open_conn_pick_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.pick.clone() else {
            return;
        };
        struct PickRow {
            id: String,
            name: String,
            detail: String,
            env: Environment,
        }
        let rows: Vec<PickRow> = self
            .connections
            .iter()
            .map(|c| PickRow {
                id: c.id.clone(),
                name: c.name.clone(),
                detail: format!("{}@{}/{}", c.user, c.host, c.service_name),
                env: c.environment,
            })
            .collect();
        let rows: Rc<Vec<PickRow>> = Rc::new(rows);
        // Search field, rebuilt per open so no stale filter survives.
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder("Type to filter connections…"));
        let view = cx.entity().downgrade();
        let tab_id = pending.tab_id.clone();
        let sql = pending.sql.clone();
        // Picker mode: Run binds the tab and runs; Cmd+K opens a new
        // tab; Shift+Cmd+K rebinds the active tab. Cloned into every
        // handler below (click, Enter) so both paths agree.
        let pick_mode = pending.after;
        // Explicit scroll handle (NOT the overflow_y_scrollbar() wrapper):
        // the wrapper's caller-id keying misbehaves for dialog content that
        // rebuilds every render, while an owned handle tracks stably.
        let scroll_handle = Rc::new(ScrollHandle::new());
        // Last filter seen: typing rewinds to the top, since a stale offset
        // could hide the whole shortened list. Compared in the builder (the
        // input's own change notification already repaints every keystroke).
        let last_filter: Rc<std::cell::RefCell<String>> =
            Rc::new(std::cell::RefCell::new(String::new()));
        // Active row (index into the filtered list below): hover drives it,
        // Enter confirms it. Reset on filter change like the scroll offset.
        let active: Rc<std::cell::RefCell<usize>> = Rc::new(std::cell::RefCell::new(0));
        // Filtered ids per render, for Enter (which runs outside the build).
        let shown_ids: Rc<std::cell::RefCell<Vec<String>>> =
            Rc::new(std::cell::RefCell::new(Vec::new()));
        // Keyboard flow: search takes focus on open; Enter confirms the
        // highlighted match. The builder re-runs every render, so focusing
        // happens one-shot on the first build (a pre-mount focus call
        // alone may not stick).
        let search_focus = search.read(cx).focus_handle(cx);
        let focused_once: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let search_in = search.clone();
        self.note_dialog_open();
        window.open_dialog(cx, move |dialog, window, cx| {
            let rows = rows.clone();
            let search = search_in.clone();
            if !focused_once.get() {
                focused_once.set(true);
                window.focus(&search.read(cx).focus_handle(cx), cx);
            }
            let muted = cx.theme().muted_foreground;
            // Interface density: match the dialogs' compact sizing.
            let d = Density::for_level(Preferences::load().ui_density);
            // Highlighted-match wash (same selected language as the
            // dialog pills): the row Enter will take, live with hover
            // and the filter.
            let first_bg = cx.theme().accent.opacity(0.25);
            let filter = search.read(cx).value().to_string();
            if *last_filter.borrow() != filter {
                *last_filter.borrow_mut() = filter.clone();
                scroll_handle.set_offset(gpui_kit::point(px(0.), px(0.)));
                *active.borrow_mut() = 0;
            }
            let needle = filter.to_lowercase();
            let shown: Vec<&PickRow> = if needle.trim().is_empty() {
                rows.iter().collect()
            } else {
                rows.iter()
                    .filter(|r| {
                        r.name.to_lowercase().contains(&needle)
                            || r.detail.to_lowercase().contains(&needle)
                    })
                    .collect()
            };
            let mut body = v_flex()
                .gap(px(d.gap))
                .w_full()
                .child(Input::new(&search).w_full().with_size(d.control_size));
            if rows.is_empty() {
                body = body.child(div().text_sm().text_color(muted).child(match pick_mode {
                    PickAfter::Run => "No connections yet — add one to run this statement.",
                    PickAfter::NewTab | PickAfter::Rebind | PickAfter::ScriptBuffer => {
                        "No connections yet — add one to get started."
                    }
                }));
            } else if shown.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(format!("No matches for “{filter}”.",)),
                );
            } else {
                body = body.child(div().text_xs().text_color(muted).child(match pick_mode {
                    PickAfter::Run => "Type to filter, Enter runs on the highlighted match.",
                    PickAfter::NewTab => {
                        "Type to filter, Enter opens a new tab on the highlighted match."
                    }
                    PickAfter::Rebind => {
                        "Type to filter, Enter rebinds the active tab to the highlighted match."
                    }
                    PickAfter::ScriptBuffer => {
                        "Type to filter, Enter runs the buffer script on the highlighted match."
                    }
                }));
            }
            // Enter works off this snapshot (it runs outside the build).
            *shown_ids.borrow_mut() = shown.iter().map(|r| r.id.clone()).collect();
            // Cap + scroll: long connection lists overflow the dialog.
            // Explicit handle + overflow_y_scroll (NOT the Scrollable
            // wrapper, whose caller-id keying misbehaves for content that
            // rebuilds every render). Rows hang directly off the scroll
            // area (not a nested column) so tracked item indices address
            // rows — the rewind-on-type below targets item 0, the first row.
            let scroll_handle = scroll_handle.clone();
            let mut scroll_body = div()
                .id("conn-pick-scroll")
                .test_support()
                .w_full()
                .max_h(px(400.))
                .overflow_y_scroll()
                .track_scroll(&scroll_handle)
                .flex()
                .flex_col()
                .gap(px(d.gap));
            // No right gutter here: it would shrink the rows, so the hover /
            // first-match wash stopped short of the dialog's right edge. The
            // scrollbar overlay sits on the wrap (full width); the inset that
            // keeps text clear of it lives on each row's content instead.
            for (rix, r) in shown.iter().enumerate() {
                let pick_view = view.clone();
                let pick_tab = tab_id.clone();
                let pick_sql = sql.clone();
                let pick_mode = pick_mode;
                let hover_view = view.clone();
                let hover_active = active.clone();
                let conn_id = r.id.clone();
                let mut line = h_flex()
                    .gap(px(d.gap))
                    .items_center()
                    .w_full()
                    .px(px(d.pane_pad))
                    // Keep row text/labels clear of the overlaid scrollbar
                    // while the row background still spans the full width.
                    .pr_5()
                    .py(px(d.row_py))
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|this| this.bg(muted.opacity(0.15)));
                line = line.child(
                    v_flex()
                        .flex_1()
                        .child(
                            div()
                                .when(d.is_compact, |e| e.text_xs())
                                .when(!d.is_compact, |e| e.text_sm())
                                .child(r.name.clone()),
                        )
                        .child(div().text_xs().text_color(muted).child(r.detail.clone())),
                );
                if let Some(tag) = env_tag(r.env, 0.0, cx) {
                    line = line.child(tag);
                }
                scroll_body = scroll_body.child(
                    div()
                        .id(("conn-pick", rix))
                        .test_support()
                        .flex_shrink_0()
                        .w_full()
                        // Highlighted row is the Enter target: hover moves
                        // the highlight here (repaint only when it changes).
                        .when(rix == *active.borrow(), |this| this.bg(first_bg))
                        .child(line)
                        .on_hover(move |hovered, _, cx| {
                            if *hovered && *hover_active.borrow() != rix {
                                *hover_active.borrow_mut() = rix;
                                hover_view.update(cx, |_, cx| cx.notify()).ok();
                            }
                        })
                        .on_click(move |_, window, cx: &mut App| {
                            pick_connection_resume(
                                &pick_view, pick_mode, &pick_tab, &pick_sql, &conn_id, window, cx,
                            );
                        }),
                );
            }
            // Same always-visible scrollbar overlay as the connection
            // dialog: the kit default (hover-only) hides picker overflow.
            let pick_sb = (*scroll_handle).clone();
            body = body.child(
                div()
                    .id("conn-pick-scroll-wrap")
                    .test_support()
                    .w_full()
                    .max_h(px(400.))
                    .relative()
                    .child(scroll_body)
                    .child(
                        div().absolute().inset_0().child(
                            Scrollbar::vertical(&pick_sb)
                                .id("conn-pick-scrollbar")
                                .mode(ScrollbarMode::Always),
                        ),
                    ),
            );
            let ok_view = view.clone();
            let ok_rows = rows.clone();
            let ok_active = active.clone();
            let ok_shown = shown_ids.clone();
            let ok_tab = tab_id.clone();
            let ok_sql = sql.clone();
            let ok_mode = pick_mode;
            let cancel_view = view.clone();
            let cancel_tab = tab_id.clone();
            let esc_view = view.clone();
            let esc_tab = tab_id.clone();
            let mut footer = h_flex()
                .gap(px(d.gap))
                .child(div().flex_1())
                // Footer buttons sit after the search field in Tab order.
                // The dialog X is hidden (Esc cancels).
                .child(
                    Button::new("pick-cancel")
                        .label("Cancel")
                        .with_size(d.control_size)
                        .tab_index(100)
                        .on_click(move |_, window, cx: &mut App| {
                            cancel_view
                                .update(cx, |this, cx| {
                                    this.pending.pick = None;
                                    cx.notify();
                                })
                                .ok();
                            window.close_dialog(cx);
                            focus_tab_editor(&cancel_view, &cancel_tab, window, cx);
                        }),
                );
            if rows.is_empty() {
                let add_view = view.clone();
                footer = footer.child(
                    Button::new("pick-add")
                        .primary()
                        .label("Add connection…")
                        .with_size(d.control_size)
                        .tab_index(101)
                        .on_click(move |_, window, cx: &mut App| {
                            window.close_dialog(cx);
                            add_view
                                .update(cx, |this, cx| {
                                    this.pending.pick = None;
                                    this.start_add(window, cx);
                                })
                                .ok();
                        }),
                );
            }
            dialog
                .title("Select connection")
                .w(px(400.))
                .close_button(false)
                .child(body)
                // Enter confirms the highlighted match (hover or, by
                // default/reset, the first). False keeps the dialog open
                // (no matches); the close is manual so a variables dialog
                // opened below lands on a clean stack.
                .on_ok(move |_, window, cx: &mut App| {
                    let ids = ok_shown.borrow();
                    let pick = (!ids.is_empty()).then(|| {
                        let ix = (*ok_active.borrow()).min(ids.len() - 1);
                        ids[ix].clone()
                    });
                    let pick = pick
                        .as_deref()
                        .and_then(|id| ok_rows.iter().find(|r| r.id == id));
                    match pick {
                        Some(row) => {
                            pick_connection_resume(
                                &ok_view, ok_mode, &ok_tab, &ok_sql, &row.id, window, cx,
                            );
                            false
                        }
                        None => false,
                    }
                })
                // Escape cancels: drop the deferred run and refocus.
                .on_cancel(move |_, window, cx: &mut App| {
                    esc_view
                        .update(cx, |this, cx| {
                            this.pending.pick = None;
                            cx.notify();
                        })
                        .ok();
                    focus_tab_editor(&esc_view, &esc_tab, window, cx);
                    true
                })
                .footer(footer)
        });
        // Search takes focus on open (open_dialog focuses the dialog layer;
        // the field must win so typing + Enter work immediately).
        window.focus(&search_focus, cx);
    }
}
