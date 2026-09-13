//! UI: connections sidebar + tabbed SQL editors + results grids + status bar.
//!
//! Model: one live Oracle session per saved connection (see `session.rs`),
//! shared by tabs. Each tab binds to a connection it can switch, and owns
//! its editor, grid, and run state. Editor drafts auto-save (debounced) and
//! tabs restore on launch.
//!
//! Blocking Oracle calls run on the background executor; the view is only
//! ever mutated on the UI thread.

use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::complete::{
    build_alias_map, byte_to_lsp_pos, describe_target, hover_markdown, is_trivia_position,
    qualifier_before, word_at,
};
use crate::config::{CompleteMode, Preferences, SavedConfig, SavedTab, TabsManifest};
use crate::db::{DbClient, OracledbSession};
use crate::filetab::{self, FileStamp};
use crate::metadata::SharedCache;
use crate::model::{
    csv_row, tab_name_from_sql, ColumnInfo, ConnectionConfig, Environment, OracleRole, PasswordMode,
    ServiceKind,
};
use crate::schema::{DbEngine, OracleProvider, SchemaProvider as _};
use crate::session::{SessionPool, lock};
use crate::conn_picker::{PendingPick, PickAfter};
use crate::bind_dialog::PendingBind;
use crate::run::file_stem;
use crate::sql::{
    format_sql, line_at, parse_at_directive, statement_at, statement_at_range,
};
use gpui_kit::component::resizable::ResizableState;
use gpui_kit::base::SelectableText;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    CompletionProvider, DefinitionProvider, Editor, EditorState, HoverProvider, InputEvent, InputState,
    Textarea, TextareaState,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::tree::{tree, TreeState};
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
        RunScript,
        ToggleSidebar,
        NewConnection,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        GrowEditor,
        ShrinkEditor,
        DismissResults,
        OpenSql,
        SaveSql,
        SaveSqlAs,
        TriggerComplete
    ]
);

/// Max popup rows per request (ranking already orders them best-first).
pub(crate) const COMPLETE_LIMIT: usize = 100;

/// The Cmd-click document hook: answers `oracle-describe:` URIs by
/// opening a viewer tab. Named type for clippy's complexity lint.
type ShowDocumentHook = Rc<dyn Fn(&lsp_types::ShowDocumentParams, &mut Window, &mut App) -> bool>;

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
pub(crate) struct ResultData {
    pub(crate) columns: Vec<ColumnInfo>,
    pub(crate) rows: Vec<Vec<Option<SharedString>>>,
    pub(crate) elapsed_ms: u128,
    pub(crate) exhausted: bool,
    pub(crate) loading: bool,
    pub(crate) capped: bool,
}

/// Convert one fetched page to ref-counted cells.
pub(crate) fn to_shared(rows: Vec<Vec<Option<String>>>) -> Vec<Vec<Option<SharedString>>> {
    rows.into_iter()
        .map(|row| row.into_iter().map(|c| c.map(SharedString::from)).collect())
        .collect()
}

/// Links one open server-side cursor to a tab's grid.
pub(crate) struct FetchState {
    pub(crate) session: Arc<Mutex<OracledbSession>>,
    pub(crate) query_id: u64,
    pub(crate) chunk: usize,
    pub(crate) cap: usize,
    pub(crate) data: Mutex<ResultData>,
    pub(crate) view: WeakEntity<SqlHighlandView>,
    pub(crate) tab_id: String,
}

/// Table delegate over the shared [`FetchState`] of a tab's latest query.
pub(crate) struct ResultsDelegate {
    fetch: Option<Arc<FetchState>>,
}

impl ResultsDelegate {
    fn empty() -> Self {
        Self { fetch: None }
    }

    pub(crate) fn set_fetch(&mut self, fetch: Option<Arc<FetchState>>) {
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
        // DB-shaped data must never crash the grid: a 0-column result or
        // a virtualized-grid race yields a positional placeholder instead
        // of panicking the app (and its unsaved drafts) away.
        let name = name.unwrap_or_else(|| format!("col{col_ix}"));
        // Key by position, not name: duplicate column names (common in
        // SELECT * joins) would otherwise collide element identities,
        // breaking reconciliation and defeating column virtualization.
        Column::new(format!("col-{col_ix}"), name).width(px(180.))
    }

