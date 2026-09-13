//! Query/script execution: run entry gates, sequential script runner,
//! background executors, export drain UI, cancellation.
//!
//! Extracted from `app.rs` (refactor Phase 2); behavior unchanged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{Context, Window};

use crate::app::{ExportFormat, FetchState, Output, ResultData, ScriptResume, SqlHighlandView, describe_fetch, to_shared};
use crate::bind_dialog::PendingBind;
use crate::config::Preferences;
use crate::conn_picker::{PendingPick, PickAfter};
use crate::db::{BindParam, DbClient, FETCH_CHUNK, FetchPage, OracledbSession, is_describe_statement};
use crate::export::{XlsxBuilder, csv_header_line_with, csv_line_with, sheet_name};
use crate::session::lock;
use crate::model::ColumnInfo;
use crate::sql::{
    apply_substitutions, exec_summary, expand_at_directives, expand_script_file, find_bind_vars,
    find_substitution_vars, is_dml, parse_at_directive, split_statements, statement_kind, txn_end,
    StatementKind, SubVar,
};

/// Outcome of the background export drain.
pub(crate) enum ExportOutcome {
    Done(u64),
    Cancelled(u64),
    Failed(String),
}

/// Suggested export filename stem from a tab name: alphanumerics, `-`, `_`.
pub(crate) fn file_stem(tab_name: &str) -> String {
    let s: String = tab_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "query".to_string()
    } else {
        s.chars().take(40).collect()
    }
}

