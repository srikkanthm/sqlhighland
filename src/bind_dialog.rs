//! Variables dialog: `&` substitution + `:bind` values with a shared
//! submit path, and the submit entry that routes to single runs or
//! sequential script runs.
//!
//! Extracted from `app.rs` (refactor Phase 2); behavior unchanged.

use std::rc::Rc;

use gpui::{px, App, Context, Entity, SharedString, Window};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::*;
use gpui_kit::*;

use crate::app::{Output, SqlHighlandView};
use crate::conn_picker::focus_tab_editor;
use crate::db::BindParam;
use crate::sql::{apply_substitutions, split_statements, SubVar};

/// A run deferred for variable input: the statement plus the variables that
/// still need values. Only one bind dialog opens at a time.
#[derive(Debug, Clone)]
pub(crate) struct PendingBind {
    pub(crate) tab_id: String,
    pub(crate) sql: String,
    pub(crate) subs: Vec<SubVar>,
    pub(crate) binds: Vec<String>,
    /// Script display name (`seed.sql`) when this run is an `@`-script:
    /// `sql` holds the fully expanded text, and submit re-splits it into
    /// statements for the sequential runner instead of a single `run_sql`.
    pub(crate) script: Option<String>,
}

/// One row in the variables dialog: `&name` substitution or `:name` bind.
struct BindField {
    /// Display key: `&name` or `:name`.
    key: String,
    /// Variable name without prefix.
    name: String,
    /// True for substitution (`&`), false for bind (`:`).
    is_sub: bool,
    input: Entity<InputState>,
}

fn submit_bind_fields(
    view: &WeakEntity<SqlHighlandView>,
    fields: &std::rc::Rc<Vec<BindField>>,
    error: &std::rc::Rc<std::cell::RefCell<Option<String>>>,
    cx: &mut App,
) -> bool {
    let mut sub_values: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut bind_values: Vec<BindParam> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for f in fields.iter() {
        let v = f.input.read(cx).value().to_string();
        if v.trim().is_empty() {
            missing.push(f.key.clone());
            continue;
        }
        if f.is_sub {
            sub_values.insert(f.name.clone(), v);
        } else {
            bind_values.push(BindParam {
                name: f.name.clone(),
                value: v,
            });
        }
    }
    if !missing.is_empty() {
        *error.borrow_mut() = Some(format!("Value required: {}", missing.join(", ")));
        // Rebuild the dialog so the message paints; the builder never moves
        // anything out, so this notify is crash-safe (see env-tag fix).
        view.update(cx, |_, cx| cx.notify()).ok();
        return false;
    }
    *error.borrow_mut() = None;
    view.update(cx, |this, cx| {
        this.submit_bind_dialog(sub_values, bind_values, cx);
    })
    .ok();
    true
}

