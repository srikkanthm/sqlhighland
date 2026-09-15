//! Connection add/edit dialog: form fields, role/service/SSL/
//! password/engine rows, keychain routing.
//!
//! Extracted from `app.rs` (refactor Phase 1). Dialog state lives in
//! [`crate::app::ConnectionDialogState`] (see `docs/HISTORY.md` Part 5 and the
//! P4 view-state grouping in `docs/REVIEW.md`). Also hosts the dialog's
//! "Test connection" action.

use std::cell::RefCell;
use std::rc::Rc;

use gpui::{px, App, Context, Entity, ScrollHandle, SharedString, Window};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputContentType, InputState};
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::*;
use gpui_kit::*;
use zeroize::Zeroizing;

use crate::app::{env_color, Density, PendingPassword, SqlHighlandView};
use crate::config::{Preferences, SavedConfig};
use crate::conn_picker::PickAfter;
use crate::model::{ConnectionConfig, Environment, OracleRole, PasswordMode, ServiceKind};
use crate::schema::DbEngine;
use crate::session::{lock, throwaway_session};

/// State of the dialog's "Test connection" action, shared with the dialog
/// builder through an `Rc` cell (like the option pills): the builder re-reads
/// it on every rebuild but must never touch the view entity.
#[derive(Default)]
enum TestState {
    #[default]
    Idle,
    Testing,
    Ok,
    Err(String),
}

impl SqlHighlandView {
    fn form_config(&self, cx: &App) -> ConnectionConfig {
        // Preserve the edited entry's id so live sessions keep matching.
        let edited = self.dialog.editing.and_then(|ix| self.connections.get(ix));
        let id = edited.map(|c| c.id.clone()).unwrap_or_default();
        // The dialog owns the engine like role/kind (today always Oracle).
        // Keychain/Ask modes never persist the typed secret in the file —
        // save_from_dialog routes it to the keychain (or drops it).
        let password = match self.dialog.pending_password_mode {
            PasswordMode::File => self.dialog.password.read(cx).value().to_string(),
            PasswordMode::Keychain | PasswordMode::Ask => String::new(),
        };
        ConnectionConfig {
            id,
            name: self.dialog.name.read(cx).value().to_string(),
            host: self.dialog.host.read(cx).value().to_string(),
            port: self.dialog.port.read(cx).value().parse().unwrap_or(1521),
            service_name: self.dialog.service.read(cx).value().to_string(),
            user: self.dialog.user.read(cx).value().to_string(),
            password: password.into(),
            environment: self.dialog.pending_env,
            engine: self.dialog.pending_engine,
            role: self.dialog.pending_role,
            service_kind: self.dialog.pending_service_kind,
            ssl: self.dialog.pending_ssl,
            password_mode: self.dialog.pending_password_mode,
        }
    }

    fn fill_form(&mut self, cfg: &ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog
            .name
            .update(cx, |s, cx| s.set_value(cfg.name.clone(), window, cx));
        self.dialog
            .host
            .update(cx, |s, cx| s.set_value(cfg.host.clone(), window, cx));
        self.dialog
            .port
            .update(cx, |s, cx| s.set_value(cfg.port.to_string(), window, cx));
        self.dialog.service.update(cx, |s, cx| {
            s.set_value(cfg.service_name.clone(), window, cx)
        });
        self.dialog
            .user
            .update(cx, |s, cx| s.set_value(cfg.user.clone(), window, cx));
        // Never fill stored secrets back into the form: File mode shows
        // its (legacy) value; Keychain/Ask always start blank.
        let shown_password = match cfg.password_mode {
            PasswordMode::File => cfg.password.to_string(),
            PasswordMode::Keychain | PasswordMode::Ask => String::new(),
        };
        // Keychain mode with a stored entry says so: a blank field
        // otherwise reads as "no password". Saving it untouched keeps
        // the entry (see save_from_dialog); typing replaces it.
        let pw_hint: SharedString = if cfg.password_mode == PasswordMode::Keychain
            && !cfg.id.is_empty()
            && crate::keychain::get(&cfg.id)
                .ok()
                .flatten()
                .is_some_and(|s| !s.is_empty())
        {
            "Saved in keychain".into()
        } else {
            "password".into()
        };
        self.dialog.password_snapshot = match cfg.password_mode {
            PasswordMode::Keychain => Some(shown_password.clone()),
            _ => None,
        };
        self.dialog.password.update(cx, |s, cx| {
            s.set_value(shown_password, &mut *window, cx);
            s.set_placeholder(pw_hint, &mut *window, cx);
        });
    }

