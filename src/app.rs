//! UI: connections sidebar + tabbed SQL editors + results grids + status bar.
//!
//! Model: one live Oracle session per saved connection (see `session.rs`),
//! shared by tabs. Each tab binds to a connection it can switch, and owns
//! its editor, grid, and run state. Editor drafts auto-save (debounced) and
//! tabs restore on launch.
//!
//! Blocking Oracle calls run on the background executor; the view is only
//! ever mutated on the UI thread.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::complete::{
    allows_empty_prefix, ambiguous_columns, build_alias_map, byte_to_lsp_pos, classify_context,
    detect_join_on, function_insert, insert_text_for, is_system_schema, is_trivia_position,
    join_condition_candidates, rank_candidates, resolve_qualifier, scope_label, short_comment,
    word_prefix, Candidate, CandidateKind, CompleteContext, ForeignKey, ScopeTable, EXPR_KEYWORDS,
    ORACLE_FUNCTIONS, ORACLE_KEYWORDS, PRED_FOLLOW, PRED_KEYWORDS, SELECT_FOLLOW, STMT_KEYWORDS,
};
use crate::config::{CompleteMode, Preferences, SavedConfig, SavedTab, TabsManifest, THEME_LIST};
use crate::db::{
    is_describe_statement, BindParam, DbClient, FetchPage, OracledbSession, FETCH_CAP, FETCH_CHUNK,
};
use crate::export::{csv_header_line, csv_line, sheet_name, XlsxBuilder};
use crate::filetab::{self, FileStamp};
use crate::metadata::{
    fetch_columns_blocking, fetch_fks_blocking, fetch_sequences_blocking, fetch_tables_blocking,
    MetadataCache, SharedCache,
};
use crate::model::{csv_row, tab_name_from_sql, ColumnInfo, ConnectionConfig, Environment};
use crate::session::SessionPool;
use crate::sql::{
    apply_substitutions, exec_summary, find_bind_vars, find_substitution_vars, format_sql, is_dml,
    statement_at, statement_kind, txn_end, StatementKind, SubVar,
};
use gpui_kit::base::SelectableText;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    CompletionProvider, Editor, EditorState, Input, InputContentType, InputEvent, InputState,
};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::setting::{
    RenderOptions, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit_assets::IconName as KitIcon;

const DEFAULT_SQL: &str = "SELECT user, sysdate FROM dual;";
/// Quiet period before an editor change is flushed to its draft file.
const DRAFT_DEBOUNCE: Duration = Duration::from_millis(1500);
/// Fixed width for the header action buttons so Run/Commit/Rollback/Format
/// (and Cancel/Export) render identical regardless of label length. Sized to
/// fit the longest label ("Rollback") with icon at small size.
const ACTION_BUTTON_W: f32 = 104.0;

gpui_kit::actions!(
    sqlhighland,
    [
        RunQuery,
        CopySelection,
        FormatQuery,
        CommitTxn,
        RollbackTxn,
        OpenSettings,
        Quit,
        NextTab,
        PrevTab,
        CloseTab,
        NewTab,
        OpenSql,
        SaveSql,
        SaveSqlAs,
        TriggerComplete
    ]
);

/// Max popup rows per request (ranking already orders them best-first).
const COMPLETE_LIMIT: usize = 100;

/// Oracle suggestion provider for one tab. Holds only a weak view handle +
/// tab id and resolves everything live (connection, cache snapshot, prefs),
/// so tab rebinding needs no reinstall.
struct OracleCompleter {
    view: WeakEntity<SqlHighlandView>,
    tab_id: String,
}

impl CompletionProvider for OracleCompleter {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        // NOTE: the editor entity is mutably leased for the whole trigger
        // path (`handle_completion_trigger` runs inside its update), so this
        // must NEVER read the editor entity — only the passed-in Rope plus
        // view-owned state (different entity, safe to touch).
        let snapshot = text.to_string();
        let out = self.view.update(cx, |this, _| {
            this.completion_items_for(&self.tab_id, &snapshot, offset, false)
        });
        match out {
            Ok((items, _, _)) => Task::ready(Ok(lsp_types::CompletionResponse::Array(items))),
            Err(_) => Task::ready(Ok(lsp_types::CompletionResponse::Array(Vec::new()))),
        }
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, cx: &mut App) -> bool {
        // Same lease rule as above: view-owned flag only, no editor access.
        // Shape-gating (prefix length, dot, trivia) happens in completions(),
        // which owns the full buffer text.
        let last = new_text.chars().last();
        let wordy =
            last.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#');
        // Single space also fires: `FROM |` should offer tables immediately.
        // (Multi-char pastes never trigger; newlines never trigger.)
        if last != Some('.') && !wordy && new_text != " " {
            return false;
        }
        // Manual mode: the shortcut path presents directly; never auto-fire.
        self.view
            .update(cx, |this, _| this.complete_auto)
            .unwrap_or(false)
    }
}

/// Buffered query data shared between the view and the table delegate.
/// Rows only ever grow; a new query swaps in a fresh [`FetchState`].
/// Cells are `SharedString` (ref-counted): the grid clones an Arc per cell
/// per frame instead of heap-allocating a `String`, which keeps smooth
/// scrolling free of allocator churn. Conversion happens once per page.
struct ResultData {
    columns: Vec<ColumnInfo>,
    rows: Vec<Vec<Option<SharedString>>>,
    elapsed_ms: u128,
    exhausted: bool,
    loading: bool,
    capped: bool,
}

/// Convert one fetched page to ref-counted cells.
fn to_shared(rows: Vec<Vec<Option<String>>>) -> Vec<Vec<Option<SharedString>>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(|c| c.map(SharedString::from)).collect())
        .collect()
}

/// Links one open server-side cursor to a tab's grid.
struct FetchState {
    session: Arc<Mutex<OracledbSession>>,
    query_id: u64,
    chunk: usize,
    cap: usize,
    data: Mutex<ResultData>,
    view: WeakEntity<SqlHighlandView>,
    tab_id: String,
}

/// Table delegate over the shared [`FetchState`] of a tab's latest query.
struct ResultsDelegate {
    fetch: Option<Arc<FetchState>>,
}

impl ResultsDelegate {
    fn empty() -> Self {
        Self { fetch: None }
    }

    fn set_fetch(&mut self, fetch: Option<Arc<FetchState>>) {
        self.fetch = fetch;
    }

    fn is_current(&self, fetch: &Arc<FetchState>) -> bool {
        match &self.fetch {
            Some(current) => Arc::ptr_eq(current, fetch),
            None => false,
        }
    }

    fn with_data<R>(&self, f: impl FnOnce(&ResultData) -> R, default: R) -> R {
        match &self.fetch {
            Some(fetch) => match fetch.data.lock() {
                Ok(data) => f(&data),
                Err(_) => default,
            },
            None => default,
        }
    }

    /// Display text of one cell for copying. SQL NULL copies as empty.
    /// `col_ix` is a grid index: 0 is the row-number column, data follows.
    /// Returns `None` only when the indices are out of range.
    fn cell_text(&self, row_ix: usize, col_ix: usize) -> Option<SharedString> {
        if col_ix == 0 {
            return self.with_data(
                |d| {
                    d.rows
                        .get(row_ix)
                        .map(|_| SharedString::from((row_ix + 1).to_string()))
                },
                None,
            );
        }
        self.with_data(
            |d| {
                d.rows
                    .get(row_ix)
                    .and_then(|row| row.get(col_ix - 1).map(|c| c.clone().unwrap_or_default()))
            },
            None,
        )
    }

    /// Whole row as CSV for copying. Returns `None` when out of range.
    fn row_csv(&self, row_ix: usize) -> Option<String> {
        self.with_data(
            |d| {
                d.rows
                    .get(row_ix)
                    .map(|row| csv_row(row.iter().map(|c| c.as_deref())))
            },
            None,
        )
    }
}

impl TableDelegate for ResultsDelegate {
    /// Data columns plus a leading 1-based row-number column. No columns at
    /// all before the first result (so execute confirmations stay clean).
    fn columns_count(&self, _: &App) -> usize {
        self.with_data(
            |d| {
                let n = d.columns.len();
                if n == 0 {
                    0
                } else {
                    n + 1
                }
            },
            0,
        )
    }

    fn rows_count(&self, _: &App) -> usize {
        self.with_data(|d| d.rows.len(), 0)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        // Grid column 0 is the row number; data columns shift by one.
        if col_ix == 0 {
            return Column::new("col-rownum", "#").width(px(52.)).text_right();
        }
        let name = self.with_data(|d| d.columns.get(col_ix - 1).map(|c| c.name.clone()), None);
        let name = name.expect("column with no result");
        // Key by position, not name: duplicate column names (common in
        // SELECT * joins) would otherwise collide element identities,
        // breaking reconciliation and defeating column virtualization.
        Column::new(format!("col-{col_ix}"), name).width(px(180.))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Grid column 0 renders the 1-based row number, muted and
        // right-aligned like SQL Developer; data columns shift by one.
        if col_ix == 0 {
            return div()
                .w_full()
                .text_right()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{}", row_ix + 1))
                .into_any_element();
        }
        let cell = self.with_data(
            |d| {
                d.rows
                    .get(row_ix)
                    .and_then(|row| row.get(col_ix - 1))
                    .cloned()
                    .flatten()
            },
            None,
        );
        match cell {
            // Single-line ellipsis: wrapping text forces tall rows and
            // expensive multi-line layout per cell.
            Some(text) => div().truncate().child(text).into_any_element(),
            None => div()
                .text_color(cx.theme().muted_foreground)
                .child("NULL")
                .into_any_element(),
        }
    }

    fn has_more(&self, _: &App) -> bool {
        self.with_data(|d| !d.exhausted && !d.loading, false)
    }