    /// Header labels as native selectable text: drag-select a name and
    /// Cmd+C copies it through the window selection layer — no special
    /// column-copy mode. (Column-select mode is off at the table, so a
    /// plain header click is inert instead of selecting the column.)
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = self.column(col_ix, cx).name;
        div()
            .size_full()
            .child(SelectableText::new(format!("col-th-{col_ix}"), name))
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
                    let mut session = lock(&fetch_bg.session);
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
                                lock(&fetch.data).loading = false;
                                return false;
                            }
                            {
                                let mut data = lock(&fetch.data);
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
                            lock(&fetch.data).loading = false;
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
pub(crate) fn describe_fetch(fetch: &FetchState) -> String {
    let data = lock(&fetch.data);
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
pub(crate) enum CopySel {
    Cell,
    Row,
}

#[derive(Clone, Copy)]
pub(crate) enum ConnMenuOp {
    Connect,
    Disconnect,
    Edit,
    Delete,
}

pub(crate) fn conn_menu_item(
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
pub(crate) struct Output {
    kind: OutputKind,
    text: SharedString,
}

impl Output {
    pub(crate) fn error(text: impl Into<SharedString>) -> Self {
        Self {
            kind: OutputKind::Error,
            text: text.into(),
        }
    }

    pub(crate) fn info(text: impl Into<SharedString>) -> Self {
        Self {
            kind: OutputKind::Info,
            text: text.into(),
        }
    }
}

/// One query tab: editor + grid + run state + connection binding.
pub(crate) struct QueryTab {
    pub(crate) id: String,
    pub(crate) name: SharedString,
    pub(crate) kind: TabKind,
    pub(crate) connection_id: Option<String>,
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) file_stamp: Option<FileStamp>,
    pub(crate) dirty: bool,
    pub(crate) editor: Entity<EditorState>,
    pub(crate) table: Entity<TableState<ResultsDelegate>>,
    /// Mirrors the delegate's fetch for lock-free (no entity read) checks.
    pub(crate) fetch: Option<Arc<FetchState>>,
    pub(crate) result_meta: SharedString,
    pub(crate) has_result: bool,
    /// Latest action outcome for the output pane: failures, and confirmations
    /// of non-query statements. Cleared on every new run; Dismiss returns to
    /// the grid (or placeholder) without re-running.
    pub(crate) output: Option<Output>,
    pub(crate) busy: bool,
    /// Generation of the tab's latest run. Bumped on every Run and on Cancel;
    /// late completions whose token mismatches are discarded. This is what
    /// makes Cancel work without driver break support (beta.3 has none):
    /// the abandoned worker finishes server-side, but its results never land.
    pub(crate) run_token: u64,
    /// When the current run started. Drives the live `Running… Ns` status.
    pub(crate) run_started: Option<std::time::Instant>,
    /// Uncommitted DML on this tab's connection. Transactions are
    /// session-scoped, so commit/rollback clears this for every tab sharing
    /// the connection. The driver never autocommits.
    pub(crate) pending_txn: bool,
    /// Most recent selection kind. See [`CopySel`].
    pub(crate) copy_sel: Option<CopySel>,
    /// Dismissed bottom pane: Dismiss (output pane) or the grid's close
    /// button hides everything below the editor — Dismiss means show
    /// only the query window. Cleared by every new run.
    pub(crate) hide_results: bool,
    /// Read-only message view for the output pane: a real text area, so
    /// the pane has a caret, selection, and native Cmd+C. Synced from
    /// `output` at render (only when the text differs, so caret and
    /// selection survive repaints).
    pub(crate) output_text: Entity<TextareaState>,
    /// Last executed statement text. Feeds the `query` sheet on Excel export.
    pub(crate) last_sql: String,
    /// An export drain is paging this tab's cursor past the grid cap.
    /// While true the grid shows fetched-so-far rows and scroll-fetching
    /// pauses (`loading` is held) so pages never interleave or duplicate.
    pub(crate) exporting: bool,
    /// Exported row count (progress display). Bumped on the UI thread.
    pub(crate) export_rows: usize,
    /// Cancellation flag polled by the drain loop each chunk.
    pub(crate) export_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    pub(crate) save_task: Option<Task<()>>,
    pub(crate) _subs: Vec<Subscription>,
}

/// Tab flavor: a full SQL editor, or an object viewer (DESCRIBE grid,
// no editor) opened from the schema browser. Viewers are ephemeral and
// reuse the run/results pipeline. The editor entity is kept but
// unrendered for viewers — every viewer branch is a marked `TabKind`
// check, so a future `Option<editor>` refactor is compiler-guided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TabKind {
    Query,
    Viewer {
        owner: String,
        name: String,
        kind: crate::metadata::TableKind,
    },
}

/// Arguments for creating a tab: the data half of `make_tab` (keeps it
/// under clippy's argument limit), leaving `window`/`cx` separate.
struct NewTabSpec {
    id: String,
    name: String,
    connection_id: Option<String>,
    text: String,
    kind: TabKind,
}

/// Export file format chosen in the menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExportFormat {
    Csv,
    Xlsx,
}

