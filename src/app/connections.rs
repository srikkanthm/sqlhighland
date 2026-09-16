//! Connection CRUD and session connect/disconnect.
//!
//! The connection dialog and sidebar call into these.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use super::*;

impl SqlHighlandView {
    /// Index of a saved connection by its stable id.
    pub(super) fn connection_index(&self, conn_id: &str) -> Option<usize> {
        self.connections.iter().position(|c| c.id == conn_id)
    }

    // -- Connection dialog (see connection_dialog.rs) ----------------------------
    /// Delete with confirmation: removing a connection drops its tabs'
    /// bindings (and its keychain entry), so it asks first.
    pub(super) fn confirm_delete_connection(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cfg) = self.connections.get(ix).cloned() else {
            return;
        };
        let view = cx.entity().downgrade();
        let conn_id = cfg.id.clone();
        let name: SharedString = format!("Delete “{}”?", cfg.name).into();
        self.note_dialog_open();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let delete_view = view.clone();
            alert
                .icon(KitIcon::TriangleAlert)
                .title(name.clone())
                .description(
                    "Tabs bound to it become unbound. A stored keychain password is removed too.",
                )
                // Return (the dialog's `Confirm` action) deletes too, so it is
                // the default action.
                .on_ok({
                    let conn_id = conn_id.clone();
                    let view = view.clone();
                    move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            if let Some(ix) = this.connections.iter().position(|c| c.id == conn_id)
                            {
                                this.delete_connection(ix, cx);
                            }
                        })
                        .ok();
                        true
                    }
                })
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_center()
                        .child(
                            Button::new("delete-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("delete-confirm")
                                .label("Delete")
                                .danger()
                                .on_click({
                                    let conn_id = conn_id.clone();
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                        delete_view
                                            .update(cx, |this, cx| {
                                                if let Some(ix) = this
                                                    .connections
                                                    .iter()
                                                    .position(|c| c.id == conn_id)
                                                {
                                                    this.delete_connection(ix, cx);
                                                }
                                            })
                                            .ok();
                                    }
                                }),
                        ),
                )
        });
    }

    pub(super) fn delete_connection(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let removed = self.connections.remove(ix);
        crate::keychain::delete(&removed.id);
        self.unlocked.remove(&removed.id);
        if self
            .pending
            .password
            .as_ref()
            .is_some_and(|p| p.conn_id == removed.id)
        {
            self.pending.password = None;
        }
        self.pool.remove(&removed.id);
        self.live.remove(&removed.id);
        // Tabs bound to it fall back to "no connection".
        for tab in &mut self.tabs {
            if tab.connection_id.as_deref() == Some(removed.id.as_str()) {
                tab.connection_id = None;
            }
        }
        self.persist(cx);
        self.persist_tabs(cx);
        self.status = format!("Deleted {}", removed.name).into();
        cx.notify();
    }

    // -- Sessions -------------------------------------------------------------

    /// Eagerly connect a saved connection (sidebar menu). Tabs auto-connect
    /// lazily on Run, so this is strictly optional.
    /// Effective password for a connection: session unlock first, then
    /// stored (File) or keychain (Keychain). None = must prompt (Ask,
    /// Keychain miss, empty File entry).
    pub(crate) fn effective_password(&self, cfg: &ConnectionConfig) -> Option<Zeroizing<String>> {
        if let Some(pw) = self.unlocked.get(&cfg.id) {
            return Some(pw.clone());
        }
        match cfg.password_mode {
            PasswordMode::File => {
                if cfg.password.is_empty() {
                    None
                } else {
                    Some(cfg.password.clone())
                }
            }
            PasswordMode::Keychain => crate::keychain::get(&cfg.id)
                .ok()
                .flatten()
                .map(Zeroizing::new),
            PasswordMode::Ask => None,
        }
    }

    // (with_password + submit_password live in connection_dialog.rs)

    pub(crate) fn connect_connection(
        &mut self,
        conn_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            return;
        };
        // Keychain/Ask modes (or an empty File password) resolve here —
        // the prompt resumes by re-entering this same function.
        let Some(cfg) = self.with_password(cfg, None, window, cx) else {
            return;
        };
        self.status = format!("Connecting to {}…", cfg.connect_string()).into();
        cx.notify();

        let session = self.pool.get_or_create(&cfg.id, cfg.engine);
        let conn_id = cfg.id.clone();
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let (outcome, cancel) = bg
                .spawn(async move {
                    let mut guard = lock(&session);
                    match guard.connect(&cfg) {
                        Ok(()) => (WorkOutcome::Connected, guard.cancel_token()),
                        Err(e) => (WorkOutcome::Failed(e.to_string()), None),
                    }
                })
                .await;
            view.update(cx, |this, cx| {
                if cancel.is_some() {
                    this.pool.set_cancel_token(&conn_id, cancel);
                }
                match outcome {
                    // Success/failure notices are transient events. Live
                    // connection *state* is never stored as text — the status
                    // bar derives it from the cached `live` set every render.
                    WorkOutcome::Connected => {
                        this.live.insert(conn_id.clone());
                        this.status = "".into();
                        // Prefetch suggestions in the background.
                        this.ensure_meta(&conn_id, cx);
                    }
                    WorkOutcome::Failed(msg) => {
                        this.live.remove(&conn_id);
                        this.status = format!("Connection failed: {msg}").into();
                        // Popup in addition to the status line: a failed
                        // connect deserves an explicit surface.
                        let name = this
                            .connections
                            .iter()
                            .find(|c| c.id == conn_id)
                            .map(|c| c.name.clone())
                            .unwrap_or_default();
                        let msg: SharedString = format!("{name}: {msg}").into();
                        if let Some(handle) = cx.windows().into_iter().next() {
                            let _ = handle.update(cx, |_, window, cx| {
                                window.open_alert_dialog(cx, move |alert, _, _| {
                                    alert
                                        .icon(KitIcon::TriangleAlert)
                                        .title("Connection failed")
                                        .description(msg.clone())
                                });
                            });
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn disconnect_connection(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        self.pool.remove(conn_id);
        self.live.remove(conn_id);
        // Mark the dictionary cache stale so a reconnect refetches it. (With
        // the TTL set to "never", this is what keeps suggestions honest across
        // a disconnect/reconnect.)
        if let Some(cache) = self.browser.meta.get(conn_id) {
            lock(cache).fetched_at = None;
        }
        // Tabs keep their fetched rows; further paging goes stale (guarded).
        // No "Disconnected" notice: the status bar derives live state itself.
        self.status = "".into();
        cx.notify();
    }

    /// Confirm, then disconnect the active tab's connection (Shift+Cmd+D).
    /// Sessions are connection-scoped, so this drops the session for every tab
    /// bound to that connection. Silent no-op when the active tab has no bound
    /// (live) connection.
    pub(super) fn disconnect_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn_id) = self.active_tab().connection_id.clone() else {
            return;
        };
        if !self.live.contains(&conn_id) {
            return;
        }
        let name = self.connection_name(&Some(conn_id.clone()));
        // Surface uncommitted work: disconnecting rolls it back.
        let description: SharedString = if self.has_pending(&conn_id) {
            format!(
                "Every tab using this connection will be disconnected. \
                 Uncommitted changes on {name} will be rolled back."
            )
            .into()
        } else {
            "Every tab using this connection will be disconnected."
                .to_string()
                .into()
        };
        let title: SharedString = format!("Disconnect from “{name}”?").into();
        let view = cx.entity().downgrade();
        self.note_dialog_open();
        window.open_alert_dialog(cx, move |alert, _, _| {
            alert
                .icon(KitIcon::TriangleAlert)
                .title(title.clone())
                .description(description.clone())
                // Return (the dialog's `Confirm` action) performs the disconnect
                // too, so the default action matches the "Disconnect" button.
                .on_ok({
                    let conn_id = conn_id.clone();
                    let view = view.clone();
                    move |_, _, cx| {
                        view.update(cx, |this, cx| {
                            this.disconnect_connection(&conn_id, cx);
                        })
                        .ok();
                        true
                    }
                })
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_center()
                        .child(
                            Button::new("disconnect-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("disconnect-confirm")
                                .label("Disconnect")
                                .danger()
                                .on_click({
                                    let conn_id = conn_id.clone();
                                    let view = view.clone();
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                        view.update(cx, |this, cx| {
                                            this.disconnect_connection(&conn_id, cx);
                                        })
                                        .ok();
                                    }
                                }),
                        ),
                )
        });
    }
}
