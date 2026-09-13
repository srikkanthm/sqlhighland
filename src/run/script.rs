//! Script execution: `@` gates, buffer-as-script, sequential runner.
//!
//! Part of the run pipeline (see `run.rs`).

use super::*;

impl SqlHighlandView {
    /// Base directory for resolving a top-level `@` path: the tab file's
    /// directory when file-backed (SQL Developer's worksheet-dir-first),
    /// else the process working directory.
    pub(crate) fn script_base_dir(&self, tab_id: &str) -> std::path::PathBuf {
        self.tabs
            .iter()
            .find(|t| t.id == tab_id)
            .and_then(|t| t.path.clone())
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            })
    }

    /// Entry point for an `@` / `@@` / `START` script run: expands nested
    /// includes, then follows the shared script gates below.
    /// `directive_line` is the raw `@…` line (caret line or resumed
    /// picker/password text).
    pub(crate) fn start_script_run(
        &mut self,
        tab_id: &str,
        directive_line: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        let Some(directive) = parse_at_directive(directive_line.trim()) else {
            return;
        };
        // Expand first: a typo'd path fails fast without needing a
        // connection, and expansion is pure local I/O.
        let base = self.script_base_dir(tab_id);
        let expanded = match expand_script_file(&directive.path, &base) {
            Ok(e) => e,
            Err(msg) => {
                self.tabs[ix].output = Some(Output::error(msg));
                cx.notify();
                return;
            }
        };
        let name = expanded
            .files
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| directive.path.clone());
        self.start_script_common(
            tab_id,
            format!("@{name}"),
            expanded.text,
            ScriptResume::File(directive_line),
            window,
            cx,
        );
    }

    /// "Run Script" (whole buffer, SQL Developer F5): expands `@` lines
    /// and plain SQL together, then follows the shared script gates. The
    /// buffer is re-read on every resume, so edits made while a picker
    /// or password prompt is open are picked up.
    pub(crate) fn run_buffer_as_script(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        let text = self.tabs[ix].editor.read(cx).value().to_string();
        if text.trim().is_empty() {
            self.tabs[ix].output = Some(Output::info("Buffer is empty — nothing to run"));
            cx.notify();
            return;
        }
        let base = self.script_base_dir(tab_id);
        let expanded = match expand_at_directives(&text, &base) {
            Ok(e) => e,
            Err(msg) => {
                self.tabs[ix].output = Some(Output::error(msg));
                cx.notify();
                return;
            }
        };
        let display = self.tabs[ix].name.to_string();
        self.start_script_common(
            tab_id,
            display,
            expanded.text,
            ScriptResume::Buffer,
            window,
            cx,
        );
    }

    /// Shared script gates: split → connection → password → one
    /// variables dialog for the whole script → sequential runner.
    /// `display` names the run verbatim in summaries (`@seed.sql` for
    /// files, the tab name for buffers).
    pub(crate) fn start_script_common(
        &mut self,
        tab_id: &str,
        display: String,
        expanded_text: String,
        resume: ScriptResume,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let statements: Vec<String> = split_statements(&expanded_text)
            .into_iter()
            .map(|s| s.text)
            .collect();
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if statements.is_empty() {
            self.tabs[ix].output =
                Some(Output::info(format!("{display} is empty — nothing to run")));
            cx.notify();
            return;
        }
        // Connection check (same as start_run): unbound tabs get the
        // picker, and resume re-enters via the `after` mode below.
        let (resume_sql, resume_after) = match &resume {
            ScriptResume::File(line) => (line.clone(), PickAfter::Run),
            ScriptResume::Buffer => (String::new(), PickAfter::ScriptBuffer),
        };
        let conn_id = match self.tabs[ix].connection_id.clone() {
            Some(id) if self.connections.iter().any(|c| c.id == id) => id,
            _ => {
                self.pending.pick = Some(PendingPick {
                    tab_id: tab_id.to_string(),
                    sql: resume_sql,
                    after: resume_after,
                });
                self.open_conn_pick_dialog(window, cx);
                return;
            }
        };
        if self.tabs[ix].busy {
            return;
        }
        // Password gate before variables (same resume shape as start_run).
        let ready = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .cloned()
            .map(|cfg| {
                let run = Some((tab_id.to_string(), resume_sql.clone(), resume_after));
                self.with_password(cfg, run, window, cx).is_some()
            })
            .unwrap_or(false);
        if !ready {
            return;
        }
        // One variables dialog for the whole expanded script.
        let sub_vars = find_substitution_vars(&expanded_text);
        let bind_names = find_bind_vars(&expanded_text);
        let defined: std::collections::HashMap<String, String> =
            self.defines.get(&conn_id).cloned().unwrap_or_default();
        let subs_needed: Vec<SubVar> = sub_vars
            .into_iter()
            .filter(|v| !defined.contains_key(&v.name))
            .collect();
        if subs_needed.is_empty() && bind_names.is_empty() {
            let final_sql = apply_substitutions(&expanded_text, &defined);
            let final_statements: Vec<String> = split_statements(&final_sql)
                .into_iter()
                .map(|s| s.text)
                .collect();
            self.run_script(tab_id, display, final_statements, Vec::new(), cx);
            return;
        }
        self.pending.bind = Some(PendingBind {
            tab_id: tab_id.to_string(),
            sql: expanded_text,
            subs: subs_needed,
            binds: bind_names,
            script: Some(display),
        });
        self.open_bind_dialog(window, cx);
    }

    /// Max per-statement lines in the script-output trail before a
    /// "+N more" cap (SQL Developer parity: scripts render text, never a
    /// grid — the pane must stay bounded for large migrations).
    const SCRIPT_TRAIL_CAP: usize = 50;

    /// Sequential `@`-script runner: executes each statement in order on
    /// the tab's session and always reports through the Script Output
    /// info pane (per-statement trail + summary; never a grid). Stops at
    /// the first error; Cancel rides the same `run_token` umbrella as
    /// `run_sql` and aborts between statements. Per-statement binds are
    /// partitioned from the shared list so unused names never reach the
    /// driver (Oracle errors on unbound extras). The per-round-trip call
    /// timeout applies per statement, unchanged.
    pub(crate) fn run_script(
        &mut self,
        tab_id: &str,
        display: String,
        statements: Vec<String>,
        binds: Vec<BindParam>,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].busy {
            return;
        }
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
        if let Some(pw) = self.effective_password(&cfg) {
            cfg.password = pw;
        }
        self.tabs[ix].busy = true;
        self.tabs[ix].output = None;
        self.tabs[ix].run_token = self.tabs[ix].run_token.wrapping_add(1);
        self.tabs[ix].run_started = Some(std::time::Instant::now());
        let run_token = self.tabs[ix].run_token;
        self.tabs[ix].last_sql = display.clone();
        for stmt in &statements {
            self.bump_usage(&conn_id, stmt);
        }
        self.ensure_meta(&conn_id, cx);
        // Drop stale results NOW (same flash-avoidance as run_sql).
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

        let session = self.pool.get_or_create(&conn_id, cfg.engine);
        let bg = cx.background_executor().clone();
        let tab_id = tab_id.to_string();
        let conn_id_bg = conn_id.clone();
        // Same live `Running… Ns` ticker as run_sql.
        {
            let view = cx.entity().downgrade();
            let tab_id_tick = tab_id.clone();
            let bg_tick = bg.clone();
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
            enum StmtOutcome {
                Rows(u128),
                Done(u64),
            }
            let total = statements.len();
            let mut total_ms: u128 = 0;
            let mut total_affected: u64 = 0;
            let mut executed: usize = 0;
            let mut last_select: Option<String> = None;
            let mut failed: Option<(usize, String)> = None;
            // Per-statement trail for the Script Output pane (SQL
            // Developer parity: scripts never show a grid). Capped so a
            // 500-statement migration doesn't explode the layout.
            let mut trail: Vec<String> = Vec::new();
            // Interrupt handle captured from the (possibly lazily connected)
            // session, so later statements in the script can be cancelled.
            let mut cancel_token: Option<Arc<dyn crate::db::CancelToken>> = None;
            for (i, stmt) in statements.iter().enumerate() {
                // Cancel/close checkpoint between statements.
                let cont = view
                    .update(cx, |this, _| {
                        let Some(t) = this.tab_by_id(&tab_id) else {
                            return false;
                        };
                        t.busy && t.run_token == run_token
                    })
                    .unwrap_or(false);
                if !cont {
                    break;
                }
                let kind = statement_kind(stmt);
                let want = find_bind_vars(stmt);
                let b: Vec<BindParam> = binds
                    .iter()
                    .filter(|x| want.contains(&x.name))
                    .cloned()
                    .collect();
                let stmt_c = stmt.clone();
                let cfg_c = cfg.clone();
                let session_bg = session.clone();
                let bg_c = bg.clone();
                let (result, ms, cancel) = bg_c
                    .spawn(async move {
                        let started = std::time::Instant::now();
                        let mut session = lock(&session_bg);
                        let result: Result<StmtOutcome, String> = (|| {
                            if !session.is_connected() {
                                session.connect(&cfg_c).map_err(|e| e.to_string())?;
                            }
                            match kind {
                                StatementKind::Query => {
                                    let inner = std::time::Instant::now();
                                    session
                                        .start_query(&stmt_c, FETCH_CHUNK, &b)
                                        .map(|_| StmtOutcome::Rows(inner.elapsed().as_millis()))
                                        .map_err(|e| e.to_string())
                                }
                                StatementKind::Execute => session
                                    .exec(&stmt_c, &b)
                                    .map(|(affected, _)| StmtOutcome::Done(affected))
                                    .map_err(|e| e.to_string()),
                            }
                        })();
                        let cancel = session.cancel_token();
                        (result, started.elapsed().as_millis(), cancel)
                    })
                    .await;
                if cancel.is_some() {
                    cancel_token = cancel;
                }
                total_ms += ms;
                executed = i + 1;
                match result {
                    Ok(StmtOutcome::Rows(elapsed)) => {
                        last_select = Some(stmt.clone());
                        if trail.len() < Self::SCRIPT_TRAIL_CAP {
                            trail.push(format!("✓ #{} query executed ({} ms)", i + 1, elapsed));
                        }
                    }
                    Ok(StmtOutcome::Done(affected)) => {
                        total_affected += affected;
                        if trail.len() < Self::SCRIPT_TRAIL_CAP {
                            trail.push(format!(
                                "✓ #{} {} ({} ms)",
                                i + 1,
                                exec_summary(stmt, affected),
                                ms
                            ));
                        }
                    }
                    Err(msg) => {
                        failed = Some((i, msg));
                        break;
                    }
                }
            }
            view.update(cx, |this, cx| {
                // Remember the interrupt handle for future Cancel clicks (the
                // connection outlives this script).
                if cancel_token.is_some() {
                    this.pool.set_cancel_token(&conn_id_bg, cancel_token);
                }
                let Some(ix) = this.tab_index(&tab_id) else {
                    return; // Tab closed while running.
                };
                if this.tabs[ix].run_token != run_token {
                    return; // Cancelled or superseded: discard.
                }
                this.tabs[ix].busy = false;
                this.tabs[ix].run_started = None;
                let summary = |errors: usize| {
                    format!(
                        "{display}: {total} statements, {errors} error{} · {total_ms} ms",
                        if errors == 1 { "" } else { "s" }
                    )
                };
                // Scripts never show the grid (SQL Developer parity: Run
                // Script renders text, only Run Statement grids). The pane
                // always gets the per-statement trail plus the summary.
                if executed > Self::SCRIPT_TRAIL_CAP {
                    trail.push(format!("… +{} more", executed - Self::SCRIPT_TRAIL_CAP));
                }
                if let Some((i, msg)) = failed {
                    trail.push(format!("✗ #{}/{} — {}", i + 1, total, msg));
                    this.tabs[ix].output = Some(Output::error(trail.join("\n")));
                    this.tabs[ix].result_meta = format!("Failed · {total_ms} ms").into();
                    cx.notify();
                    return;
                }
                // Transaction flags, replayed in order over what ran.
                let mut pending = false;
                let mut saw_txn_end = false;
                for stmt in statements.iter().take(executed) {
                    if txn_end(stmt).is_some() {
                        pending = false;
                        saw_txn_end = true;
                    } else if is_dml(stmt) {
                        pending = true;
                    }
                }
                this.tabs[ix].pending_txn = pending;
                if saw_txn_end {
                    this.clear_pending(&conn_id_bg);
                }
                // Export audit stays truthful: last SELECT when one ran.
                if let Some(label) = last_select {
                    this.tabs[ix].last_sql = label;
                }
                // No grid fetch of our own: Dismiss restores the stashed
                // pre-script results (or the full-height editor on a fresh
                // tab) instead of an empty grid. Session bookkeeping still
                // applies — the connection went live either way.
                this.mark_siblings_exhausted(&tab_id, &session);
                this.live.insert(conn_id_bg.clone());
                this.ensure_meta(&conn_id_bg, cx);
                let meta = if total_affected > 0 {
                    format!(
                        "{summary} · {total_affected} row{} affected",
                        if total_affected == 1 { "" } else { "s" },
                        summary = summary(0)
                    )
                } else {
                    summary(0)
                };
                trail.push(meta.clone());
                this.tabs[ix].result_meta = meta.into();
                this.tabs[ix].output = Some(Output::info(trail.join("\n")));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}