impl ExportFormat {
    pub(crate) fn ext(self) -> &'static str {
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

    // (ExportOutcome/file_stem/unix_timestamp/export_drain_blocking live in run.rs)

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

/// Password prompt in flight: which connection, and the run to resume
/// afterwards (`None` = plain connect from the sidebar/tree). The resume
/// carries how to re-enter: a stored statement re-runs, a buffer script
/// re-reads the live editor text.
pub(crate) struct PendingPassword {
    pub(crate) conn_id: String,
    pub(crate) run: Option<(String, String, PickAfter)>,
}

/// How a script run resumes after the connection picker or password
/// prompt. File entries re-expand the directive line; buffer runs
/// re-read the live editor text.
#[derive(Debug, Clone)]
pub(crate) enum ScriptResume {
    /// File entry: the raw `@…` line, re-expanded on resume.
    File(String),
    /// Whole buffer: re-read from the editor on resume.
    Buffer,
}

    // -- Variables dialog (see bind_dialog.rs) ---------------------------------

/// Read every dialog field and submit the run. Shared by the Run button and
/// the dialog's Enter-to-confirm (`on_ok`) so both paths behave identically.
/// Blank fields block submission: the error slot is filled (shown in the
/// dialog on rebuild) and false is returned so the dialog stays open.
// (submit_bind_fields lives in bind_dialog.rs;
// pick_connection_resume + focus_tab_editor live in conn_picker.rs)
pub struct SqlHighlandView {
    pub(crate) pool: SessionPool,
    /// Connection ids with a live pooled session. The ONLY liveness signal
    /// the render path may use: locking a session mutex during render blocks
    /// the UI thread behind in-flight queries (a `std` Mutex blocks, it does
    /// not yield). Updated on connect/disconnect/run outcomes, never read
    /// from a render through the pool.
    pub(crate) live: std::collections::HashSet<String>,
    pub(crate) connections: Vec<ConnectionConfig>,
    pub(crate) tabs: Vec<QueryTab>,
    pub(crate) active: usize,
    pub(crate) tab_scroll: ScrollHandle,
    pub(crate) untitled_counter: usize,
    pub(crate) sidebar_collapsed: bool,
    /// Owned splitter state for the query editor/results split. Held
    /// (not keyed) so keyboard height steps drive the same state the
    /// mouse drags — `ResizablePanel::size()` is initial-only, which is
    /// why an `editor_h` field never moved the panel.
    pub(crate) editor_split: Entity<ResizableState>,
    /// Index being edited in the connection dialog (`None` = adding).
    pub(crate) editing: Option<usize>,
    /// Pending environment tag for the open connection dialog. Set by
    /// start_add/start_edit, mutated by the dialog's pill row, read by save.
    /// (Only one connection dialog opens at a time, like `editing`.)
    pub(crate) pending_env: Environment,
    /// Same pattern for role / service-kind / SSL / password-mode rows.
    pub(crate) pending_role: OracleRole,
    pub(crate) pending_service_kind: ServiceKind,
    pub(crate) pending_ssl: bool,
    pub(crate) pending_password_mode: PasswordMode,
    /// Pending database engine for the open connection dialog. Only
    /// Oracle exists today, so the row is display-only — but the dialog
    /// owns the value like role/kind, ready for a second pill.
    pub(crate) pending_engine: DbEngine,
    /// Dialog open counter + the counter value when Settings opened.
    /// Cmd+, toggles Settings off only when no other dialog opened since
    /// (top must be Settings); otherwise Settings stacks on top instead
    /// of closing whatever is showing. Cells: every open site has only
    /// &self in some cases (dialog builders re-run every render).
    pub(crate) dialog_seq: std::cell::Cell<u64>,
    pub(crate) settings_seq: std::cell::Cell<Option<u64>>,
    /// Password field value when a Keychain-mode dialog opened (None
    /// otherwise). Keychain mode saves only *typed* changes: an untouched
    /// blank field keeps the stored entry instead of deleting it.
    pub(crate) password_snapshot: Option<String>,
    // Dialog form fields (entities persist across dialog open/close).
    pub(crate) name: Entity<InputState>,
    pub(crate) host: Entity<InputState>,
    pub(crate) port: Entity<InputState>,
    pub(crate) service: Entity<InputState>,
    pub(crate) user: Entity<InputState>,
    pub(crate) password: Entity<InputState>,
    /// Password prompt field (Ask mode / Keychain miss). Cleared on submit.
    pub(crate) pwd_prompt: Entity<InputState>,
    /// Transient notice for the status bar ("Saved X", "Connecting…").
    pub(crate) status: SharedString,
    /// `&&name` values defined this session, per connection id. Once defined,
    /// even `&name` reuses the value without prompting (SQL*Plus parity).
    pub(crate) defines: std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// Run waiting on the bind dialog (cleared on submit or cancel).
    pub(crate) pending_bind: Option<PendingBind>,
    /// Run waiting on the connection picker (cleared on pick or cancel).
    pub(crate) pending_pick: Option<PendingPick>,
    /// Session-unlocked passwords, per connection id. Memory only, never
    /// persisted: Ask mode and Keychain-miss prompts land here, and every
    /// connect/run path prefers them over whatever is stored.
    pub(crate) unlocked: std::collections::HashMap<String, String>,
    /// Password prompt in flight (cleared on submit or cancel). `run` is
    /// set when the prompt gates a query run rather than a plain connect.
    pub(crate) pending_password: Option<PendingPassword>,
    /// Dictionary snapshots per connection id for autocomplete. Filled on
    /// the background executor; the provider only clones the `Arc`.
    pub(crate) meta: std::collections::HashMap<String, SharedCache>,
    /// Usage counts `(connection_id, UPPER_LABEL)` bumping executed table
    /// names; feeds completion ranking (recency/frequency boost).
    pub(crate) usage: std::collections::HashMap<(String, String), u64>,
    /// Suggestion popup fires while typing (vs manual shortcut only).
    /// Mirrors `Preferences.completion`; toggled in Settings.
    pub(crate) complete_auto: bool,
    /// Include SYS/SYSTEM/etc. objects in suggestions. Mirrors preferences.
    pub(crate) show_system: bool,
    /// Schema-browser trees, per connection id. Trees appear under their
    /// connection row, independent of the active tab — expand warms the
    /// dictionary via `ensure_meta`, and its completion hook rebuilds.
    pub(crate) browser_open: std::collections::HashSet<String>,
    /// Expanded tree node ids per connection (`s:{schema}`,
    /// `g:{schema}/{group}`, `o:{schema}/{T|V|S}/{object}`); the source of
    /// truth reapplied on every rebuild (filter/cache refresh), fed by
    /// `TreeEvent`s. Per-connection so identical schemas don't mirror.
    pub(crate) browser_expanded: std::collections::HashMap<String, std::collections::HashSet<String>>,
    pub(crate) browser_trees: std::collections::HashMap<String, Entity<TreeState>>,
    /// Client-side tree filter, one input per open connection (entity
    /// persists while open; Change rebuilds that connection's tree).
    pub(crate) browser_filters: std::collections::HashMap<String, Entity<InputState>>,
    /// Window-lifetime subscriptions (OS appearance observer for System
    /// theme mode). Kept alive by ownership, like per-tab `_subs`.
    pub(crate) _subs: Vec<Subscription>,
}

impl SqlHighlandView {
    /// Every view-level dialog/alert open funnels through here so Cmd+,
    /// can tell whether Settings is the top dialog (toggle off) or
    /// something else opened since (stack Settings on top). Cells: several
    /// open sites only hold &self. (The async connect-failed alert has no
    /// view borrow and skips it — worst case there is the old toggle
    /// behavior for that one transient popup.)
    pub(crate) fn note_dialog_open(&self) {
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
        // Whole buffer as a script (SQL Developer F5 equivalent, friendlier
        // chord): paired with Cmd+Enter (statement) as Shift+Cmd+Enter
        // (buffer). No kit Input binding uses it (checked), editor-only.
        cx.bind_keys([KeyBinding::new("shift-cmd-enter", RunScript, Some("Input"))]);
        // Ergonomics: sidebar toggle (VSCode-standard Cmd+B), new
        // connection, font zoom, editor-height step. All context-free
        // (verified free of kit/macOS claims) so they work from the
        // editor, the grid, and the sidebar alike.
        cx.bind_keys([KeyBinding::new("cmd-b", ToggleSidebar, None)]);
        cx.bind_keys([KeyBinding::new("cmd-shift-n", NewConnection, None)]);
        cx.bind_keys([KeyBinding::new("cmd-=", ZoomIn, None)]);
        cx.bind_keys([KeyBinding::new("cmd--", ZoomOut, None)]);
        cx.bind_keys([KeyBinding::new("cmd-0", ZoomReset, None)]);
        cx.bind_keys([KeyBinding::new("ctrl-cmd-up", GrowEditor, None)]);
        cx.bind_keys([KeyBinding::new("ctrl-cmd-down", ShrinkEditor, None)]);
        // Dismiss the bottom pane (VSCode panel-toggle parallel). Free
        // in the kit and macOS; context-free so it fires from the
        // editor, grid, and sidebar alike.
        cx.bind_keys([KeyBinding::new("cmd-j", DismissResults, None)]);
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
            editor_split: cx.new(|_| ResizableState::default()),
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

    pub(crate) fn tab_index(&self, tab_id: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == tab_id)
    }