    pub(crate) fn persist(&mut self, cx: &mut Context<Self>) {
        if let Ok(path) = SavedConfig::default_path() {
            let saved = SavedConfig {
                connections: self.connections.clone(),
            };
            if let Err(e) = saved.save(&path) {
                self.status = format!("Connections save failed: {e:#}").into();
                cx.notify();
            }
        }
    }

    // -- Connection CRUD + dialog ------------------------------------------

    pub(crate) fn start_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dialog.editing = None;
        self.dialog.pending_env = Environment::Untagged;
        self.dialog.pending_role = OracleRole::default();
        self.dialog.pending_service_kind = ServiceKind::default();
        self.dialog.pending_ssl = false;
        self.dialog.pending_password_mode = PasswordMode::default_for_new();
        self.dialog.pending_engine = DbEngine::default();
        // Blank form: text fields empty, standard Oracle port kept.
        self.fill_form(
            &ConnectionConfig {
                name: String::new(),
                host: String::new(),
                port: 1521,
                service_name: String::new(),
                user: String::new(),
                password_mode: self.dialog.pending_password_mode,
                ..Default::default()
            },
            window,
            cx,
        );
        self.open_connection_dialog("Add connection", window, cx);
    }

    pub(crate) fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let title = format!("Edit {}", self.connections[ix].name);
        self.dialog.editing = Some(ix);
        let cfg = self.connections[ix].clone();
        self.dialog.pending_env = cfg.environment;
        self.dialog.pending_role = cfg.role;
        self.dialog.pending_service_kind = cfg.service_kind;
        self.dialog.pending_ssl = cfg.ssl;
        self.dialog.pending_password_mode = cfg.password_mode;
        self.dialog.pending_engine = cfg.engine;
        self.fill_form(&cfg, window, cx);
        self.open_connection_dialog(&title, window, cx);
    }

    // -- Settings dialog (see settings_dialog.rs) -------------------------------

    fn open_connection_dialog(&self, title: &str, window: &mut Window, cx: &mut Context<Self>) {
        let title: SharedString = title.to_string().into();
        let view = cx.entity().downgrade();
        let (name, host, port, service, user, password) = (
            self.dialog.name.clone(),
            self.dialog.host.clone(),
            self.dialog.port.clone(),
            self.dialog.service.clone(),
            self.dialog.user.clone(),
            self.dialog.password.clone(),
        );
        // Dialog-local copy of the env tag. The dialog builder re-runs on every
        // render, so it must NOT touch the view entity here (that double-leases
        // and aborts). Click handlers (safe, outside render) sync the cell back
        // to `pending_env` and notify to rebuild with the new highlight.
        let pending_cell: Rc<RefCell<Environment>> = Rc::new(RefCell::new(self.dialog.pending_env));
        // Same pattern for role / service-kind / SSL / password-mode rows.
        let role_cell: Rc<RefCell<OracleRole>> = Rc::new(RefCell::new(self.dialog.pending_role));
        let kind_cell: Rc<RefCell<ServiceKind>> =
            Rc::new(RefCell::new(self.dialog.pending_service_kind));
        let ssl_cell: Rc<RefCell<bool>> = Rc::new(RefCell::new(self.dialog.pending_ssl));
        let pwmode_cell: Rc<RefCell<PasswordMode>> =
            Rc::new(RefCell::new(self.dialog.pending_password_mode));
        // Same pattern for the engine row (today a single Oracle pill).
        let engine_cell: Rc<RefCell<DbEngine>> = Rc::new(RefCell::new(self.dialog.pending_engine));
        // Owned scroll handle: the form now spans role/service/SSL/password
        // rows, so small windows overflow. Explicit handle + overflow_y_scroll
        // (NOT the Scrollable wrapper, whose caller-id keying misbehaves for
        // dialog content that rebuilds every render).
        let scroll_handle = Rc::new(ScrollHandle::new());
        // Live "Test connection" state, shared with the builder (which reads
        // it on every rebuild) and reset each time the dialog opens.
        let test_cell: Rc<RefCell<TestState>> = Rc::new(RefCell::new(TestState::Idle));
        self.note_dialog_open();
        window.open_dialog(cx, move |dialog, _, cx| {
            let save_view = view.clone();
            let pending = pending_cell.clone();
            let muted = cx.theme().muted_foreground;
            let d = Density::for_level(Preferences::load().ui_density);
            // Plain clone for the scrollbar overlay (same handle the
            // scroll area tracks, so the thumb stays in sync).
            let scroll_sb = (*scroll_handle).clone();
            // Fresh each rebuild so the picked pill highlights live.
            let current_env = *pending.borrow();
            dialog
                .title(title.clone())
                .w(px(400.))
                .child(
                    div()
                        .id("conn-dialog-scroll-wrap")
                        .test_support()
                        .w_full()
                        .max_h(px(480.))
                        .relative()
                        .child(
                            div()
                                .id("conn-dialog-scroll")
                                .test_support()
                                .w_full()
                                .max_h(px(480.))
                                .overflow_y_scroll()
                                .track_scroll(&scroll_handle)
                                .child(
                                    v_flex()
                                        .gap(px(d.gap))
                                        .w_full()
                                        // Gutter for the overlaid scrollbar track
                                        // (16px): without it the thumb sits on top
                                        // of the full-width inputs.
                                        .pr_5()
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Database type"),
                                                )
                                                .child({
                                                    let cell = engine_cell.clone();
                                                    let current = *cell.borrow();
                                                    let row_view = view.clone();
                                                    dialog_pills(
                                                        "conn-engine",
                                                        &[(
                                                            DbEngine::Oracle,
                                                            DbEngine::Oracle.label(),
                                                        )],
                                                        current,
                                                        Rc::new(move |e, cx: &mut App| {
                                                            *cell.borrow_mut() = e;
                                                            row_view
                                                                .update(cx, |this, cx| {
                                                                    this.dialog.pending_engine = e;
                                                                    cx.notify();
                                                                })
                                                                .ok();
                                                        }),
                                                        cx,
                                                    )
                                                }),
                                        )
                                        .child(dialog_field("Name", &name, false, muted))
                                        .child(dialog_field("Host", &host, false, muted))
                                        .child(
                                            h_flex()
                                                .gap_2()
                                                .child(div().flex_1().child(dialog_field(
                                                    "Port", &port, false, muted,
                                                )))
                                                .child(div().flex_1().child(dialog_field(
                                                    if *kind_cell.borrow() == ServiceKind::Sid {
                                                        "SID"
                                                    } else {
                                                        "Service name"
                                                    },
                                                    &service,
                                                    false,
                                                    muted,
                                                ))),
                                        )
                                        .child(dialog_field("User", &user, false, muted))
                                        .child(dialog_field("Password", &password, true, muted))
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(
                                                    div().text_xs().text_color(muted).child("Role"),
                                                )
                                                .child({
                                                    let cell = role_cell.clone();
                                                    let current = *cell.borrow();
                                                    let row_view = view.clone();
                                                    dialog_pills(
                                                        "conn-role",
                                                        &[
                                                            (OracleRole::Default, "SYSDEFAULT"),
                                                            (OracleRole::Sysdba, "SYSDBA"),
                                                            (OracleRole::Sysoper, "SYSOPER"),
                                                        ],
                                                        current,
                                                        Rc::new(move |r, cx: &mut App| {
                                                            *cell.borrow_mut() = r;
                                                            row_view
                                                                .update(cx, |this, cx| {
                                                                    this.dialog.pending_role = r;
                                                                    cx.notify();
                                                                })
                                                                .ok();
                                                        }),
                                                        cx,
                                                    )
                                                }),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Service lookup"),
                                                )
                                                .child({
                                                    let cell = kind_cell.clone();
                                                    let current = *cell.borrow();
                                                    let row_view = view.clone();
                                                    dialog_pills(
                                                        "conn-kind",
                                                        &[
                                                            (ServiceKind::ServiceName, "Service"),
                                                            (ServiceKind::Sid, "SID"),
                                                        ],
                                                        current,
                                                        Rc::new(move |k, cx: &mut App| {
                                                            *cell.borrow_mut() = k;
                                                            row_view
                                                                .update(cx, |this, cx| {
                                                                    this.dialog
                                                                        .pending_service_kind = k;
                                                                    cx.notify();
                                                                })
                                                                .ok();
                                                        }),
                                                        cx,
                                                    )
                                                }),
                                        )
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .justify_between()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Use SSL (TCPS)"),
                                                )
                                                .child({
                                                    let cell = ssl_cell.clone();
                                                    let current = *cell.borrow();
                                                    let row_view = view.clone();
                                                    Switch::new("conn-ssl")
                                                        .small()
                                                        .checked(current)
                                                        .on_change(move |checked, _, cx| {
                                                            *cell.borrow_mut() = *checked;
                                                            row_view
                                                                .update(cx, |this, cx| {
                                                                    this.dialog.pending_ssl =
                                                                        *checked;
                                                                    cx.notify();
                                                                })
                                                                .ok();
                                                        })
                                                }),
                                        )
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Password storage"),
                                                )
                                                .child({
                                                    let cell = pwmode_cell.clone();
                                                    let current = *cell.borrow();
                                                    let row_view = view.clone();
                                                    dialog_pills(
                                                        "conn-pwmode",
                                                        &[
                                                            (PasswordMode::File, "Save in file"),
                                                            (PasswordMode::Keychain, "Keychain"),
                                                            (PasswordMode::Ask, "Prompt each time"),
                                                        ],
                                                        current,
                                                        Rc::new(move |m, cx: &mut App| {
                                                            *cell.borrow_mut() = m;
                                                            row_view
                                                                .update(cx, |this, cx| {
                                                                    this.dialog
                                                                        .pending_password_mode = m;
                                                                    cx.notify();
                                                                })
                                                                .ok();
                                                        }),
                                                        cx,
                                                    )
                                                }),
                                        )
                                        .child(div().text_xs().text_color(muted).child(
                                            "Database: the PDB service name (e.g. highlandpdb).",
                                        ))
                                        .child(
                                            v_flex()
                                                .gap_1()
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .text_color(muted)
                                                        .child("Environment"),
                                                )
                                                .child(h_flex().gap(px(d.gap)).children(
                                                    Environment::ALL.iter().enumerate().map(
                                                        |(ix, env)| {
                                                            let selected = current_env == *env;
                                                            let row_view = save_view.clone();
                                                            let pending_click = pending.clone();
                                                            let (label, text_color, bg) =
                                                                match env_color(*env, cx) {
                                                                    Some(color) => (
                                                                        env.label()
                                                                            .unwrap_or("")
                                                                            .to_string(),
                                                                        color,
                                                                        if selected {
                                                                            color.opacity(0.25)
                                                                        } else {
                                                                            color.opacity(0.0)
                                                                        },
                                                                    ),
                                                                    None => (
                                                                        "None".to_string(),
                                                                        muted,
                                                                        if selected {
                                                                            muted.opacity(0.25)
                                                                        } else {
                                                                            muted.opacity(0.0)
                                                                        },
                                                                    ),
                                                                };
                                                            div()
                                            .id(("conn-env", ix))
                                            .px(px(d.pane_pad))
                                            .py(px(d.row_py))
                                            .rounded_md()
                                            .cursor_pointer()
                                            .bg(bg)
                                            .text_xs()
                                            .text_color(text_color)
                                            .hover(move |this| this.bg(text_color.opacity(0.25)))
                                            .on_click(move |_, _, cx: &mut App| {
                                                *pending_click.borrow_mut() = *env;
                                                row_view
                                                    .update(cx, |this, cx| {
                                                        this.dialog.pending_env = *env;
                                                        cx.notify();
                                                    })
                                                    .ok();
                                            })
                                            .child(label)
                                                        },
                                                    ),
                                                )),
                                        ),
                                )
                                // Visible scrollbar: the kit default only shows on
                                // hover, which hides overflow in a short dialog.
                                // Overlay bound to the same owned handle, so it
                                // tracks without the caller-id Scrollable wrapper.
                                .child(
                                    div().absolute().inset_0().child(
                                        Scrollbar::vertical(&scroll_sb)
                                            .id("conn-dialog-scrollbar")
                                            .mode(ScrollbarMode::Always),
                                    ),
                                ),
                        ),
                )
                .footer(
                    h_flex()
                        .gap(px(d.gap))
                        .child({
                            let cell = test_cell.clone();
                            let testing = matches!(*cell.borrow(), TestState::Testing);
                            let test_view = view.clone();
                            Button::new("dlg-test")
                                .label("Test connection")
                                .with_size(d.control_size)
                                .loading(testing)
                                .disabled(testing)
                                .on_click(move |_, _window, cx: &mut App| {
                                    let cell = cell.clone();
                                    test_view
                                        .update(cx, |this, cx| {
                                            this.test_connection_from_dialog(cell, cx);
                                        })
                                        .ok();
                                })
                        })
                        .child({
                            // Result sits right next to the button (no extra
                            // row above the footer).
                            let cell = test_cell.clone();
                            let state = cell.borrow();
                            match &*state {
                                TestState::Idle => div().into_any_element(),
                                TestState::Testing => div()
                                    .text_xs()
                                    .text_color(muted)
                                    .child("Testing…")
                                    .into_any_element(),
                                TestState::Ok => div()
                                    .text_xs()
                                    .text_color(cx.theme().success)
                                    .child("Success")
                                    .into_any_element(),
                                TestState::Err(msg) => div()
                                    .text_xs()
                                    .text_color(cx.theme().danger)
                                    .max_w(px(200.))
                                    .overflow_hidden()
                                    .child(msg.clone())
                                    .into_any_element(),
                            }
                        })
                        .child(div().flex_1())
                        .child(
                            Button::new("dlg-cancel")
                                .label("Cancel")
                                .with_size(d.control_size)
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("dlg-save")
                                .primary()
                                .label("Save")
                                .with_size(d.control_size)
                                .on_click(move |_, window, cx: &mut App| {
                                    save_view
                                        .update(cx, |this, cx| {
                                            this.save_from_dialog(window, cx);
                                        })
                                        .ok();
                                    window.close_dialog(cx);
                                }),
                        ),
                )
        });
    }

    fn save_from_dialog(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let mut cfg = self.form_config(cx);
        if cfg.name.trim().is_empty() {
            // Always succeed: derive a name rather than erroring in the dialog.
            cfg.name = format!("{}@{}/{}", cfg.user, cfg.host, cfg.service_name);
        }
        if cfg.name.trim().is_empty() {
            cfg.name = "Untitled".to_string();
        }
        // Keychain mode: a newly typed secret goes to the login keychain
        // (or is cleared there when blanked); an untouched field keeps
        // the stored entry — blank means "keep", not "delete". The file
        // never holds the secret.
        if cfg.password_mode == PasswordMode::Keychain {
            cfg.ensure_id();
            let typed = self.dialog.password.read(cx).value().to_string();
            if self.dialog.password_snapshot.as_deref() == Some(typed.as_str()) {
                // Untouched since the dialog opened: leave the entry alone.
            } else if typed.is_empty() {
                crate::keychain::delete(&cfg.id);
            } else if let Err(e) = crate::keychain::set(&cfg.id, &typed) {
                self.status = format!("Keychain store failed: {e}").into();
            }
        }
        // Leaving Keychain mode orphans nothing: drop the entry.
        if cfg.password_mode != PasswordMode::Keychain {
            if let Some(old) = self.dialog.editing.and_then(|ix| self.connections.get(ix)) {
                if old.password_mode == PasswordMode::Keychain && old.id == cfg.id {
                    crate::keychain::delete(&cfg.id);
                }
            }
        }
        if let Some(ix) = self.dialog.editing {
            if ix < self.connections.len() {
                cfg.ensure_id();
                self.connections[ix] = cfg.clone();
            } else {
                cfg.ensure_id();
                self.connections.push(cfg.clone());
            }
        } else if let Some(pos) = self.connections.iter().position(|c| c.name == cfg.name) {
            cfg.ensure_id();
            self.connections[pos] = cfg.clone();
        } else {
            cfg.ensure_id();
            self.connections.push(cfg.clone());
        }
        self.dialog.editing = None;
        self.persist(cx);
        self.status = format!("Saved {}", cfg.name).into();
        cx.notify();
    }

    /// Password to use for a "Test connection": the typed field wins;
    /// otherwise a saved Keychain entry (when editing) or the File-mode value.
    /// `None` means the mode needs a secret the form cannot supply.
    fn test_password(&self, cfg: &ConnectionConfig, cx: &App) -> Option<Zeroizing<String>> {
        let typed = self.dialog.password.read(cx).value().to_string();
        if !typed.is_empty() {
            return Some(Zeroizing::new(typed));
        }
        match cfg.password_mode {
            PasswordMode::File => (!cfg.password.is_empty()).then(|| cfg.password.clone()),
            PasswordMode::Keychain if !cfg.id.is_empty() => crate::keychain::get(&cfg.id)
                .ok()
                .flatten()
                .map(Zeroizing::new),
            _ => None,
        }
    }

    /// Connect with the current form values on a throwaway session and report
    /// the result inline. Never touches the pool or the `live` set, so it is
    /// safe even while a saved connection is already connected.
    fn test_connection_from_dialog(
        &mut self,
        cell: Rc<RefCell<TestState>>,
        cx: &mut Context<Self>,
    ) {
        // A second click while a test is in flight is a no-op.
        if matches!(*cell.borrow(), TestState::Testing) {
            return;
        }
        let mut cfg = self.form_config(cx);
        if let Err(msg) = cfg.validate() {
            *cell.borrow_mut() = TestState::Err(msg);
            cx.notify();
            return;
        }
        let Some(password) = self.test_password(&cfg, cx) else {
            *cell.borrow_mut() = TestState::Err("Enter a password to test".to_string());
            cx.notify();
            return;
        };
        cfg.password = password;

        *cell.borrow_mut() = TestState::Testing;
        cx.notify();

        let session = throwaway_session(cfg.engine);
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    let mut guard = lock(&session);
                    let outcome = guard.connect(&cfg).map_err(|e| e.to_string());
                    // The probe is done either way: close it explicitly rather
                    // than leaving teardown to Drop.
                    guard.disconnect();
                    outcome
                })
                .await;
            view.update(cx, |_this, cx| {
                *cell.borrow_mut() = match outcome {
                    Ok(()) => TestState::Ok,
                    Err(e) => TestState::Err(e),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

pub(crate) fn dialog_field(
    label: impl Into<SharedString>,
    state: &Entity<InputState>,
    password: bool,
    muted: Hsla,
) -> impl IntoElement {
    let label: SharedString = label.into();
    let d = Density::for_level(Preferences::load().ui_density);
    let mut input = Input::new(state).w_full().with_size(d.control_size);
    if password {
        input = input.content_type(InputContentType::Password);
    }
    v_flex()
        .gap_1()
        .child(div().text_xs().text_color(muted).child(label))
        .child(input)
}

/// Pick callback for a dialog pill row: receives the chosen value.
/// Named type for clippy's complexity lint.
type DialogPick<T> = std::rc::Rc<dyn Fn(T, &mut App)>;

fn dialog_pills<T: Copy + PartialEq + 'static>(
    id_base: &'static str,
    options: &[(T, &'static str)],
    current: T,
    pick: DialogPick<T>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let accent = cx.theme().accent;
    let d = Density::for_level(Preferences::load().ui_density);
    h_flex()
        .gap(px(d.gap))
        .children(options.iter().enumerate().map(|(ix, (value, label))| {
            let value = *value;
            let selected = current == value;
            let pick = pick.clone();
            div()
                .id((id_base, ix))
                .test_support()
                .px(px(d.pane_pad))
                .py(px(d.row_py))
                .rounded_md()
                .cursor_pointer()
                .bg(if selected {
                    accent.opacity(0.25)
                } else {
                    accent.opacity(0.0)
                })
                .text_xs()
                .text_color(if selected {
                    cx.theme().foreground
                } else {
                    muted
                })
                .hover(move |this| this.bg(accent.opacity(0.25)))
                .on_click(move |_, _, cx| pick(value, cx))
                .child(label.to_string())
        }))
        .into_any_element()
}

/// Standard form footer: right-aligned Cancel (closes the top dialog)
/// plus one primary action. Covers the dialogs whose Cancel does
/// nothing else; alerts and custom footers (picker, bind, delete)
/// keep their own.
pub(crate) fn dialog_footer(cancel_id: &'static str, confirm: Button) -> impl IntoElement {
    let d = Density::for_level(Preferences::load().ui_density);
    h_flex()
        .gap(px(d.gap))
        .child(div().flex_1())
        .child(
            Button::new(cancel_id)
                .label("Cancel")
                .on_click(|_, window, cx| window.close_dialog(cx)),
        )
        .child(confirm)
}

impl SqlHighlandView {
    /// Resolve the password, opening the prompt when the mode needs one
    /// and none is available. Returns the config with the usable password,
    /// or None when the prompt took over — it resumes via
    /// `submit_password` (re-running `run`, or plain-connecting).
    pub(crate) fn with_password(
        &mut self,
        mut cfg: ConnectionConfig,
        run: Option<(String, String, PickAfter)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ConnectionConfig> {
        match self.effective_password(&cfg) {
            Some(pw) => {
                cfg.password = pw;
                Some(cfg)
            }
            None => {
                self.pending.password = Some(PendingPassword {
                    conn_id: cfg.id.clone(),
                    run,
                });
                self.pwd_prompt
                    .update(cx, |s, cx| s.set_value(String::new(), window, cx));
                let name = cfg.name.clone();
                let pwd = self.pwd_prompt.clone();
                let view = cx.entity().downgrade();
                self.note_dialog_open();
                window.open_dialog(cx, move |dialog, _, cx| {
                    let submit = view.clone();
                    let pwd_in = pwd.clone();
                    dialog
                        .title(format!("Password for {name}"))
                        .w(px(360.))
                        .child(crate::connection_dialog::dialog_field(
                            "Password",
                            &pwd_in,
                            true,
                            cx.theme().muted_foreground,
                        ))
                        .footer(crate::connection_dialog::dialog_footer(
                            "pwd-cancel",
                            Button::new("pwd-connect")
                                .label("Connect")
                                .primary()
                                .on_click(move |_, window, cx| {
                                    submit
                                        .update(cx, |this, cx| {
                                            this.submit_password(window, cx);
                                        })
                                        .ok();
                                }),
                        ))
                });
                None
            }
        }
    }

    /// Password prompt submit: unlock the session (persisting to the
    /// keychain when that mode is missing its entry), close the prompt,
    /// then resume the pending connect or run.
    fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending.password.take() else {
            return;
        };
        let pw = self.pwd_prompt.read(cx).value().to_string();
        self.pwd_prompt
            .update(cx, |s, cx| s.set_value(String::new(), window, cx));
        if pw.is_empty() {
            self.status = "Password required — cancelled".into();
            cx.notify();
            return;
        }
        let mode = self
            .connections
            .iter()
            .find(|c| c.id == pending.conn_id)
            .map(|c| c.password_mode);
        if mode == Some(PasswordMode::Keychain) {
            if let Err(e) = crate::keychain::set(&pending.conn_id, &pw) {
                self.status = format!("Keychain store failed: {e}").into();
                cx.notify();
                return;
            }
        }
        self.unlocked
            .insert(pending.conn_id.clone(), Zeroizing::new(pw));
        window.close_dialog(cx);
        match pending.run {
            Some((tab_id, _, PickAfter::ScriptBuffer)) => {
                self.run_buffer_as_script(&tab_id, window, cx)
            }
            Some((tab_id, sql, _)) => self.start_run(&tab_id, sql, window, cx),
            None => self.connect_connection(&pending.conn_id, window, cx),
        }
    }
}
