//! Export drain: streaming CSV/XLSX to disk on the background executor.
//!
//! Part of the run pipeline (see `run.rs`).

use super::*;

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
    let buffered: Vec<Vec<Option<String>>> = lock(&fetch.data)
        .rows
        .iter()
        .map(|r| r.iter().map(|c| c.as_deref().map(str::to_string)).collect())
        .collect();
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
    let exhausted_already = lock(&fetch.data).exhausted;
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
                lock(&fetch.data).rows.extend(to_shared(page.rows.clone()));
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
    {
        let mut data = lock(&fetch.data);
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
        let columns: Vec<String> = lock(&fetch.data)
            .columns
            .iter()
            .map(|c| c.name.clone())
            .collect();
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
        lock(&fetch.data).loading = true;
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
            lock(&fetch.data).loading = false;
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
}
