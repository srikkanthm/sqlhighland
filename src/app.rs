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
    describe_target, detect_join_on, display_name, function_insert, hover_markdown, insert_text_for, is_system_schema,
    is_trivia_position, join_condition_candidates, qualifier_before, rank_candidates,
    resolve_qualifier, scope_label, short_comment, word_at, word_prefix, Candidate, CandidateKind, CompleteContext,
    ForeignKey, ScopeTable, EXPR_KEYWORDS, ORACLE_FUNCTIONS, ORACLE_KEYWORDS, PRED_FOLLOW,
    PRED_KEYWORDS, SELECT_FOLLOW, STMT_KEYWORDS,
};
use crate::config::{CompleteMode, Preferences, SavedConfig, SavedTab, TabsManifest, THEME_LIST};
use crate::db::{
    is_describe_statement, BindParam, DbClient, FetchPage, OracledbSession, FETCH_CHUNK,
};
use crate::export::{csv_header_line_with, csv_line_with, sheet_name, XlsxBuilder};
use crate::filetab::{self, FileStamp};
use crate::metadata::{
    fetch_columns_blocking, fetch_fks_blocking, fetch_sequences_blocking, fetch_tables_blocking,
    MetadataCache, SharedCache,
};
use crate::model::{
    csv_row, tab_name_from_sql, ColumnInfo, ConnectionConfig, Environment, OracleRole, PasswordMode,
    ServiceKind,
};
use crate::schema::{DbEngine, OracleProvider, SchemaProvider as _};
use crate::session::SessionPool;
use crate::sql::{
    apply_substitutions, exec_summary, find_bind_vars, find_substitution_vars, format_sql, is_dml,
    statement_at, statement_at_range, statement_kind, txn_end, StatementKind, SubVar,
};
use gpui_kit::base::SelectableText;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    CompletionProvider, DefinitionProvider, Editor, EditorState, HoverProvider, Input,
    InputContentType, InputEvent, InputState,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::switch::Switch;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::scroll::{Scrollbar, ScrollbarMode};
use gpui_kit::component::setting::{
    RenderOptions, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tree::{tree, TreeEvent, TreeItem, TreeState};
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
        PickConnection,
        RebindConnection,
        OpenSql,
        SaveSql,
        SaveSqlAs,
        TriggerComplete
    ]
);

/// Max popup rows per request (ranking already orders them best-first).
const COMPLETE_LIMIT: usize = 100;

/// Oracle definition provider for one tab: Cmd-hover underlines a table
/// word, Cmd-click jumps to its DESCRIBE output. Table-only (v1, same
/// rule as the hover table cards). Resolution is snapshot-only like the
/// other providers; the actual DESCRIBE run happens in the
/// `show_document` host hook below, which owns a `Context` + `Window`.
struct OracleDefiner {
    view: WeakEntity<SqlHighlandView>,
    tab_id: String,
}

impl DefinitionProvider for OracleDefiner {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let snapshot = text.to_string();
        let out = self.view.update(cx, |this, _| {
            let tab = this.tabs.iter().find(|t| t.id == self.tab_id)?;
            let conn_id = tab.connection_id.clone();
            if is_trivia_position(&snapshot, offset) {
                return None;
            }
            let (word, start) = word_at(&snapshot, offset);
            if word.is_empty() {
                return None;
            }
            let stmt = statement_at(&snapshot, offset).unwrap_or_else(|| snapshot.clone());
            let aliases = build_alias_map(&stmt);
            let qualifier = qualifier_before(&snapshot, start);
            let cache = conn_id
                .as_deref()
                .and_then(|id| this.meta.get(id))
                .cloned()?;
            let cache = cache.lock().ok()?;
            let tgt = describe_target(
                &word,
                qualifier.as_deref(),
                &aliases,
                &cache,
                this.show_system,
                &this.own_schema_of(&conn_id),
            );
            let (owner, table) = tgt?;
            let owner = owner?;
            // Single slash: `oracle-describe:/OWNER/TABLE` keeps both
            // segments in the path. (`://` would parse OWNER as the
            // authority/host, leaving the path with TABLE only.)
            let uri: lsp_types::Uri = format!("oracle-describe:/{owner}/{table}").parse().ok()?;
            let (sl, sc) = byte_to_lsp_pos(&snapshot, start);
            let (el, ec) = byte_to_lsp_pos(&snapshot, start + word.len());
            let origin = lsp_types::Range {
                start: lsp_types::Position {
                    line: sl,
                    character: sc,
                },
                end: lsp_types::Position {
                    line: el,
                    character: ec,
                },
            };
            let zero = lsp_types::Position {
                line: 0,
                character: 0,
            };
            Some(lsp_types::LocationLink {
                origin_selection_range: Some(origin),
                target_uri: uri,
                target_range: lsp_types::Range {
                    start: zero,
                    end: zero,
                },
                target_selection_range: lsp_types::Range {
                    start: zero,
                    end: zero,
                },
            })
        });
        match out {
            Ok(Some(link)) => Task::ready(Ok(vec![link])),
            _ => Task::ready(Ok(vec![])),
        }
    }
}

/// Oracle hover provider for one tab: table cards (columns) and column
/// cards (type/table/comment) from the cached dictionary. Snapshot-only —
/// same entity-lease rule as completions: the editor is mutably leased
/// along the hover path, so only the `&Rope` plus view-owned state.
struct OracleHover {
    view: WeakEntity<SqlHighlandView>,
    tab_id: String,
}

