//! Run entry gates, the SQL executor, cancellation, and session helpers.
//!
//! Part of the run pipeline (see `run.rs`).

use super::*;

impl SqlHighlandView {
    /// Entry point for a run: checks the connection binding, detects `&`/`&&`
    /// substitution variables and `:binds`, and either runs directly or opens
    /// the variables dialog first. With no connection bound, offers the
    /// connection picker first and runs right after the pick.
    pub(crate) fn start_run(
        &mut self,
        tab_id: &str,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // `@`-directive resumes (picker/password carrying the directive
        // line) route to the script path — same as a direct caret run.
        if parse_at_directive(sql.trim()).is_some() {
            self.start_script_run(tab_id, sql, window, cx);
            return;
        }
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        // Connection check first so we never prompt for variables just to
        // then fail. Unbound (or dangling) tabs get the picker, and the run
        // continues automatically once a connection is chosen.
        let conn_id = match self.tabs[ix].connection_id.clone() {
            Some(id) if self.connections.iter().any(|c| c.id == id) => id,
            _ => {
                self.pending_pick = Some(PendingPick {
                    tab_id: tab_id.to_string(),
                    sql,
                    after: PickAfter::Run,
                });
                self.open_conn_pick_dialog(window, cx);
                return;
            }
        };
        if self.tabs[ix].busy {
            return;
        }
        // Password gate before variables: Ask/Keychain-miss prompts here,
        // resuming this same run on submit (unlocked for the session).
        // run_sql picks up the unlock below — no password travels further.
        let ready = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .cloned()
            .map(|cfg| {
                let run = Some((tab_id.to_string(), sql.clone(), PickAfter::Run));
                self.with_password(cfg, run, window, cx).is_some()
            })
            .unwrap_or(false);
        if !ready {
            return;
        }
        let sub_vars = find_substitution_vars(&sql);
        let bind_names = find_bind_vars(&sql);
        // `&&`-defined values (and any re-reference of them via `&`) reuse
        // without prompting, like SQL*Plus.
        let defined: std::collections::HashMap<String, String> =
            self.defines.get(&conn_id).cloned().unwrap_or_default();
        let subs_needed: Vec<SubVar> = sub_vars
            .into_iter()
            .filter(|v| !defined.contains_key(&v.name))
            .collect();
        if subs_needed.is_empty() && bind_names.is_empty() {
            let final_sql = apply_substitutions(&sql, &defined);
            self.run_sql(tab_id, final_sql, Vec::new(), cx);
            return;
        }
        self.pending_bind = Some(PendingBind {
            tab_id: tab_id.to_string(),
            sql,
            subs: subs_needed,
            binds: bind_names,
            script: None,
        });
        self.open_bind_dialog(window, cx);
    }

    // (open_pick_for_new_tab/open_pick_for_rebind live in conn_picker.rs)

    // (open_conn_pick_dialog lives in conn_picker.rs)

    /// Variables dialog: one blank field per `&name` / `:name` (always blank,
    /// no memory). Builder-safe: everything the dialog renders is cloned in —
    /// the builder never touches the view entity (see env-tag crash fix).
    /// (open_bind_dialog lives in bind_dialog.rs)
    /// Run-button handler: stores `&&` values in the session defines, applies
    /// substitution, and launches the run with native binds.
    /// (submit_bind_dialog lives in bind_dialog.rs)
    pub(crate) fn run_sql(
        &mut self,
        tab_id: &str,
        sql: String,
        binds: Vec<BindParam>,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].busy {
            return;
        }
        // A new run would supersede the cursor an export drain is paging.
        if self.tabs[ix].exporting {
            self.tabs[ix].output = Some(Output::error(
                "Export in progress — cancel it before running",
            ));
            cx.notify();
            return;
        }
        let conn_id = match self.tabs[ix].connection_id.clone() {
            Some(id) => id,
            None => {
                self.tabs[ix].output = Some(Output::error("Select a connection for this tab"));
                cx.notify();
                return;
            }
        };
        let Some(mut cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            self.tabs[ix].output = Some(Output::error("Connection not found — pick another"));
            cx.notify();
            return;
        };
        // Session unlock (or keychain) wins over the stored password.
        // No prompt here — start_run gated already; direct callers resume
        // unlocked sessions only.
        if let Some(pw) = self.effective_password(&cfg) {
            cfg.password = pw;
        }
        self.tabs[ix].busy = true;
        self.tabs[ix].output = None;
        self.tabs[ix].run_token = self.tabs[ix].run_token.wrapping_add(1);
        self.tabs[ix].run_started = Some(std::time::Instant::now());
        let run_token = self.tabs[ix].run_token;
        // Remembered for the export audit tab.
        self.tabs[ix].last_sql = sql.clone();
        // Executed table names boost future suggestion rankings.
        self.bump_usage(&conn_id, &sql);
        // Warm the suggestion cache alongside the run (no-op when fresh),
        // so typing after a run completes from data, not keywords.
        self.ensure_meta(&conn_id, cx);
        // Drop stale results NOW, not on completion: otherwise the previous
        // query's rows keep painting (flash) while the new one runs. The
        // grid repaints empty with `Running…` in the status bar until the
        // fresh fetch lands. `has_result` stays true so the first-run
        // placeholder doesn't flash in its place.
        // A new run also reopens a dismissed bottom pane.
        self.tabs[ix].hide_results = false;
        self.tabs[ix].fetch = None;
        self.tabs[ix].copy_sel = None;
        self.tabs[ix].table.update(cx, |table, cx| {
            table.delegate_mut().set_fetch(None);
            table.clear_selection(cx);
            table.refresh(cx);
        });
        cx.notify();