    pub(crate) fn tab_by_id(&mut self, tab_id: &str) -> Option<&mut QueryTab> {
        self.tabs.iter_mut().find(|t| t.id == tab_id)
    }

    pub(crate) fn active_tab(&self) -> &QueryTab {
        &self.tabs[self.active]
    }

    fn connection_name(&self, id: &Option<String>) -> String {
        id.as_ref()
            .and_then(|cid| self.connections.iter().find(|c| &c.id == cid))
            .map(|c| c.name.clone())
            .unwrap_or_else(|| "Select connection".to_string())
    }

    fn make_tab(&mut self, spec: NewTabSpec, window: &mut Window, cx: &mut Context<Self>) {
        let NewTabSpec {
            id,
            name,
            connection_id,
            text,
            kind,
        } = spec;
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
            let show_document: ShowDocumentHook = Rc::new(move |params, window, cx| {
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
            .new(|cx| {
                TableState::new(ResultsDelegate::empty(), window, cx)
                    .cell_selectable(true)
                    // No column-select mode: header clicks must not select
                    // whole columns — header labels are plain text, copied
                    // with a normal drag-select + Cmd+C (see render_th).
                    .col_selectable(false)
            });
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
                    // Column-select mode is off at the table, so header
                    // clicks never reach here; keyboard column nav still
                    // can — it clears, as before.
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
            hide_results: false,
            output_text: cx.new(|cx| TextareaState::new(window, cx)),
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
            self.make_tab(
                NewTabSpec {
                    id: saved.id,
                    name,
                    connection_id,
                    text,
                    kind: TabKind::Query,
                },
                window,
                cx,
            );
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
            self.make_tab(
                NewTabSpec {
                    id,
                    name,
                    connection_id: None,
                    text,
                    kind: TabKind::Query,
                },
                window,
                cx,
            );
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
            self.persist_tabs(cx);
        }
        self.active = 0;
    }

    pub(crate) fn add_tab(
        &mut self,
        connection_id: Option<String>,
        text: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> String {
        self.untitled_counter += 1;
        let id = uuid::Uuid::new_v4().to_string();
        let name = tab_name_from_sql(&text, &format!("Untitled {}", self.untitled_counter));
        self.make_tab(
            NewTabSpec {
                id: id.clone(),
                name,
                connection_id,
                text: text.clone(),
                kind: TabKind::Query,
            },
            window,
            cx,
        );
        // Persist immediately so a crash before the first keystroke loses nothing.
        if let Err(e) = TabsManifest::write_draft(&id, &text) {
            self.status = format!("Draft save failed: {e:#}").into();
            cx.notify();
        }
        self.persist_tabs(cx);
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
            NewTabSpec {
                id: id.clone(),
                name: title,
                connection_id: Some(conn_id.to_string()),
                text: String::new(),
                kind: TabKind::Viewer {
                    owner: owner.clone(),
                    name: name.clone(),
                    kind,
                },
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
                self.make_tab(
                    NewTabSpec {
                        id,
                        name,
                        connection_id: None,
                        text: String::new(),
                        kind: TabKind::Query,
                    },
                    window,
                    cx,
                );
            }
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            self.persist_tabs(cx);
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

    pub(crate) fn select_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(crate) fn cycle_tab(&mut self, dir: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.tabs.len() < 2 {
            return;
        }
        let next = (self.active as isize + dir).rem_euclid(self.tabs.len() as isize) as usize;
        self.select_tab(next, window, cx);
    }

    pub(crate) fn persist_tabs(&mut self, cx: &mut Context<Self>) {
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
        // A failed save must shout: silent loss (disk-full, permissions)
        // is worse than an alarming status line.
        if let Err(e) = manifest.save() {
            self.status = format!("Tabs save failed: {e:#}").into();
            cx.notify();
        }
    }

    pub(crate) fn save_active_tab_as(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                        this.persist_tabs(cx);
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

    pub(crate) fn save_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                self.persist_tabs(cx);
                cx.notify();
            }
            Err(err) => {
                self.status = format!("Save failed: {err}").into();
                cx.notify();
            }
        }
    }

    pub(crate) fn open_sql_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
                    this.make_tab(
                        NewTabSpec {
                            id,
                            name,
                            connection_id: None,
                            text,
                            kind: TabKind::Query,
                        },
                        window,
                        cx,
                    );
                    if let Some(tab) = this.tabs.last_mut() {
                        tab.path = Some(path);
                        tab.file_stamp = Some(stamp);
                        tab.dirty = false;
                    }
                    let ix = this.tabs.len() - 1;
                    this.select_tab(ix, window, cx);
                    this.persist_tabs(cx);
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
                        this.persist_tabs(cx);
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

    // -- Connection dialog (see connection_dialog.rs) ----------------------------
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
    pub(crate) fn effective_password(&self, cfg: &ConnectionConfig) -> Option<String> {
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

        let session = self.pool.get_or_create(&cfg.id);
        let conn_id = cfg.id.clone();
        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    let mut guard = lock(&session);
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
    pub(crate) fn editor_focused(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
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
        // `@`-directive lines run the script file named on the caret's
        // own line (a bare `@file` has no terminator, so statement
        // splitting would merge it with whatever follows — line
        // semantics instead). Everything else runs as one statement.
        if parse_at_directive(&line_at(&text, cursor)).is_some() {
            let tab_id = self.active_tab().id.clone();
            let line = line_at(&text, cursor);
            self.start_script_run(&tab_id, line, window, cx);
            return;
        }
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

    // -- Run executors (see run.rs) --------------------------------------------
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
                    let mut session = lock(&session_bg);
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

    // -- Autocomplete provider (see providers.rs) -------------------------------

    /// Connected user's schema (for own-schema ranking); "" when unbound.
    pub(crate) fn own_schema_of(&self, conn_id: &Option<String>) -> String {
        conn_id
            .as_deref()
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
            .map(|c| c.user.clone())
            .unwrap_or_default()
    }

    // (ensure_meta lives in browser.rs)
    // (trigger_complete lives in providers.rs)

    // (bump_usage lives in providers.rs)

    /// Copy the grid selection to the clipboard: the most recently selected
    /// cell or row. Silent no-op with no selection.
    fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Native text selection wins wherever it exists (grid header
        // labels, output-pane message): grid cell/row copy below is the
        // fallback when nothing is selected as text. Editor inputs match
        // neither copy context, so native copy there is untouched.
        let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            return;
        }
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

    // -- Schema browser (see browser.rs) --------------------------------------

    /// Render one open connection's schema tree. Object rows click through
    /// to viewer tabs; folders toggle via the kit's own row handling.
    pub(crate) fn render_browser_tree(&self, conn_id: &str, cx: &mut Context<Self>) -> impl IntoElement {
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
                let chevron: Option<KitIcon> = folder.then_some(if expanded {
                    KitIcon::ChevronDown
                } else {
                    KitIcon::ChevronRight
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
                // handlers and clicks died silently). The debug_assert
                // guards the scheme in debug; the eprintln keeps release
                // builds from failing silently (view updates are illegal
                // mid-render — leases — so logging is the only signal).
                let target = crate::schema::parse_object_id(&ids);
                if ids.starts_with("o:") && target.is_none() {
                    debug_assert!(false, "unparsed object row {ids}");
                    eprintln!("unparsed schema-browser object row: {ids}");
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

    // (render_connection_row + render_sidebar live in sidebar.rs)

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
                                                this.persist_tabs(cx);
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
            .on_action(cx.listener(|this, _: &RunScript, window, cx| {
                // Same focus gate: the Input-scoped binding must not fire
                // from dialog fields or the picker search.
                if this.editor_focused(window, cx) {
                    let tab_id = this.active_tab().id.clone();
                    this.run_buffer_as_script(&tab_id, window, cx);
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
                        Button::new("run-script")
                            .secondary()
                            .small()
                            .w(px(ACTION_BUTTON_W))
                            .icon(KitIcon::FileTerminal)
                            .label("Script")
                            .tooltip("Run buffer as script (⇧⌘↵)")
                            .loading(tab.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                let tab_id = this.active_tab().id.clone();
                                this.run_buffer_as_script(&tab_id, window, cx);
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
                    .test_support()
                    .when_some(ring, |this, ring| {
                        this.border_1().border_color(ring).rounded_md()
                    })
                    .child(
                        Editor::new(&editor).size_full(),
                    ),
            )
    }

    fn render_results(
        &self,
        tab: &QueryTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // min_w_0 + overflow_hidden: without them flex items refuse to shrink
        // below the table's full content width, the table sees an unbounded
        // viewport, renders every column (no virtualization), and its
        // horizontal scrollbar never engages. min_h_0 likewise for height:
        // viewer tabs have no resizable panel forcing a pixel height, so
        // without it the virtualized body collapses to zero rows while the
        // fixed-height header/export/column rows still paint.
        if tab.output.is_some() {
            self.render_output_pane(tab, window, cx).into_any_element()
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
                    h_flex()
                        .w_full()
                        .justify_end()
                        .items_center()
                        .gap_1()
                        .px_2()
                        .pt_2()
                        .pb_1()
                        .child(
                            Button::new("dismiss-results")
                                .ghost()
                                .small()
                                .icon(KitIcon::X)
                                .tooltip("Dismiss results (⌘J)")
                                .on_click(cx.listener({
                                    let dismiss_tab = tab.id.clone();
                                    move |this, _: &ClickEvent, _, cx| {
                                        this.dismiss_results(&dismiss_tab, cx);
                                    }
                                })),
                        )
                        .child(
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
    /// failures (red) and non-query confirmations (neutral) alike. The
    /// message is a read-only text area (caret, select, native Cmd+C); a
    /// new run clears the output and reopens automatically. Dismiss hides
    /// the whole bottom pane, showing only the query editor.
    fn render_output_pane(
        &self,
        tab: &QueryTab,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let output = tab.output.clone().unwrap_or(Output::info(""));
        let tab_id = tab.id.clone();
        // Sync the read-only view from the message (only when different,
        // so caret and selection survive repaints). A view-entity update
        // would double-lease here — this is a *different* entity, which
        // is safe — and the guard keeps it a no-op past the first frame.
        let message = output.text.clone();
        tab.output_text.update(cx, |s, cx| {
            if s.value() != message {
                s.set_value(message, window, cx);
            }
        });
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
            .id("output-pane")
            .test_support()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .p_2()
            .gap_2()
            // No focus/context/copy plumbing by design: the message below
            // is a real (read-only) text area — caret, selection, and the
            // kit's native Input-context Cmd+C all come free.
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_color(accent).child(icon))
                    .child(div().text_sm().text_color(accent).child(title))
                    .child(div().flex_1())
                    // Icon-only ×, matching the grid's dismiss control.
                    .child(
                        Button::new("output-dismiss")
                            .ghost()
                            .small()
                            .icon(KitIcon::X)
                            .tooltip("Dismiss results (⌘J)")
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                // Dismiss means show only the query window:
                                // the whole bottom pane (output or grid)
                                // goes away until the next run.
                                this.dismiss_results(&tab_id, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p_3()
                    .rounded_md()
                    .bg(bg)
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    // Read-only text area: caret, select, native Cmd+C.
                    .child(
                        Textarea::new(&tab.output_text)
                            .readonly(true)
                            .appearance(false)
                            .bordered(false)
                            .size_full(),
                    ),
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

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.active_tab();
        // Object-viewer tabs (schema browser): grid only, no editor.
        // Layout mirrors the query split (resizable panel + body) on
        // purpose: the grid is virtualized and needs the resizable's
        // definite pixel sizing — a pure flex chain collapses its body
        // to zero rows while fixed-height siblings still paint.
        let content: AnyElement = if matches!(tab.kind, TabKind::Viewer { .. }) {
            let body: AnyElement = if tab.output.is_some() {
                self.render_output_pane(tab, window, cx).into_any_element()
            } else if tab.has_result {
                self.render_results(tab, window, cx).into_any_element()
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
            // Dismissed (output Dismiss or grid close): editor takes the
            // full height — no empty bottom pane. A fresh tab (never ran,
            // no output) renders the same way. Any new run reopens.
            let dismissed = tab.hide_results;
            let fresh = !tab.has_result && tab.output.is_none();
            if dismissed || fresh {
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_editor(cx))
                    .into_any_element()
            } else {
                let body: AnyElement = match tab.output.is_some() {
                    true => self.render_output_pane(tab, window, cx).into_any_element(),
                    false => self.render_results(tab, window, cx).into_any_element(),
                };
                v_resizable("query-split")
                    // Owned state: keyboard steps (`resize_panel`) and
                    // mouse drags share it, so neither fights the other.
                    // The panel's `.size()` below is initial-only.
                    .with_state(&self.editor_split)
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
            // Dismiss lives here too (same bubble-path reason as the tab
            // actions above; dialogs are outside every root, so modals
            // never see it).
            .on_action(cx.listener(|this, _: &DismissResults, _, cx| {
                let tab_id = this.active_tab().id.clone();
                this.dismiss_results(&tab_id, cx);
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

    /// Dismiss the bottom pane (output or grid): show only the query
    /// window until the next run reopens it. Shared by the output-pane
    /// Dismiss, the grid's close button, and the Cmd+J action.
    pub(crate) fn dismiss_results(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        if let Some(t) = self.tab_by_id(tab_id) {
            t.output = None;
            t.hide_results = true;
        }
        cx.notify();
    }
    // (toggle_sidebar + set_sidebar_collapsed live in sidebar.rs)

    /// Editor font zoom: step the saved size by `delta` points, clamped
    /// to the Settings stepper bounds (10..24), then persist + apply.
    /// Takes `&mut Context` directly — `Context` derefs to `App`, which
    /// is all `apply_font_prefs` needs.
    fn zoom_font(&mut self, delta: i32, cx: &mut Context<Self>) {
        let mut prefs = Preferences::load();
        prefs.font_size = (prefs.font_size as i32 + delta).clamp(10, 24) as u32;
        if let Err(e) = prefs.save() {
            self.status = format!("Preferences save failed: {e:#}").into();
        }
        crate::guitheme::apply_font_prefs(&prefs, cx);
        cx.notify();
    }

    /// Reset the editor font to the 13pt default.
    fn zoom_font_reset(&mut self, cx: &mut Context<Self>) {
        let mut prefs = Preferences::load();
        prefs.font_size = 13;
        if let Err(e) = prefs.save() {
            self.status = format!("Preferences save failed: {e:#}").into();
        }
        crate::guitheme::apply_font_prefs(&prefs, cx);
        cx.notify();
    }

    /// Step the query-editor pane height through the owned splitter
    /// state (the same state mouse drags write), clamped to the panel's
    /// own range (160..900). No-op before first layout (empty sizes).
    fn step_editor_h(&mut self, dir: i32, window: &mut Window, cx: &mut Context<Self>) {
        let cur: f32 = self
            .editor_split
            .read(cx)
            .sizes()
            .first()
            .map(|s| (*s).into())
            .unwrap_or(300.0);
        let next = (cur + dir as f32 * 48.0).clamp(160.0, 900.0);
        self.editor_split.update(cx, |s, cx| {
            s.resize_panel(0, px(next), window, cx)
        });
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
pub(crate) fn env_color(env: Environment, cx: &App) -> Option<Hsla> {
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
// (settings_pick_row lives in settings_dialog.rs)
pub(crate) fn env_tag(env: Environment, cx: &App) -> Option<AnyElement> {    let label = env.label()?;
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

    // (dialog_field lives in connection_dialog.rs)

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
                    MenuItem::action("New Connection…", NewConnection),
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
                    MenuItem::action("Run as Script", RunScript),
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
                    MenuItem::action("Toggle Sidebar", ToggleSidebar),
                    MenuItem::Separator,
                    MenuItem::action("Next Tab", NextTab),
                    MenuItem::action("Previous Tab", PrevTab),
                    MenuItem::Separator,
                    MenuItem::action("Dismiss Results", DismissResults),
                    MenuItem::Separator,
                    MenuItem::action("Zoom In", ZoomIn),
                    MenuItem::action("Zoom Out", ZoomOut),
                    MenuItem::action("Reset Zoom", ZoomReset),
                    MenuItem::Separator,
                    MenuItem::action("Grow Editor", GrowEditor),
                    MenuItem::action("Shrink Editor", ShrinkEditor),
                ],
                disabled: false,
            },
    ]
}

    // (DialogPick + dialog_pills live in connection_dialog.rs)

impl Render for SqlHighlandView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = if self.sidebar_collapsed {
            h_flex()
                .size_full()
                .child(self.render_sidebar(cx))
                .child(self.render_main(window, cx))
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
                .child(self.render_main(window, cx))
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
            // Ergonomics singletons (sole registrations, same reasoning
            // as PickConnection above): sidebar toggle, new connection,
            // font zoom, editor-height step.
            .on_action(cx.listener(|this, _: &ToggleSidebar, _window, cx| {
                this.set_sidebar_collapsed(!this.sidebar_collapsed, cx);
            }))
            .on_action(cx.listener(|this, _: &NewConnection, window, cx| {
                this.start_add(window, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _window, cx| {
                this.zoom_font(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _window, cx| {
                this.zoom_font(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomReset, _window, cx| {
                this.zoom_font_reset(cx);
            }))
            .on_action(cx.listener(|this, _: &GrowEditor, window, cx| {
                this.step_editor_h(1, window, cx);
            }))
            .on_action(cx.listener(|this, _: &ShrinkEditor, window, cx| {
                this.step_editor_h(-1, window, cx);
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