impl SqlHighlandView {
    pub(crate) fn open_bind_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.bind.clone() else {
            return;
        };
        let mut fields: Vec<BindField> = Vec::new();
        for sub in &pending.subs {
            let prefix = if sub.double { "&&" } else { "&" };
            let input = cx.new(|cx| {
                InputState::new(window, cx).placeholder(format!("Value for {prefix}{}", sub.name))
            });
            fields.push(BindField {
                key: format!("&{}", sub.name),
                name: sub.name.clone(),
                is_sub: true,
                input,
            });
        }
        for b in &pending.binds {
            let input =
                cx.new(|cx| InputState::new(window, cx).placeholder(format!("Value for :{b}")));
            fields.push(BindField {
                key: format!(":{b}"),
                name: b.clone(),
                is_sub: false,
                input,
            });
        }
        // Short single-line preview so users know what they're feeding.
        // Scripts show their display name + statement count instead.
        let preview: SharedString = match &pending.script {
            Some(display) => {
                let n = split_statements(&pending.sql).len();
                format!("{display} · {n} statements").into()
            }
            None => {
                let flat: String = pending.sql.split_whitespace().collect::<Vec<_>>().join(" ");
                const CAP: usize = 200;
                if flat.len() > CAP {
                    format!("{}…", flat.chars().take(CAP).collect::<String>()).into()
                } else {
                    flat.into()
                }
            }
        };
        let view = cx.entity().downgrade();
        let title: SharedString = if pending.subs.is_empty() {
            "Enter binds".into()
        } else if pending.binds.is_empty() {
            "Enter substitution variables".into()
        } else {
            "Enter variables".into()
        };
        // Shared across builder re-runs (the dialog rebuilds every render):
        // the builder is `Fn`, so nothing may be moved out of it.
        let fields: Rc<Vec<BindField>> = Rc::new(fields);
        // Validation message slot, dialog-local like the env-tag pill state:
        // submit handlers write it and notify; the builder only reads it,
        // so the view entity is never touched during render (no double-lease).
        let submit_error: Rc<std::cell::RefCell<Option<String>>> =
            Rc::new(std::cell::RefCell::new(None));
        // Keyboard flow: focus the first field on open so typing + Enter
        // works without touching the mouse. (Focusing a handle before its
        // element mounts is fine — GPUI resolves it on render.)
        let first_input = fields.first().map(|f| f.input.clone());
        let tab_id = pending.tab_id.clone();
        self.note_dialog_open();
        window.open_dialog(cx, move |dialog, _, cx| {
            let fields = fields.clone();
            let submit_error = submit_error.clone();
            let muted = cx.theme().muted_foreground;
            let danger = cx.theme().danger;
            let mut body = v_flex().gap_2().w_full();
            body = body.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("{} variable(s)", fields.len())),
            );
            // Substitution section first, then binds (fields already ordered).
            for (fx, f) in fields.iter().enumerate() {
                let section = if f.is_sub { "& substitution" } else { ": bind" };
                body = body.child(
                    v_flex()
                        .gap_1()
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child(format!("{} · {}", f.key, section)),
                        )
                        .child(Input::new(&f.input).w_full()),
                );
                let _ = fx;
            }
            if let Some(err) = submit_error.borrow().clone() {
                body = body.child(div().text_xs().text_color(danger).child(err));
            }
            body = body.child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child(format!("Statement: {preview}")),
            );
            let ok_view = view.clone();
            let ok_fields = fields.clone();
            let ok_err = submit_error.clone();
            let ok_tab = tab_id.clone();
            let cancel_view_ok = view.clone();
            let cancel_tab_ok = tab_id.clone();
            let cancel_view_btn = view.clone();
            let cancel_tab_btn = tab_id.clone();
            let run_view = view.clone();
            let run_fields = fields.clone();
            let run_err = submit_error.clone();
            let run_tab = tab_id.clone();
            dialog
                .title(title.clone())
                .w(px(440.))
                .child(body)
                // Enter confirms (the kit binds `enter` → Confirm in the
                // dialog context; single-line inputs don't consume it, so it
                // bubbles here). False keeps the dialog open on empty fields.
                .on_ok(move |_, window, cx: &mut App| {
                    if submit_bind_fields(&ok_view, &ok_fields, &ok_err, cx) {
                        focus_tab_editor(&ok_view, &ok_tab, window, cx);
                        true
                    } else {
                        false
                    }
                })
                // Escape cancels: drop the deferred run and refocus.
                .on_cancel(move |_, window, cx: &mut App| {
                    cancel_view_ok
                        .update(cx, |this, cx| {
                            this.pending.bind = None;
                            cx.notify();
                        })
                        .ok();
                    focus_tab_editor(&cancel_view_ok, &cancel_tab_ok, window, cx);
                    true
                })
                .footer(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("bind-cancel").label("Cancel").on_click(
                            move |_, window, cx: &mut App| {
                                cancel_view_btn
                                    .update(cx, |this, cx| {
                                        this.pending.bind = None;
                                        cx.notify();
                                    })
                                    .ok();
                                window.close_dialog(cx);
                                focus_tab_editor(&cancel_view_btn, &cancel_tab_btn, window, cx);
                            },
                        ))
                        .child(Button::new("bind-run").primary().label("Run").on_click(
                            move |_, window, cx: &mut App| {
                                if submit_bind_fields(&run_view, &run_fields, &run_err, cx) {
                                    window.close_dialog(cx);
                                    focus_tab_editor(&run_view, &run_tab, window, cx);
                                }
                            },
                        )),
                )
        });
        // Focus after opening (open_dialog focuses the dialog layer; the
        // field must win so keystrokes land in it).
        if let Some(input) = first_input {
            let handle = input.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
    }

    fn submit_bind_dialog(
        &mut self,
        sub_values: std::collections::HashMap<String, String>,
        bind_values: Vec<BindParam>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending.bind.take() else {
            return;
        };
        let Some(ix) = self.tab_index(&pending.tab_id) else {
            return;
        };
        let conn_id = match self.tabs[ix].connection_id.clone() {
            Some(id) => id,
            None => {
                self.tabs[ix].output = Some(Output::error("Select a connection for this tab"));
                cx.notify();
                return;
            }
        };
        // Persist `&&` values for the session; `&`-only values are one-shot.
        {
            let entry = self.defines.entry(conn_id).or_default();
            for sub in &pending.subs {
                if sub.double {
                    if let Some(v) = sub_values.get(&sub.name) {
                        entry.insert(sub.name.clone(), v.clone());
                    }
                }
            }
            // Full map = previously defined + just-entered.
            let mut full = entry.clone();
            for (k, v) in &sub_values {
                full.insert(k.clone(), v.clone());
            }
            let final_sql = apply_substitutions(&pending.sql, &full);
            match pending.script {
                Some(name) => {
                    // Substitute first, split second: values are literals
                    // that must not disturb statement boundaries.
                    let statements: Vec<String> = split_statements(&final_sql)
                        .into_iter()
                        .map(|s| s.text)
                        .collect();
                    self.run_script(&pending.tab_id, name, statements, bind_values, cx);
                }
                None => {
                    self.record_user_run(&pending.tab_id, &final_sql);
                    self.run_sql(&pending.tab_id, final_sql, bind_values, cx);
                }
            }
        }
    }
}