/// Seconds-since-epoch stamp for export filenames (no chrono dependency).
fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Blocking drain: page the cursor to exhaustion, streaming rows to the file
/// and appending them to the grid's buffer. Runs on the background executor
/// holding the session lock; polls `cancel` each chunk. Writes to a `.part`
/// sibling and renames on success so a failed/cancelled export never leaves
/// a half file behind at the target path.
#[allow(clippy::too_many_arguments)]
fn export_drain_blocking(
    session: &Arc<Mutex<OracledbSession>>,
    fetch: &Arc<FetchState>,
    query_id: u64,
    columns: &[String],
    fmt: ExportFormat,
    path: &std::path::Path,
    sheet: &str,
    query_sql: &str,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    csv_delim: char,
    csv_header: bool,
) -> ExportOutcome {
    use std::sync::atomic::Ordering;
    let tmp = path.with_extension("part");
    // CSV streams line by line; XLSX buffers through its constant-memory
    // worksheet (flat memory either way — no FETCH_CAP on exports).
    let mut csv_out: Option<std::io::BufWriter<std::fs::File>> = None;
    let mut xlsx: Option<XlsxBuilder> = None;
    match fmt {
        ExportFormat::Csv => {
            let file = match std::fs::File::create(&tmp) {
                Ok(f) => f,
                Err(e) => return ExportOutcome::Failed(format!("cannot write file: {e}")),
            };
            let mut w = std::io::BufWriter::new(file);
            if csv_header {
                if let Err(e) = (|| -> std::io::Result<()> {
                    use std::io::Write as _;
                    w.write_all(csv_header_line_with(columns, csv_delim).as_bytes())?;
                    w.write_all(b"\n")?;
                    Ok(())
                })() {
                    return ExportOutcome::Failed(format!("cannot write file: {e}"));
                }
            }
            csv_out = Some(w);
        }
        ExportFormat::Xlsx => match XlsxBuilder::new(sheet, columns, query_sql) {
            Ok(b) => xlsx = Some(b),
            Err(e) => return ExportOutcome::Failed(e),
        },
    }
    let mut rows: u64 = 0;
    // Rows already buffered by scrolling are exported first, then the cursor
    // is paged. The grid keeps everything (uncapped by design here).
    let buffered: Vec<Vec<Option<String>>> = fetch
        .data
        .lock()
        .map(|d| {
            d.rows
                .iter()
                .map(|r| r.iter().map(|c| c.as_deref().map(str::to_string)).collect())
                .collect()
        })
        .unwrap_or_default();
    // Write buffered rows through the same path (counts + file).
    let mut write_row = |row: &Vec<Option<String>>| -> Result<(), String> {
        match fmt {
            ExportFormat::Csv => {
                use std::io::Write as _;
                let w = csv_out.as_mut().ok_or("csv writer missing")?;
                w.write_all(csv_line_with(row, csv_delim).as_bytes())
                    .and_then(|_| w.write_all(b"\n"))
                    .map_err(|e| format!("cannot write file: {e}"))
            }
            ExportFormat::Xlsx => xlsx
                .as_mut()
                .ok_or("xlsx builder missing".to_string())?
                .push_row(row),
        }
    };
    for row in &buffered {
        if cancel.load(Ordering::Relaxed) {
            let _ = std::fs::remove_file(&tmp);
            return ExportOutcome::Cancelled(rows);
        }
        if let Err(e) = write_row(row) {
            let _ = std::fs::remove_file(&tmp);
            return ExportOutcome::Failed(e);
        }
        rows += 1;
    }
    // Mark grid buffer state: buffered rows are now "consumed" for export
    // purposes but stay visible; further pages append below.
    let exhausted_already = fetch.data.lock().map(|d| d.exhausted).unwrap_or(true);
    if !exhausted_already {
        loop {
            if cancel.load(Ordering::Relaxed) {
                let _ = std::fs::remove_file(&tmp);
                return ExportOutcome::Cancelled(rows);
            }
            let page = {
                let mut session = lock(session);
                session.fetch_more(query_id, FETCH_CHUNK)
            };
            let page = match page {
                Ok(p) => p,
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return ExportOutcome::Failed(e.to_string());
                }
            };
            if !page.current {
                let _ = std::fs::remove_file(&tmp);
                return ExportOutcome::Failed(
                    "results changed mid-export (superseded) — export again".to_string(),
                );
            }
            let failed = if page.rows.is_empty() {
                None
            } else {
                // Append to the grid buffer (uncapped) and the file.
                if let Ok(mut data) = fetch.data.lock() {
                    data.rows.extend(to_shared(page.rows.clone()));
                }
                let mut err = None;
                for row in &page.rows {
                    if let Err(e) = write_row(row) {
                        err = Some(e);
                        break;
                    }
                    rows += 1;
                }
                err
            };
            if let Some(e) = failed {
                let _ = std::fs::remove_file(&tmp);
                return ExportOutcome::Failed(e);
            }
            if page.exhausted {
                break;
            }
        }
    }
    if let Ok(mut data) = fetch.data.lock() {
        data.exhausted = true;
        data.loading = false;
    }
    // Finalize the file.
    let finalized: Result<(), String> = match fmt {
        ExportFormat::Csv => {
            use std::io::Write as _;
            match csv_out.take() {
                Some(mut w) => w.flush().map_err(|e| format!("cannot write file: {e}")),
                None => Err("csv writer missing".to_string()),
            }
        }
        ExportFormat::Xlsx => match xlsx.take() {
            Some(b) => match b.finish() {
                Ok(bytes) => {
                    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write file: {e}"))
                }
                Err(e) => Err(e),
            },
            None => Err("xlsx builder missing".to_string()),
        },
    };
    if let Err(e) = finalized {
        let _ = std::fs::remove_file(&tmp);
        return ExportOutcome::Failed(e);
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return ExportOutcome::Failed(format!("cannot move file into place: {e}"));
    }
    ExportOutcome::Done(rows)
}

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
    pub(crate) fn run_buffer_as_script(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        self.start_script_common(tab_id, display, expanded.text, ScriptResume::Buffer, window, cx);
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
            self.tabs[ix].output = Some(Output::info(format!(
                "{display} is empty — nothing to run"
            )));
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
                self.pending_pick = Some(PendingPick {
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
        self.pending_bind = Some(PendingBind {
            tab_id: tab_id.to_string(),
            sql: expanded_text,
            subs: subs_needed,
            binds: bind_names,
            script: Some(display),
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

        let session = self.pool.get_or_create(&conn_id);
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
                let (result, ms) = bg_c
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
                                        .map(|_| {
                                            StmtOutcome::Rows(inner.elapsed().as_millis())
                                        })
                                        .map_err(|e| e.to_string())
                                }
                                StatementKind::Execute => session
                                    .exec(&stmt_c, &b)
                                    .map(|(affected, _)| StmtOutcome::Done(affected))
                                    .map_err(|e| e.to_string()),
                            }
                        })();
                        (result, started.elapsed().as_millis())
                    })
                    .await;
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

    /// Cancel an in-flight export. The drain loop polls the flag each chunk
    /// and discards the partial file; the grid keeps rows fetched so far.
    pub(crate) fn cancel_export(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(t) = self.tab_by_id(tab_id) else {
            return;
        };
        if !t.exporting {
            return;
        }
        if let Some(flag) = t.export_cancel.clone() {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        t.result_meta = "Cancelling export…".into();
        cx.notify();
    }

    /// Start an export of the tab's full result set (paged past the grid cap
    /// to exhaustion). Opens the native save dialog first; the drain runs on
    /// the background executor with progress in the status bar.
    pub(crate) fn start_export(
        &mut self,
        tab_id: &str,
        fmt: ExportFormat,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].busy {
            self.status = "Wait for the run to finish before exporting".into();
            cx.notify();
            return;
        }
        if self.tabs[ix].exporting {
            return;
        }
        let Some(fetch) = self.tabs[ix].fetch.clone() else {
            self.status = "Nothing to export — run a query first".into();
            cx.notify();
            return;
        };
        // Snapshot everything the background drain needs; the tab may be
        // edited or closed while it runs.
        let columns: Vec<String> = fetch
            .data
            .lock()
            .map(|d| d.columns.iter().map(|c| c.name.clone()).collect())
            .unwrap_or_default();
        let query_id = fetch.query_id;
        let session = fetch.session.clone();
        let sql = self.tabs[ix].last_sql.clone();
        let tab_name = self.tabs[ix].name.to_string();
        let tab_id = tab_id.to_string();
        let ext = fmt.ext();
        let suggested = format!("{}-{}.{}", file_stem(&tab_name), unix_timestamp(), ext);
        let dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let view = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let rx = match cx.update(|_, cx| cx.prompt_for_new_path(&dir, Some(&suggested))) {
                Ok(rx) => rx,
                Err(_) => return,
            };
            let path = match rx.await {
                Ok(Ok(Some(p))) => p,
                _ => return, // Cancelled in the dialog or picker unavailable.
            };
            view.update(cx, |this, cx| {
                this.begin_export_drain(&tab_id, fmt, path, columns, query_id, session, sql, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Mark exporting and spawn the drain loop. The grid's `loading` flag is
    /// held so scroll-fetching pauses — pages never interleave or duplicate —
    /// and the status bar shows live progress until completion swaps it for
    /// the exported path.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn begin_export_drain(
        &mut self,
        tab_id: &str,
        fmt: ExportFormat,
        path: std::path::PathBuf,
        columns: Vec<String>,
        query_id: u64,
        session: Arc<Mutex<OracledbSession>>,
        sql: String,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].exporting {
            return;
        }
        // Re-check the fetch is still this tab's current one (a run may have
        // landed between menu click and dialog confirm).
        let Some(fetch) = self.tabs[ix].fetch.clone() else {
            return;
        };
        if fetch.query_id != query_id {
            self.tabs[ix].output = Some(Output::error(
                "Results changed while choosing a file — export again",
            ));
            cx.notify();
            return;
        }
        self.tabs[ix].exporting = true;
        self.tabs[ix].export_rows = 0;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.tabs[ix].export_cancel = Some(cancel.clone());
        if let Ok(mut data) = fetch.data.lock() {
            data.loading = true;
        }
        cx.notify();
        // Live `Exporting… N rows` ticker, same pattern as runs.
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
                        if !t.exporting {
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
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        let tab_id = tab_id.to_string();
        let sheet = sheet_name(&self.tabs[ix].name);
        let path_done = path.clone();
        // Snapshot export settings: the drain runs detached on a worker.
        let csv_prefs = Preferences::load();
        let csv_delim = crate::export::csv_delim(&csv_prefs.csv_delimiter);
        let csv_header = csv_prefs.csv_header;
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    export_drain_blocking(
                        &session, &fetch, query_id, &columns, fmt, &path, &sheet, &sql, &cancel,
                        csv_delim, csv_header,
                    )
                })
                .await;
            view.update(cx, |this, cx| {
                this.finish_export(&tab_id, fmt, path_done, outcome, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Land an export outcome: grid flags and status message.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish_export(
        &mut self,
        tab_id: &str,
        fmt: ExportFormat,
        path: std::path::PathBuf,
        outcome: ExportOutcome,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return; // Tab closed mid-export: partial file already removed.
        };
        // Re-fetch the tab's live fetch: a newer run replaces it, in which
        // case the drain already aborted as superseded.
        if let Some(fetch) = self.tabs[ix].fetch.clone() {
            if let Ok(mut data) = fetch.data.lock() {
                data.loading = false;
            }
        }
        self.tabs[ix].exporting = false;
        self.tabs[ix].export_cancel = None;
        match outcome {
            ExportOutcome::Done(rows) => {
                self.tabs[ix].export_rows = rows as usize;
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.to_string_lossy().into_owned());
                self.tabs[ix].result_meta =
                    format!("Exported {rows} rows ({}) to {name}", fmt.ext()).into();
                cx.notify();
            }
            ExportOutcome::Cancelled(rows) => {
                self.tabs[ix].export_rows = rows as usize;
                self.tabs[ix].result_meta = format!("Export cancelled after {rows} rows").into();
                cx.notify();
            }
            ExportOutcome::Failed(msg) => {
                self.tabs[ix].output = Some(Output::error(format!("Export failed: {msg}")));
                self.tabs[ix].result_meta = "Export failed".into();
                cx.notify();
            }
        }
    }

    /// Mark other tabs' fetches on the same session exhausted: a new query
    /// or execute on a shared session kills their open server-side cursor.
    pub(crate) fn mark_siblings_exhausted(&self, tab_id: &str, session: &Arc<Mutex<OracledbSession>>) {
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
