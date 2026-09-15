//! Export drain: streaming CSV/XLSX to disk on the background executor.
//!
//! Part of the run pipeline (see `run.rs`).

use super::*;
use crate::model::ConnectionConfig;
use crate::session::throwaway_session;

/// Outcome of the background export drain.
pub(crate) enum ExportOutcome {
    Done(u64),
    /// Buffered-only export: the grid was incomplete and the query wasn't
    /// re-run (e.g. `FOR UPDATE` locks). Carries a user-facing notice.
    Partial(u64, String),
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

/// True when the statement is a `SELECT ... FOR UPDATE` (row locks). A re-run
/// on a fresh session would try to re-lock rows the tab still holds, so those
/// exports fall back to the buffered rows.
fn sql_locks_rows(sql: &str) -> bool {
    sql.to_ascii_lowercase().contains("for update")
}

/// Disconnect a throwaway export session, if one was opened.
fn close_throwaway(session: &Option<SharedSession>) {
    if let Some(s) = session {
        lock(s).disconnect();
    }
}

/// Blocking drain: writes the result set to the file with constant memory. The
/// grid buffer is the source when it already holds the complete set; otherwise
/// the query is re-executed on a **throwaway session** so the grid's cursor and
/// other tabs are untouched. `FOR UPDATE` queries never re-run (lock conflict):
/// they export the buffered rows with a notice. Writes to a `.part` sibling and
/// renames on success so a failed/cancelled export never leaves a half file.
#[allow(clippy::too_many_arguments)] // drain config is threaded from the UI snapshot
fn export_drain_blocking(
    fetch: &Arc<FetchState>,
    cfg: &ConnectionConfig,
    sql: &str,
    binds: &[BindParam],
    complete: bool,
    fmt: ExportFormat,
    path: &std::path::Path,
    sheet: &str,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    csv_delim: char,
    csv_header: bool,
    // Rows per page for the drain, from the export setting (independent of the
    // grid's fetch size).
    export_page: usize,
) -> ExportOutcome {
    use std::sync::atomic::Ordering;
    let tmp = path.with_extension("part");

    let locked = sql_locks_rows(sql);
    let re_run = !complete && !locked && !sql.is_empty();
    let partial_notice = locked && !complete;

    // Re-execute on a throwaway session when the grid is incomplete: connect
    // and run the first page, which also gives us the columns.
    let mut session: Option<SharedSession> = None;
    let mut fresh: Option<(Vec<ColumnInfo>, FetchPage, u64)> = None;
    if re_run {
        let s = throwaway_session(cfg.engine);
        let started = {
            let mut guard = lock(&s);
            if let Err(e) = guard.connect(cfg) {
                Err(e)
            } else {
                guard.start_query(sql, export_page, binds)
            }
        };
        match started {
            Ok((columns, page, id)) => {
                fresh = Some((columns, page, id));
                session = Some(s);
            }
            Err(e) => {
                // Release the (possibly connected) session promptly.
                lock(&s).disconnect();
                return ExportOutcome::Failed(e.to_string());
            }
        }
    }

    // Columns come from the fresh execution, or from the grid buffer (names
    // only — the buffered rows themselves are cloned lazily below).
    let columns: Vec<String> = match &fresh {
        Some((cols, _, _)) => cols.iter().map(|c| c.name.clone()).collect(),
        None => lock(&fetch.data)
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect(),
    };

    // Open the writer (CSV streams; XLSX uses its constant-memory worksheet).
    let mut csv_out: Option<std::io::BufWriter<std::fs::File>> = None;
    let mut xlsx: Option<XlsxBuilder> = None;
    match fmt {
        ExportFormat::Csv => {
            let file = match std::fs::File::create(&tmp) {
                Ok(f) => f,
                Err(e) => {
                    close_throwaway(&session);
                    return ExportOutcome::Failed(format!("cannot write file: {e}"));
                }
            };
            let mut w = std::io::BufWriter::new(file);
            if csv_header {
                if let Err(e) = (|| -> std::io::Result<()> {
                    use std::io::Write as _;
                    w.write_all(csv_header_line_with(&columns, csv_delim).as_bytes())?;
                    w.write_all(b"\n")?;
                    Ok(())
                })() {
                    close_throwaway(&session);
                    let _ = std::fs::remove_file(&tmp);
                    return ExportOutcome::Failed(format!("cannot write file: {e}"));
                }
            }
            csv_out = Some(w);
        }
        ExportFormat::Xlsx => match XlsxBuilder::new(sheet, &columns, sql) {
            Ok(b) => xlsx = Some(b),
            Err(e) => {
                close_throwaway(&session);
                return ExportOutcome::Failed(e);
            }
        },
    }

    let mut rows: u64 = 0;
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
    // Write a batch, honoring cancel between rows. `Err("cancelled")` marks a
    // user cancel (vs a write failure).
    let mut write_batch = |batch: &[Vec<Option<String>>], rows: &mut u64| -> Result<(), String> {
        for row in batch {
            if cancel.load(Ordering::Relaxed) {
                return Err("cancelled".to_string());
            }
            write_row(row)?;
            *rows += 1;
        }
        Ok(())
    };

    if let Some((_, first_page, query_id)) = fresh {
        // First page from the fresh execution, then page to exhaustion.
        let first_exhausted = first_page.exhausted;
        if let Err(e) = write_batch(&first_page.rows, &mut rows) {
            close_throwaway(&session);
            let _ = std::fs::remove_file(&tmp);
            return if e == "cancelled" {
                ExportOutcome::Cancelled(rows)
            } else {
                ExportOutcome::Failed(e)
            };
        }
        // Release the first page's rows before paging the rest.
        drop(first_page);
        if !first_exhausted {
            loop {
                if cancel.load(Ordering::Relaxed) {
                    close_throwaway(&session);
                    let _ = std::fs::remove_file(&tmp);
                    return ExportOutcome::Cancelled(rows);
                }
                let page = {
                    let mut guard = lock(session.as_ref().expect("export session"));
                    guard.fetch_more(query_id, export_page)
                };
                match page {
                    Ok(p) => {
                        if let Err(e) = write_batch(&p.rows, &mut rows) {
                            close_throwaway(&session);
                            let _ = std::fs::remove_file(&tmp);
                            return if e == "cancelled" {
                                ExportOutcome::Cancelled(rows)
                            } else {
                                ExportOutcome::Failed(e)
                            };
                        }
                        if p.exhausted {
                            break;
                        }
                    }
                    Err(e) => {
                        close_throwaway(&session);
                        let _ = std::fs::remove_file(&tmp);
                        if cancel.load(Ordering::Relaxed) {
                            return ExportOutcome::Cancelled(rows);
                        }
                        return ExportOutcome::Failed(e.to_string());
                    }
                }
            }
        }
        close_throwaway(&session);
    } else {
        // Buffered-only export (grid complete, or a locked query we won't
        // re-run). Snapshot in chunks so a huge complete grid isn't duplicated
        // in memory all at once; the length is fixed up front (a snapshot).
        let total = lock(&fetch.data).rows.len();
        let mut start = 0usize;
        while start < total {
            let batch: Vec<Vec<Option<String>>> = {
                let data = lock(&fetch.data);
                data.rows[start..total]
                    .iter()
                    .take(export_page)
                    .map(|r| r.iter().map(|c| c.as_deref().map(str::to_string)).collect())
                    .collect()
            };
            if let Err(e) = write_batch(&batch, &mut rows) {
                let _ = std::fs::remove_file(&tmp);
                return if e == "cancelled" {
                    ExportOutcome::Cancelled(rows)
                } else {
                    ExportOutcome::Failed(e)
                };
            }
            start += batch.len();
        }
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
    if partial_notice {
        return ExportOutcome::Partial(
            rows,
            "Exported buffered rows only — FOR UPDATE results can't be re-run".to_string(),
        );
    }
    ExportOutcome::Done(rows)
}

impl SqlHighlandView {
    /// Cancel an in-flight export. The drain loop polls the flag between
    /// chunks and discards the partial file. The export runs on its own
    /// throwaway session, so the connection's cancel token is not involved.
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
        if let Some(t) = self.tab_by_id(tab_id) {
            t.result_meta = "Cancelling export…".into();
        }
        cx.notify();
    }

    /// Start an export of the tab's full result set. The grid buffer is used
    /// when it already holds every row; otherwise the query is re-executed on
    /// its own throwaway session (so the grid and other tabs are untouched).
    /// Opens the native save dialog first; the drain runs on the background
    /// executor with progress in the status bar.
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
        if self.tabs[ix].fetch.is_none() {
            self.status = "Nothing to export — run a query first".into();
            cx.notify();
            return;
        }
        // Snapshot everything the background drain needs; the tab may be
        // edited, rebound, or closed while the save dialog is open.
        let Some(conn_id) = self.tabs[ix].connection_id.clone() else {
            self.status = "Select a connection for this tab".into();
            cx.notify();
            return;
        };
        let Some(mut cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            self.status = "Connection not found — pick another".into();
            cx.notify();
            return;
        };
        if let Some(pw) = self.effective_password(&cfg) {
            cfg.password = pw;
        }
        let sql = self.tabs[ix].last_sql.clone();
        let binds = self.tabs[ix].last_binds.clone();
        let tab_name = self.tabs[ix].name.to_string();
        let tab_id = tab_id.to_string();
        let ext = fmt.ext();
        let suggested = format!("{}-{}.{}", file_stem(&tab_name), unix_timestamp(), ext);
        let dir = crate::fsutil::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
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
                this.begin_export_drain(&tab_id, fmt, path, cfg, conn_id, sql, binds, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Mark exporting and spawn the drain loop. The grid is left untouched
    /// (the drain uses its own session when it needs one), so other tabs and
    /// the grid stay usable; the status bar shows live progress until
    /// completion swaps it for the exported path.
    #[allow(clippy::too_many_arguments)] // one-shot UI snapshot; grouping would move the same data
    pub(crate) fn begin_export_drain(
        &mut self,
        tab_id: &str,
        fmt: ExportFormat,
        path: std::path::PathBuf,
        cfg: ConnectionConfig,
        conn_id: String,
        sql: String,
        binds: Vec<BindParam>,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].exporting {
            return;
        }
        // The tab may have been re-run or rebound while the save dialog was
        // open; the snapshot would then export the wrong data.
        if self.tabs[ix].connection_id.as_deref() != Some(conn_id.as_str())
            || self.tabs[ix].last_sql != sql
        {
            self.tabs[ix].output = Some(Output::error(
                "Results changed while choosing a file — export again",
            ));
            cx.notify();
            return;
        }
        let Some(fetch) = self.tabs[ix].fetch.clone() else {
            return;
        };
        self.tabs[ix].exporting = true;
        self.tabs[ix].export_rows = 0;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        self.tabs[ix].export_cancel = Some(cancel.clone());
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
        // Export page size (Settings → Results) for the drain — independent
        // of the grid's fetch size.
        let export_page = self.export_fetch_size.max(1);
        // The grid buffer is the source only when it holds every row: the
        // cursor was drained (exhausted) and the cap wasn't hit. A cursor
        // killed by another tab sets `exhausted` too, so `capped` is checked
        // as well — otherwise a truncated buffer would look "complete".
        let complete = {
            let data = lock(&fetch.data);
            data.exhausted && !data.capped
        };
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    export_drain_blocking(
                        &fetch,
                        &cfg,
                        &sql,
                        &binds,
                        complete,
                        fmt,
                        &path,
                        &sheet,
                        &cancel,
                        csv_delim,
                        csv_header,
                        export_page,
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
            ExportOutcome::Partial(rows, notice) => {
                self.tabs[ix].export_rows = rows as usize;
                self.tabs[ix].result_meta = format!("Exported {rows} rows — {notice}").into();
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
}

#[cfg(test)]
mod tests {
    use super::sql_locks_rows;

    #[test]
    fn detects_for_update() {
        assert!(sql_locks_rows("select * from t for update"));
        assert!(sql_locks_rows("SELECT * FROM T FOR UPDATE NOWAIT"));
        assert!(!sql_locks_rows("select * from t"));
        // Known false positive: text inside a string literal still matches.
        // Harmless — it only skips the re-run and exports the buffer.
        assert!(sql_locks_rows("select 'for update' as x from t"));
    }
}