    fn load_more_threshold(&self) -> usize {
        200
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let Some(fetch) = self.fetch.clone() else {
            return;
        };
        {
            let mut data = match fetch.data.lock() {
                Ok(data) => data,
                Err(_) => return,
            };
            if data.loading || data.exhausted {
                return;
            }
            data.loading = true;
        }

        let bg = cx.background_executor().clone();
        let fetch_bg = fetch.clone();
        cx.spawn(async move |table_view, cx| {
            let outcome = bg
                .spawn(async move {
                    let mut session = fetch_bg.session.lock().expect("session lock");
                    session.fetch_more(fetch_bg.query_id, fetch_bg.chunk)
                })
                .await;
            let (page_opt, fetch_err) = match outcome {
                Ok(page) => (Some(page), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let applied = table_view
                .update(cx, |table, cx| {
                    if !table.delegate().is_current(&fetch) {
                        return false; // Superseded by a newer query: discard.
                    }
                    match page_opt {
                        Some(page) => {
                            if !page.current {
                                fetch.data.lock().expect("data lock").loading = false;
                                return false;
                            }
                            {
                                let mut data = fetch.data.lock().expect("data lock");
                                let room = fetch.cap.saturating_sub(data.rows.len());
                                let take = page.rows.len().min(room);
                                data.rows
                                    .extend(to_shared(page.rows).into_iter().take(take));
                                data.capped = data.rows.len() >= fetch.cap;
                                data.exhausted = page.exhausted || data.capped;
                                data.loading = false;
                            }
                            table.refresh(cx);
                            true
                        }
                        None => {
                            // Leave `exhausted` false so the next scroll retries.
                            fetch.data.lock().expect("data lock").loading = false;
                            false
                        }
                    }
                })
                .unwrap_or(false);

            // Surface the outcome in the owning tab, if it still exists and
            // still shows this fetch.
            fetch
                .view
                .update(cx, |view, cx| {
                    let Some(tab) = view.tab_by_id(&fetch.tab_id) else {
                        return;
                    };
                    if !tab.table.read(cx).delegate().is_current(&fetch) {
                        return;
                    }
                    if applied {
                        tab.result_meta = describe_fetch(&fetch).into();
                    } else if let Some(msg) = fetch_err {
                        // A failed page surfaces once; the next scroll retries.
                        tab.output = Some(Output::error(format!("Fetch failed: {msg}")));
                        tab.result_meta = "Fetch failed — scroll to retry".into();
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }
}

/// One-line status for buffered data, e.g. `2,400 rows · 12 ms`.
fn describe_fetch(fetch: &FetchState) -> String {
    let data = fetch.data.lock().expect("data lock");
    let mut s = format!("{} rows · {} ms", data.rows.len(), data.elapsed_ms);
    if data.loading {
        s.push_str(" · fetching…");
    } else if data.capped {
        s.push_str(&format!(" · stopped at {}-row cap", fetch.cap));
    } else if !data.exhausted {
        s.push_str(" · more available");
    }
    s
}

enum WorkOutcome {
    Connected,
    Failed(String),
}

/// Which selection the user made most recently. The kit keeps
/// `selected_cell` and `selected_row` independently (neither clears the
/// other), so recency — not field presence — decides what Cmd+C copies.
#[derive(Clone, Copy)]
enum CopySel {
    Cell,
    Row,
}

#[derive(Clone, Copy)]
enum ConnMenuOp {
    Connect,
    Disconnect,
    Edit,
    Delete,
}

fn conn_menu_item(
    label: &str,
    icon: KitIcon,
    view: WeakEntity<SqlHighlandView>,
    conn_id: String,
    op: ConnMenuOp,
) -> PopupMenuItem {
    PopupMenuItem::new(label)
        .icon(icon)
        .on_click(move |_, window, cx| {
            view.update(cx, |this, cx| match op {
                ConnMenuOp::Connect => this.connect_connection(&conn_id, cx),
                ConnMenuOp::Disconnect => this.disconnect_connection(&conn_id, cx),
                ConnMenuOp::Edit => {
                    if let Some(ix) = this.connection_index(&conn_id) {
                        this.start_edit(ix, window, cx);
                    }
                }
                ConnMenuOp::Delete => {
                    if let Some(ix) = this.connection_index(&conn_id) {
                        this.delete_connection(ix, cx);
                    }
                }
            })
            .ok();
        })
}

/// What the output pane shows for a tab: a failure, or the confirmation of
/// a non-query statement (DML/DDL). Successful SELECTs show the grid instead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Error,
    Info,
}

#[derive(Clone)]
struct Output {
    kind: OutputKind,
    text: SharedString,
}

impl Output {
    fn error(text: impl Into<SharedString>) -> Self {
        Self {
            kind: OutputKind::Error,
            text: text.into(),
        }
    }

    fn info(text: impl Into<SharedString>) -> Self {
        Self {
            kind: OutputKind::Info,
            text: text.into(),
        }
    }
}

/// One query tab: editor + grid + run state + connection binding.
struct QueryTab {
    id: String,
    name: SharedString,
    connection_id: Option<String>,
    path: Option<std::path::PathBuf>,
    file_stamp: Option<FileStamp>,
    dirty: bool,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultsDelegate>>,
    /// Mirrors the delegate's fetch for lock-free (no entity read) checks.
    fetch: Option<Arc<FetchState>>,
    result_meta: SharedString,
    has_result: bool,
    /// Latest action outcome for the output pane: failures, and confirmations
    /// of non-query statements. Cleared on every new run; Dismiss returns to
    /// the grid (or placeholder) without re-running.
    output: Option<Output>,
    busy: bool,
    /// Generation of the tab's latest run. Bumped on every Run and on Cancel;
    /// late completions whose token mismatches are discarded. This is what
    /// makes Cancel work without driver break support (beta.3 has none):
    /// the abandoned worker finishes server-side, but its results never land.
    run_token: u64,
    /// When the current run started. Drives the live `Running… Ns` status.
    run_started: Option<std::time::Instant>,
    /// Uncommitted DML on this tab's connection. Transactions are
    /// session-scoped, so commit/rollback clears this for every tab sharing
    /// the connection. The driver never autocommits.
    pending_txn: bool,
    /// Most recent selection kind. See [`CopySel`].
    copy_sel: Option<CopySel>,
    /// Last executed statement text. Feeds the `query` sheet on Excel export.
    last_sql: String,
    /// An export drain is paging this tab's cursor past the grid cap.
    /// While true the grid shows fetched-so-far rows and scroll-fetching
    /// pauses (`loading` is held) so pages never interleave or duplicate.
    exporting: bool,
    /// Exported row count (progress display). Bumped on the UI thread.
    export_rows: usize,
    /// Cancellation flag polled by the drain loop each chunk.
    export_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    save_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
}

/// Export file format chosen in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExportFormat {
    Csv,
    Xlsx,
}

impl ExportFormat {
    fn ext(self) -> &'static str {
        match self {
            ExportFormat::Csv => "csv",
            ExportFormat::Xlsx => "xlsx",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ExportFormat::Csv => "Export as CSV…",
            ExportFormat::Xlsx => "Export as Excel (.xlsx)…",
        }
    }
}

/// Outcome of the background export drain.
enum ExportOutcome {
    Done(u64),
    Cancelled(u64),
    Failed(String),
}

/// Suggested export filename stem from a tab name: alphanumerics, `-`, `_`.
fn file_stem(tab_name: &str) -> String {
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
            if let Err(e) = (|| -> std::io::Result<()> {
                use std::io::Write as _;
                w.write_all(csv_header_line(columns).as_bytes())?;
                w.write_all(b"\n")?;
                Ok(())
            })() {
                return ExportOutcome::Failed(format!("cannot write file: {e}"));
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
                w.write_all(csv_line(row).as_bytes())
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
                let mut session = session.lock().expect("session lock");
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

/// Global handle to the main view, stashed at window creation. Lets
/// window-level/global handlers (which have a window but no view) reach it —
/// notably to `notify()` a full re-render after a theme switch, since GPUI
/// only re-renders dirty views and window repaints alone reuse cached ones.
pub struct AppView(pub WeakEntity<SqlHighlandView>);

impl Global for AppView {}

/// The main view's entity, if the window is still alive.
pub fn app_view(cx: &App) -> Option<Entity<SqlHighlandView>> {
    AppView::global(cx).0.upgrade()
}

/// A run deferred for variable input: the statement plus the variables that
/// still need values. Only one bind dialog opens at a time.
#[derive(Debug, Clone)]
struct PendingBind {
    tab_id: String,
    sql: String,
    subs: Vec<SubVar>,
    binds: Vec<String>,
}

/// A run deferred for connection choice: the statement waits while the user
/// picks the tab's connection. Only one pick dialog opens at a time.
#[derive(Debug, Clone)]
struct PendingPick {
    tab_id: String,
    sql: String,
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

/// Read every dialog field and submit the run. Shared by the Run button and
/// the dialog's Enter-to-confirm (`on_ok`) so both paths behave identically.
/// Blank fields block submission: the error slot is filled (shown in the
/// dialog on rebuild) and false is returned so the dialog stays open.
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

/// Bind the tab to the picked connection and run the deferred statement
/// (variables dialog next, if needed). Shared by picker click and Enter
/// handlers so both paths behave identically.
fn pick_connection_and_run(
    view: &WeakEntity<SqlHighlandView>,
    tab_id: &str,
    sql: &str,
    conn_id: &str,
    window: &mut Window,
    cx: &mut App,
) {
    // Close the picker first so a variables dialog opened below lands on a
    // clean dialog stack.
    window.close_dialog(cx);
    view.update(cx, |this, cx| {
        if let Some(t) = this.tab_by_id(tab_id) {
            t.connection_id = Some(conn_id.to_string());
        }
        this.pending_pick = None;
        this.persist_tabs();
        this.start_run(tab_id, sql.to_string(), window, cx);
    })
    .ok();
}

/// Return keyboard focus to the tab's editor so the next Cmd+Enter works
/// immediately after the dialog closes (no reliance on focus-restore alone).
fn focus_tab_editor(
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

pub struct SqlHighlandView {
    pool: SessionPool,
    /// Connection ids with a live pooled session. The ONLY liveness signal
    /// the render path may use: locking a session mutex during render blocks
    /// the UI thread behind in-flight queries (a `std` Mutex blocks, it does
    /// not yield). Updated on connect/disconnect/run outcomes, never read
    /// from a render through the pool.
    live: std::collections::HashSet<String>,
    connections: Vec<ConnectionConfig>,
    tabs: Vec<QueryTab>,
    active: usize,
    tab_scroll: ScrollHandle,
    untitled_counter: usize,
    sidebar_collapsed: bool,
    /// Index being edited in the connection dialog (`None` = adding).
    editing: Option<usize>,
    /// Pending environment tag for the open connection dialog. Set by
    /// start_add/start_edit, mutated by the dialog's pill row, read by save.
    /// (Only one connection dialog opens at a time, like `editing`.)
    pending_env: Environment,
    // Dialog form fields (entities persist across dialog open/close).
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    service: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    /// Transient notice for the status bar ("Saved X", "Connecting…").
    status: SharedString,
    /// `&&name` values defined this session, per connection id. Once defined,
    /// even `&name` reuses the value without prompting (SQL*Plus parity).
    defines: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// Run waiting on the bind dialog (cleared on submit or cancel).
    pending_bind: Option<PendingBind>,
    /// Run waiting on the connection picker (cleared on pick or cancel).
    pending_pick: Option<PendingPick>,
    /// Dictionary snapshots per connection id for autocomplete. Filled on
    /// the background executor; the provider only clones the `Arc`.
    meta: std::collections::HashMap<String, SharedCache>,
    /// Usage counts `(connection_id, UPPER_LABEL)` bumping executed table
    /// names; feeds completion ranking (recency/frequency boost).
    usage: std::collections::HashMap<(String, String), u64>,
    /// Suggestion popup fires while typing (vs manual shortcut only).
    /// Mirrors `Preferences.completion`; toggled in Settings.
    complete_auto: bool,
    /// Include SYS/SYSTEM/etc. objects in suggestions. Mirrors preferences.
    show_system: bool,
    /// Window-lifetime subscriptions (OS appearance observer for System
    /// theme mode). Kept alive by ownership, like per-tab `_subs`.
    _subs: Vec<Subscription>,
}

impl SqlHighlandView {
    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.tabs.iter().any(|tab| tab.dirty && tab.path.is_some());
        if !dirty {
            cx.quit();
            return;
        }
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let save_view = view.clone();
            alert
                .title("Unsaved changes")
                .description("Save changes to external SQL files before quitting?")
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_center()
                        .child(
                            Button::new("quit-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(
                            Button::new("quit-discard")
                                .label("Discard")
                                .on_click(|_, _, cx| cx.quit()),
                        )
                        .child(
                            Button::new("quit-save")
                                .primary()
                                .label("Save and Quit")
                                .on_click(move |_, window, cx| {
                                    save_view
                                        .update(cx, |this, cx| {
                                            let mut failed = None;
                                            for tab in &mut this.tabs {
                                                if !tab.dirty {
                                                    continue;
                                                }
                                                let Some(path) = tab.path.clone() else {
                                                    continue;
                                                };
                                                let text = tab.editor.read(cx).value().to_string();
                                                match filetab::write(&path, &text) {
                                                    Ok(stamp) => {
                                                        tab.file_stamp = Some(stamp);
                                                        tab.dirty = false;
                                                    }
                                                    Err(err) => {
                                                        failed = Some(err.to_string());
                                                        break;
                                                    }
                                                }
                                            }
                                            if let Some(err) = failed {
                                                this.status = format!("Save failed: {err}").into();
                                                cx.notify();
                                            } else {
                                                window.close_dialog(cx);
                                                cx.quit();
                                            }
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
    }

    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = SavedConfig::default_path()
            .ok()
            .and_then(|p| SavedConfig::load(&p).ok())
            .map(|c| c.connections)
            .unwrap_or_default();
        let blank = ConnectionConfig::default();

        let name = cx.new(|cx| InputState::new(window, cx).placeholder("My database"));
        let host = cx.new(|cx| InputState::new(window, cx).default_value(blank.host.clone()));
        let port = cx.new(|cx| InputState::new(window, cx).default_value(blank.port.to_string()));
        let service =
            cx.new(|cx| InputState::new(window, cx).default_value(blank.service_name.clone()));
        let user = cx.new(|cx| InputState::new(window, cx).default_value(blank.user.clone()));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("password"));

        // Cmd+Enter runs the statement under the cursor. Scoped to the
        // editor's own `Input` key context; the handler double-checks focus.
        // Cmd+C copies the grid selection, scoped to the table's `DataTable`
        // context so the editor's own copy is untouched.
        // Shortcuts below verified free in the kit's `Input` bindings
        // (notably cmd-shift-f is taken by Replace, hence shift-alt-f;
        // cmd-alt-up/down are multi-cursor).
        cx.bind_keys([KeyBinding::new("cmd-enter", RunQuery, Some("Input"))]);
        cx.bind_keys([KeyBinding::new("cmd-c", CopySelection, Some("DataTable"))]);
        cx.bind_keys([KeyBinding::new("shift-alt-f", FormatQuery, Some("Input"))]);
        cx.bind_keys([KeyBinding::new("cmd-shift-c", CommitTxn, Some("Input"))]);
        cx.bind_keys([KeyBinding::new("cmd-shift-r", RollbackTxn, Some("Input"))]);
        // Tab switching is global (no context) so it works from the editor
        // and the grid alike; nothing in the kit binds ctrl-tab.
        cx.bind_keys([KeyBinding::new("ctrl-tab", NextTab, None)]);
        cx.bind_keys([KeyBinding::new("ctrl-shift-tab", PrevTab, None)]);
        cx.bind_keys([KeyBinding::new("cmd-w", CloseTab, None)]);
        cx.bind_keys([KeyBinding::new("cmd-t", NewTab, None)]);
        cx.bind_keys([KeyBinding::new("cmd-o", OpenSql, None)]);
        cx.bind_keys([KeyBinding::new("cmd-s", SaveSql, None)]);
        cx.bind_keys([KeyBinding::new("cmd-shift-s", SaveSqlAs, None)]);
        // Manual completion trigger (Zed-style); auto-popup is governed by
        // the completion preference in `is_completion_trigger`.
        cx.bind_keys([KeyBinding::new(
            "ctrl-space",
            TriggerComplete,
            Some("Input"),
        )]);

        let prefs = Preferences::load();
        let mut this = Self {
            pool: SessionPool::new(),
            live: std::collections::HashSet::new(),
            connections,
            tabs: Vec::new(),
            active: 0,
            tab_scroll: ScrollHandle::new(),
            untitled_counter: 0,
            sidebar_collapsed: false,
            editing: None,
            pending_env: Environment::default(),
            name,
            host,
            port,
            service,
            user,
            password,
            status: "".into(),
            defines: std::collections::HashMap::new(),
            pending_bind: None,
            pending_pick: None,
            meta: std::collections::HashMap::new(),
            usage: std::collections::HashMap::new(),
            complete_auto: prefs.completion == CompleteMode::Auto,
            show_system: prefs.show_system_schemas,
            _subs: Vec::new(),
        };
        // Follow the OS appearance while the theme mode is System. The
        // subscription lives as long as the view; the callback no-ops for
        // explicit Light/Dark preferences.
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            crate::guitheme::reapply_for_system_appearance(window, cx);
        });
        this._subs.push(appearance_sub);
        this.restore_tabs(window, cx);
        // Give the window an initial focus target. Without this, GPUI has no
        // focused dispatch node until the user clicks the editor, so the
        // global Cmd+, keybinding is not translated into an action.
        if let Some(tab) = this.tabs.get(this.active) {
            let editor = tab.editor.clone();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
        }
        this
    }

    // -- Tabs ---------------------------------------------------------------

    fn tab_index(&self, tab_id: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == tab_id)
    }

    fn tab_by_id(&mut self, tab_id: &str) -> Option<&mut QueryTab> {
        self.tabs.iter_mut().find(|t| t.id == tab_id)
    }

    fn active_tab(&self) -> &QueryTab {
        &self.tabs[self.active]
    }

    fn connection_name(&self, id: &Option<String>) -> String {
        id.as_ref()
            .and_then(|cid| self.connections.iter().find(|c| &c.id == cid))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Select connection".to_string())
    }

    fn make_tab(
        &mut self,
        id: String,
        name: String,
        connection_id: Option<String>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .default_value(text)
        });
        // Suggestion provider: popup/keyboard rendering is automatic once
        // installed; candidates resolve live per tab (connection + cache).
        {
            let completer = OracleCompleter {
                view: cx.entity().downgrade(),
                tab_id: id.clone(),
            };
            editor.update(cx, |editor, _| {
                editor.lsp_mut().completion_provider =
                    Some(Rc::new(completer) as Rc<dyn CompletionProvider>);
            });
        }
        let table = cx
            .new(|cx| TableState::new(ResultsDelegate::empty(), window, cx).cell_selectable(true));
        let tab_id = id.clone();
        let table_tab_id = id.clone();
        let subs = vec![
            cx.subscribe_in(&editor, window, move |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    if let Some(tab) = this.tab_by_id(&tab_id) {
                        tab.dirty = true;
                    }
                    this.schedule_draft_save(&tab_id, cx);
                }
            }),
            cx.subscribe_in(&table, window, move |this, _, ev: &TableEvent, _, _| {
                let Some(tab) = this.tab_by_id(&table_tab_id) else {
                    return;
                };
                match ev {
                    TableEvent::SelectCell(..) => tab.copy_sel = Some(CopySel::Cell),
                    TableEvent::SelectRow(..) => tab.copy_sel = Some(CopySel::Row),
                    TableEvent::ClearSelection | TableEvent::SelectColumn(..) => {
                        tab.copy_sel = None;
                    }
                    _ => {}
                }
            }),
        ];
        self.tabs.push(QueryTab {
            id,
            name: name.into(),
            connection_id,
            path: None,
            file_stamp: None,
            dirty: false,
            editor,
            table,
            fetch: None,
            result_meta: "".into(),
            has_result: false,
            output: None,
            busy: false,
            run_token: 0,
            run_started: None,
            pending_txn: false,
            copy_sel: None,
            last_sql: String::new(),
            exporting: false,
            export_rows: 0,
            export_cancel: None,
            save_task: None,
            _subs: subs,
        });
    }

    fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Corrupt manifests are renamed aside (never silently defaulted —
        // the next persist would otherwise overwrite them permanently).
        let manifest_existed = TabsManifest::manifest_path()
            .map(|p| p.exists())
            .unwrap_or(false);
        let manifest = TabsManifest::load_preserving();
        for saved in manifest.tabs {
            // Always start unbound after a restart so the user explicitly
            // picks a connection per tab; editor text still restores.
            let connection_id = None;
            let external = saved.path.as_deref().and_then(|path| {
                filetab::normalize(path).ok().and_then(|path| {
                    filetab::read(&path)
                        .ok()
                        .map(|(text, stamp)| (path, text, stamp))
                })
            });
            let text = external
                .as_ref()
                .map(|(_, text, _)| text.clone())
                .unwrap_or_else(|| TabsManifest::read_draft(&saved.id));
            self.untitled_counter += 1;
            let name = if saved.name.is_empty() {
                format!("Untitled {}", self.untitled_counter)
            } else {
                saved.name.clone()
            };
            self.make_tab(saved.id, name, connection_id, text, window, cx);
            if let Some((path, _, stamp)) = external {
                if let Some(tab) = self.tabs.last_mut() {
                    tab.path = Some(path);
                    tab.file_stamp = Some(stamp);
                    tab.dirty = false;
                }
            }
        }
        // Adopt orphaned drafts: `.sql` files with no manifest entry are
        // leftovers of a lost manifest — restore them as tabs rather than
        // abandoning the user's queries.
        let known: std::collections::HashSet<String> =
            self.tabs.iter().map(|t| t.id.clone()).collect();
        let orphans = TabsManifest::orphan_drafts(&known);
        let adopted = !orphans.is_empty();
        if adopted {
            self.status = format!("Recovered {} tab(s) from unsaved drafts", orphans.len()).into();
        }
        for (id, text) in orphans {
            self.untitled_counter += 1;
            let name = tab_name_from_sql(&text, &format!("Untitled {}", self.untitled_counter));
            self.make_tab(id, name, None, text, window, cx);
        }
        if self.tabs.is_empty() {
            // First launch (or empty manifest): one starter tab, unbound so
            // the user explicitly picks a connection.
            let text = if manifest_existed {
                String::new()
            } else {
                DEFAULT_SQL.to_string()
            };
            self.add_tab(None, text, window, cx);
        } else if adopted {
            // Manifest now matches the adopted set on disk.
            self.persist_tabs();
        }
        self.active = 0;
    }

    fn add_tab(
        &mut self,
        connection_id: Option<String>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.untitled_counter += 1;
        let id = uuid::Uuid::new_v4().to_string();
        let name = tab_name_from_sql(&text, &format!("Untitled {}", self.untitled_counter));
        self.make_tab(id.clone(), name, connection_id, text.clone(), window, cx);
        // Persist immediately so a crash before the first keystroke loses nothing.
        let _ = TabsManifest::write_draft(&id, &text);
        self.persist_tabs();
        self.active = self.tabs.len() - 1;
        self.tab_scroll.scroll_to_item(self.active);
        cx.notify();
        id
    }

    fn close_tab_now(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_index(tab_id) {
            let removed = self.tabs.remove(ix);
            TabsManifest::delete_draft(&removed.id);
            if self.tabs.is_empty() {
                // Always keep one tab open. Untitled tabs remain internally
                // auto-saved; external tabs are guarded before this path.
                self.untitled_counter += 1;
                let id = uuid::Uuid::new_v4().to_string();
                let name = format!("Untitled {}", self.untitled_counter);
                self.make_tab(id, name, None, String::new(), window, cx);
            }
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            self.persist_tabs();
            self.tab_scroll.scroll_to_item(self.active);
            // The closed editor may still own keyboard focus. Move focus to
            // the replacement active tab so repeated shortcuts keep working.
            let editor = self.tabs[self.active].editor.clone();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
            cx.notify();
        }
    }

    fn request_close_tab(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.tabs.iter().find(|tab| tab.id == tab_id) else {
            return;
        };
        if !tab.dirty || tab.path.is_none() {
            self.close_tab_now(tab_id, window, cx);
            return;
        }

        let view = cx.entity().downgrade();
        let tab_id_save = tab_id.to_string();
        let tab_id_discard = tab_id.to_string();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let discard_view = view.clone();
            let save_view = view.clone();
            let discard_id = tab_id_discard.clone();
            let save_id = tab_id_save.clone();
            alert
                .title("Unsaved changes")
                .description("Save changes before closing this SQL file?")
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_center()
                        .child(
                            Button::new("close-cancel")
                                .label("Cancel")
                                .on_click(|_, window, cx| window.close_dialog(cx)),
                        )
                        .child(Button::new("close-discard").label("Discard").on_click({
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                discard_view
                                    .update(cx, |this, cx| {
                                        this.close_tab_now(&discard_id, window, cx);
                                    })
                                    .ok();
                            }
                        }))
                        .child(Button::new("close-save").primary().label("Save").on_click({
                            move |_, window, cx| {
                                save_view
                                    .update(cx, |this, cx| {
                                        let Some(ix) = this.tab_index(&save_id) else {
                                            return;
                                        };
                                        let Some(path) = this.tabs[ix].path.clone() else {
                                            return;
                                        };
                                        let text =
                                            this.tabs[ix].editor.read(cx).value().to_string();
                                        match filetab::write(&path, &text) {
                                            Ok(stamp) => {
                                                this.tabs[ix].file_stamp = Some(stamp);
                                                this.tabs[ix].dirty = false;
                                                window.close_dialog(cx);
                                                this.close_tab_now(&save_id, window, cx);
                                            }
                                            Err(err) => {
                                                this.status = format!("Save failed: {err}").into();
                                                cx.notify();
                                            }
                                        }
                                    })
                                    .ok();
                            }
                        })),
                )
        });
    }