        let session = self.pool.get_or_create(&conn_id);
        let session_bg = session.clone();
        let bg = cx.background_executor().clone();
        let tab_id = tab_id.to_string();
        let conn_id_bg = conn_id.clone();
        // Route before spawning: queries page through a held cursor,
        // everything else executes and reports an action summary.
        // The text is cloned for labeling the completion below.
        let kind = statement_kind(&sql);
        let dml = is_dml(&sql);
        let sql_label = sql.clone();
        // Grid row cap from Settings (exports stay uncapped by design).
        // Clamped so a hand-edited preferences file can't OOM the grid.
        let cap = Preferences::load().result_cap.clamp(1_000, 5_000_000);
        // Ticker repainting the live `Running… Ns` status twice a second.
        // Exits on its own once the run ends (token mismatch or not busy);
        // no handle needed because a newer run's ticker supersedes it.
        {
            let view = cx.entity().downgrade();
            let tab_id_tick = tab_id.to_string();
            let bg_tick = cx.background_executor().clone();
            cx.spawn(async move |_, cx| loop {
                bg_tick.timer(Duration::from_millis(500)).await;
                let cont = view
                    .update(cx, |this, cx| {
                        let Some(t) = this.tab_by_id(&tab_id_tick) else {
                            return false;
                        };
                        if !t.busy || t.run_token != run_token {
                            return false;
                        }
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !cont {
                    break;
                }
            })
            .detach();
        }
        cx.spawn(async move |view, cx| {
            enum Outcome {
                Rows(Vec<ColumnInfo>, FetchPage, u64, u128),
                Done(u64, u128, u64),
            }
            let outcome = bg
                .spawn(async move {
                    // Wall-clock for the whole blocking section, so failures
                    // can also report timing in the status line.
                    let started = std::time::Instant::now();
                    let mut session = lock(&session_bg);
                    let result: Result<Outcome, String> = (|| {
                        // Reconnect lazily; a live pooled session is reused.
                        if !session.is_connected() {
                            session.connect(&cfg).map_err(|e| e.to_string())?;
                        }
                        match kind {
                            StatementKind::Query => {
                                let inner = std::time::Instant::now();
                                session
                                    .start_query(&sql, FETCH_CHUNK, &binds)
                                    .map(|(columns, page, id)| {
                                        Outcome::Rows(
                                            columns,
                                            page,
                                            id,
                                            inner.elapsed().as_millis(),
                                        )
                                    })
                                    .map_err(|e| e.to_string())
                            }
                            StatementKind::Execute => session
                                .exec(&sql, &binds)
                                .map(|(affected, ms)| {
                                    // Read while holding the bg lock: locking the
                                    // session on the UI thread would block repaints
                                    // behind in-flight sibling queries.
                                    let qid = session.query_id();
                                    Outcome::Done(affected, ms, qid)
                                })
                                .map_err(|e| e.to_string()),
                        }
                    })();
                    (result, started.elapsed().as_millis())
                })
                .await;
            let (outcome, elapsed_ms) = outcome;
            view.update(cx, |this, cx| {
                let Some(ix) = this.tab_index(&tab_id) else {
                    return; // Tab closed while running.
                };
                if this.tabs[ix].run_token != run_token {
                    return; // Cancelled or superseded by a newer run: discard.
                }
                this.tabs[ix].busy = false;
                this.tabs[ix].run_started = None;
                match outcome {
                    Ok(Outcome::Rows(columns, page, query_id, elapsed_ms)) => {
                        // A DESCRIBE with zero rows means the object isn't
                        // visible (real tables always have columns) — say so
                        // instead of showing a bare empty grid.
                        if page.rows.is_empty() && is_describe_statement(&sql_label) {
                            this.tabs[ix].output = Some(Output::info(
                                "Table not found or no access — DESCRIBE returned no columns",
                            ));
                        }
                        let fetch = Arc::new(FetchState {
                            session: session.clone(),
                            query_id,
                            chunk: FETCH_CHUNK,
                            cap,
                            data: Mutex::new(ResultData {
                                columns,
                                rows: to_shared(page.rows),
                                elapsed_ms,
                                exhausted: page.exhausted,
                                loading: false,
                                capped: false,
                            }),
                            view: view.clone(),
                            tab_id: tab_id.clone(),
                        });
                        this.tabs[ix].fetch = Some(fetch.clone());
                        this.mark_siblings_exhausted(&tab_id, &session);
                        // Lazy auto-connect may have connected just now.
                        this.live.insert(conn_id_bg.clone());
                        this.ensure_meta(&conn_id_bg, cx);
                        this.tabs[ix].result_meta = describe_fetch(&fetch).into();
                        this.tabs[ix].has_result = true;
                        // Fresh data invalidates any selection: indices belong
                        // to the old result. (Also emits ClearSelection, which
                        // resets the copy tracker via the table subscription.)
                        this.tabs[ix].table.update(cx, |table, cx| {
                            table.delegate_mut().set_fetch(Some(fetch));
                            table.clear_selection(cx);
                            table.refresh(cx);
                        });
                    }
                    Ok(Outcome::Done(affected, elapsed_ms, query_id)) => {
                        let fetch = Arc::new(FetchState {
                            session: session.clone(),
                            query_id,
                            chunk: FETCH_CHUNK,
                            cap,
                            data: Mutex::new(ResultData {
                                columns: Vec::new(),
                                rows: Vec::new(),
                                elapsed_ms,
                                exhausted: true,
                                loading: false,
                                capped: false,
                            }),
                            view: view.clone(),
                            tab_id: tab_id.clone(),
                        });
                        this.tabs[ix].fetch = Some(fetch.clone());
                        this.mark_siblings_exhausted(&tab_id, &session);
                        this.live.insert(conn_id_bg.clone());
                        this.ensure_meta(&conn_id_bg, cx);
                        // DDL auto-commits server-side; only DML leaves a
                        // pending transaction (the driver never autocommits).
                        this.tabs[ix].pending_txn = dml;
                        // A typed COMMIT/ROLLBACK commits server-side: mirror
                        // the buttons by clearing pending flags connection-wide.
                        if txn_end(&sql_label).is_some() {
                            this.clear_pending(&conn_id_bg);
                        }
                        let meta =
                            format!("{} · {} ms", exec_summary(&sql_label, affected), elapsed_ms);
                        this.tabs[ix].result_meta = meta.clone().into();
                        // Confirmations live in the output pane too, so the
                        // results area tells the story without the status bar.
                        this.tabs[ix].output = Some(Output::info(meta));
                        this.tabs[ix].has_result = true;
                        this.tabs[ix].table.update(cx, |table, cx| {
                            table.delegate_mut().set_fetch(Some(fetch));
                            table.clear_selection(cx);
                            table.refresh(cx);
                        });
                    }
                    Err(msg) => {
                        this.tabs[ix].output = Some(Output::error(msg));
                        // Stamp the failure over the previous run's summary —
                        // otherwise the status line keeps reporting stale success.
                        this.tabs[ix].result_meta = format!("Failed · {elapsed_ms} ms").into();
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Cancel the tab's in-flight run (client-side abandon — see
    /// `run_token`). The worker keeps its session lock until the server
    /// responds, so a same-connection re-run queues behind it; its results
    /// are discarded on arrival and the fresh run proceeds normally.
    pub(crate) fn cancel_run(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(t) = self.tab_by_id(tab_id) else {
            return;
        };
        if !t.busy {
            return;
        }
        t.run_token = t.run_token.wrapping_add(1);
        t.busy = false;
        t.run_started = None;
        t.result_meta = "Cancelled".into();
        cx.notify();
    }

    /// Mark other tabs' fetches on the same session exhausted: a new query
    /// or execute on a shared session kills their open server-side cursor.
    pub(crate) fn mark_siblings_exhausted(
        &self,
        tab_id: &str,
        session: &Arc<Mutex<OracledbSession>>,
    ) {
        for t in &self.tabs {
            if t.id == tab_id {
                continue;
            }
            if let Some(f) = &t.fetch {
                if Arc::ptr_eq(&f.session, session) {
                    if let Ok(mut d) = f.data.lock() {
                        d.exhausted = true;
                    }
                }
            }
        }
    }

    /// Clear the pending-transaction flag on every tab sharing a connection.
    /// Transactions are session-scoped, so one commit/rollback settles all.
    pub(crate) fn clear_pending(&mut self, conn_id: &str) {
        for t in &mut self.tabs {
            if t.connection_id.as_deref() == Some(conn_id) {
                t.pending_txn = false;
            }
        }
    }
}