impl HoverProvider for OracleHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<lsp_types::Hover>>> {
        let snapshot = text.to_string();
        let out = self.view.update(cx, |this, _| {
            let tab = this.tabs.iter().find(|t| t.id == self.tab_id)?;
            let conn_id = tab.connection_id.clone();
            if is_trivia_position(&snapshot, offset) {
                return None;
            }
            let (word, start) = word_at(&snapshot, offset);
            if word.is_empty() {
                return None;
            }
            let stmt =
                statement_at(&snapshot, offset).unwrap_or_else(|| snapshot.clone());
            let aliases = build_alias_map(&stmt);
            let qualifier = qualifier_before(&snapshot, start);
            let cache = conn_id.as_deref().and_then(|id| this.meta.get(id)).cloned()?;
            let md = {
                let c = cache.lock().ok()?;
                hover_markdown(
                    &word,
                    qualifier.as_deref(),
                    &aliases,
                    &c,
                    this.show_system,
                    &this.own_schema_of(&conn_id),
                )
            }?;
            Some(lsp_types::Hover {
                contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                    kind: lsp_types::MarkupKind::Markdown,
                    value: md,
                }),
                range: None,
            })
        });
        match out {
            Ok(Some(h)) => Task::ready(Ok(Some(h))),
            _ => Task::ready(Ok(None)),
        }
    }
}

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
                ConnMenuOp::Connect => this.connect_connection(&conn_id, window, cx),
                ConnMenuOp::Disconnect => this.disconnect_connection(&conn_id, cx),
                ConnMenuOp::Edit => {
                    if let Some(ix) = this.connection_index(&conn_id) {
                        this.start_edit(ix, window, cx);
                    }
                }
                ConnMenuOp::Delete => {
                    if let Some(ix) = this.connection_index(&conn_id) {
                        this.confirm_delete_connection(ix, window, cx);
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
    kind: TabKind,
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

/// Tab flavor: a full SQL editor, or an object viewer (DESCRIBE grid,
/// no editor) opened from the schema browser. Viewers are ephemeral and
/// reuse the run/results pipeline. The editor entity is kept but
/// unrendered for viewers — every viewer branch is a marked `TabKind`
/// check, so a future `Option<editor>` refactor is compiler-guided.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TabKind {
    Query,
    Viewer {
        owner: String,
        name: String,
        kind: crate::metadata::TableKind,
    },
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
/// What picking a connection does once chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PickAfter {
    /// Classic unbound-run mode: bind the tab and run its statement.
    Run,
    /// Cmd+K mode: open a NEW tab bound to the pick (no run).
    NewTab,
    /// Shift+Cmd+K mode: rebind the ACTIVE tab to the pick (no run).
    Rebind,
}

#[derive(Debug, Clone)]
struct PendingPick {
    tab_id: String,
    sql: String,
    after: PickAfter,
}

/// Password prompt in flight: which connection, and the run to resume
/// afterwards (`None` = plain connect from the sidebar/tree).
struct PendingPassword {
    conn_id: String,
    run: Option<(String, String)>,
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

/// Bind a NEW tab to the picked connection (Cmd+K flow): closes the
/// picker, creates + selects a blank tab bound to conn_id, focuses its
/// editor. Never runs anything (contrast pick_connection_and_run, which
/// binds the pending tab and resumes its deferred statement).
fn pick_connection_for_new_tab(
    view: &WeakEntity<SqlHighlandView>,
    conn_id: &str,
    window: &mut Window,
    cx: &mut App,
) {
    // Close the picker first so focus lands cleanly below.
    window.close_dialog(cx);
    let new_id = view
        .update(cx, |this, cx| {
            let tab_id = this.add_tab(Some(conn_id.to_string()), String::new(), window, cx);
            if let Some(ix) = this.tab_index(&tab_id) {
                this.select_tab(ix, window, cx);
            }
            this.pending_pick = None;
            this.persist_tabs();
            // Establish the session now so the tab is live before the
            // first run (failures surface through the standard
            // connect-failed path, same as sidebar Connect).
            this.connect_connection(conn_id, window, cx);
            tab_id
        })
        .ok();
    if let Some(tab_id) = new_id {
        focus_tab_editor(view, &tab_id, window, cx);
    }
}

/// Rebind the ACTIVE tab to the picked connection (Shift+Cmd+K flow):
/// closes the picker, swaps the tab's binding, connects eagerly, and
/// refocuses its editor. No new tab, no run (contrast the siblings
/// above/below).
fn pick_connection_for_rebind(
    view: &WeakEntity<SqlHighlandView>,
    tab_id: &str,
    conn_id: &str,
    window: &mut Window,
    cx: &mut App,
) {
    // Close the picker first so focus lands cleanly below.
    window.close_dialog(cx);
    view.update(cx, |this, cx| {
        if let Some(t) = this.tab_by_id(tab_id) {
            t.connection_id = Some(conn_id.to_string());
        }
        this.pending_pick = None;
        this.persist_tabs();
        // Same eager session as a fresh Cmd+K tab (failures surface
        // through the standard connect-failed path).
        this.connect_connection(conn_id, window, cx);
    })
    .ok();
    // Outside the update above: reading the leased view here would panic.
    focus_tab_editor(view, tab_id, window, cx);
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
    /// Same pattern for role / service-kind / SSL / password-mode rows.
    pending_role: OracleRole,
    pending_service_kind: ServiceKind,
    pending_ssl: bool,
    pending_password_mode: PasswordMode,
    /// Pending database engine for the open connection dialog. Only
    /// Oracle exists today, so the row is display-only — but the dialog
    /// owns the value like role/kind, ready for a second pill.
    pending_engine: DbEngine,
    /// Dialog open counter + the counter value when Settings opened.
    /// Cmd+, toggles Settings off only when no other dialog opened since
    /// (top must be Settings); otherwise Settings stacks on top instead
    /// of closing whatever is showing. Cells: every open site has only
    /// &self in some cases (dialog builders re-run every render).
    dialog_seq: std::cell::Cell<u64>,
    settings_seq: std::cell::Cell<Option<u64>>,
    /// Password field value when a Keychain-mode dialog opened (None
    /// otherwise). Keychain mode saves only *typed* changes: an untouched
    /// blank field keeps the stored entry instead of deleting it.
    password_snapshot: Option<String>,
    // Dialog form fields (entities persist across dialog open/close).
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    service: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    /// Password prompt field (Ask mode / Keychain miss). Cleared on submit.
    pwd_prompt: Entity<InputState>,
    /// Transient notice for the status bar ("Saved X", "Connecting…").
    status: SharedString,
    /// `&&name` values defined this session, per connection id. Once defined,
    /// even `&name` reuses the value without prompting (SQL*Plus parity).
    defines: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// Run waiting on the bind dialog (cleared on submit or cancel).
    pending_bind: Option<PendingBind>,
    /// Run waiting on the connection picker (cleared on pick or cancel).
    pending_pick: Option<PendingPick>,
    /// Session-unlocked passwords, per connection id. Memory only, never
    /// persisted: Ask mode and Keychain-miss prompts land here, and every
    /// connect/run path prefers them over whatever is stored.
    unlocked: std::collections::HashMap<String, String>,
    /// Password prompt in flight (cleared on submit or cancel). `run` is
    /// set when the prompt gates a query run rather than a plain connect.
    pending_password: Option<PendingPassword>,
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
    /// Schema-browser trees, per connection id. Trees appear under their
    /// connection row, independent of the active tab — expand warms the
    /// dictionary via `ensure_meta`, and its completion hook rebuilds.
    browser_open: std::collections::HashSet<String>,
    /// Expanded tree node ids per connection (`s:{schema}`,
    /// `g:{schema}/{group}`, `o:{schema}/{T|V|S}/{object}`); the source of
    /// truth reapplied on every rebuild (filter/cache refresh), fed by
    /// `TreeEvent`s. Per-connection so identical schemas don't mirror.
    browser_expanded: std::collections::HashMap<String, std::collections::HashSet<String>>,
    browser_trees: std::collections::HashMap<String, Entity<TreeState>>,
    /// Client-side tree filter, one input per open connection (entity
    /// persists while open; Change rebuilds that connection's tree).
    browser_filters: std::collections::HashMap<String, Entity<InputState>>,
    /// Window-lifetime subscriptions (OS appearance observer for System
    /// theme mode). Kept alive by ownership, like per-tab `_subs`.
    _subs: Vec<Subscription>,
}

impl SqlHighlandView {
    /// Every view-level dialog/alert open funnels through here so Cmd+,
    /// can tell whether Settings is the top dialog (toggle off) or
    /// something else opened since (stack Settings on top). Cells: several
    /// open sites only hold &self. (The async connect-failed alert has no
    /// view borrow and skips it — worst case there is the old toggle
    /// behavior for that one transient popup.)
    fn note_dialog_open(&self) {
        self.dialog_seq.set(self.dialog_seq.get() + 1);
    }

    pub fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty = self.tabs.iter().any(|tab| tab.dirty && tab.path.is_some());
        if !dirty {
            cx.quit();
            return;
        }
        let view = cx.entity().downgrade();
        self.note_dialog_open();
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
        let password =
            cx.new(|cx| InputState::new(window, cx).placeholder("password").masked(true));
        let pwd_prompt = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("password for this session")
                .masked(true)
        });

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
        // Cmd+K picks a connection, then opens a new tab bound to it.
        // No kit Input binding uses cmd-k (checked), so it fires from
        // the editor, the grid, and dialogs alike.
        cx.bind_keys([KeyBinding::new("cmd-k", PickConnection, None)]);
        // Shift+Cmd+K rebinds the ACTIVE tab instead of opening one.
        // Same freedom: no kit binding uses it.
        cx.bind_keys([KeyBinding::new("cmd-shift-k", RebindConnection, None)]);
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
        // Refresh the native menu now that every binding exists: AppKit
        // resolves key equivalents from the keymap snapshot at set_menus
        // time, and main.rs runs before these bindings are registered
        // (leaving menu items without shown shortcuts). Idempotent rebuild;
        // re-call here if bindings are ever registered later than this.
        cx.set_menus(app_menus());

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
            pending_role: OracleRole::default(),
            pending_service_kind: ServiceKind::default(),
            pending_ssl: false,
            pending_password_mode: PasswordMode::default(),
            pending_engine: DbEngine::default(),
            dialog_seq: std::cell::Cell::new(0),
            settings_seq: std::cell::Cell::new(None),
            password_snapshot: None,
            name,
            host,
            port,
            service,
            user,
            password,
            pwd_prompt,
            status: "".into(),
            defines: std::collections::HashMap::new(),
            pending_bind: None,
            pending_pick: None,
            unlocked: std::collections::HashMap::new(),
            pending_password: None,
            meta: std::collections::HashMap::new(),
            usage: std::collections::HashMap::new(),
            complete_auto: prefs.completion == CompleteMode::Auto,
            show_system: prefs.show_system_schemas,
            browser_open: std::collections::HashSet::new(),
            browser_expanded: std::collections::HashMap::new(),
            browser_trees: std::collections::HashMap::new(),
            browser_filters: std::collections::HashMap::new(),
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
        kind: TabKind,
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
            let hover = OracleHover {
                view: cx.entity().downgrade(),
                tab_id: id.clone(),
            };
            let definer = OracleDefiner {
                view: cx.entity().downgrade(),
                tab_id: id.clone(),
            };
            // Definition jump: Cmd-click a table word runs DESCRIBE for it.
            // The provider answers with an `oracle-describe:/OWNER/TABLE`
            // location; this host hook performs the run and reports handled.
            // Unknown schemes fall through to the kit default.
            let view = cx.entity().downgrade();
            let jump_tab_id = id.clone();
            let show_document: Rc<
                dyn Fn(&lsp_types::ShowDocumentParams, &mut Window, &mut App) -> bool,
            > = Rc::new(move |params, window, cx| {
                if !params.uri.scheme().is_some_and(|s| s.as_str() == "oracle-describe") {
                    return false;
                }
                let mut segs = params.uri.path().as_str().split('/').filter(|s| !s.is_empty());
                let (Some(owner), Some(table)) = (segs.next(), segs.next()) else {
                    return false;
                };
                let tab_id = jump_tab_id.clone();
                view.update(cx, |this, cx| {
                    // Engine-owned statement (Oracle: DESCRIBE, bare for own
                    // schema). The provider — not the UI — knows the dialect.
                    let sql = match this.tab_by_id(&tab_id).and_then(|t| t.connection_id.clone()) {
                        Some(cid) => {
                            let own = this.own_schema_of(&Some(cid));
                            // Cmd-click jumps resolve tables only.
                            OracleProvider.describe_sql(
                                owner,
                                table,
                                &own,
                                crate::metadata::TableKind::Table,
                            )
                        }
                        None => format!("DESCRIBE {owner}.{table}"),
                    };
                    this.start_run(&tab_id, sql, window, cx);
                })
                .ok();
                true
            });
            editor.update(cx, |editor, _| {
                editor.lsp_mut().completion_provider =
                    Some(Rc::new(completer) as Rc<dyn CompletionProvider>);
                editor.lsp_mut().hover_provider = Some(Rc::new(hover) as Rc<dyn HoverProvider>);
                editor.lsp_mut().definition_provider =
                    Some(Rc::new(definer) as Rc<dyn DefinitionProvider>);
                editor.lsp_mut().show_document = Some(show_document);
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
            kind,
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
            self.make_tab(saved.id, name, connection_id, text, TabKind::Query, window, cx);
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
            self.make_tab(id, name, None, text, TabKind::Query, window, cx);
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
        self.make_tab(id.clone(), name, connection_id, text.clone(), TabKind::Query, window, cx);
        // Persist immediately so a crash before the first keystroke loses nothing.
        let _ = TabsManifest::write_draft(&id, &text);
        self.persist_tabs();
        self.active = self.tabs.len() - 1;
        self.tab_scroll.scroll_to_item(self.active);
        cx.notify();
        id
    }

    /// Open (or focus) an object-viewer tab for a schema-browser object
    /// and run its DESCRIBE. Viewer tabs are ephemeral editor-less grids.
    fn open_viewer(
        &mut self,
        conn_id: &str,
        owner: String,
        name: String,
        kind: crate::metadata::TableKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ix) = self.tabs.iter().position(|t| {
            t.connection_id.as_deref() == Some(conn_id)
                && matches!(&t.kind, TabKind::Viewer{ owner: o, name: n, kind: k }
                    if o == &owner && n == &name && k == &kind)
        }) {
            self.select_tab(ix, window, cx);
            return;
        }
        let own = self.own_schema_of(&Some(conn_id.to_string()));
        let title = OracleProvider.object_title(&owner, &name, &own);
        let id = uuid::Uuid::new_v4().to_string();
        self.make_tab(
            id.clone(),
            title,
            Some(conn_id.to_string()),
            String::new(),
            TabKind::Viewer {
                owner: owner.clone(),
                name: name.clone(),
                kind,
            },
            window,
            cx,
        );
        self.active = self.tabs.len() - 1;
        self.tab_scroll.scroll_to_item(self.active);
        let sql = OracleProvider.describe_sql(&owner, &name, &own, kind);
        self.start_run(&id, sql, window, cx);
        cx.notify();
    }

    /// Re-run the DESCRIBE behind a viewer tab (its Refresh button).
    /// No-op for query tabs and unknown tab ids.
    fn refresh_viewer(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (conn_id, owner, name, kind) = match self.tabs.iter().find(|t| t.id == tab_id) {
            Some(t) => match (&t.connection_id, &t.kind) {
                (Some(cid), TabKind::Viewer { owner, name, kind }) => {
                    (cid.clone(), owner.clone(), name.clone(), *kind)
                }
                _ => return,
            },
            None => return,
        };
        let own = self.own_schema_of(&Some(conn_id));
        let sql = OracleProvider.describe_sql(&owner, &name, &own, kind);
        self.start_run(tab_id, sql, window, cx);
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
                self.make_tab(id, name, None, String::new(), TabKind::Query, window, cx);
            }
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            self.persist_tabs();
            self.tab_scroll.scroll_to_item(self.active);
            // The closed editor may still own keyboard focus. Move focus to
            // the replacement active tab so repeated shortcuts keep working.
            // Viewers have no visible editor: leave focus alone.
            if matches!(self.tabs[self.active].kind, TabKind::Query) {
                let editor = self.tabs[self.active].editor.clone();
                editor.update(cx, |editor, cx| editor.focus(window, cx));
            }
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
        self.note_dialog_open();
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
        // Viewers have no visible editor: don't steal focus.
        if matches!(self.tabs[ix].kind, TabKind::Query) {
            let editor = self.tabs[ix].editor.clone();
            editor.update(cx, |editor, cx| editor.focus(window, cx));
            self.check_external_change(ix, window, cx);
        }
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
        self.note_dialog_open();
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
                // Viewer tabs are ephemeral: never persisted.
                .filter(|t| matches!(t.kind, TabKind::Query))
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
        // Viewers have no editor text to save.
        if !matches!(self.active_tab().kind, TabKind::Query) {
            return;
        }
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
        // Viewers have no editor text to save.
        if !matches!(self.active_tab().kind, TabKind::Query) {
            return;
        }
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
                    this.make_tab(id, name, None, text, TabKind::Query, window, cx);
                    if let Some(tab) = this.tabs.last_mut() {
                        tab.path = Some(path);
                        tab.file_stamp = Some(stamp);
                        tab.dirty = false;
                    }
                    let ix = this.tabs.len() - 1;
                    this.select_tab(ix, window, cx);
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
                            .map(Some)
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
        let edited = self.editing.and_then(|ix| self.connections.get(ix));
        let id = edited.map(|c| c.id.clone()).unwrap_or_default();
        // The dialog owns the engine like role/kind (today always Oracle).
        // Keychain/Ask modes never persist the typed secret in the file —
        // save_from_dialog routes it to the keychain (or drops it).
        let password = match self.pending_password_mode {
            PasswordMode::File => self.password.read(cx).value().to_string(),
            PasswordMode::Keychain | PasswordMode::Ask => String::new(),
        };
        ConnectionConfig {
            id,
            name: self.name.read(cx).value().to_string(),
            host: self.host.read(cx).value().to_string(),
            port: self.port.read(cx).value().parse().unwrap_or(1521),
            service_name: self.service.read(cx).value().to_string(),
            user: self.user.read(cx).value().to_string(),
            password,
            environment: self.pending_env,
            engine: self.pending_engine,
            role: self.pending_role,
            service_kind: self.pending_service_kind,
            ssl: self.pending_ssl,
            password_mode: self.pending_password_mode,
        }
    }

    fn fill_form(&mut self, cfg: &ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
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
        // Never fill stored secrets back into the form: File mode shows
        // its (legacy) value; Keychain/Ask always start blank.
        let shown_password = match cfg.password_mode {
            PasswordMode::File => cfg.password.clone(),
            PasswordMode::Keychain | PasswordMode::Ask => String::new(),
        };
        // Keychain mode with a stored entry says so: a blank field
        // otherwise reads as "no password". Saving it untouched keeps
        // the entry (see save_from_dialog); typing replaces it.
        let pw_hint: SharedString = if cfg.password_mode == PasswordMode::Keychain
            && !cfg.id.is_empty()
            && crate::keychain::get(&cfg.id).ok().flatten().is_some_and(|s| !s.is_empty())
        {
            "Saved in keychain".into()
        } else {
            "password".into()
        };
        self.password_snapshot = match cfg.password_mode {
            PasswordMode::Keychain => Some(shown_password.clone()),
            _ => None,
        };
        self.password.update(cx, |s, cx| {
            s.set_value(shown_password, &mut *window, cx);
            s.set_placeholder(pw_hint, &mut *window, cx);
        });
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
        self.pending_role = OracleRole::default();
        self.pending_service_kind = ServiceKind::default();
        self.pending_ssl = false;
        self.pending_password_mode = PasswordMode::default();
        self.pending_engine = DbEngine::default();
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
        self.pending_role = cfg.role;
        self.pending_service_kind = cfg.service_kind;
        self.pending_ssl = cfg.ssl;
        self.pending_password_mode = cfg.password_mode;
        self.pending_engine = cfg.engine;
        self.fill_form(&cfg, window, cx);
        self.open_connection_dialog(&title, window, cx);
    }

    /// App-global entry points (main.rs): the deferred bodies behind
    /// Cmd+T / Cmd+W. Same calls the removed element listeners made;
    /// public only for main.rs.
    pub fn new_tab_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = self.add_tab(None, String::new(), window, cx);
        if let Some(ix) = self.tab_index(&tab_id) {
            self.select_tab(ix, window, cx);
        }
    }

    /// App-global entry for Cmd+W: closes whatever is active when the
    /// deferred body runs (not when the key was pressed).
    pub fn close_active_tab_command(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let tab_id = self.active_tab().id.clone();
        self.request_close_tab(&tab_id, window, cx);
    }

    /// Settings dialog: theme family + appearance mode. Selections apply
    /// live, persist to preferences.toml, and close the dialog (menu-like).
    /// Owns the Cmd+, toggle bookkeeping (direct field access: this runs
    /// under the action listener's lease, so Entity::update would panic).
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.take_settings_toggle(window, cx) {
            return;
        }
        Self::open_settings_dialog(&cx.entity(), window, cx);
    }

    /// Cmd+, toggle decision shared by the view-level entry (above) and the
    /// global App::on_action entry in main.rs (which mirrors it inside a
    /// view.update — safe there, no outer lease). Returns true when Settings
    /// is the top dialog and has just been dismissed: the caller opens
    /// nothing. Otherwise records the new opening and the caller stacks
    /// Settings on top, so the key always opens it instead of killing
    /// another popup. Needs &mut Window only for has_active_dialog; the
    /// App borrow is unused (kept for call-site symmetry).
    fn take_settings_toggle(&mut self, window: &mut Window, cx: &mut App) -> bool {
        let top = self.note_dialog_open_for_settings(window.has_active_dialog(cx));
        if top {
            window.close_dialog(cx);
        }
        top
    }

    /// Core toggle step for the global entry point (main.rs), which cannot
    /// use take_settings_toggle for lack of &mut self. Same contract.
    /// Public only for main.rs; not part of the app's UI surface.
    pub fn note_dialog_open_for_settings(&mut self, active: bool) -> bool {
        let top_is_settings =
            self.settings_seq.get() == Some(self.dialog_seq.get()) && active;
        self.dialog_seq.set(self.dialog_seq.get() + 1);
        if top_is_settings {
            self.settings_seq.set(None);
        } else {
            self.settings_seq.set(Some(self.dialog_seq.get()));
        }
        top_is_settings
    }

    /// Settings dialog as an associated function so the global App::on_action
    /// handler (which has a window but no view handle) can open it too.
    /// Pure open: toggle bookkeeping lives with the callers (see above).
    /// Rows apply, notify the view for a full re-render (GPUI only repaints
    /// dirty views — window refreshes alone reuse cached ones), then close.
    pub fn open_settings_dialog(view: &Entity<SqlHighlandView>, window: &mut Window, cx: &mut App) {
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
            // Reloaded on every rebuild so switches follow live prefs.
            let current_mode = Preferences::load().completion;
            let show_system = Preferences::load().show_system_schemas;
            let complete_view = view.clone();
            let system_view = view.clone();
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
                                    vec![SettingItem::render(theme_list).keywords([
                                        "theme",
                                        "appearance",
                                        "color",
                                        "dark",
                                        "light",
                                    ])],
                                )]),
                            SettingPage::new("Editor")
                                .icon(KitIcon::SquarePen)
                                .groups(vec![SettingGroup::new()
                                    .title("Suggestions")
                                    .items(vec![
                                        SettingItem::render(move |_, _, _| {
                                            let auto_view = complete_view.clone();
                                            div()
                                                .id("settings-complete-toggle")
                                                .w_full()
                                                .p_2()
                                                .rounded_md()
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .items_center()
                                                        .child(
                                                            v_flex().flex_1()
                                                                .child(
                                                                    div().text_sm().child(
                                                                        "Automatic suggestions",
                                                                    ),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(muted)
                                                                        .child(
                                                                            "Popup while typing (off: Ctrl+Space only)",
                                                                        ),
                                                                ),
                                                        )
                                                        .child(
                                                            Switch::new("settings-complete-auto")
                                                                .small()
                                                                .checked(
                                                                    current_mode
                                                                        == CompleteMode::Auto,
                                                                )
                                                                .on_change(
                                                                    move |checked, _, cx| {
                                                                        let apply = if *checked {
                                                                            CompleteMode::Auto
                                                                        } else {
                                                                            CompleteMode::Manual
                                                                        };
                                                                        auto_view
                                                                            .update(
                                                                                cx,
                                                                                |this, cx| {
                                                                                    let mut prefs =
                                                                                        Preferences::load(
                                                                                        );
                                                                                    prefs.completion =
                                                                                        apply;
                                                                                    let _ =
                                                                                        prefs.save();
                                                                                    this.complete_auto =
                                                                                        apply
                                                                                            == CompleteMode::Auto;
                                                                                    cx.notify();
                                                                                },
                                                                            );
                                                                    },
                                                                ),
                                                        ),
                                                )
                                        })
                                        .keywords([
                                            "completion",
                                            "suggest",
                                            "automatic",
                                            "manual",
                                            "typing",
                                            "popup",
                                            "shortcut",
                                        ]),
                                        SettingItem::render(move |_, _, _| {
                                            let system_switch_view = system_view_outer.clone();
                                            v_flex().gap_1().child(
                                                div()
                                                    .id("settings-system-schemas")
                                                    .w_full()
                                                    .p_2()
                                                    .rounded_md()
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
                                                            .child(
                                                                Switch::new("settings-system")
                                                                    .small()
                                                                    .checked(system_selected)
                                                                    .on_change(
                                                                        move |checked, _, cx| {
                                                                            system_switch_view
                                                                                .update(
                                                                                    cx,
                                                                                    |this, cx| {
                                                                                        let mut prefs =
                                                                                            Preferences::load(
                                                                                            );
                                                                                        prefs.show_system_schemas =
                                                                                            *checked;
                                                                                        let _ = prefs
                                                                                            .save(
                                                                                            );
                                                                                        this.show_system =
                                                                                            prefs.show_system_schemas;
                                                                                        // Scope changed: drop caches so
                                                                                        // the next trigger refetches
                                                                                        // with the new filter.
                                                                                        for cache in this
                                                                                            .meta
                                                                                            .values(
                                                                                            )
                                                                                        {
                                                                                            if let Ok(mut c) =
                                                                                                cache.lock(
                                                                                                )
                                                                                            {
                                                                                                c.fetched_at =
                                                                                                    None;
                                                                                            }
                                                                                        }
                                                                                        cx.notify();
                                                                                    },
                                                                                );
                                                                        },
                                                                    ),
                                                            ),
                                                    ),
                                            )
                                        })
                                        .keywords([
                                            "system",
                                            "schemas",
                                            "sys",
                                            "hidden",
                                            "filter",
                                        ]),
                                    ]),
                                    SettingGroup::new().title("Font").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            let current = Preferences::load().font_family;
                                            let rows = [
                                                ("", "Theme default"),
                                                ("SF Mono", "SF Mono"),
                                                ("Menlo", "Menlo"),
                                                ("JetBrains Mono", "JetBrains Mono"),
                                                ("Fira Code", "Fira Code"),
                                            ];
                                            v_flex().gap_1().children(rows.into_iter().enumerate().map(
                                                |(ix, (value, label))| {
                                                    let value = value.to_string();
                                                    let ids = [
                                                        "settings-font-default",
                                                        "settings-font-sf",
                                                        "settings-font-menlo",
                                                        "settings-font-jb",
                                                        "settings-font-fira",
                                                    ];
                                                    settings_pick_row(
                                                        ids[ix],
                                                        label.to_string(),
                                                        None,
                                                        current == value,
                                                        cx,
                                                        move |_, window, cx| {
                                                            let mut prefs =
                                                                Preferences::load();
                                                            prefs.font_family = value.clone();
                                                            let _ = prefs.save();
                                                            crate::guitheme::apply_font_prefs(
                                                                &prefs, cx,
                                                            );
                                                            window.refresh();
                                                        },
                                                    )
                                                },
                                            ))
                                        })
                                        .keywords(["font", "family", "mono", "typeface"]),
                                        SettingItem::render(move |_, _, cx| {
                                            let size = Preferences::load().font_size;
                                            div()
                                                .id("settings-font-size")
                                                .w_full()
                                                .p_2()
                                                .rounded_md()
                                                .child(
                                                    h_flex()
                                                        .gap_2()
                                                        .items_center()
                                                        .child(
                                                            v_flex().flex_1()
                                                                .child(
                                                                    div().text_sm().child("Size"),
                                                                )
                                                                .child(
                                                                    div()
                                                                        .text_xs()
                                                                        .text_color(
                                                                            cx.theme()
                                                                                .muted_foreground,
                                                                        )
                                                                        .child(
                                                                            "Editor text size in points",
                                                                        ),
                                                                ),
                                                        )
                                                        .child(
                                                            Button::new("settings-font-minus")
                                                                .label("−")
                                                                .small()
                                                                .on_click(
                                                                    move |_,
                                                                          window,
                                                                          cx| {
                                                                        let mut prefs =
                                                                            Preferences::load();
                                                                        prefs.font_size = prefs
                                                                            .font_size
                                                                            .saturating_sub(1)
                                                                            .max(10);
                                                                        let _ = prefs.save();
                                                                        crate::guitheme::apply_font_prefs(
                                                                            &prefs, cx,
                                                                        );
                                                                        window.refresh();
                                                                    },
                                                                ),
                                                        )
                                                        .child(
                                                            div()
                                                                .text_sm()
                                                                .w(px(28.))
                                                                .text_center()
                                                                .child(size.to_string()),
                                                        )
                                                        .child(
                                                            Button::new("settings-font-plus")
                                                                .label("+")
                                                                .small()
                                                                .on_click(
                                                                    move |_,
                                                                          window,
                                                                          cx| {
                                                                        let mut prefs =
                                                                            Preferences::load();
                                                                        prefs.font_size = prefs
                                                                            .font_size
                                                                            .saturating_add(1)
                                                                            .min(24);
                                                                        let _ = prefs.save();
                                                                        crate::guitheme::apply_font_prefs(
                                                                            &prefs, cx,
                                                                        );
                                                                        window.refresh();
                                                                    },
                                                                ),
                                                        ),
                                                )
                                        })
                                        .keywords(["font", "size", "text"]),
                                    ]),
                                ]),
                            SettingPage::new("Results")
                                .icon(KitIcon::Table)
                                .groups(vec![
                                    SettingGroup::new().title("Row limit").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            const CAPS: &[(usize, &str)] = &[
                                                (10_000, "10,000"),
                                                (50_000, "50,000"),
                                                (100_000, "100,000"),
                                                (500_000, "500,000"),
                                                (1_000_000, "1,000,000"),
                                            ];
                                            let current =
                                                Preferences::load().result_cap;
                                            let ids = [
                                                "settings-cap-0",
                                                "settings-cap-1",
                                                "settings-cap-2",
                                                "settings-cap-3",
                                                "settings-cap-4",
                                            ];
                                            v_flex().gap_1().children(CAPS.iter().enumerate().map(
                                                |(ix, (n, label))| {
                                                    let n = *n;
                                                    settings_pick_row(
                                                        ids[ix],
                                                        format!("{label} rows"),
                                                        None,
                                                        current == n,
                                                        cx,
                                                        move |_, window, _| {
                                                            let mut prefs =
                                                                Preferences::load();
                                                            prefs.result_cap = n;
                                                            let _ = prefs.save();
                                                            window.refresh();
                                                        },
                                                    )
                                                },
                                            ))
                                        })
                                        .keywords([
                                            "results", "limit", "rows", "cap", "grid",
                                        ]),
                                    ]),
                                    SettingGroup::new().title("Query timeout").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            const OPTS: &[(u64, &str)] = &[
                                                (30, "30 seconds"),
                                                (60, "1 minute"),
                                                (120, "2 minutes"),
                                                (300, "5 minutes"),
                                                (0, "Unlimited"),
                                            ];
                                            let current =
                                                Preferences::load().query_timeout_secs;
                                            let ids = [
                                                "settings-timeout-0",
                                                "settings-timeout-1",
                                                "settings-timeout-2",
                                                "settings-timeout-3",
                                                "settings-timeout-4",
                                            ];
                                            v_flex().gap_1().children(OPTS.iter().enumerate().map(
                                                |(ix, (n, label))| {
                                                    let n = *n;
                                                    settings_pick_row(
                                                        ids[ix],
                                                        label.to_string(),
                                                        None,
                                                        current == n,
                                                        cx,
                                                        move |_, window, _| {
                                                            let mut prefs =
                                                                Preferences::load();
                                                            prefs.query_timeout_secs = n;
                                                            let _ = prefs.save();
                                                            window.refresh();
                                                        },
                                                    )
                                                },
                                            ))
                                        })
                                        .keywords([
                                            "query", "timeout", "seconds", "slow",
                                            "cancel", "stuck", "hang",
                                        ]),
                                    ]),
                                    SettingGroup::new().title("CSV export").items(vec![
                                        SettingItem::render(move |_, _, cx| {
                                            const DELIMS: &[(&str, &str)] = &[
                                                (",", "Comma"),
                                                (";", "Semicolon"),
                                                ("\t", "Tab"),
                                                ("|", "Pipe"),
                                            ];
                                            let current =
                                                Preferences::load().csv_delimiter.clone();
                                            let ids = [
                                                "settings-delim-0",
                                                "settings-delim-1",
                                                "settings-delim-2",
                                                "settings-delim-3",
                                            ];
                                            v_flex().gap_1().children(
                                                DELIMS.iter().enumerate().map(
                                                    |(ix, (value, label))| {
                                                        let value = value.to_string();
                                                        settings_pick_row(
                                                            ids[ix],
                                                            label.to_string(),
                                                            None,
                                                            current == value,
                                                            cx,
                                                            move |_, window, _| {
                                                                let mut prefs =
                                                                    Preferences::load();
                                                                prefs.csv_delimiter =
                                                                    value.clone();
                                                                let _ = prefs.save();
                                                                window.refresh();
                                                            },
                                                        )
                                                    },
                                                ),
                                            )
                                        })
                                        .keywords([
                                            "export", "csv", "delimiter", "separator",
                                        ]),
                                        SettingItem::render(move |_, _, cx| {
                                            v_flex().gap_1().child(
                                                div()
                                                    .id("settings-csv-header")
                                                    .w_full()
                                                    .p_2()
                                                    .rounded_md()
                                                    .child(
                                                        h_flex()
                                                            .gap_2()
                                                            .items_center()
                                                            .child(
                                                                v_flex().flex_1()
                                                                    .child(
                                                                        div().text_sm().child(
                                                                            "Header row",
                                                                        ),
                                                                    )
                                                                    .child(
                                                                        div()
                                                                            .text_xs()
                                                                            .text_color(
                                                                                cx.theme()
                                                                                    .muted_foreground,
                                                                            )
                                                                            .child(
                                                                                "First line holds column names",
                                                                            ),
                                                                    ),
                                                            )
                                                            .child(
                                                                Switch::new("settings-csv-header-sw")
                                                                    .small()
                                                                    .checked(
                                                                        Preferences::load()
                                                                            .csv_header,
                                                                    )
                                                                    .on_change(
                                                                        move |checked, window, _| {
                                                                            let mut prefs =
                                                                                Preferences::load(
                                                                                );
                                                                            prefs.csv_header =
                                                                                *checked;
                                                                            let _ =
                                                                                prefs.save();
                                                                            window.refresh();
                                                                        },
                                                                    ),
                                                            ),
                                                    ),
                                            )
                                        })
                                        .keywords(["export", "csv", "header", "columns"]),
                                    ]),
                                ]),
                            SettingPage::new("About")
                                .icon(KitIcon::Info)
                                .groups(vec![SettingGroup::new().title("About").items(vec![
                                    SettingItem::render(move |_, _, _| {
                                        v_flex().gap_1().child(
                                            div().text_sm().child(format!(
                                                "SQLHighland {} — SQL database client",
                                                env!("CARGO_PKG_VERSION")
                                            )),
                                        )
                                    })
                                    .keywords(["about", "version"]),
                                    SettingItem::render(move |_, _, _| {
                                        v_flex().gap_1().child(
                                            div()
                                                .text_xs()
                                                .text_color(muted)
                                                .child(
                                                    "Oracle support today, built on a provider \
                                                     architecture for more databases. Rust + GPUI \
                                                     with the official thin driver (no Oracle \
                                                     Client required).",
                                                ),
                                        )
                                    })
                                    .keywords(["about", "database", "oracle", "driver"]),
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
                                    })
                                    .keywords(["about", "paths", "files", "config"]),
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
        // Same pattern for role / service-kind / SSL / password-mode rows.
        let role_cell: Rc<RefCell<OracleRole>> = Rc::new(RefCell::new(self.pending_role));
        let kind_cell: Rc<RefCell<ServiceKind>> =
            Rc::new(RefCell::new(self.pending_service_kind));
        let ssl_cell: Rc<RefCell<bool>> = Rc::new(RefCell::new(self.pending_ssl));
        let pwmode_cell: Rc<RefCell<PasswordMode>> =
            Rc::new(RefCell::new(self.pending_password_mode));
        // Same pattern for the engine row (today a single Oracle pill).
        let engine_cell: Rc<RefCell<DbEngine>> =
            Rc::new(RefCell::new(self.pending_engine));
        // Owned scroll handle: the form now spans role/service/SSL/password
        // rows, so small windows overflow. Explicit handle + overflow_y_scroll
        // (NOT the Scrollable wrapper, whose caller-id keying misbehaves for
        // dialog content that rebuilds every render).
        let scroll_handle = Rc::new(ScrollHandle::new());
        self.note_dialog_open();
        window.open_dialog(cx, move |dialog, _, cx| {
            let save_view = view.clone();
            let pending = pending_cell.clone();
            let muted = cx.theme().muted_foreground;
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
                                .gap_2()
                                .w_full()
                                // Gutter for the overlaid scrollbar track
                                // (16px): without it the thumb sits on top
                                // of the full-width inputs.
                                .pr_5()
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_xs().text_color(muted).child("Database type"))
                                        .child({
                                            let cell = engine_cell.clone();
                                            let current = *cell.borrow();
                                            let row_view = view.clone();
                                            dialog_pills(
                                                "conn-engine",
                                                &[(DbEngine::Oracle, DbEngine::Oracle.label())],
                                                current,
                                                Rc::new(move |e, cx: &mut App| {
                                                    *cell.borrow_mut() = e;
                                                    row_view
                                                        .update(cx, |this, cx| {
                                                            this.pending_engine = e;
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                }),
                                                cx
                                            )
                                        })
                                )
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
                                        .child(dialog_field(
                                            if *kind_cell.borrow() == ServiceKind::Sid {
                                                "SID"
                                            } else {
                                                "Service name"
                                            },
                                            &service,
                                            false,
                                            muted
                                        ))
                                ),
                        )
                        .child(dialog_field("User", &user, false, muted))
                        .child(dialog_field("Password", &password, true, muted))
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(muted).child("Role"))
                                .child({
                                    let cell = role_cell.clone();
                                    let current = *cell.borrow();
                                    let row_view = view.clone();
                                    dialog_pills(
                                        "conn-role",
                                        &[
                                            (OracleRole::Default, "SYSDEFAULT"),
                                            (OracleRole::Sysdba, "SYSDBA"),
                                            (OracleRole::Sysoper, "SYSOPER")
                                        ],
                                        current,
                                        Rc::new(move |r, cx: &mut App| {
                                            *cell.borrow_mut() = r;
                                            row_view
                                                .update(cx, |this, cx| {
                                                    this.pending_role = r;
                                                    cx.notify();
                                                })
                                                .ok();
                                        }),
                                        cx
                                    )
                                })
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(muted).child("Service lookup"))
                                .child({
                                    let cell = kind_cell.clone();
                                    let current = *cell.borrow();
                                    let row_view = view.clone();
                                    dialog_pills(
                                        "conn-kind",
                                        &[
                                            (ServiceKind::ServiceName, "Service"),
                                            (ServiceKind::Sid, "SID")
                                        ],
                                        current,
                                        Rc::new(move |k, cx: &mut App| {
                                            *cell.borrow_mut() = k;
                                            row_view
                                                .update(cx, |this, cx| {
                                                    this.pending_service_kind = k;
                                                    cx.notify();
                                                })
                                                .ok();
                                        }),
                                        cx
                                    )
                                })
                        )
                        .child(
                            h_flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_xs().text_color(muted).child("Use SSL (TCPS)"))
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
                                                    this.pending_ssl = *checked;
                                                    cx.notify();
                                                })
                                                .ok();
                                        })
                                })
                        )
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(muted).child("Password storage"))
                                .child({
                                    let cell = pwmode_cell.clone();
                                    let current = *cell.borrow();
                                    let row_view = view.clone();
                                    dialog_pills(
                                        "conn-pwmode",
                                        &[
                                            (PasswordMode::File, "Save in file"),
                                            (PasswordMode::Keychain, "Keychain"),
                                            (PasswordMode::Ask, "Prompt each time")
                                        ],
                                        current,
                                        Rc::new(move |m, cx: &mut App| {
                                            *cell.borrow_mut() = m;
                                            row_view
                                                .update(cx, |this, cx| {
                                                    this.pending_password_mode = m;
                                                    cx.notify();
                                                })
                                                .ok();
                                        }),
                                        cx
                                    )
                                })
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(muted)
                                .child("Database: the PDB service name (e.g. highlandpdb).")
                        )
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
                        )
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
                        )
                        )
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
        // Keychain mode: a newly typed secret goes to the login keychain
        // (or is cleared there when blanked); an untouched field keeps
        // the stored entry — blank means "keep", not "delete". The file
        // never holds the secret.
        if cfg.password_mode == PasswordMode::Keychain {
            cfg.ensure_id();
            let typed = self.password.read(cx).value().to_string();
            if self.password_snapshot.as_deref() == Some(typed.as_str()) {
                // Untouched since the dialog opened: leave the entry alone.
            } else if typed.is_empty() {
                crate::keychain::delete(&cfg.id);
            } else if let Err(e) = crate::keychain::set(&cfg.id, &typed) {
                self.status = format!("Keychain store failed: {e}").into();
            }
        }
        // Leaving Keychain mode orphans nothing: drop the entry.
        if cfg.password_mode != PasswordMode::Keychain {
            if let Some(old) = self.editing.and_then(|ix| self.connections.get(ix)) {
                if old.password_mode == PasswordMode::Keychain && old.id == cfg.id {
                    crate::keychain::delete(&cfg.id);
                }
            }
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

    /// Delete with confirmation: removing a connection drops its tabs'
    /// bindings (and its keychain entry), so it asks first.
    fn confirm_delete_connection(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
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

    fn delete_connection(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let removed = self.connections.remove(ix);
        crate::keychain::delete(&removed.id);
        self.unlocked.remove(&removed.id);
        if self.pending_password.as_ref().is_some_and(|p| p.conn_id == removed.id) {
            self.pending_password = None;
        }
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
    /// Effective password for a connection: session unlock first, then
    /// stored (File) or keychain (Keychain). None = must prompt (Ask,
    /// Keychain miss, empty File entry).
    fn effective_password(&self, cfg: &ConnectionConfig) -> Option<String> {
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
            PasswordMode::Keychain => crate::keychain::get(&cfg.id).ok().flatten(),
            PasswordMode::Ask => None,
        }
    }

    /// Resolve the password, opening the prompt when the mode needs one
    /// and none is available. Returns the config with the usable password,
    /// or None when the prompt took over — it resumes via
    /// `submit_password` (re-running `run`, or plain-connecting).
    fn with_password(
        &mut self,
        mut cfg: ConnectionConfig,
        run: Option<(String, String)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<ConnectionConfig> {
        match self.effective_password(&cfg) {
            Some(pw) => {
                cfg.password = pw;
                Some(cfg)
            }
            None => {
                self.pending_password = Some(PendingPassword {
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
                        .child(dialog_field("Password", &pwd_in, true, cx.theme().muted_foreground))
                        .footer(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1())
                                .child(Button::new("pwd-cancel").label("Cancel").on_click(
                                    move |_, window, cx| {
                                        window.close_dialog(cx);
                                    },
                                ))
                                .child(
                                    Button::new("pwd-connect")
                                        .label("Connect")
                                        .primary()
                                        .on_click(move |_, window, cx| {
                                            submit.update(cx, |this, cx| {
                                                this.submit_password(window, cx);
                                            })
                                            .ok();
                                        }),
                                ),
                        )
                });
                None
            }
        }
    }

    /// Password prompt submit: unlock the session (persisting to the
    /// keychain when that mode is missing its entry), close the prompt,
    /// then resume the pending connect or run.
    fn submit_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pending) = self.pending_password.take() else {
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
        self.unlocked.insert(pending.conn_id.clone(), pw);
        window.close_dialog(cx);
        match pending.run {
            Some((tab_id, sql)) => self.start_run(&tab_id, sql, window, cx),
            None => self.connect_connection(&pending.conn_id, window, cx),
        }
    }

    fn connect_connection(
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
                let run = Some((tab_id.to_string(), sql.clone()));
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
        });
        self.open_bind_dialog(window, cx);
    }

    /// Cmd+K: open the connection picker; choosing opens a NEW tab
    /// bound to the pick (no run). No-op while a pick is already pending
    /// (picker open) so the deferred run it carries is never clobbered.
    /// The active tab id is kept only to refocus it on cancel/Esc.
    fn open_pick_for_new_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_pick.is_some() {
            return;
        }
        let tab_id = self.active_tab().id.clone();
        self.pending_pick = Some(PendingPick {
            tab_id,
            sql: String::new(),
            after: PickAfter::NewTab,
        });
        self.open_conn_pick_dialog(window, cx);
    }

    /// Shift+Cmd+K: open the connection picker; choosing rebinds the
    /// ACTIVE tab to the pick and connects (no new tab, no run). Same
    /// already-pending guard as Cmd+K.
    fn open_pick_for_rebind(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.pending_pick.is_some() {
            return;
        }
        let tab_id = self.active_tab().id.clone();
        self.pending_pick = Some(PendingPick {
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
                .gap_1()
                .w_full()
                .child(Input::new(&search).w_full());
            if rows.is_empty() {
                body = body.child(
                    div()
                        .text_sm()
                        .text_color(muted)
                        .child(match pick_mode {
                            PickAfter::Run => "No connections yet — add one to run this statement.",
                            PickAfter::NewTab | PickAfter::Rebind => {
                                "No connections yet — add one to get started."
                            }
                        }),
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
                        .child(match pick_mode {
                            PickAfter::Run => "Type to filter, Enter runs on the highlighted match.",
                            PickAfter::NewTab => {
                                "Type to filter, Enter opens a new tab on the highlighted match."
                            }
                            PickAfter::Rebind => {
                                "Type to filter, Enter rebinds the active tab to the highlighted match."
                            }
                        }),
                );
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
                .gap_1()
                // Gutter for the overlaid scrollbar track (see dialog).
                // NOTE: this used to live inside each row so the
                // first-match wash spanned full width, but per-row
                // restyle made hover laggy — container-level stays.
                .pr_5();
            for (rix, r) in shown.iter().enumerate() {
                let pick_view = view.clone();
                let pick_tab = tab_id.clone();
                let pick_sql = sql.clone();
                let pick_mode = pick_mode;
                let hover_view = view.clone();
                let hover_active = active.clone();
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
                            match pick_mode {
                                PickAfter::Run => {
                                    pick_connection_and_run(
                                        &pick_view, &pick_tab, &pick_sql, &conn_id, window, cx,
                                    );
                                }
                                PickAfter::NewTab => {
                                    pick_connection_for_new_tab(&pick_view, &conn_id, window, cx);
                                }
                                PickAfter::Rebind => {
                                    pick_connection_for_rebind(
                                        &pick_view, &pick_tab, &conn_id, window, cx,
                                    );
                                }
                            }
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
                    let pick = pick.as_deref().and_then(|id| {
                        ok_rows.iter().find(|r| r.id == id)
                    });
                    match pick {
                        Some(row) => {
                            match ok_mode {
                                PickAfter::Run => {
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
                                }
                                PickAfter::NewTab => {
                                    pick_connection_for_new_tab(&ok_view, &row.id, window, cx);
                                }
                                PickAfter::Rebind => {
                                    pick_connection_for_rebind(
                                        &ok_view, &ok_tab, &row.id, window, cx,
                                    );
                                }
                            }
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

    /// Format the statement at the cursor (same scope as Run), leaving
    /// the rest of the buffer untouched. No statement → no-op.
    fn format_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.active;
        if !matches!(self.tabs[ix].kind, TabKind::Query) {
            return;
        }
        let editor = self.tabs[ix].editor.clone();
        let (text, cursor) = editor.read_with(cx, |e, _| (e.value().to_string(), e.cursor()));
        let Some((_, start, end)) = statement_at_range(&text, cursor) else {
            return;
        };
        // The splitter trims the statement text but keeps raw span bounds;
        // reformat the trimmed core and preserve the original surrounding
        // whitespace so neighboring statements never join or drift.
        let span = &text[start..end];
        let core = span.trim();
        if core.is_empty() {
            return;
        }
        let formatted = format_sql(core).trim().to_string();
        if formatted == core {
            return;
        }
        let lead = span.len() - span.trim_start().len();
        let trail = span.len() - span.trim_end().len();
        let mut out =
            String::with_capacity(text.len() + formatted.len().saturating_sub(span.len()));
        out.push_str(&text[..start]);
        out.push_str(&span[..lead]);
        out.push_str(&formatted);
        out.push_str(&span[span.len() - trail..]);
        out.push_str(&text[end..]);
        // Keep the caret glued to its surroundings: before the core it
        // stays put; inside it lands at the formatted end; after it
        // shifts by the length delta. Floored to a char boundary.
        let core_start = start + lead;
        let core_end = end - trail;
        let mut new_cursor = if cursor <= core_start {
            cursor
        } else if cursor >= core_end {
            (cursor + formatted.len()).saturating_sub(core_end - core_start)
        } else {
            core_start + formatted.len()
        }
        .min(out.len());
        while !out.is_char_boundary(new_cursor) {
            new_cursor -= 1;
        }
        self.tabs[ix].editor.update(cx, |editor, cx| {
            editor.set_value(out, window, cx);
            editor.set_selected_range(new_cursor..new_cursor, cx);
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
        let own_schema = self.own_schema_of(&conn_id);
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
                // Tables only — keywords never follow FROM. Own-schema
                // tables show (and insert) bare: Oracle resolves
                // unqualified names to the connected schema first.
                if let Some(cache) = &cache {
                    let cache = cache.lock().expect("meta lock");
                    for t in &cache.tables {
                        if hide_system(&t.owner) {
                            continue;
                        }
                        let label = display_name(Some(&t.owner), &t.name, &own_schema);
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

    /// Connected user's schema (for own-schema ranking); "" when unbound.
    fn own_schema_of(&self, conn_id: &Option<String>) -> String {
        conn_id
            .as_deref()
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
            .map(|c| c.user.clone())
            .unwrap_or_default()
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
        let Some(mut cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            return;
        };
        // Same unlock/keychain preference as runs; background triggers
        // never prompt — a missing password just fails this fetch.
        if let Some(pw) = self.effective_password(&cfg) {
            cfg.password = pw;
        }
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
                // Fresh dictionaries rebuild an open schema-browser tree.
                this.refresh_browser(&conn_bg, cx);
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

    // -- Schema browser ---------------------------------------------------

    /// Tree node ids (per-connection trees, so no connection prefix):
    /// `s:{schema}` folder, `g:{schema}/{Tables|Views|Sequences}` folder,
    /// `o:{schema}/{T|V|S}/{object}` (click → viewer tab),
    /// `c:{schema}/{object}/{column}` leaf.
    fn browser_tree_items(&self, conn_id: &str, filter: &str) -> Vec<TreeItem> {
        let loading_item = |label: &str| {
            vec![
                TreeItem::new(format!("b:note:{label}"), label).disabled(true),
            ]
        };
        let Some(cache) = self.meta.get(conn_id) else {
            return loading_item("Loading schema…");
        };
        let Ok(cache) = cache.lock() else {
            return loading_item("Loading schema…");
        };
        if cache.loading
            && cache.tables.is_empty()
            && cache.columns.is_empty()
            && cache.sequences.is_empty()
        {
            return loading_item("Loading schema…");
        }
        let own = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .map(|c| c.user.clone())
            .unwrap_or_default();
        let tree = OracleProvider.tree(&cache, self.show_system, &own, false);
        let tree = crate::schema::filter_tree(&tree, filter);
        if tree.schemas.is_empty() {
            let label = if filter.trim().is_empty() {
                "No objects found"
            } else {
                "No matches"
            };
            return loading_item(label);
        }
        let expanded = self.browser_expanded.get(conn_id).cloned().unwrap_or_default();
        let exp = |id: &str| expanded.contains(id);
        // Own schema's groups sit at the root (no schema folder); every
        // other visible schema nests under "Other Users". The tree model
        // already orders own-first, so partition preserves display order.
        const OTHER_ID: &str = "u:users";
        let (own_schemas, other_schemas): (Vec<_>, Vec<_>) = tree
            .schemas
            .iter()
            .partition(|g| g.name.eq_ignore_ascii_case(&own));
        let mut roots = Vec::with_capacity(own_schemas.len() + 1);
        for g in own_schemas {
            roots.extend(Self::browser_group_items(g, &exp));
        }
        if !other_schemas.is_empty() {
            let mut users = Vec::with_capacity(other_schemas.len());
            for g in other_schemas {
                let sid = format!("s:{}", g.name);
                let groups = Self::browser_group_items(g, &exp);
                users.push(
                    TreeItem::new(sid.clone(), g.name.clone())
                        .children(groups)
                        .expanded(exp(&sid)),
                );
            }
            roots.push(
                TreeItem::new(OTHER_ID, format!("Other Users ({})", users.len()))
                    .children(users)
                    .expanded(exp(OTHER_ID)),
            );
        }
        roots
    }

    /// Tables/Views/Sequences group items for one schema (shared by the
    /// single-schema root path and the per-schema folder path).
    fn browser_group_items(
        g: &crate::schema::SchemaGroup,
        exp: &impl Fn(&str) -> bool,
    ) -> Vec<TreeItem> {
        {
            let mut groups = Vec::with_capacity(3);
            for (group, objs) in [("Tables", &g.tables), ("Views", &g.views)] {
                if objs.is_empty() {
                    continue;
                }
                // Groups stay collapsed until opened: a fresh expand shows
                // only the three group rows, not hundreds of objects.
                let gid = format!("g:{}/{group}", g.name);
                let kind = group.as_bytes()[0] as char;
                let mut items = Vec::with_capacity(objs.len());
                for o in objs {
                    let oid = format!("o:{}:{kind}:{}", g.name, o.name);
                    let cols: Vec<TreeItem> = o
                        .columns
                        .iter()
                        .map(|c| {
                            TreeItem::new(
                                format!("c:{}:{}:{}", g.name, o.name, c.name),
                                format!("{} — {}", c.name, c.data_type),
                            )
                        })
                        .collect();
                    items.push(
                        TreeItem::new(oid.clone(), o.name.clone())
                            .children(cols)
                            .expanded(exp(&oid)),
                    );
                }
                groups.push(
                    TreeItem::new(gid.clone(), format!("{group} ({})", objs.len()))
                        .children(items)
                        .expanded(exp(&gid)),
                );
            }
            if !g.sequences.is_empty() {
                let gid = format!("g:{}/Sequences", g.name);
                let items: Vec<TreeItem> = g
                    .sequences
                    .iter()
                    .map(|s| {
                        TreeItem::new(format!("o:{}:S:{s}", g.name), s.clone())
                    })
                    .collect();
                groups.push(
                    TreeItem::new(gid.clone(), format!("Sequences ({})", g.sequences.len()))
                        .children(items)
                        .expanded(exp(&gid)),
                );
            }
            groups
        }
    }

    /// Rebuild one open browser tree from cache (filter + expansion kept).
    /// No-op for closed or untracked connections.
    fn refresh_browser(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        if !self.browser_open.contains(conn_id) {
            return;
        }
        let Some(tree) = self.browser_trees.get(conn_id).cloned() else {
            return;
        };
        let filter = self
            .browser_filters
            .get(conn_id)
            .map(|f| f.read(cx).value().to_string())
            .unwrap_or_default();
        let items = self.browser_tree_items(conn_id, &filter);
        tree.update(cx, |t, cx| t.set_items(items, cx));
    }

    /// Expand/collapse the schema tree under a connection. Expanding a
    /// dead connection auto-connects first; `ensure_meta` warms the
    /// dictionary and its completion hook rebuilds the tree on arrival.
    fn toggle_browser(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser_open.contains(conn_id) {
            self.browser_open.remove(conn_id);
            self.browser_trees.remove(conn_id);
            self.browser_expanded.remove(conn_id);
            self.browser_filters.remove(conn_id);
            cx.notify();
            return;
        }
        self.browser_open.insert(conn_id.to_string());
        if !self.live.contains(conn_id) {
            self.connect_connection(conn_id, window, cx);
        }
        self.ensure_meta(conn_id, cx);
        // Per-connection filter: keystrokes rebuild only this tree.
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter schema…"));
        let filter_in = filter.clone();
        let filter_conn = conn_id.to_string();
        let filter_sub = cx.subscribe_in(
            &filter_in,
            window,
            move |this: &mut Self, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.refresh_browser(&filter_conn, cx);
                    // Same as toggles: the container height is computed at
                    // render time from visible rows.
                    cx.notify();
                }
            },
        );
        self._subs.push(filter_sub);
        self.browser_filters.insert(conn_id.to_string(), filter);
        let filter = self
            .browser_filters
            .get(conn_id)
            .map(|f| f.read(cx).value().to_string())
            .unwrap_or_default();
        let items = self.browser_tree_items(conn_id, &filter);
        let state = cx.new(|cx| TreeState::new(cx).items(items));
        let sub_conn = conn_id.to_string();
        let sub = cx.subscribe(&state, move |this: &mut Self, _, event: &TreeEvent, cx| {
            // Expansion state only — no auto-scroll. (An auto-reveal that
            // pinned expanded nodes to the top used to live here; it moved
            // rows under the cursor mid-gesture and broke double-click
            // opens, so expansion leaves the scroll alone now.)
            match event {
                TreeEvent::Expanded(id) => {
                    this.browser_expanded
                        .entry(sub_conn.clone())
                        .or_default()
                        .insert(id.to_string());
                }
                TreeEvent::Collapsed(id) => {
                    if let Some(set) = this.browser_expanded.get_mut(&sub_conn) {
                        set.remove(id.as_ref());
                    }
                }
            };
            // Container height derives from visible rows (read at render),
            // so toggles must repaint the view, not just the tree.
            cx.notify();
        });
        self._subs.push(sub);
        self.browser_trees.insert(conn_id.to_string(), state);
        cx.notify();
    }

    /// Render one open connection's schema tree. Object rows click through
    /// to viewer tabs; folders toggle via the kit's own row handling.
    fn render_browser_tree(&self, conn_id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(state) = self.browser_trees.get(conn_id).cloned() else {
            return div().into_any_element();
        };
        let view = cx.entity().downgrade();
        let conn = conn_id.to_string();
        // Shared by the row renderer and the context menu below (both
        // `move` closures, so each gets its own clone).
        let menu_view = view.clone();
        let menu_conn = conn.clone();
        tree(
            &state,
            move |_ix, entry, _selected, _window, cx| {
                let id = entry.item().id.clone();
                let depth = entry.depth();
                let folder = entry.is_folder();
                let expanded = entry.is_expanded();
                let label = entry.item().label.clone();
                let ids = id.to_string();
                // Two fixed glyph slots: disclosure chevron (folders) +
                // type icon. The kit draws neither itself.
                let chevron: Option<KitIcon> = folder.then(|| {
                    if expanded {
                        KitIcon::ChevronDown
                    } else {
                        KitIcon::ChevronRight
                    }
                });
                let type_icon: Option<KitIcon> = if ids == "u:users" {
                    Some(KitIcon::Users)
                } else if ids.starts_with("s:") {
                    Some(KitIcon::User)
                } else if ids.starts_with("g:") {
                    if ids.ends_with("/Views") {
                        Some(KitIcon::Eye)
                    } else if ids.ends_with("/Sequences") {
                        Some(KitIcon::Hash)
                    } else {
                        Some(KitIcon::Table)
                    }
                } else if ids.starts_with("o:") {
                    match ids.split(':').nth(2) {
                        Some("V") => Some(KitIcon::Eye),
                        Some("S") => Some(KitIcon::Hash),
                        _ => Some(KitIcon::Table),
                    }
                } else if ids.starts_with("c:") {
                    Some(KitIcon::Dot)
                } else {
                    None
                };
                let mut row = h_flex()
                    .gap_1()
                    .items_center()
                    .pl(px(4.0 + depth as f32 * 12.0));
                // Chevron slot (folders only) + type-icon slot: fixed
                // widths keep labels aligned down the tree.
                row = match chevron {
                    Some(g) => row.child(
                        div()
                            .w(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .child(g),
                    ),
                    None => row.child(div().w(px(16.)).flex_shrink_0()),
                };
                row = match type_icon {
                    Some(g) => row.child(
                        div()
                            .w(px(16.))
                            .flex_shrink_0()
                            .flex()
                            .justify_center()
                            .text_color(cx.theme().muted_foreground)
                            .child(g),
                    ),
                    None => row.child(div().w(px(16.)).flex_shrink_0()),
                };
                // Object rows (not columns, not folders) open viewer tabs.
                // Shared id parse (a `/`-split here once attached zero
                // handlers: every object parsed to nothing and clicks died
                // silently — the debug_assert below guards the scheme).
                let target = crate::schema::parse_object_id(&ids);
                if ids.starts_with("o:") && target.is_none() {
                    debug_assert!(false, "unparsed object row {ids}");
                }
                // Click synthesis (`on_click`) never fires inside the kit's
                // virtualized rows (its mousedown rebuild drops the pending
                // click), but press and release both land — so release opens
                // the viewer. Folders keep the kit's own toggle behavior.
                let up_target = target.clone();
                let up_view = view.clone();
                let up_conn = conn.clone();
                row = row
                    .child(div().text_xs().truncate().child(label.to_string()))
                    .on_mouse_up(MouseButton::Left, move |event, window, cx| {
                        // Single release only selects (the kit handles that);
                        // double-click opens the viewer tab.
                        if event.click_count < 2 {
                            return;
                        }
                        if let Some((schema, name, tk)) = &up_target {
                            up_view
                                .update(cx, |this, cx| {
                                    this.open_viewer(
                                        &up_conn,
                                        schema.clone(),
                                        name.clone(),
                                        *tk,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }
                    });
                ListItem::new(ids.clone()).child(row)
            },
        )
        .context_menu(move |_ix, entry, menu, _window, _cx| {
            // Own menu per row: without it, right-clicks fall through to
            // the connection row's menu (Connect/Edit/Delete).
            let ids = entry.item().id.to_string();
            let view = menu_view.clone();
            let conn = menu_conn.clone();
            if let Some((schema, name, tk)) = crate::schema::parse_object_id(&ids) {
                let open_view = view.clone();
                let open_conn = conn.clone();
                let open_schema = schema.clone();
                let open_name = name.clone();
                menu.item(
                    PopupMenuItem::new("Open description")
                        .icon(KitIcon::FileText)
                        .on_click(move |_, window, cx| {
                            open_view
                                .update(cx, |this, cx| {
                                    this.open_viewer(
                                        &open_conn,
                                        open_schema.clone(),
                                        open_name.clone(),
                                        tk,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        }),
                )
                .item(
                    PopupMenuItem::new("Copy name")
                        .icon(KitIcon::ClipboardCopy)
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(name.clone()));
                        }),
                )
            } else if let Some(rest) = ids.strip_prefix("c:") {
                // Column leaf `c:{schema}:{object}:{column}`: copy the
                // column name (last segment).
                let col = rest.rsplit(':').next().unwrap_or(rest).to_string();
                menu.item(
                    PopupMenuItem::new("Copy name")
                        .icon(KitIcon::ClipboardCopy)
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(col.clone()));
                        }),
                )
            } else {
                menu
            }
        })
        .into_any_element()
    }

    // -- Render ---------------------------------------------------------------

    fn render_connection_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = &self.connections[ix];
        let is_live = self.live.contains(&cfg.id);
        let view = cx.entity().downgrade();
        let conn_id = cfg.id.clone();
        let browser_open = self.browser_open.contains(&cfg.id);
        let toggle_id = conn_id.clone();
        let tree_conn = conn_id.clone();
        // NOTE: the row div and the tree are siblings under this wrapper.
        // The tree must NEVER nest inside the row div: it owns the
        // connection context menu, and anything inside its hitbox fires
        // both menus on right-click (one menu per hitbox, last wins).
        let row_body = h_flex()
                    .gap_2()
                    .items_stretch()
                    .px_2()
                    .py_1()
                    // Status bar: the live indicator — success green when
                    // connected, faint border tone when idle. First child
                    // of the horizontal body so items_stretch gives it
                    // full row height (as a row-level child it collapsed
                    // to zero height and never painted).
                    .child(div().id(("conn-status", ix)).test_support().w(px(3.)).rounded_full().bg(if is_live {
                        cx.theme().success
                    } else {
                        cx.theme().border
                    }))
                    // Schema-browser disclosure: per-connection tree below.
                    .child(
                        Button::new(("conn-expand", ix))
                            .icon(if browser_open {
                                KitIcon::ChevronDown
                            } else {
                                KitIcon::ChevronRight
                            })
                            .ghost()
                            .with_size(px(24.))
                            .tooltip("Browse schema")
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.toggle_browser(&toggle_id, window, cx);
                            })),
                    )
                    .child(
                        Button::new(("conn-icon", ix))
                            .icon(KitIcon::Database)
                            .ghost()
                            .with_size(px(24.))
                            // Live state reads from the icon + the 3px
                            // status bar, not a full-row wash.
                            .text_color(if is_live {
                                cx.theme().success
                            } else {
                                cx.theme().muted_foreground
                            }),
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
                            )
                    );
        let row = div()
            .id(("conn-row", ix))
            .w_full()
            .rounded_md()
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
            .child(row_body);
        // NOTE: the tree is a SIBLING of the row div, never a child — the
        // row div owns the connection context menu, and anything nested
        // inside it (visually below or not) shares its hitbox and fires
        // both menus on right-click (one menu slot, last opener wins).
        v_flex()
            .w_full()
            .child(row)
            .when(browser_open, |this| {
                let filter_row = self
                    .browser_filters
                    .get(&tree_conn)
                    .map(|f| {
                        div().w_full().pt_1().pb_1().child(
                            Input::new(f).w_full().h(px(18.)).text_xs(),
                        )
                    });
                // Fit the visible rows with a little breathing room: row
                // pixels vary a hair by font metrics, and any shortfall
                // overflows into a scrollbar, while a small surplus reads
                // as intentional padding. Capped: beyond it the tree owns
                // its scroll and the bar is legitimate.
                let mut rows = 0;
                if let Some(state) = self.browser_trees.get(&tree_conn) {
                    let state = state.read(cx);
                    while state.entry(rows).is_some() {
                        rows += 1;
                    }
                }
                let height_px = (44.0 + rows as f32 * 30.0).min(320.0);
                this.child(
                    v_flex()
                        .w_full()
                        .h(px(height_px))
                        .pl(px(8.))
                        .pr(px(2.))
                        .pb_1()
                    .children(filter_row)
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_hidden()
                            .child(self.render_browser_tree(&tree_conn, cx)),
                    ),
            )
        })
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sidebar_collapsed {
            // Slim rail: just expand + settings. Connections (and adding)
            // live only in the expanded pane — the rail stays a narrow
            // launcher, not a second connection list.
            return v_flex()
                .w(px(44.))
                .h_full()
                .bg(cx.theme().sidebar)
                // Right-edge divider: the expanded pane gets its separator
                // from the resizable handle, which doesn't exist in this
                // branch — without an explicit border the rail bleeds
                // into main content. (border_color alone paints nothing.)
                .border_r_1()
                .border_color(cx.theme().border)
                // NewTab/CloseTab stay app-global (main.rs): element-level
                // duplicates double-fire, and dialogs sit outside these
                // roots so they never see dialog-focused keypresses.
                .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                    this.cycle_tab(1, window, cx);
                }))
                .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                    this.cycle_tab(-1, window, cx);
                }))
                // File commands live here too (same bubble-path reason as
                // the tab actions above): with focus in the sidebar the
                // main-area listeners never fire, so Cmd+S/O would die.
                .on_action(cx.listener(|this, _: &OpenSql, window, cx| {
                    this.open_sql_file(window, cx);
                }))
                .on_action(cx.listener(|this, _: &SaveSql, window, cx| {
                    this.save_active_tab(window, cx);
                }))
                .on_action(cx.listener(|this, _: &SaveSqlAs, window, cx| {
                    this.save_active_tab_as(window, cx);
                }))
                // Quit/Settings stay app-global (main.rs): a second,
                // element-level registration double-fires the action.
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
                .child(div().flex_1())
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
        // Tab actions live here too (duplicated from render_main): actions
        // bubble from the focused element up through ancestors only, so with
        // focus in the sidebar the main-area listeners never fire. Dialog
        // focus paths are unaffected (dialogs are not under either root),
        // so this changes nothing while a dialog is open.

        v_flex()
            .size_full()
            .bg(cx.theme().sidebar)
            // NewTab/CloseTab stay app-global (main.rs): element-level
            // duplicates double-fire, and dialogs sit outside these
            // roots so they never see dialog-focused keypresses.
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                this.cycle_tab(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &PrevTab, window, cx| {
                this.cycle_tab(-1, window, cx);
            }))
            // File commands live here too (same bubble-path reason as
            // the tab actions above): with focus in the sidebar the
            // main-area listeners never fire, so Cmd+S/O would die.
            .on_action(cx.listener(|this, _: &OpenSql, window, cx| {
                this.open_sql_file(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSql, window, cx| {
                this.save_active_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSqlAs, window, cx| {
                this.save_active_tab_as(window, cx);
            }))
            // Quit/Settings stay app-global (main.rs): a second,
            // element-level registration double-fires the action.
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
                div()
                    .w_full()
                    .h(px(36.))
                    .px_1()
                    .flex()
                    .items_center()
                    .child(
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
                    .id("settings-footer")
                    .w_full()
                    .flex()
                    .items_center()
                    .px_1()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.open_settings(window, cx);
                    }))
                    .child(
                        Button::new("settings-labeled")
                            .icon(KitIcon::Settings)
                            .ghost()
                            .small()
                            .w_full()
                            .justify_start()
                            .label("Settings")
                            .tooltip("Settings (⌘,)"),
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
        let tab_env = self.tab_environment(tab);
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
                                    move |_, window, cx| {
                                        view.update(cx, |this, cx| {
                                            if let Some(t) = this.tab_by_id(&tab_id) {
                                                t.connection_id = Some(conn_id.clone());
                                                this.persist_tabs();
                                                this.connect_connection(&conn_id, window, cx);
                                            }
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

    /// The bound connection's environment (Untagged when unbound): drives
    /// the env badge in the picker and the editor's tint ring.
    fn tab_environment(&self, tab: &QueryTab) -> Environment {
        tab.connection_id
            .as_deref()
            .and_then(|cid| self.connections.iter().find(|c| c.id == cid))
            .map(|c| c.environment)
            .unwrap_or(Environment::Untagged)
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab();
        let editor = tab.editor.clone();
        let pending = tab.pending_txn;
        // Subtle ring in the bound connection's environment color (same
        // hue as its badge): none when untagged.
        let ring = env_color(self.tab_environment(tab), cx).map(|c| c.opacity(0.45));
        v_flex()
            .id("query-section")
            .size_full()
            .gap_2()
            .p_2()
            .bg(cx.theme().tab_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| {
                // Focus-gated like its siblings: without this, Cmd+Enter
                // in any kit input (dialog fields, picker search) runs
                // the active tab's query behind the dialog.
                if this.editor_focused(window, cx) {
                    this.run_at_cursor(window, cx);
                }
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
                    .id("sql-editor")
                    .when_some(ring, |this, ring| {
                        this.border_1().border_color(ring).rounded_md()
                    })
                    .child(
                        Editor::new(&editor).size_full(),
                    ),
            )
    }

    fn render_results(&self, tab: &QueryTab, cx: &mut Context<Self>) -> impl IntoElement {
        // min_w_0 + overflow_hidden: without them flex items refuse to shrink
        // below the table's full content width, the table sees an unbounded
        // viewport, renders every column (no virtualization), and its
        // horizontal scrollbar never engages. min_h_0 likewise for height:
        // viewer tabs have no resizable panel forcing a pixel height, so
        // without it the virtualized body collapses to zero rows while the
        // fixed-height header/export/column rows still paint.
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
                .min_h_0()
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
            .min_h_0()
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
            .bg(cx.theme().status_bar)
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

    /// Slim header for object-viewer tabs (schema browser): object title +
    /// kind tag + connection, Refresh, and Cancel while busy. No editor,
    /// no run actions — viewers only ever show one DESCRIBE.
    fn render_viewer_header(&self, tab: &QueryTab, cx: &mut Context<Self>) -> impl IntoElement {
        let kind_tag = match &tab.kind {
            TabKind::Viewer { kind, .. } => match kind {
                crate::metadata::TableKind::Table => "TABLE",
                crate::metadata::TableKind::View => "VIEW",
                crate::metadata::TableKind::Sequence => "SEQUENCE",
            },
            TabKind::Query => "",
        };
        let refresh_id = tab.id.clone();
        h_flex()
            .h(px(36.))
            .gap_2()
            .px_2()
            .items_center()
            .bg(cx.theme().tab_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(KitIcon::Database)
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .child(tab.name.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(kind_tag),
            )
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(self.connection_name(&tab.connection_id)),
            )
            .child(
                Button::new("viewer-refresh")
                    .secondary()
                    .small()
                    .label("Refresh")
                    .tooltip("Re-run DESCRIBE")
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.refresh_viewer(&refresh_id, window, cx);
                    })),
            )
            .when(tab.busy, |this| {
                let cancel_id = tab.id.clone();
                this.child(
                    Button::new("viewer-cancel")
                        .danger()
                        .small()
                        .icon(KitIcon::X)
                        .label("Cancel")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.cancel_run(&cancel_id, cx);
                        })),
                )
            })
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.active_tab();
        // Object-viewer tabs (schema browser): grid only, no editor.
        // Layout mirrors the query split (resizable panel + body) on
        // purpose: the grid is virtualized and needs the resizable's
        // definite pixel sizing — a pure flex chain collapses its body
        // to zero rows while fixed-height siblings still paint.
        let content: AnyElement = if matches!(tab.kind, TabKind::Viewer { .. }) {
            let body: AnyElement = if tab.output.is_some() {
                self.render_output_pane(tab, cx).into_any_element()
            } else if tab.has_result {
                self.render_results(tab, cx).into_any_element()
            } else {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Loading description…")
                    .into_any_element()
            };
            v_resizable("viewer-split")
                .child(
                    resizable_panel()
                        .size(px(36.))
                        .size_range(px(36.)..px(200.))
                        .flex_none()
                        .child(self.render_viewer_header(tab, cx)),
                )
                .child(body)
                .into_any_element()
        } else {
            // Fresh tab (never ran, no output): editor takes the full
            // height — no empty bottom pane. Once anything runs, the
            // resizable editor/results split appears and stays.
            let fresh = !tab.has_result && tab.output.is_none();
            if fresh {
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
            }
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
            // NewTab/CloseTab stay app-global (main.rs): element-level
            // duplicates double-fire, and dialogs sit outside this
            // root so they never see dialog-focused keypresses.
            .on_action(cx.listener(|this, _: &OpenSql, window, cx| {
                this.open_sql_file(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSql, window, cx| {
                this.save_active_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SaveSqlAs, window, cx| {
                this.save_active_tab_as(window, cx);
            }))
            // Quit/Settings stay app-global (main.rs): a second,
            // element-level registration double-fires the action.
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
        .child(DataTable::new(table).xsmall().stripe(true))
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
/// Check-list row (theme-list pattern) for settings pickers: label +
/// optional detail + check, click runs apply. Shared by font, row-limit,
/// and delimiter lists so they stay visually identical.
fn settings_pick_row(
    id: impl Into<ElementId>,
    label: String,
    detail: Option<String>,
    selected: bool,
    cx: &App,
    apply: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> AnyElement {
    let hover_bg = cx.theme().accent;
    let muted = cx.theme().muted_foreground;
    div()
        .id(id)
        .w_full()
        .p_2()
        .rounded_md()
        .hover(move |this| this.bg(hover_bg))
        .on_click(move |ev, window, cx| apply(ev, window, cx))
        .child(
            h_flex()
                .gap_2()
                .items_center()
                .child(
                    v_flex().flex_1().child(div().text_sm().child(label)).children(
                        detail.map(|d| div().text_xs().text_color(muted).child(d)),
                    ),
                )
                .when(selected, |this| {
                    this.child(div().text_color(muted).child(KitIcon::Check))
                }),
        )
        .into_any_element()
}

fn env_tag(env: Environment, cx: &App) -> Option<AnyElement> {    let label = env.label()?;
    let color = env_color(env, cx)?;
    Some(
        div()
            .w(px(48.))
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .px_1()
            .rounded_md()
            .bg(color.opacity(0.15))
            .text_xs()
            .text_center()
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

/// Pill radio row for the connection dialog: one pill per option, the
/// current one highlighted. `pick` syncs the dialog-local cell; callers
/// also mirror it into the matching `pending_*` view field + notify.
/// Ids are namespaced per row via `id_base`.
/// Native application menu: every command with a shortcut is listed
/// (except CopySelection: its ⌘C is grid-scoped, and a menu item would
/// route AppKit's Cmd+C through global dispatch, breaking normal copy
/// in the editor). Key equivalents render from the keymap. Menu dispatch
/// reaches the same handlers as the keypresses; focus-gated items grey
/// out via is_action_available. Defined here (not main.rs) so tests can
/// assert coverage when shortcuts are added.
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "SQLHighland".into(),
            items: vec![
                MenuItem::action("Preferences…", OpenSettings),
                MenuItem::Separator,
                MenuItem::action("Quit", Quit),
            ],
            disabled: false,
        },
                Menu {
                    name: "File".into(),
                    items: vec![
                        MenuItem::action("New Tab", NewTab),
                        MenuItem::action("New Tab with Connection…", PickConnection),
                        MenuItem::action("Change Tab Connection…", RebindConnection),
                        MenuItem::action("Close Tab", CloseTab),
                MenuItem::Separator,
                MenuItem::action("Open SQL File…", OpenSql),
                MenuItem::action("Save", SaveSql),
                MenuItem::action("Save As…", SaveSqlAs),
            ],
            disabled: false,
        },
        Menu {
            name: "Query".into(),
            items: vec![
                MenuItem::action("Run Query", RunQuery),
                MenuItem::action("Format Query", FormatQuery),
                MenuItem::Separator,
                MenuItem::action("Commit Transaction", CommitTxn),
                MenuItem::action("Rollback Transaction", RollbackTxn),
                MenuItem::Separator,
                MenuItem::action("Trigger Completion", TriggerComplete),
            ],
            disabled: false,
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Next Tab", NextTab),
                MenuItem::action("Previous Tab", PrevTab),
            ],
            disabled: false,
        },
    ]
}

fn dialog_pills<T: Copy + PartialEq + 'static>(
    id_base: &'static str,
    options: &[(T, &'static str)],
    current: T,
    pick: std::rc::Rc<dyn Fn(T, &mut App)>,
    cx: &App,
) -> AnyElement {
    let muted = cx.theme().muted_foreground;
    let accent = cx.theme().accent;
    h_flex().gap_1().children(options.iter().enumerate().map(
        |(ix, (value, label))| {
            let value = *value;
            let selected = current == value;
            let pick = pick.clone();
            div()
                .id((id_base, ix))
                .test_support()
                .px_2()
                .py_1()
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
        },
    ))
    .into_any_element()
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
            // Quit/Settings stay app-global (main.rs): a second,
            // element-level registration double-fires the action.
            // PickConnection lives here (sole registration): the dialog
            // layer renders under this root, so it fires with any focus,
            // and the listener gets &mut Window directly (no re-take).
            .on_action(cx.listener(|this, _: &PickConnection, window, cx| {
                this.open_pick_for_new_tab(window, cx);
            }))
            .on_action(cx.listener(|this, _: &RebindConnection, window, cx| {
                this.open_pick_for_rebind(window, cx);
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