    fn select_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active = ix;
        self.tab_scroll.scroll_to_item(ix);
        let editor = self.tabs[ix].editor.clone();
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        self.check_external_change(ix, window, cx);
        cx.notify();
    }

    fn check_external_change(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.tabs[ix].path.clone() else {
            return;
        };
        let Ok(current) = filetab::stamp(&path) else {
            self.status = format!("File is unavailable: {}", path.display()).into();
            return;
        };
        if self.tabs[ix].file_stamp.as_ref() == Some(&current) {
            return;
        }
        if self.tabs[ix].dirty {
            self.status = format!("External changes detected: {}", path.display()).into();
            return;
        }

        let tab_id = self.tabs[ix].id.clone();
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let keep_view = view.clone();
            let reload_view = view.clone();
            let keep_id = tab_id.clone();
            let reload_id = tab_id.clone();
            let reload_path = path.clone();
            alert
                .title("File changed on disk")
                .description("Reload the file or keep the current buffer?")
                .footer(
                    h_flex()
                        .gap_2()
                        .justify_center()
                        .child(Button::new("file-keep").label("Keep").on_click({
                            let current = current.clone();
                            move |_, window, cx| {
                                window.close_dialog(cx);
                                keep_view
                                    .update(cx, |this, cx| {
                                        if let Some(tab) = this.tab_by_id(&keep_id) {
                                            tab.file_stamp = Some(current.clone());
                                        }
                                        cx.notify();
                                    })
                                    .ok();
                            }
                        }))
                        .child(
                            Button::new("file-reload")
                                .primary()
                                .label("Reload")
                                .on_click(move |_, window, cx| {
                                    let Ok((text, stamp)) = filetab::read(&reload_path) else {
                                        return;
                                    };
                                    reload_view
                                        .update(cx, |this, cx| {
                                            if let Some(tab) = this.tab_by_id(&reload_id) {
                                                tab.editor.update(cx, |editor, cx| {
                                                    editor.set_value(text, window, cx);
                                                });
                                                tab.file_stamp = Some(stamp);
                                                tab.dirty = false;
                                            }
                                            window.close_dialog(cx);
                                            cx.notify();
                                        })
                                        .ok();
                                }),
                        ),
                )
        });
    }

    /// Cycle tabs with wrapping (`ctrl-tab` / `ctrl-shift-tab`). Focus
    /// follows so the next Cmd+Enter runs in the newly shown tab.
    fn cycle_tab(&mut self, dir: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = (self.active as isize + dir).rem_euclid(self.tabs.len() as isize) as usize;
        self.select_tab(next, window, cx);
    }

    fn persist_tabs(&self) {
        let manifest = TabsManifest {
            tabs: self
                .tabs
                .iter()
                .map(|t| SavedTab {
                    id: t.id.clone(),
                    name: t.name.to_string(),
                    connection_id: t.connection_id.clone(),
                    path: t.path.clone(),
                })
                .collect(),
        };
        let _ = manifest.save();
    }

    fn save_active_tab_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = self.active_tab().id.clone();
        let suggested = format!("{}.sql", file_stem(&self.active_tab().name));
        let dir = std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        let view = cx.entity().downgrade();
        cx.spawn_in(window, async move |_, cx| {
            let rx = match cx.update(|_, cx| cx.prompt_for_new_path(&dir, Some(&suggested))) {
                Ok(rx) => rx,
                Err(_) => return,
            };
            let Ok(Ok(Some(path))) = rx.await else {
                return;
            };
            let path = if filetab::is_sql(&path) {
                path
            } else {
                path.with_extension("sql")
            };
            view.update(cx, |this, cx| {
                let Some(ix) = this.tab_index(&tab_id) else {
                    return;
                };
                let text = this.tabs[ix].editor.read(cx).value().to_string();
                match filetab::normalize(&path)
                    .and_then(|path| filetab::write(&path, &text).map(|stamp| (path, stamp)))
                {
                    Ok((path, stamp)) => {
                        this.tabs[ix].path = Some(path.clone());
                        this.tabs[ix].file_stamp = Some(stamp);
                        this.tabs[ix].dirty = false;
                        this.tabs[ix].name = path
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or("Untitled")
                            .to_string()
                            .into();
                        this.persist_tabs();
                        cx.notify();
                    }
                    Err(err) => {
                        this.status = format!("Save failed: {err}").into();
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn save_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_tab().path.is_none() {
            self.save_active_tab_as(window, cx);
            return;
        }
        let tab_id = self.active_tab().id.clone();
        let path = self.active_tab().path.clone().expect("path checked above");
        let text = self.active_tab().editor.read(cx).value().to_string();
        match filetab::write(&path, &text) {
            Ok(stamp) => {
                if let Some(tab) = self.tab_by_id(&tab_id) {
                    tab.file_stamp = Some(stamp);
                    tab.dirty = false;
                }
                self.persist_tabs();
                cx.notify();
            }
            Err(err) => {
                self.status = format!("Save failed: {err}").into();
                cx.notify();
            }
        }
    }

    fn open_sql_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let view = cx.entity().downgrade();
        let window_handle = window.window_handle();
        cx.spawn_in(window, async move |_, cx| {
            let rx = match cx.update(|_, cx| {
                cx.prompt_for_paths(PathPromptOptions {
                    files: true,
                    directories: false,
                    multiple: false,
                    prompt: Some("Open SQL file".into()),
                })
            }) {
                Ok(rx) => rx,
                Err(_) => return,
            };
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            let Some(path) = paths.into_iter().next() else {
                return;
            };
            if !filetab::is_sql(&path) {
                let _ = window_handle.update(cx, |_, _, cx| {
                    view.update(cx, |this, cx| {
                        this.status = "Only .sql files can be opened".into();
                        cx.notify();
                    })
                    .ok();
                });
                return;
            }
            let Ok(path) = filetab::normalize(&path) else {
                return;
            };
            let Ok((text, stamp)) = filetab::read(&path) else {
                return;
            };
            let _ = window_handle.update(cx, |_, window, cx| {
                view.update(cx, |this, cx| {
                    if let Some(ix) = this
                        .tabs
                        .iter()
                        .position(|tab| tab.path.as_ref() == Some(&path))
                    {
                        this.select_tab(ix, window, cx);
                        return;
                    }
                    let id = uuid::Uuid::new_v4().to_string();
                    let name = path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("Untitled")
                        .to_string();
                    this.make_tab(id, name, None, text, window, cx);
                    if let Some(tab) = this.tabs.last_mut() {
                        tab.path = Some(path);
                        tab.file_stamp = Some(stamp);
                        tab.dirty = false;
                    }
                    this.active = this.tabs.len() - 1;
                    this.persist_tabs();
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    /// Debounced draft flush: replaces any pending save for the tab.
    fn schedule_draft_save(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        let text = self.tabs[ix].editor.read(cx).value().to_string();
        let name = tab_name_from_sql(&text, &self.tabs[ix].name);
        self.tabs[ix].save_task = None; // drop cancels the pending flush

        let bg = cx.background_executor().clone();
        let save_id = self.tabs[ix].id.clone();
        let write_id = save_id.clone();
        let path = self.tabs[ix].path.clone();
        let task = cx.spawn(async move |view, cx| {
            bg.timer(DRAFT_DEBOUNCE).await;
            let outcome = bg
                .spawn(async move {
                    match path {
                        Some(path) => filetab::write(&path, &text)
                            .map(|stamp| Some(stamp))
                            .map_err(|e| e.to_string()),
                        None => TabsManifest::write_draft(&write_id, &text)
                            .map(|_| None)
                            .map_err(|e| e.to_string()),
                    }
                })
                .await;
            view.update(cx, |this, cx| {
                let Some(tab) = this.tab_by_id(&save_id) else {
                    return; // Tab closed while waiting; draft already removed.
                };
                match outcome {
                    Ok(stamp) => {
                        tab.dirty = false;
                        if let Some(stamp) = stamp {
                            tab.file_stamp = Some(stamp);
                        }
                        tab.name = name.into();
                        this.persist_tabs();
                    }
                    Err(msg) => {
                        this.status = format!("Draft save failed: {msg}").into();
                    }
                }
                cx.notify();
            })
            .ok();
        });
        // Store the task on the tab so a newer keystroke cancels this flush.
        if let Some(tab) = self.tab_by_id(tab_id) {
            tab.save_task = Some(task);
        }
    }

    // -- Connections ----------------------------------------------------------

    fn connection_index(&self, conn_id: &str) -> Option<usize> {
        self.connections.iter().position(|c| c.id == conn_id)
    }

    fn form_config(&self, cx: &App) -> ConnectionConfig {
        // Preserve the edited entry's id so live sessions keep matching.
        let id = self
            .editing
            .and_then(|ix| self.connections.get(ix))
            .map(|c| c.id.clone())
            .unwrap_or_default();
        ConnectionConfig {
            id,
            name: self.name.read(cx).value().to_string(),
            host: self.host.read(cx).value().to_string(),
            port: self.port.read(cx).value().parse().unwrap_or(1521),
            service_name: self.service.read(cx).value().to_string(),
            user: self.user.read(cx).value().to_string(),
            password: self.password.read(cx).value().to_string(),
            environment: self.pending_env,
        }
    }

    fn fill_form(&self, cfg: &ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        self.name
            .update(cx, |s, cx| s.set_value(cfg.name.clone(), window, cx));
        self.host
            .update(cx, |s, cx| s.set_value(cfg.host.clone(), window, cx));
        self.port
            .update(cx, |s, cx| s.set_value(cfg.port.to_string(), window, cx));
        self.service.update(cx, |s, cx| {
            s.set_value(cfg.service_name.clone(), window, cx)
        });
        self.user
            .update(cx, |s, cx| s.set_value(cfg.user.clone(), window, cx));
        self.password
            .update(cx, |s, cx| s.set_value(cfg.password.clone(), window, cx));
    }

    fn persist(&self) {
        if let Ok(path) = SavedConfig::default_path() {
            let saved = SavedConfig {
                connections: self.connections.clone(),
            };
            let _ = saved.save(&path);
        }
    }

    // -- Connection CRUD + dialog ------------------------------------------

    fn start_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        self.pending_env = Environment::Untagged;
        // Blank form: text fields empty, standard Oracle port kept.
        self.fill_form(
            &ConnectionConfig {
                name: String::new(),
                host: String::new(),
                port: 1521,
                service_name: String::new(),
                user: String::new(),
                ..Default::default()
            },
            window,
            cx,
        );
        self.open_connection_dialog("Add connection", window, cx);
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let title = format!("Edit {}", self.connections[ix].name);
        self.editing = Some(ix);
        let cfg = self.connections[ix].clone();
        self.pending_env = cfg.environment;
        self.fill_form(&cfg, window, cx);
        self.open_connection_dialog(&title, window, cx);
    }

    /// Settings dialog: theme family + appearance mode. Selections apply
    /// live, persist to preferences.toml, and close the dialog (menu-like).
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        Self::open_settings_dialog(&cx.entity(), window, cx);
    }

    /// Settings dialog as an associated function so the global App::on_action
    /// handler (which has a window but no view handle) can open it too.
    /// Rows apply, notify the view for a full re-render (GPUI only repaints
    /// dirty views — window refreshes alone reuse cached ones), then close.
    pub fn open_settings_dialog(view: &Entity<SqlHighlandView>, window: &mut Window, cx: &mut App) {
        // One settings dialog at a time: without this every Cmd+, stacks
        // another instance (there is no universal on-close hook to track
        // overlay/Esc dismissal with a flag, so ask the window instead).
        if window.has_active_dialog(cx) {
            return;
        }
        // Owned for the 'static dialog builder below.
        let view = view.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let hover_bg = cx.theme().accent;
            // Reloaded on every rebuild so the check mark follows the
            // selection while the dialog stays open.
            let current = Preferences::load().theme_name();
            let theme_list_view = view.clone();
            let theme_list = move |_: &RenderOptions, _: &mut Window, _: &mut App| {
                let mut rows = Vec::new();
                for (row_ix, name) in THEME_LIST.into_iter().enumerate() {
                    let selected = name == current;
                    let id = ("settings-theme", row_ix);
                    let row_view = theme_list_view.clone();
                    rows.push(
                        div()
                            .id(id)
                            .w_full()
                            .p_2()
                            .rounded_md()
                            .hover(move |this| this.bg(hover_bg))
                            .on_click(move |_, window, cx| {
                                let view = row_view.clone();
                                view.update(cx, |_, cx| {
                                    let mut prefs = Preferences::load();
                                    prefs.theme = name.to_string();
                                    let _ = prefs.save();
                                    crate::guitheme::apply_preferences(&prefs, Some(window), cx);
                                    // Full view re-render: GPUI only repaints
                                    // dirty views, and window refreshes alone
                                    // reuse cached ones. Root too: its background
                                    // paints behind the transparent sidebar/status.
                                    cx.notify();
                                    gpui_kit::component::Root::update(
                                        window,
                                        cx,
                                        |_, _, cx| cx.notify(),
                                    );
                            });
                            window.refresh();
                        })
                            .child(
                                h_flex()
                                    .gap_2()
                                    .items_center()
                                    .child(div().flex_1().text_sm().child(name))
                                    .when(selected, |this| {
                                        this.child(
                                            div()
                                                .text_color(muted)
                                                .child(KitIcon::Check),
                                        )
                                    }),
                            )
                            .into_any_element(),
                    );
                }
                v_flex().gap_1().children(rows)
            };
            // Reloaded on every rebuild so check marks follow live prefs.
            let current_mode = Preferences::load().completion;
            let show_system = Preferences::load().show_system_schemas;
            let complete_view = view.clone();
            let system_view = view.clone();
            // Row helper: label + sublabel + check mark, click applies.
            let complete_row = move |ix: usize,
                                    label: &str,
                                    sub: &str,
                                    selected: bool,
                                    apply: CompleteMode,
                                    view: Entity<SqlHighlandView>| {
                let row_view = view.clone();
                div()
                    .id(("settings-complete", ix))
                    .w_full()
                    .p_2()
                    .rounded_md()
                    .hover(move |this| this.bg(hover_bg))
                    .on_click(move |_, _, cx| {
                        row_view.update(cx, |this, cx| {
                            let mut prefs = Preferences::load();
                            prefs.completion = apply;
                            let _ = prefs.save();
                            this.complete_auto = apply == CompleteMode::Auto;
                            cx.notify();
                        });
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                v_flex()
                                    .flex_1()
                                    .child(div().text_sm().child(label.to_string()))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(muted)
                                            .child(sub.to_string()),
                                    ),
                            )
                            .when(selected, |this| {
                                this.child(
                                    div().text_color(muted).child(KitIcon::Check),
                                )
                            }),
                    )
                    .into_any_element()
            };
            let system_selected = show_system;
            let system_view_outer = system_view.clone();
            dialog
                .title("Settings")
                .w(px(640.))
                .child(
                    div().w_full().h(px(440.)).child(
                        Settings::new("sqlhighland-settings").pages(vec![
                            SettingPage::new("Themes")
                                .icon(KitIcon::Palette)
                                .groups(vec![SettingGroup::new().title("Appearance").items(
                                    vec![SettingItem::render(theme_list)],
                                )]),
                            SettingPage::new("Editor")
                                .icon(KitIcon::SquarePen)
                                .groups(vec![SettingGroup::new()
                                    .title("Suggestions")
                                    .items(vec![
                                        SettingItem::render(move |_, _, _| {
                                            v_flex().gap_1().children([
                                                complete_row(
                                                    0,
                                                    "Automatic",
                                                    "Popup while typing (Ctrl+Space also works)",
                                                    current_mode == CompleteMode::Auto,
                                                    CompleteMode::Auto,
                                                    complete_view.clone(),
                                                ),
                                                complete_row(
                                                    1,
                                                    "Manual",
                                                    "Popup only on Ctrl+Space",
                                                    current_mode == CompleteMode::Manual,
                                                    CompleteMode::Manual,
                                                    complete_view.clone(),
                                                ),
                                            ])
                                        }),
                                        SettingItem::render(move |_, _, _| {
                                            let system_row_view = system_view_outer.clone();
                                            v_flex().gap_1().child(
                                                div()
                                                    .id("settings-system-schemas")
                                                    .w_full()
                                                    .p_2()
                                                    .rounded_md()
                                                    .hover(move |this| this.bg(hover_bg))
                                                    .on_click(move |_, _, cx| {
                                                        system_row_view.update(
                                                            cx,
                                                            |this, cx| {
                                                                let mut prefs =
                                                                    Preferences::load();
                                                                prefs.show_system_schemas =
                                                                    !prefs.show_system_schemas;
                                                                let _ = prefs.save();
                                                                this.show_system =
                                                                    prefs.show_system_schemas;
                                                                // Scope changed: drop caches so
                                                                // the next trigger refetches
                                                                // with the new filter.
                                                                for cache in this.meta.values() {
                                                                    if let Ok(mut c) =
                                                                        cache.lock()
                                                                    {
                                                                        c.fetched_at = None;
                                                                    }
                                                                }
                                                                cx.notify();
                                                            },
                                                        );
                                                    })
                                                    .child(
                                                        h_flex()
                                                            .gap_2()
                                                            .items_center()
                                                            .child(
                                                                v_flex()
                                                                    .flex_1()
                                                                    .child(
                                                                        div()
                                                                            .text_sm()
                                                                            .child(
                                                                                "Show system schemas",
                                                                            ),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_xs()
                                                                            .text_color(muted)
                                                                            .child(
                                                                                "Include SYS, SYSTEM, XDB, … in suggestions",
                                                                            ),
                                                                    ),
                                                            )
                                                            .when(system_selected, |this| {
                                                                this.child(
                                                                    div()
                                                                        .text_color(muted)
                                                                        .child(KitIcon::Check),
                                                                )
                                                            }),
                                                    ),
                                            )
                                        }),
                                    ])]),
                            SettingPage::new("About")
                                .icon(KitIcon::Info)
                                .groups(vec![SettingGroup::new().title("About").items(vec![
                                    SettingItem::render(move |_, _, _| {
                                        v_flex().gap_1().child(
                                            div().text_sm().child(format!(
                                                "SQLHighland {} — Oracle SQL client",
                                                env!("CARGO_PKG_VERSION")
                                            )),
                                        )
                                    }),
                                    SettingItem::render(move |_, _, _| {
                                        v_flex().gap_1().child(
                                            div()
                                                .text_xs()
                                                .text_color(muted)
                                                .child(
                                                    "Oracle-only GUI client built with Rust \
                                                     and GPUI, using the official thin driver \
                                                     (no Oracle Client required).",
                                                ),
                                        )
                                    }),
                                    SettingItem::render(move |_, _, _| {
                                        v_flex().gap_1().children(
                                            [
                                                ("Connections", SavedConfig::default_path()),
                                                ("Preferences", Preferences::path()),
                                                (
                                                    "Tabs",
                                                    TabsManifest::manifest_path(),
                                                ),
                                            ]
                                            .into_iter()
                                            .map(|(label, path)| {
                                                div()
                                                    .child(
                                                        div().text_xs().child(label),
                                                    )
                                                    .child(
                                                        div()
                                                            .text_xs()
                                                            .text_color(muted)
                                                            .child(
                                                                path.map(|p| {
                                                                    p.to_string_lossy()
                                                                        .into_owned()
                                                                })
                                                                .unwrap_or_else(|_| {
                                                                    "unknown".to_string()
                                                                }),
                                                            ),
                                                    )
                                                    .into_any_element()
                                            })
                                            .collect::<Vec<_>>(),
                                        )
                                    }),
                                ])]),
                        ]),
                    ),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("settings-done").label("Done").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                            },
                        )),
                )
        });
    }

    fn open_connection_dialog(&self, title: &str, window: &mut Window, cx: &mut Context<Self>) {
        let title: SharedString = title.to_string().into();
        let view = cx.entity().downgrade();
        let (name, host, port, service, user, password) = (
            self.name.clone(),
            self.host.clone(),
            self.port.clone(),
            self.service.clone(),
            self.user.clone(),
            self.password.clone(),
        );
        // Dialog-local copy of the env tag. The dialog builder re-runs on every
        // render, so it must NOT touch the view entity here (that double-leases
        // and aborts). Click handlers (safe, outside render) sync the cell back
        // to `pending_env` and notify to rebuild with the new highlight.
        let pending_cell: Rc<RefCell<Environment>> = Rc::new(RefCell::new(self.pending_env));
        window.open_dialog(cx, move |dialog, _, cx| {
            let save_view = view.clone();
            let pending = pending_cell.clone();
            let muted = cx.theme().muted_foreground;
            // Fresh each rebuild so the picked pill highlights live.
            let current_env = *pending.borrow();
            dialog
                .title(title.clone())
                .w(px(400.))
                .child(
                    v_flex()
                        .gap_2()
                        .w_full()
                        .child(dialog_field("Name", &name, false, muted))
                        .child(dialog_field("Host", &host, false, muted))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_1()
                                        .child(dialog_field("Port", &port, false, muted)),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .child(dialog_field("Service", &service, false, muted)),
                                ),
                        )
                        .child(dialog_field("User", &user, false, muted))
                        .child(dialog_field("Password", &password, true, muted))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(muted).child("Environment"))
                                .child(h_flex().gap_1().children(
                                    Environment::ALL.iter().enumerate().map(|(ix, env)| {
                                        let selected = current_env == *env;
                                        let row_view = save_view.clone();
                                        let pending_click = pending.clone();
                                        let (label, text_color, bg) = match env_color(*env, cx) {
                                            Some(color) => (
                                                env.label().unwrap_or("").to_string(),
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
                                            .px_2()
                                            .py_1()
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
                                                        this.pending_env = *env;
                                                        cx.notify();
                                                    })
                                                    .ok();
                                            })
                                            .child(label)
                                    }),
                                )),
                        ),
                )
                .footer(
                    h_flex()
                        .gap_2()
                        .child(div().flex_1())
                        .child(Button::new("dlg-cancel").label("Cancel").on_click(
                            move |_, window, cx| {
                                window.close_dialog(cx);
                            },
                        ))
                        .child(Button::new("dlg-save").primary().label("Save").on_click(
                            move |_, window, cx: &mut App| {
                                save_view
                                    .update(cx, |this, cx| {
                                        this.save_from_dialog(window, cx);
                                    })
                                    .ok();
                                window.close_dialog(cx);
                            },
                        )),
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
        if let Some(ix) = self.editing {
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
        self.editing = None;
        self.persist();
        self.status = format!("Saved {}", cfg.name).into();
        cx.notify();
    }

    fn delete_connection(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let removed = self.connections.remove(ix);
        self.pool.remove(&removed.id);
        self.live.remove(&removed.id);
        // Tabs bound to it fall back to "no connection".
        for tab in &mut self.tabs {
            if tab.connection_id.as_deref() == Some(removed.id.as_str()) {
                tab.connection_id = None;
            }
        }
        self.persist();
        self.persist_tabs();
        self.status = format!("Deleted {}", removed.name).into();
        cx.notify();
    }

    // -- Sessions -------------------------------------------------------------

    /// Eagerly connect a saved connection (sidebar menu). Tabs auto-connect
    /// lazily on Run, so this is strictly optional.
    fn connect_connection(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        let Some(cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            return;
        };
        self.status = format!("Connecting to {}…", cfg.connect_string()).into();
        cx.notify();

        let session = self.pool.get_or_create(&cfg.id);
        let conn_id = cfg.id.clone();
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    let mut guard = session.lock().expect("session lock");
                    match guard.connect(&cfg) {
                        Ok(()) => WorkOutcome::Connected,
                        Err(e) => WorkOutcome::Failed(e.to_string()),
                    }
                })
                .await;
            view.update(cx, |this, cx| {
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

    fn disconnect_connection(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        self.pool.remove(conn_id);
        self.live.remove(conn_id);
        // Tabs keep their fetched rows; further paging goes stale (guarded).
        // No "Disconnected" notice: the status bar derives live state itself.
        self.status = "".into();
        cx.notify();
    }

    // -- Query ------------------------------------------------------------------

    /// Run the statement under the cursor (Cmd+Enter) or via the Run button.
    /// With a single statement in the buffer, caret position is ignored.
    /// True when the query editor (not a dialog field) holds focus.
    /// All keyboard shortcuts double-check this so Cmd+Enter etc. in a
    /// connection dialog field never fire actions behind the modal.
    fn editor_focused(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        window
            .focused(cx)
            .map(|h| h == self.active_tab().editor.read(cx).focus_handle(cx))
            .unwrap_or(false)
    }

    fn run_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_focused(window, cx) {
            return;
        }
        let text = self.active_tab().editor.read(cx).value().to_string();
        let cursor = self.active_tab().editor.read(cx).cursor();
        match statement_at(&text, cursor) {
            Some(sql) => {
                let tab_id = self.active_tab().id.clone();
                self.start_run(&tab_id, sql, window, cx);
            }
            None => {
                let ix = self.active;
                self.tabs[ix].result_meta = "No statement at cursor".into();
                cx.notify();
            }
        }
    }

    /// Entry point for a run: checks the connection binding, detects `&`/`&&`
    /// substitution variables and `:binds`, and either runs directly or opens
    /// the variables dialog first. With no connection bound, offers the
    /// connection picker first and runs right after the pick.
    fn start_run(
        &mut self,
        tab_id: &str,
        sql: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                });
                self.open_conn_pick_dialog(window, cx);
                return;
            }
        };
        if self.tabs[ix].busy {
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
        });
        self.open_bind_dialog(window, cx);
    }

    /// Connection picker for unbound runs: choosing binds the tab and the
    /// deferred statement runs immediately (variables dialog next, if needed).
    /// Search-first: type to filter, Enter runs on the first match, click
    /// picks any row. Builder-safe like the other dialogs: everything is
    /// cloned in, the builder never touches the view entity.
    fn open_conn_pick_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_pick.clone() else {
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
        // Explicit scroll handle (NOT the overflow_y_scrollbar() wrapper):
        // the wrapper's caller-id keying misbehaves for dialog content that
        // rebuilds every render, while an owned handle tracks stably.
        let scroll_handle = Rc::new(ScrollHandle::new());
        // Last filter seen: typing rewinds to the top, since a stale offset
        // could hide the whole shortened list. Compared in the builder (the
        // input's own change notification already repaints every keystroke).
        let last_filter: Rc<std::cell::RefCell<String>> =
            Rc::new(std::cell::RefCell::new(String::new()));
        // Keyboard flow: search takes focus on open; Enter confirms the first
        // match. The builder re-runs every render, so focusing happens
        // one-shot on the first build (a pre-mount focus call alone may not
        // stick).
        let search_focus = search.read(cx).focus_handle(cx);
        let focused_once: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
        let search_in = search.clone();
        window.open_dialog(cx, move |dialog, window, cx| {
            let rows = rows.clone();
            let search = search_in.clone();
            if !focused_once.get() {
                focused_once.set(true);
                window.focus(&search.read(cx).focus_handle(cx), cx);
            }
            let muted = cx.theme().muted_foreground;
            let filter = search.read(cx).value().to_string();
            if *last_filter.borrow() != filter {
                *last_filter.borrow_mut() = filter.clone();
                scroll_handle.set_offset(gpui_kit::point(px(0.), px(0.)));
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
                .gap_1()
                .w_full()
                .child(Input::new(&search).w_full());
            if rows.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child("No connections yet — add one to run this statement."),
                );
            } else if shown.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(format!("No matches for “{filter}”.",)),
                );
            } else {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child("Type to filter, Enter runs on the first match."),
                );
            }
            // Cap + scroll: long connection lists overflow the dialog.
            // Explicit handle + overflow_y_scroll (NOT the Scrollable
            // wrapper, whose caller-id keying misbehaves for content that
            // rebuilds every render). Rows hang directly off the scroll
            // area (not a nested column) so tracked item indices address
            // rows — the rewind-on-type below targets item 0, the first row.
            let scroll_handle = scroll_handle.clone();
            let mut scroll_body = div()
                .id("conn-pick-scroll")
                .w_full()
                .max_h(px(400.))
                .overflow_y_scroll()
                .track_scroll(&scroll_handle)
                .flex()
                .flex_col()
                .gap_1();
            for (rix, r) in shown.iter().enumerate() {
                let pick_view = view.clone();
                let pick_tab = tab_id.clone();
                let pick_sql = sql.clone();
                let conn_id = r.id.clone();
                let mut line = h_flex()
                    .gap_2()
                    .items_center()
                    .w_full()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|this| this.bg(muted.opacity(0.15)));
                line = line.child(
                    v_flex()
                        .flex_1()
                        .child(div().text_sm().child(r.name.clone()))
                        .child(div().text_xs().text_color(muted).child(r.detail.clone())),
                );
                if let Some(tag) = env_tag(r.env, cx) {
                    line = line.child(tag);
                }
                scroll_body = scroll_body.child(
                    div()
                        .id(("conn-pick", rix))
                        .test_support()
                        .flex_shrink_0()
                        .w_full()
                        .child(line)
                        .on_click(move |_, window, cx: &mut App| {
                            pick_connection_and_run(
                                &pick_view, &pick_tab, &pick_sql, &conn_id, window, cx,
                            );
                        }),
                );
            }
            body = body.child(scroll_body);
            let ok_view = view.clone();
            let ok_rows = rows.clone();
            let ok_search = search_in.clone();
            let ok_tab = tab_id.clone();
            let ok_sql = sql.clone();
            let cancel_view = view.clone();
            let cancel_tab = tab_id.clone();
            let esc_view = view.clone();
            let esc_tab = tab_id.clone();
            let mut footer = h_flex()
                .gap_2()
                .child(div().flex_1())
                // Footer buttons sit after the search field in Tab order.
                // The dialog X is hidden (Esc cancels).
                .child(
                    Button::new("pick-cancel")
                        .label("Cancel")
                        .tab_index(100)
                        .on_click(move |_, window, cx: &mut App| {
                            cancel_view
                                .update(cx, |this, cx| {
                                    this.pending_pick = None;
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
                        .tab_index(101)
                        .on_click(move |_, window, cx: &mut App| {
                            window.close_dialog(cx);
                            add_view
                                .update(cx, |this, cx| {
                                    this.pending_pick = None;
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
                // Enter runs on the first filtered match. False keeps the
                // dialog open (no matches); the close is manual so a
                // variables dialog opened below lands on a clean stack.
                .on_ok(move |_, window, cx: &mut App| {
                    let needle = ok_search.read(cx).value().to_string().to_lowercase();
                    let pick = ok_rows.iter().find(|r| {
                        needle.trim().is_empty()
                            || r.name.to_lowercase().contains(&needle)
                            || r.detail.to_lowercase().contains(&needle)
                    });
                    match pick {
                        Some(row) => {
                            window.close_dialog(cx);
                            ok_view
                                .update(cx, |this, cx| {
                                    if let Some(t) = this.tab_by_id(&ok_tab) {
                                        t.connection_id = Some(row.id.clone());
                                    }
                                    this.pending_pick = None;
                                    this.persist_tabs();
                                    this.start_run(&ok_tab, ok_sql.clone(), window, cx);
                                })
                                .ok();
                            false
                        }
                        None => false,
                    }
                })
                // Escape cancels: drop the deferred run and refocus.
                .on_cancel(move |_, window, cx: &mut App| {
                    esc_view
                        .update(cx, |this, cx| {
                            this.pending_pick = None;
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

    /// Variables dialog: one blank field per `&name` / `:name` (always blank,
    /// no memory). Builder-safe: everything the dialog renders is cloned in —
    /// the builder never touches the view entity (see env-tag crash fix).
    fn open_bind_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_bind.clone() else {
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
        let preview: SharedString = {
            let flat: String = pending.sql.split_whitespace().collect::<Vec<_>>().join(" ");
            const CAP: usize = 200;
            if flat.len() > CAP {
                format!("{}…", flat.chars().take(CAP).collect::<String>()).into()
            } else {
                flat.into()
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
                            this.pending_bind = None;
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
                                        this.pending_bind = None;
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

    /// Run-button handler: stores `&&` values in the session defines, applies
    /// substitution, and launches the run with native binds.
    fn submit_bind_dialog(
        &mut self,
        sub_values: std::collections::HashMap<String, String>,
        bind_values: Vec<BindParam>,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.pending_bind.take() else {
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
            self.run_sql(&pending.tab_id, final_sql, bind_values, cx);
        }
    }

    fn run_sql(
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
        let Some(cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            self.tabs[ix].output = Some(Output::error("Connection not found — pick another"));
            cx.notify();
            return;
        };
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
                    let mut session = session_bg.lock().expect("session lock");
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
                            cap: FETCH_CAP,
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
                            cap: FETCH_CAP,
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
    fn cancel_run(&mut self, tab_id: &str, cx: &mut Context<Self>) {
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
    fn cancel_export(&mut self, tab_id: &str, cx: &mut Context<Self>) {
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
    fn start_export(
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
    fn begin_export_drain(
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
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    export_drain_blocking(
                        &session, &fetch, query_id, &columns, fmt, &path, &sheet, &sql, &cancel,
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
    fn finish_export(
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
    fn mark_siblings_exhausted(&self, tab_id: &str, session: &Arc<Mutex<OracledbSession>>) {
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
    fn clear_pending(&mut self, conn_id: &str) {
        for t in &mut self.tabs {
            if t.connection_id.as_deref() == Some(conn_id) {
                t.pending_txn = false;
            }
        }
    }

    /// Commit the active tab's connection. Clears the pending flag on every
    /// tab sharing that connection (transactions are session-scoped).
    fn commit_now(&mut self, cx: &mut Context<Self>) {
        self.finish_txn(true, cx);
    }

    /// Roll back the active tab's connection (same scope rules as commit).
    fn rollback_now(&mut self, cx: &mut Context<Self>) {
        self.finish_txn(false, cx);
    }

    fn finish_txn(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(conn_id) = self.active_tab().connection_id.clone() else {
            self.tabs[self.active].output = Some(Output::error("Select a connection for this tab"));
            cx.notify();
            return;
        };
        let session = self.pool.get_or_create(&conn_id);
        let session_bg = session.clone();
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    let mut session = session_bg.lock().expect("session lock");
                    if commit {
                        session.commit().map_err(|e| e.to_string())
                    } else {
                        session.rollback().map_err(|e| e.to_string())
                    }
                })
                .await;
            view.update(cx, |this, cx| {
                match outcome {
                    Ok(()) => {
                        this.clear_pending(&conn_id);
                        this.tabs[this.active].result_meta =
                            (if commit { "Committed" } else { "Rolled back" }).into();
                    }
                    Err(msg) => {
                        this.tabs[this.active].output = Some(Output::error(msg));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn format_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.active;
        let raw = self.tabs[ix].editor.read(cx).value().to_string();
        let formatted = format_sql(&raw);
        self.tabs[ix].editor.update(cx, |editor, cx| {
            editor.set_value(formatted, window, cx);
        });
        cx.notify();
    }

    // -- Autocomplete --------------------------------------------------------

    /// Build popup items for `text` at byte `offset`: returns
    /// (items, word_start, prefix). Pure snapshot read — safe from provider
    /// tasks and the manual shortcut alike. Empty items = no popup.
    /// Gating lives here (not the trigger) because only this path owns the
    /// buffer text; the trigger sees just the typed fragment. `force`
    /// (manual shortcut) skips the 2-char gate but never trivia.
    fn completion_items_for(
        &mut self,
        tab_id: &str,
        text: &str,
        offset: usize,
        force: bool,
    ) -> (Vec<lsp_types::CompletionItem>, usize, String) {
        let empty = (Vec::new(), offset, String::new());
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return empty;
        };
        let conn_id = tab.connection_id.clone();
        if is_trivia_position(text, offset) {
            return empty;
        }
        let (prefix, word_start) = word_prefix(text, offset);
        // 2-char gate; a trailing dot forces the column list even empty, and
        // an empty prefix is allowed right after operand-expecting keywords
        // (`FROM |` lists tables immediately — the space-trigger path).
        if !force && prefix.len() < 2 {
            let dot_forced =
                word_start > 0 && text[..word_start.min(text.len())].trim_end().ends_with('.');
            if !dot_forced && !(prefix.is_empty() && allows_empty_prefix(text, offset)) {
                return empty;
            }
        }
        // Snapshot the cache (clone the Arc; never lock the session here).
        // Missing/stale cache kicks a background refresh; this request
        // completes from keywords + whatever is cached.
        let cache = conn_id.as_deref().and_then(|id| self.meta.get(id)).cloned();
        let stale = cache
            .as_ref()
            .map(|c| c.lock().map(|c| c.is_stale()).unwrap_or(true))
            .unwrap_or(true);
        if stale && conn_id.is_some() {
            // Bound tab with a cold cache: a refresh is possible (runs,
            // connects, and the manual trigger all call ensure_meta), so
            // the notice is honest. Unbound tabs skip it — no connection
            // means no refresh could ever clear it (stuck-status bug).
            self.status = "Loading suggestions…".into();
        }
        let stmt = statement_at(text, offset).unwrap_or_else(|| text.to_string());
        let aliases = build_alias_map(&stmt);
        // JOIN … ON with a fresh condition short-circuits everything else:
        // FK-derived conditions or no popup at all.
        let head_end = offset.min(text.len());
        if let Some((right_alias, right)) = detect_join_on(&text[..head_end], &aliases) {
            let fks: Vec<ForeignKey> = cache
                .as_ref()
                .and_then(|c| c.lock().ok().map(|c| c.fks.clone()))
                .unwrap_or_default();
            let cands = join_condition_candidates(&right_alias, &right, &aliases, &fks);
            if !cands.is_empty() {
                return (
                    Self::to_items(cands, text, word_start, offset),
                    word_start,
                    prefix,
                );
            }
            // No FK links the pair: fall through to Predicate (columns for a
            // hand-written condition) instead of an empty popup.
        }
        let is_seq = |n: &str| {
            cache
                .as_ref()
                .is_some_and(|c| c.lock().map(|c| c.is_sequence(n)).unwrap_or(false))
        };
        let ctx = classify_context(text, offset, &is_seq);
        let show_system = self.show_system;
        let mut cands: Vec<Candidate> = Vec::new();
        let usage_of = |conn: &Option<String>, label: &str| {
            conn.as_ref()
                .and_then(|id| {
                    self.usage
                        .get(&(id.clone(), label.to_ascii_uppercase()))
                        .copied()
                })
                .unwrap_or(0)
        };
        // Own schema (connected user): its objects rank above the shared
        // catalog, so a DBA login still sees their own tables first.
        let own_schema: String = conn_id
            .as_deref()
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
            .map(|c| c.user.clone())
            .unwrap_or_default();
        // Client-side mirror of the SQL filter (cache may predate a toggle,
        // or hold system rows from an unfiltered fetch): hide system owners
        // except the connected user's own schema.
        let hide_system = |owner: &str| {
            !show_system && !owner.eq_ignore_ascii_case(&own_schema) && is_system_schema(owner)
        };
        // In-scope tables as ScopeTables: single resolution shared by
        // column completion (with ambiguity info) and qualifier detail.
        // Deterministic alias order.
        let mut scope_order: Vec<String> = aliases.keys().cloned().collect();
        scope_order.sort();
        let scope_tables: Vec<ScopeTable> = if let Some(cache) = &cache {
            let cache = cache.lock().expect("meta lock");
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for alias in &scope_order {
                let Some(tref) = aliases.get(alias) else {
                    continue;
                };
                let key = (
                    tref.owner.clone().unwrap_or_default().to_ascii_uppercase(),
                    tref.name.to_ascii_uppercase(),
                );
                if !seen.insert(key) {
                    continue;
                }
                let cols = cache.columns_for(tref.owner.as_deref(), &tref.name);
                out.push(ScopeTable {
                    owner: tref.owner.clone(),
                    table: tref.name.clone(),
                    cols,
                });
            }
            out
        } else {
            Vec::new()
        };
        // Shared builders: columns of in-scope tables, keyword lists,
        // function skeletons. Each context composes only what SQL allows.
        let column_detail = |col: &crate::metadata::ColumnMeta, scope_name: &str| -> String {
            let mut d = if col.data_type.is_empty() {
                "COLUMN".to_string()
            } else {
                col.data_type.clone()
            };
            d.push_str(" · ");
            d.push_str(scope_name);
            let c = short_comment(&col.comments);
            if !c.is_empty() {
                d.push_str(" — ");
                d.push_str(&c);
            }
            d
        };
        let push_scope_columns = |cands: &mut Vec<Candidate>| {
            let ambiguous = ambiguous_columns(&scope_tables);
            for t in &scope_tables {
                for col in &t.cols {
                    // Collision across scope tables: qualify so the insert
                    // is unambiguous SQL (`e.DEPTNO`, never bare `DEPTNO`).
                    let (label, owner_out) = if ambiguous.contains(&col.name.to_ascii_uppercase()) {
                        match scope_label(&t.owner, &t.table, &aliases) {
                            Some(scoped) => (format!("{scoped}.{}", col.name), t.owner.clone()),
                            None => (col.name.clone(), t.owner.clone()),
                        }
                    } else {
                        (col.name.clone(), t.owner.clone())
                    };
                    cands.push(Candidate {
                        label,
                        detail: column_detail(col, &t.table),
                        kind: CandidateKind::ColumnInScope,
                        owner: owner_out,
                        usage: usage_of(&conn_id, &col.name),
                    });
                }
            }
        };
        let push_keywords = |cands: &mut Vec<Candidate>, kws: &[&str]| {
            for kw in kws {
                cands.push(Candidate {
                    label: kw.to_string(),
                    detail: "KEYWORD".to_string(),
                    kind: CandidateKind::Keyword,
                    owner: None,
                    usage: 0,
                });
            }
        };
        let push_functions = |cands: &mut Vec<Candidate>| {
            for (name, sig) in ORACLE_FUNCTIONS {
                cands.push(Candidate {
                    label: function_insert(name),
                    detail: sig.to_string(),
                    kind: CandidateKind::Function,
                    owner: None,
                    usage: usage_of(&conn_id, name),
                });
            }
        };
        let push_sequences = |cands: &mut Vec<Candidate>| {
            let Some(cache) = &cache else {
                return;
            };
            let cache = cache.lock().expect("meta lock");
            for s in &cache.sequences {
                if hide_system(&s.owner) {
                    continue;
                }
                cands.push(Candidate {
                    label: s.name.clone(),
                    detail: format!("SEQUENCE · {}", s.owner),
                    kind: CandidateKind::Sequence,
                    owner: Some(s.owner.clone()),
                    usage: usage_of(&conn_id, &s.name),
                });
            }
        };
        match &ctx {
            // Handled above via detect_join_on — unreachable here.
            CompleteContext::JoinOn { .. } => {}
            CompleteContext::SequenceMember(_) => {
                for kw in ["NEXTVAL", "CURRVAL"] {
                    cands.push(Candidate {
                        label: kw.to_string(),
                        detail: "SEQUENCE".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                    });
                }
            }
            CompleteContext::ColumnOf(q) => {
                if let Some(tref) = resolve_qualifier(q, &aliases) {
                    let cols = cache.as_ref().map(|c| {
                        let c = c.lock().expect("meta lock");
                        c.columns_for(tref.owner.as_deref(), &tref.name)
                    });
                    if let Some(cols) = cols {
                        for col in cols {
                            cands.push(Candidate {
                                label: col.name.clone(),
                                detail: column_detail(&col, &tref.name),
                                kind: CandidateKind::ColumnInScope,
                                owner: tref.owner.clone(),
                                usage: usage_of(&conn_id, &col.name),
                            });
                        }
                    }
                }
            }
            CompleteContext::StatementStart => {
                push_keywords(&mut cands, STMT_KEYWORDS);
            }
            CompleteContext::AfterFrom => {
                // Tables only — keywords never follow FROM.
                if let Some(cache) = &cache {
                    let cache = cache.lock().expect("meta lock");
                    for t in &cache.tables {
                        if hide_system(&t.owner) {
                            continue;
                        }
                        let label = format!("{}.{}", t.owner, t.name);
                        cands.push(Candidate {
                            label,
                            detail: "TABLE".to_string(),
                            kind: CandidateKind::Table,
                            owner: Some(t.owner.clone()),
                            usage: usage_of(&conn_id, &t.name),
                        });
                    }
                }
            }
            CompleteContext::OwnerTables(owner) => {
                // `FROM owner.|` — that owner's tables, bare names.
                if let Some(cache) = &cache {
                    let cache = cache.lock().expect("meta lock");
                    for t in &cache.tables {
                        if !t.owner.eq_ignore_ascii_case(owner) {
                            continue;
                        }
                        cands.push(Candidate {
                            label: t.name.clone(),
                            detail: format!("TABLE · {}", t.owner),
                            kind: CandidateKind::Table,
                            owner: Some(t.owner.clone()),
                            usage: usage_of(&conn_id, &t.name),
                        });
                    }
                }
            }
            CompleteContext::SelectList => {
                push_scope_columns(&mut cands);
                push_functions(&mut cands);
                push_sequences(&mut cands);
                push_keywords(&mut cands, EXPR_KEYWORDS);
                push_keywords(&mut cands, SELECT_FOLLOW);
            }
            CompleteContext::Predicate => {
                push_scope_columns(&mut cands);
                push_functions(&mut cands);
                push_sequences(&mut cands);
                push_keywords(&mut cands, PRED_KEYWORDS);
                push_keywords(&mut cands, PRED_FOLLOW);
            }
            CompleteContext::BareWord => {
                // Ambiguous position: keywords + functions, minus the
                // function names (which complete as call skeletons below).
                for kw in ORACLE_KEYWORDS {
                    if ORACLE_FUNCTIONS.iter().any(|(n, _)| n == kw) {
                        continue;
                    }
                    cands.push(Candidate {
                        label: kw.to_string(),
                        detail: "KEYWORD".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                    });
                }
                push_functions(&mut cands);
            }
        }
        let ranked = rank_candidates(&prefix, cands, &own_schema, COMPLETE_LIMIT);
        if ranked.is_empty() {
            return empty;
        }
        (
            Self::to_items(ranked, text, word_start, offset),
            word_start,
            prefix,
        )
    }

    /// Map ranked candidates to popup items with explicit edit ranges (see
    /// the sticky-`trigger_start_offset` note in `completion_items_for`).
    fn to_items(
        ranked: Vec<Candidate>,
        text: &str,
        word_start: usize,
        offset: usize,
    ) -> Vec<lsp_types::CompletionItem> {
        // Explicit edit range per item: the kit falls back to its sticky
        // `trigger_start_offset` otherwise, which survives across accepts
        // and replaces the whole buffer on the second completion.
        let (s_line, s_char) = byte_to_lsp_pos(text, word_start);
        let (e_line, e_char) = byte_to_lsp_pos(text, offset);
        ranked
            .into_iter()
            .enumerate()
            .map(|(ix, c)| lsp_types::CompletionItem {
                label: c.label.clone(),
                detail: Some(c.detail),
                kind: Some(match c.kind {
                    CandidateKind::JoinCondition => lsp_types::CompletionItemKind::SNIPPET,
                    CandidateKind::ColumnInScope | CandidateKind::Column => {
                        lsp_types::CompletionItemKind::FIELD
                    }
                    CandidateKind::Table => lsp_types::CompletionItemKind::CLASS,
                    CandidateKind::Sequence => lsp_types::CompletionItemKind::VALUE,
                    CandidateKind::Function => lsp_types::CompletionItemKind::FUNCTION,
                    CandidateKind::Keyword => lsp_types::CompletionItemKind::KEYWORD,
                }),
                sort_text: Some(format!("{ix:04}")),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range: lsp_types::Range {
                        start: lsp_types::Position {
                            line: s_line,
                            character: s_char,
                        },
                        end: lsp_types::Position {
                            line: e_line,
                            character: e_char,
                        },
                    },
                    new_text: insert_text_for(c.kind, &c.label),
                })),
                ..Default::default()
            })
            .collect()
    }

    /// Fetch (or refresh) the dictionary cache for a connection on the
    /// background executor. No-op when fresh or already loading. Safe to
    /// call from any run/connect completion or the manual trigger.
    fn ensure_meta(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        let cache = self
            .meta
            .entry(conn_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(MetadataCache::default())))
            .clone();
        let stale = cache
            .lock()
            .map(|c| c.is_stale() && !c.loading)
            .unwrap_or(false);
        if !stale {
            return;
        }
        let Some(cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            return;
        };
        if let Ok(mut c) = cache.lock() {
            c.loading = true;
        }
        self.status = "Loading suggestions…".into();
        cx.notify();
        let session = self.pool.get_or_create(conn_id);
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        let conn_bg = conn_id.to_string();
        // Snapshot the filter: a toggle mid-fetch must not mix scopes.
        // The connected user's own schema is always exempt server-side.
        let include_system = self.show_system;
        let own_schema = cfg.user.clone();
        cx.spawn(async move |_, cx| {
            // Per-dictionary outcomes: one failing query must never nuke
            // the rest (a broken FK query once emptied every cache).
            struct Parts {
                tables: Result<Vec<crate::metadata::TableId>, String>,
                columns: Result<
                    std::collections::HashMap<(String, String), Vec<crate::metadata::ColumnMeta>>,
                    String,
                >,
                sequences: Result<Vec<crate::metadata::TableId>, String>,
                fks: Result<Vec<crate::complete::ForeignKey>, String>,
            }
            let outcome = bg
                .spawn(async move {
                    let mut s = session.lock().expect("session lock");
                    if !s.is_connected() {
                        if let Err(e) = s.connect(&cfg).map_err(|e| e.to_string()) {
                            let e = e.to_string();
                            return Parts {
                                tables: Err(e.clone()),
                                columns: Err(e.clone()),
                                sequences: Err(e.clone()),
                                fks: Err(e),
                            };
                        }
                    }
                    Parts {
                        tables: fetch_tables_blocking(&mut *s, include_system, &own_schema)
                            .map_err(|e| e.to_string()),
                        columns: fetch_columns_blocking(&mut *s, include_system, &own_schema)
                            .map_err(|e| e.to_string()),
                        sequences: fetch_sequences_blocking(&mut *s, include_system, &own_schema)
                            .map_err(|e| e.to_string()),
                        fks: fetch_fks_blocking(&mut *s, include_system, &own_schema)
                            .map_err(|e| e.to_string()),
                    }
                })
                .await;
            view.update(cx, |this, cx| {
                let Some(cache) = this.meta.get(&conn_bg).cloned() else {
                    // Entry vanished mid-flight (connection deleted):
                    // never leave the loading notice up.
                    this.status = "".into();
                    cx.notify();
                    return;
                };
                if let Ok(mut c) = cache.lock() {
                    c.loading = false;
                    // Install each dictionary independently; anything that
                    // failed keeps its previous content (possibly empty).
                    // fetched_at advances on tables (the core set) so a
                    // partial failure retries next TTL, not every keystroke.
                    if let Ok(tables) = outcome.tables {
                        c.tables = tables;
                        c.fetched_at = Some(std::time::Instant::now());
                    }
                    if let Ok(columns) = outcome.columns {
                        c.columns = columns;
                    }
                    if let Ok(sequences) = outcome.sequences {
                        c.sequences = sequences;
                    }
                    if let Ok(fks) = outcome.fks {
                        c.fks = fks;
                    }
                    // Degrade silently: keywords + whatever is cached work.
                    this.status = "".into();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Manual trigger (ctrl-space): compute synchronously and present.
    /// Kicks a metadata refresh first when the cache is stale so the next
    /// keystroke completes from data.
    fn trigger_complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_focused(window, cx) {
            return;
        }
        let tab_id = self.active_tab().id.clone();
        let conn_id = self.active_tab().connection_id.clone();
        if let Some(id) = conn_id {
            self.ensure_meta(&id, cx);
        }
        let text = self.active_tab().editor.read(cx).value().to_string();
        let cursor = self.active_tab().editor.read(cx).cursor();
        let (items, start, prefix) = self.completion_items_for(&tab_id, &text, cursor, true);
        if items.is_empty() {
            return;
        }
        self.active_tab().editor.update(cx, |editor, cx| {
            editor.present_completion_items(start, prefix, items, cx);
        });
    }

    /// Bump usage counts for tables named in an executed statement so
    /// future rankings prefer working objects. Bounded: cleared past 5k.
    fn bump_usage(&mut self, conn_id: &str, sql: &str) {
        let map = build_alias_map(sql);
        if map.is_empty() {
            return;
        }
        for tref in map.values() {
            let key = (conn_id.to_string(), tref.name.to_ascii_uppercase());
            *self.usage.entry(key).or_insert(0) += 1;
        }
        if self.usage.len() > 5000 {
            self.usage.clear();
        }
    }

    /// Copy the grid selection to the clipboard: the most recently selected
    /// cell or row. Silent no-op with no selection.
    fn copy_selection(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let tab = self.active_tab();
        let text = {
            let table = tab.table.read(cx);
            let delegate = table.delegate();
            let cell = || {
                table
                    .selected_cell()
                    .and_then(|(r, c)| delegate.cell_text(r, c))
                    .map(|s| s.to_string())
            };
            let row = || table.selected_row().and_then(|r| delegate.row_csv(r));
            match tab.copy_sel {
                Some(CopySel::Cell) => cell(),
                Some(CopySel::Row) => row(),
                // Untracked (e.g. programmatic) selection: prefer cell, then row.
                None => cell().or_else(row),
            }
        };
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    // -- Render ---------------------------------------------------------------

    fn render_connection_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = &self.connections[ix];
        let is_live = self.live.contains(&cfg.id);
        let view = cx.entity().downgrade();
        let conn_id = cfg.id.clone();
        div()
            .id(("conn-row", ix))
            .w_full()
            .rounded_md()
            // Live rows get a success-tinted background so the active
            // connection reads at a glance, not just via the status bar.
            .when(is_live, |this| this.bg(cx.theme().success.opacity(0.12)))
            .hover(|this| this.bg(cx.theme().accent.opacity(0.5)))
            .context_menu(move |menu, _, _| {
                let connect_label = if is_live { "Disconnect" } else { "Connect" };
                let connect_icon = if is_live {
                    KitIcon::Unplug
                } else {
                    KitIcon::Plug
                };
                let connect_op = if is_live {
                    ConnMenuOp::Disconnect
                } else {
                    ConnMenuOp::Connect
                };
                menu.item(conn_menu_item(
                    connect_label,
                    connect_icon,
                    view.clone(),
                    conn_id.clone(),
                    connect_op,
                ))
                .item(conn_menu_item(
                    "Edit…",
                    KitIcon::SquarePen,
                    view.clone(),
                    conn_id.clone(),
                    ConnMenuOp::Edit,
                ))
                .separator()
                .item(conn_menu_item(
                    "Delete",
                    KitIcon::X,
                    view.clone(),
                    conn_id.clone(),
                    ConnMenuOp::Delete,
                ))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_stretch()
                    .px_2()
                    .py_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .text_color(cx.theme().muted_foreground)
                            .child(KitIcon::Database),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .justify_center()
                            // Truncate (not clip): long names collapse
                            // to an ellipsis when the pane shrinks instead of
                            // overflowing or pushing the layout.
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .text_sm()
                                            .truncate()
                                            .child(cfg.name.clone()),
                                    )
                                    .when_some(env_tag(cfg.environment, cx), |this, tag| {
                                        this.child(tag)
                                    }),
                            ),
                    )
                    // Status bar: the live indicator — success green when
                    // connected, faint border tone when idle.
                    .child(div().w(px(3.)).rounded_full().bg(if is_live {
                        cx.theme().success
                    } else {
                        cx.theme().border
                    })),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sidebar_collapsed {
            // Slim rail: connections stay visible (and status-readable) even
            // with the pane closed. Clicking anything here expands; all
            // connection actions live in the full pane's context menu.
            return v_flex()
                .w(px(44.))
                .h_full()
                .border_r_1()
                .border_color(cx.theme().border)
                .items_center()
                .gap_1()
                .child(
                    div()
                        .w_full()
                        .h(px(36.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Button::new("expand")
                                .icon(KitIcon::PanelLeftOpen)
                                .ghost()
                                .small()
                                .tooltip("Expand connections")
                                .on_click(cx.listener(Self::toggle_sidebar)),
                        ),
                )
                .child(
                    Button::new("rail-add")
                        .icon(KitIcon::Plus)
                        .ghost()
                        .small()
                        .tooltip("Add connection")
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.start_add(window, cx);
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .w_full()
                        .min_h_0()
                        .overflow_y_scrollbar()
                        .child(v_flex().w_full().items_center().gap_1().children(
                            self.connections.iter().enumerate().map(|(ix, cfg)| {
                                let is_live = self.live.contains(&cfg.id);
                                let view = cx.entity().downgrade();
                                let conn_id = cfg.id.clone();
                                div()
                                    .id(("conn-rail-wrap", ix))
                                    .context_menu(move |menu, _, _| {
                                        let connect_label =
                                            if is_live { "Disconnect" } else { "Connect" };
                                        let connect_icon = if is_live {
                                            KitIcon::Unplug
                                        } else {
                                            KitIcon::Plug
                                        };
                                        let connect_op = if is_live {
                                            ConnMenuOp::Disconnect
                                        } else {
                                            ConnMenuOp::Connect
                                        };
                                        menu.item(conn_menu_item(
                                            connect_label,
                                            connect_icon,
                                            view.clone(),
                                            conn_id.clone(),
                                            connect_op,
                                        ))
                                        .item(conn_menu_item(
                                            "Edit…",
                                            KitIcon::SquarePen,
                                            view.clone(),
                                            conn_id.clone(),
                                            ConnMenuOp::Edit,
                                        ))
                                        .separator()
                                        .item(
                                            conn_menu_item(
                                                "Delete",
                                                KitIcon::X,
                                                view.clone(),
                                                conn_id.clone(),
                                                ConnMenuOp::Delete,
                                            ),
                                        )
                                    })
                                    .child(
                                        Button::new(("conn-rail", ix))
                                            .icon(KitIcon::Database)
                                            .ghost()
                                            .small()
                                            .tooltip(format!(
                                                "{}{} — {}@{}/{}{}",
                                                cfg.environment
                                                    .label()
                                                    .map(|e| format!("[{e}] "))
                                                    .unwrap_or_default(),
                                                cfg.name,
                                                cfg.user,
                                                cfg.host,
                                                cfg.service_name,
                                                if is_live { " (connected)" } else { "" }
                                            ))
                                            .when(is_live, |b| {
                                                b.bg(cx.theme().success.opacity(0.15))
                                            })
                                            .on_click(cx.listener(Self::toggle_sidebar)),
                                    )
                            }),
                        )),
                )
                .child(
                    div()
                        .w_full()
                        .h(px(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            Button::new("rail-settings")
                                .icon(KitIcon::Settings)
                                .ghost()
                                .small()
                                .tooltip("Settings (⌘,)")
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.open_settings(window, cx);
                                })),
                        ),
                )
                .into_any_element();
        }

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .h(px(36.))
                    .gap_1()
                    .px_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_sm()
                            .truncate()
                            .child(format!("Connections ({})", self.connections.len())),
                    )
                    .child(
                        Button::new("collapse")
                            .icon(KitIcon::PanelLeftClose)
                            .ghost()
                            .small()
                            .on_click(cx.listener(Self::toggle_sidebar)),
                    ),
            )
            .child(
                div().w_full().px_1().pt_1().child(
                    div()
                        .id("add-connection-row")
                        .w_full()
                        .flex()
                        .items_center()
                        .gap_2()
                        .px_2()
                        .py_1()
                        .rounded_md()
                        .cursor_pointer()
                        .text_color(cx.theme().muted_foreground)
                        .hover(|this| this.bg(cx.theme().accent.opacity(0.5)))
                        .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.start_add(window, cx);
                        }))
                        .child(KitIcon::Plus)
                        .child(div().text_sm().child("Add New Connection")),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_w_0()
                    .overflow_y_scrollbar()
                    .p_1()
                    .child(if self.connections.is_empty() {
                        div()
                            .w_full()
                            .p_2()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("No connections yet — click + to add one.")
                            .into_any_element()
                    } else {
                        v_flex()
                            .gap_1()
                            .children(
                                (0..self.connections.len())
                                    .map(|ix| self.render_connection_row(ix, cx)),
                            )
                            .into_any_element()
                    }),
            )
            .child(
                div()
                    .w_full()
                    .h(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .px_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(
                        Button::new("settings-labeled")
                            .icon(KitIcon::Settings)
                            .ghost()
                            .small()
                            .label("Settings")
                            .tooltip("Settings (⌘,)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TabBar::new("query-tabs")
            .w_full()
            .selected_index(self.active)
            .track_scroll(&self.tab_scroll)
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.select_tab(*ix, window, cx);
            }))
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                let tab_id = tab.id.clone();
                let label = if tab.dirty {
                    format!("{} *", tab.name)
                } else {
                    tab.name.to_string()
                };
                Tab::new().label(label).selected(self.active == ix).suffix(
                    Button::new(("tab-close", ix))
                        .icon(KitIcon::X)
                        .ghost()
                        .small()
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.request_close_tab(&tab_id, window, cx);
                        })),
                )
            }))
            .suffix(
                Button::new("tab-add")
                    .icon(KitIcon::Plus)
                    .ghost()
                    .small()
                    .tooltip("New tab")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.add_tab(None, String::new(), window, cx);
                    })),
            )
    }

    fn render_connection_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab();
        let label = self.connection_name(&tab.connection_id);
        let live = tab
            .connection_id
            .as_deref()
            .map(|cid| self.live.contains(cid))
            .unwrap_or(false);
        let tab_env = tab
            .connection_id
            .as_deref()
            .and_then(|cid| self.connections.iter().find(|c| c.id == cid))
            .map(|c| c.environment)
            .unwrap_or(Environment::Untagged);
        let view = cx.entity().downgrade();
        let tab_id = tab.id.clone();
        let current_conn = tab.connection_id.clone();
        let connections = self.connections.clone();
        h_flex()
            .gap_1()
            .items_center()
            .child(div().size(px(8.)).rounded_full().bg(if live {
                cx.theme().success
            } else {
                cx.theme().muted_foreground
            }))
            .child(
                Button::new("conn-picker")
                    .outline()
                    .small()
                    .icon(KitIcon::Database)
                    .label(label)
                    .tooltip("Connection for this tab — click to change")
                    .dropdown_menu(move |menu, _, _| {
                        // Cap + scroll: long connection lists overflow the
                        // viewport otherwise.
                        let mut menu = menu.max_h(px(320.)).scrollable(true);
                        if connections.is_empty() {
                            return menu.item(PopupMenuItem::new(
                                "No connections — add one in the sidebar",
                            ));
                        }
                        for conn in &connections {
                            let current = current_conn.as_deref() == Some(conn.id.as_str());
                            let prefix = if current { "● " } else { "○ " };
                            let view = view.clone();
                            let tab_id = tab_id.clone();
                            let conn_id = conn.id.clone();
                            let name = conn.name.clone();
                            menu =
                                menu.item(PopupMenuItem::new(format!("{prefix}{name}")).on_click(
                                    move |_, _, cx| {
                                        view.update(cx, |this, cx| {
                                            if let Some(t) = this.tab_by_id(&tab_id) {
                                                t.connection_id = Some(conn_id.clone());
                                            }
                                            this.persist_tabs();
                                            cx.notify();
                                        })
                                        .ok();
                                    },
                                ));
                        }
                        menu
                    }),
            )
            .when_some(env_tag(tab_env, cx), |this, tag| this.child(tag))
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab();
        let editor = tab.editor.clone();
        let pending = tab.pending_txn;
        v_flex()
            .id("query-section")
            .size_full()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| {
                this.run_at_cursor(window, cx);
            }))
            .on_action(cx.listener(|this, _: &FormatQuery, window, cx| {
                if this.editor_focused(window, cx) {
                    this.format_now(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &CommitTxn, window, cx| {
                if this.editor_focused(window, cx) {
                    this.commit_now(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &TriggerComplete, window, cx| {
                this.trigger_complete(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RollbackTxn, window, cx| {
                if this.editor_focused(window, cx) {
                    this.rollback_now(cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .when(pending, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(div().size(px(8.)).rounded_full().bg(cx.theme().warning))
                                .child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().warning)
                                        .child("Uncommitted"),
                                ),
                        )
                    })
                    .child(
                        Button::new("run")
                            .primary()
                            .small()
                            .w(px(ACTION_BUTTON_W))
                            .icon(KitIcon::Play)
                            .label("Run")
                            .tooltip("Run statement at cursor (⌘↵)")
                            .loading(tab.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run_at_cursor(window, cx);
                            })),
                    )
                    .child(
                        Button::new("commit")
                            .success()
                            .small()
                            .w(px(ACTION_BUTTON_W))
                            .icon(KitIcon::Check)
                            .label("Commit")
                            .tooltip("Commit transaction (⇧⌘C)")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.commit_now(cx);
                            })),
                    )
                    .child(
                        Button::new("rollback")
                            .danger()
                            .small()
                            .w(px(ACTION_BUTTON_W))
                            .icon(KitIcon::Undo2)
                            .label("Rollback")
                            .tooltip("Roll back transaction (⇧⌘R)")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.rollback_now(cx);
                            })),
                    )
                    .child(
                        Button::new("format")
                            .secondary()
                            .small()
                            .w(px(ACTION_BUTTON_W))
                            .icon(KitIcon::WandSparkles)
                            .label("Format")
                            .tooltip("Format SQL (⇧⌥F)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.format_now(window, cx);
                            })),
                    )
                    .when(tab.busy || tab.exporting, |this| {
                        let tab_id = tab.id.clone();
                        let exporting = tab.exporting;
                        let tip = if exporting {
                            "Stop the export — the partial file is discarded"
                        } else {
                            "Stop waiting — the server finishes in the background and its results are discarded"
                        };
                        this.child(
                            Button::new("cancel-run")
                                .danger()
                                .small()
                                .w(px(ACTION_BUTTON_W))
                                .icon(KitIcon::X)
                                .label("Cancel")
                                .tooltip(tip)
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if exporting {
                                        this.cancel_export(&tab_id, cx);
                                    } else {
                                        this.cancel_run(&tab_id, cx);
                                    }
                                })),
                        )
                    })
                    .child(div().flex_1())
                    .child(self.render_connection_picker(cx)),
            )
            .child(
                div()
                    .min_h_0()
                    .flex_1()
                    .child(Editor::new(&editor).size_full()),
            )
    }

    fn render_results(&self, tab: &QueryTab, cx: &mut Context<Self>) -> impl IntoElement {
        // min_w_0 + overflow_hidden: without them flex items refuse to shrink
        // below the table's full content width, the table sees an unbounded
        // viewport, renders every column (no virtualization), and its
        // horizontal scrollbar never engages.
        if tab.output.is_some() {
            self.render_output_pane(tab, cx).into_any_element()
        } else {
            let view = cx.entity().downgrade();
            let tab_id = tab.id.clone();
            let exp_view = view.clone();
            let exp_tab = tab.id.clone();
            v_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(
                    h_flex().w_full().justify_end().px_2().pt_2().pb_1().child(
                        Button::new("export")
                            .outline()
                            .small()
                            .w(px(ACTION_BUTTON_W))
                            .icon(KitIcon::Download)
                            .label("Export")
                            .tooltip("Export all result rows to CSV or Excel")
                            .dropdown_menu(move |menu, _, _| {
                                let mut menu = menu.max_h(px(320.)).scrollable(true);
                                for fmt in [ExportFormat::Csv, ExportFormat::Xlsx] {
                                    let view = exp_view.clone();
                                    let tab_id = exp_tab.clone();
                                    menu = menu.item(PopupMenuItem::new(fmt.label()).on_click(
                                        move |_, window, cx| {
                                            view.update(cx, |this, cx| {
                                                this.start_export(&tab_id, fmt, window, cx);
                                            })
                                            .ok();
                                        },
                                    ));
                                }
                                menu
                            }),
                    ),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .p_2()
                        .context_menu(move |menu, _, _| {
                            let mut menu = menu;
                            for fmt in [ExportFormat::Csv, ExportFormat::Xlsx] {
                                let view = view.clone();
                                let tab_id = tab_id.clone();
                                menu = menu.item(PopupMenuItem::new(fmt.label()).on_click(
                                    move |_, window, cx| {
                                        view.update(cx, |this, cx| {
                                            this.start_export(&tab_id, fmt, window, cx);
                                        })
                                        .ok();
                                    },
                                ));
                            }
                            menu
                        })
                        .child(render_tab_table(&tab.table)),
                )
                .into_any_element()
        }
    }

    /// Output pane: replaces the grid with the tab's latest action outcome —
    /// failures (red) and non-query confirmations (neutral) alike. A new run
    /// clears the output and returns to the grid automatically; Dismiss
    /// reveals the previous results (if any) without re-running.
    fn render_output_pane(&self, tab: &QueryTab, cx: &mut Context<Self>) -> impl IntoElement {
        let output = tab.output.clone().unwrap_or(Output::info(""));
        let tab_id = tab.id.clone();
        let copy_text = output.text.clone();
        let select_id = tab.id.clone();
        let is_error = output.kind == OutputKind::Error;
        let (accent, title, icon, bg) = if is_error {
            (
                cx.theme().danger,
                "Query failed",
                KitIcon::TriangleAlert,
                cx.theme().muted,
            )
        } else {
            (
                cx.theme().success,
                "Statement executed",
                KitIcon::Check,
                cx.theme().muted,
            )
        };
        v_flex()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .p_2()
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_color(accent).child(icon))
                    .child(div().text_sm().text_color(accent).child(title))
                    .child(div().flex_1())
                    .child(
                        Button::new("output-copy")
                            .ghost()
                            .small()
                            .icon(KitIcon::Copy)
                            .label("Copy")
                            .tooltip("Copy the full message")
                            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    copy_text.to_string(),
                                ));
                            })),
                    )
                    .child(
                        Button::new("output-dismiss")
                            .ghost()
                            .small()
                            .icon(KitIcon::X)
                            .label("Dismiss")
                            .tooltip("Back to results")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                if let Some(t) = this.tab_by_id(&tab_id) {
                                    t.output = None;
                                }
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scrollbar()
                    .p_3()
                    .rounded_md()
                    .bg(bg)
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    // Drag-selectable message (Cmd+C via the window
                    // selection layer) plus the header Copy button.
                    .child(SelectableText::new(
                        format!("output-text-{select_id}"),
                        output.text.clone(),
                    )),
            )
    }

    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        let tab = self.active_tab();
        let fetching = tab
            .table
            .read(cx)
            .delegate()
            .with_data(|d| d.loading, false);
        let left = if tab.busy {
            match tab.run_started {
                Some(started) => format!("Running… {}s", started.elapsed().as_secs()),
                None => "Running…".to_string(),
            }
        } else if tab.exporting {
            format!("Exporting… {} rows", tab.export_rows)
        } else if fetching {
            "Fetching more…".to_string()
        } else {
            tab.result_meta.to_string()
        };
        let (dot, right) = match &tab.connection_id {
            Some(cid) => {
                let live = self.live.contains(cid);
                let name = self.connection_name(&Some(cid.clone()));
                (
                    if live {
                        cx.theme().success
                    } else {
                        cx.theme().muted_foreground
                    },
                    if live {
                        format!("Connected to {name}")
                    } else {
                        format!("{name} (not connected)")
                    },
                )
            }
            None => (cx.theme().muted_foreground, "No connection".to_string()),
        };
        h_flex()
            .gap_2()
            .px_2()
            .h(px(28.))
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .child(div().text_color(cx.theme().muted_foreground).child(left))
            .child(div().flex_1())
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.status.clone()),
            )
            .child(div().size(px(8.)).rounded_full().bg(dot))
            .child(div().text_color(cx.theme().muted_foreground).child(right))
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.active_tab();
        // Fresh tab (never ran, no output): editor takes the full height —
        // no empty bottom pane. Once anything runs, the resizable
        // editor/results split appears and stays.
        let fresh = !tab.has_result && tab.output.is_none();
        let content: AnyElement = if fresh {
            div()
                .flex_1()
                .min_h_0()
                .child(self.render_editor(cx))
                .into_any_element()
        } else {
            let body: AnyElement = match tab.output.is_some() {
                true => self.render_output_pane(tab, cx).into_any_element(),
                false => self.render_results(tab, cx).into_any_element(),
            };
            v_resizable("query-split")
                .child(
                    resizable_panel()
                        .size(px(300.))
                        .size_range(px(160.)..px(900.))
                        .flex_none()
                        .child(self.render_editor(cx)),
                )
                .child(body)
                .into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .on_action(cx.listener(|this, _: &CopySelection, window, cx| {
                this.copy_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                this.cycle_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                this.cycle_tab(-1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                let tab_id = this.active_tab().id.clone();
                this.request_close_tab(&tab_id, window, cx);
            }))
            .on_action(cx.listener(|this, _: &NewTab, window, cx| {
                let tab_id = this.add_tab(None, String::new(), window, cx);
                if let Some(ix) = this.tab_index(&tab_id) {
                    this.select_tab(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSql, window, cx| {
                this.open_sql_file(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSql, window, cx| {
                this.save_active_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSqlAs, window, cx| {
                this.save_active_tab_as(window, cx);
            }))
            .child(self.render_tab_bar(cx))
            .child(content)
            .child(self.render_status_bar(cx))
            .into_any_element()
    }

    fn toggle_sidebar(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }
}

fn render_tab_table(table: &Entity<TableState<ResultsDelegate>>) -> impl IntoElement {
    div()
        .size_full()
        .min_w_0()
        .overflow_hidden()
        .child(DataTable::new(table).xsmall())
}

/// Theme color for an environment, or `None` when untagged.
fn env_color(env: Environment, cx: &App) -> Option<Hsla> {
    match env {
        Environment::Untagged => None,
        Environment::Prod => Some(cx.theme().danger),
        Environment::Dev => Some(cx.theme().success),
        Environment::Qa => Some(cx.theme().warning),
        Environment::Uat => Some(cx.theme().info),
    }
}

/// Environment tag pill. Returns `None` for untagged connections so callers
/// can drop it into trees with `.when_some(...)`. Colors come from theme
/// tokens, so tags adapt to light/dark like everything else.
fn env_tag(env: Environment, cx: &App) -> Option<AnyElement> {
    let label = env.label()?;
    let color = env_color(env, cx)?;
    Some(
        div()
            .px_1()
            .rounded_md()
            .bg(color.opacity(0.15))
            .text_xs()
            .text_color(color)
            .child(label)
            .into_any_element(),
    )
}

fn dialog_field(
    label: impl Into<SharedString>,
    state: &Entity<InputState>,
    password: bool,
    muted: Hsla,
) -> impl IntoElement {
    let label: SharedString = label.into();
    let mut input = Input::new(state).w_full();
    if password {
        input = input.content_type(InputContentType::Password);
    }
    v_flex()
        .gap_1()
        .child(div().text_xs().text_color(muted).child(label))
        .child(input)
}

impl Render for SqlHighlandView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.sidebar_collapsed {
            h_flex()
                .size_full()
                .child(self.render_sidebar(cx))
                .child(self.render_main(cx))
                .into_any_element()
        } else {
            h_resizable("main-split")
                .child(
                    resizable_panel()
                        .size(px(264.))
                        .size_range(px(180.)..px(480.))
                        .flex_none()
                        .child(self.render_sidebar(cx)),
                )
                .child(self.render_main(cx))
                .into_any_element()
        };

        div()
            .size_full()
            // Keep the background in the dirty main view rather than on the
            // Root wrapper created once at startup. The wrapper's cached
            // background was why grid colors changed while transparent
            // sidebar/status regions stayed white until restart.
            .bg(cx.theme().background)
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| {
                this.open_settings(window, cx);
            }))
            .child(
                v_flex()
                    .size_full()
                    .child(TitleBar::new().child("SQLHighland"))
                    .child(div().flex_1().min_h_0().child(content)),
            )
            .children(Root::render_dialog_layer(window, cx))
    }
}
