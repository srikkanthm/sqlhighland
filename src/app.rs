//! UI: connections sidebar + tabbed SQL editors + results grids + status bar.
//!
//! Model: one live Oracle session per saved connection (see `session.rs`),
//! shared by tabs. Each tab binds to a connection it can switch, and owns
//! its editor, grid, and run state. Editor drafts auto-save (debounced) and
//! tabs restore on launch.
//!
//! Blocking Oracle calls run on the background executor; the view is only
//! ever mutated on the UI thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    Editor, EditorState, Input, InputContentType, InputEvent, InputState,
};
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit_assets::IconName as KitIcon;
use sqlhighland::config::{Preferences, SavedConfig, SavedTab, TabsManifest, THEME_LIST};
use sqlhighland::db::{FETCH_CAP, FETCH_CHUNK, DbClient, FetchPage, OracledbSession};
use sqlhighland::model::{ColumnInfo, ConnectionConfig, csv_row, tab_name_from_sql};
use sqlhighland::session::SessionPool;
use sqlhighland::sql::{
    StatementKind, exec_summary, format_sql, is_dml, statement_at, statement_kind, txn_end,
};

const DEFAULT_SQL: &str = "SELECT user, sysdate FROM dual;";
/// Quiet period before an editor change is flushed to its draft file.
const DRAFT_DEBOUNCE: Duration = Duration::from_millis(1500);

gpui_kit::actions!(
    sqlhighland,
    [
        RunQuery,
        CopySelection,
        FormatQuery,
        CommitTxn,
        RollbackTxn,
        OpenSettings
    ]
);

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
    rows
        .into_iter()
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
                d.rows.get(row_ix).and_then(|row| {
                    row.get(col_ix - 1).map(|c| c.clone().unwrap_or_default())
                })
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
                if n == 0 { 0 } else { n + 1 }
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
        let name = self.with_data(
            |d| d.columns.get(col_ix - 1).map(|c| c.name.clone()),
            None,
        );
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
                                data.rows.extend(
                                    to_shared(page.rows).into_iter().take(take),
                                );
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
            fetch.view
                .update(cx, |view, cx| {
                    let Some(tab) = view.tab_by_id(&fetch.tab_id) else {
                        return;
                    };
                    if !tab
                        .table
                        .read(cx)
                        .delegate()
                        .is_current(&fetch)
                    {
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
    PopupMenuItem::new(label).icon(icon).on_click(move |_, window, cx| {
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
    save_task: Option<Task<()>>,
    _subs: Vec<Subscription>,
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
    untitled_counter: usize,
    sidebar_collapsed: bool,
    /// Index being edited in the connection dialog (`None` = adding).
    editing: Option<usize>,
    // Dialog form fields (entities persist across dialog open/close).
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    service: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    /// Transient notice for the status bar ("Saved X", "Connecting…").
    status: SharedString,
    /// Window-lifetime subscriptions (OS appearance observer for System
    /// theme mode). Kept alive by ownership, like per-tab `_subs`.
    _subs: Vec<Subscription>,
}

impl SqlHighlandView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let connections = SavedConfig::default_path()
            .ok()
            .and_then(|p| SavedConfig::load(&p).ok())
            .map(|c| c.connections)
            .unwrap_or_default();
        let blank = ConnectionConfig::default();

        let name = cx.new(|cx| InputState::new(window, cx).placeholder("My database"));
        let host = cx.new(|cx| InputState::new(window, cx).default_value(blank.host.clone()));
        let port = cx.new(|cx| {
            InputState::new(window, cx).default_value(blank.port.to_string())
        });
        let service = cx.new(|cx| {
            InputState::new(window, cx).default_value(blank.service_name.clone())
        });
        let user = cx.new(|cx| InputState::new(window, cx).default_value(blank.user.clone()));
        let password = cx.new(|cx| InputState::new(window, cx).placeholder("password"));

        // Cmd+Enter runs the statement under the cursor. Scoped to the
        // editor's own `Input` key context; the handler double-checks focus.
        // Cmd+C copies the grid selection, scoped to the table's `DataTable`
        // context so the editor's own copy is untouched.
        // Shortcuts below verified free in the kit's `Input` bindings
        // (notably cmd-shift-f is taken by Replace, hence shift-alt-f).
        cx.bind_keys([KeyBinding::new("cmd-enter", RunQuery, Some("Input"))]);
        cx.bind_keys([KeyBinding::new("cmd-c", CopySelection, Some("DataTable"))]);
        cx.bind_keys([KeyBinding::new("shift-alt-f", FormatQuery, Some("Input"))]);
        cx.bind_keys([KeyBinding::new("cmd-shift-c", CommitTxn, Some("Input"))]);
        cx.bind_keys([KeyBinding::new("cmd-shift-r", RollbackTxn, Some("Input"))]);

        let mut this = Self {
            pool: SessionPool::new(),
            live: std::collections::HashSet::new(),
            connections,
            tabs: Vec::new(),
            active: 0,
            untitled_counter: 0,
            sidebar_collapsed: false,
            editing: None,
            name,
            host,
            port,
            service,
            user,
            password,
            status: "".into(),
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
        let table = cx.new(|cx| {
            TableState::new(ResultsDelegate::empty(), window, cx).cell_selectable(true)
        });
        let tab_id = id.clone();
        let table_tab_id = id.clone();
        let subs = vec![
            cx.subscribe_in(
                &editor,
                window,
                move |this, _, ev: &InputEvent, _, cx| {
                    if matches!(ev, InputEvent::Change) {
                        this.schedule_draft_save(&tab_id, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &table,
                window,
                move |this, _, ev: &TableEvent, _, _| {
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
                },
            ),
        ];
        self.tabs.push(QueryTab {
            id,
            name: name.into(),
            connection_id,
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
            save_task: None,
            _subs: subs,
        });
    }

    fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let manifest = TabsManifest::load().unwrap_or_default();
        let manifest_existed = TabsManifest::manifest_path()
            .map(|p| p.exists())
            .unwrap_or(false);
        for saved in manifest.tabs {
            let connection_id = saved
                .connection_id
                .filter(|cid| self.connections.iter().any(|c| &c.id == cid));
            let text = TabsManifest::read_draft(&saved.id);
            self.untitled_counter += 1;
            let name = if saved.name.is_empty() {
                format!("Untitled {}", self.untitled_counter)
            } else {
                saved.name.clone()
            };
            self.make_tab(saved.id, name, connection_id, text, window, cx);
        }
        if self.tabs.is_empty() {
            // First launch (or empty manifest): one starter tab.
            let text = if manifest_existed {
                String::new()
            } else {
                DEFAULT_SQL.to_string()
            };
            self.add_tab(self.active_connection(), text, window, cx);
        }
        self.active = 0;
    }

    fn active_connection(&self) -> Option<String> {
        self.tabs.get(self.active).and_then(|t| t.connection_id.clone())
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
        cx.notify();
        id
    }

    fn close_tab(&mut self, tab_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.tab_index(tab_id) {
            let removed = self.tabs.remove(ix);
            TabsManifest::delete_draft(&removed.id);
            if self.tabs.is_empty() {
                // Always keep one tab open; no dirty prompts needed — every
                // keystroke is already auto-saved.
                self.untitled_counter += 1;
                let id = uuid::Uuid::new_v4().to_string();
                let name = format!("Untitled {}", self.untitled_counter);
                self.make_tab(id, name, None, String::new(), window, cx);
            }
            self.active = self.active.min(self.tabs.len().saturating_sub(1));
            self.persist_tabs();
            cx.notify();
        }
    }

    fn select_tab(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.tabs.len() {
            return;
        }
        self.active = ix;
        let editor = self.tabs[ix].editor.clone();
        editor.update(cx, |editor, cx| editor.focus(window, cx));
        cx.notify();
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
                })
                .collect(),
        };
        let _ = manifest.save();
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
        let task = cx.spawn(async move |view, cx| {
            bg.timer(DRAFT_DEBOUNCE).await;
            let outcome = bg
                .spawn(async move {
                    TabsManifest::write_draft(&write_id, &text).map_err(|e| e.to_string())
                })
                .await;
            view.update(cx, |this, cx| {
                let Some(tab) = this.tab_by_id(&save_id) else {
                    return; // Tab closed while waiting; draft already removed.
                };
                match outcome {
                    Ok(()) => {
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
        }
    }

    fn fill_form(&self, cfg: &ConnectionConfig, window: &mut Window, cx: &mut Context<Self>) {
        self.name.update(cx, |s, cx| s.set_value(cfg.name.clone(), window, cx));
        self.host.update(cx, |s, cx| s.set_value(cfg.host.clone(), window, cx));
        self.port.update(cx, |s, cx| {
            s.set_value(cfg.port.to_string(), window, cx)
        });
        self.service.update(cx, |s, cx| {
            s.set_value(cfg.service_name.clone(), window, cx)
        });
        self.user.update(cx, |s, cx| s.set_value(cfg.user.clone(), window, cx));
        self.password.update(cx, |s, cx| {
            s.set_value(cfg.password.clone(), window, cx)
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
        self.fill_form(&ConnectionConfig::default(), window, cx);
        self.open_connection_dialog("Add connection", window, cx);
    }

    fn start_edit(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let title = format!("Edit {}", self.connections[ix].name);
        self.editing = Some(ix);
        let cfg = self.connections[ix].clone();
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
        let prefs = Preferences::load();
        // Owned for the 'static dialog builder below.
        let view = view.clone();
        window.open_dialog(cx, move |dialog, _, cx| {
            let muted = cx.theme().muted_foreground;
            let hover_bg = cx.theme().accent;
            // Live applied selection: reopening the dialog after a pick must
            // show the new name — if it doesn't, applying (not repainting)
            // is broken.
            let active = format!("Active: {}", prefs.theme_name());
            let current = prefs.theme_name();
            let mut theme_rows = Vec::new();
            for (row_ix, name) in THEME_LIST.into_iter().enumerate() {
                let selected = name == current;
                let id = ("settings-theme", row_ix);
                let row_view = view.clone();
                theme_rows.push(
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
                            window.close_dialog(cx);
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
            dialog
                .title("Settings")
                .w(px(340.))
                .child(
                    v_flex()
                        .gap_2()
                        .w_full()
                        .child(div().text_xs().text_color(muted).child("Applies immediately."))
                        .child(div().text_xs().text_color(muted).child(active))
                        .child(div().text_xs().text_color(muted).child("Theme"))
                        .child(v_flex().gap_1().children(theme_rows)),
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
        window.open_dialog(cx, move |dialog, _, cx| {
            let save_view = view.clone();
            let muted = cx.theme().muted_foreground;
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
                                .child(div().flex_1().child(dialog_field("Port", &port, false, muted)))
                                .child(
                                    div()
                                        .flex_1()
                                        .child(dialog_field("Service", &service, false, muted)),
                                ),
                        )
                        .child(dialog_field("User", &user, false, muted))
                        .child(dialog_field("Password", &password, true, muted)),
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
        let Some(cfg) = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .cloned()
        else {
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
                    }
                    WorkOutcome::Failed(msg) => {
                        this.live.remove(&conn_id);
                        this.status = format!("Connection failed: {msg}").into();
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
                self.run_sql(&tab_id, sql, cx);
            }
            None => {
                let ix = self.active;
                self.tabs[ix].result_meta = "No statement at cursor".into();
                cx.notify();
            }
        }
    }

    fn run_sql(&mut self, tab_id: &str, sql: String, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].busy {
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
        let Some(cfg) = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .cloned()
        else {
            self.tabs[ix].output = Some(Output::error("Connection not found — pick another"));
            cx.notify();
            return;
        };
        self.tabs[ix].busy = true;
        self.tabs[ix].output = None;
        self.tabs[ix].run_token = self.tabs[ix].run_token.wrapping_add(1);
        self.tabs[ix].run_started = Some(std::time::Instant::now());
        let run_token = self.tabs[ix].run_token;
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
            cx.spawn(async move |_, cx| {
                loop {
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
                                .start_query(&sql, FETCH_CHUNK)
                                .map(|(columns, page, id)| {
                                    Outcome::Rows(columns, page, id, inner.elapsed().as_millis())
                                })
                                .map_err(|e| e.to_string())
                        }
                        StatementKind::Execute => session
                            .exec(&sql)
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
                        this.tabs[ix].result_meta =
                            format!("Failed · {elapsed_ms} ms").into();
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
            let row = || {
                table
                    .selected_row()
                    .and_then(|r| delegate.row_csv(r))
            };
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
        let dot = if is_live { cx.theme().success } else { cx.theme().muted_foreground };
        let view = cx.entity().downgrade();
        let conn_id = cfg.id.clone();
        div()
            .id(("conn-row", ix))
            .w_full()
            .p_2()
            .rounded_md()
            .context_menu(move |menu, _, _| {
                let connect_label = if is_live { "Disconnect" } else { "Connect" };
                let connect_icon = if is_live { KitIcon::Unplug } else { KitIcon::Plug };
                let connect_op = if is_live {
                    ConnMenuOp::Disconnect
                } else {
                    ConnMenuOp::Connect
                };
                menu.item(conn_menu_item(connect_label, connect_icon, view.clone(), conn_id.clone(), connect_op))
                    .item(conn_menu_item("Edit…", KitIcon::SquarePen, view.clone(), conn_id.clone(), ConnMenuOp::Edit))
                    .separator()
                    .item(conn_menu_item("Delete", KitIcon::X, view.clone(), conn_id.clone(), ConnMenuOp::Delete))
            })
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().size(px(8.)).rounded_full().bg(dot))
                    .child(
                        v_flex()
                            .flex_1()
                            .child(div().text_sm().child(cfg.name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{}@{}/{}", cfg.user, cfg.host, cfg.service_name)),
                            ),
                    ),
            )
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        if self.sidebar_collapsed {
            return div()
                .w(px(44.))
                .h_full()
                .border_r_1()
                .border_color(cx.theme().border)
                .items_center()
                .p_1()
                .child(
                    Button::new("expand")
                        .icon(KitIcon::PanelLeftOpen)
                        .ghost()
                        .on_click(cx.listener(Self::toggle_sidebar)),
                )
                .into_any_element();
        }

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_1()
                    .p_2()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().text_sm().child("Connections"))
                    .child(
                        Button::new("collapse")
                            .icon(KitIcon::PanelLeftClose)
                            .ghost()
                            .on_click(cx.listener(Self::toggle_sidebar)),
                    )
                    .child(
                        Button::new("add")
                            .icon(KitIcon::Plus)
                            .ghost()
                            .tooltip("Add connection")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.start_add(window, cx);
                            })),
                    )
                    .child(
                        Button::new("settings")
                            .icon(KitIcon::Settings)
                            .ghost()
                            .tooltip("Settings (⌘,)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.open_settings(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .overflow_y_scrollbar()
                    .p_2()
                    .child(
                        v_flex()
                            .gap_1()
                            .children(
                                (0..self.connections.len())
                                    .map(|ix| self.render_connection_row(ix, cx)),
                            ),
                    ),
            )
            .into_any_element()
    }

    fn render_tab_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        TabBar::new("query-tabs")
            .w_full()
            .selected_index(self.active)
            .on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.select_tab(*ix, window, cx);
            }))
            .children(self.tabs.iter().enumerate().map(|(ix, tab)| {
                let tab_id = tab.id.clone();
                Tab::new()
                    .label(tab.name.clone())
                    .selected(self.active == ix)
                    .suffix(
                        Button::new(("tab-close", ix))
                            .icon(KitIcon::X)
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                this.close_tab(&tab_id, window, cx);
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
                        let conn = this.active_connection();
                        this.add_tab(conn, String::new(), window, cx);
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
                        let mut menu = menu;
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
                            menu = menu.item(
                                PopupMenuItem::new(format!("{prefix}{name}")).on_click(
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
                                ),
                            );
                        }
                        menu
                    }),
            )
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
            .on_action(cx.listener(|this, _: &RollbackTxn, window, cx| {
                if this.editor_focused(window, cx) {
                    this.rollback_now(cx);
                }
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(self.render_connection_picker(cx))
                    .child(div().flex_1())
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
                        Button::new("commit")
                            .icon(KitIcon::Check)
                            .label("Commit")
                            .tooltip("Commit transaction (⇧⌘C)")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.commit_now(cx);
                            })),
                    )
                    .child(
                        Button::new("rollback")
                            .icon(KitIcon::Undo2)
                            .label("Rollback")
                            .tooltip("Roll back transaction (⇧⌘R)")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.rollback_now(cx);
                            })),
                    )
                    .child(
                        Button::new("format")
                            .icon(KitIcon::WandSparkles)
                            .label("Format")
                            .tooltip("Format SQL (⇧⌥F)")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.format_now(window, cx);
                            })),
                    )
                    .child(
                        Button::new("run")
                            .primary()
                            .icon(KitIcon::Play)
                            .label("Run")
                            .tooltip("Run statement at cursor (⌘↵)")
                            .loading(tab.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run_at_cursor(window, cx);
                            })),
                    )
                    .when(tab.busy, |this| {
                        let tab_id = tab.id.clone();
                        this.child(
                            Button::new("cancel-run")
                                .danger()
                                .icon(KitIcon::X)
                                .label("Cancel")
                                .tooltip("Stop waiting — the server finishes in the background and its results are discarded")
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    this.cancel_run(&tab_id, cx);
                                })),
                        )
                    }),
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
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .p_2()
                .child(render_tab_table(&tab.table))
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
        let is_error = output.kind == OutputKind::Error;
        let (accent, title, icon, bg) = if is_error {
            (
                cx.theme().danger,
                "Query failed",
                KitIcon::TriangleAlert,
                cx.theme().muted,
            )
        } else {
            (cx.theme().success, "Statement executed", KitIcon::Check, cx.theme().muted)
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
                    .child(output.text),
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
                    if live { cx.theme().success } else { cx.theme().muted_foreground },
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
            .child(div().text_color(cx.theme().muted_foreground).child(self.status.clone()))
            .child(div().size(px(8.)).rounded_full().bg(dot))
            .child(div().text_color(cx.theme().muted_foreground).child(right))
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.active_tab();
        let body: AnyElement = if !tab.has_result && tab.output.is_none() {
            div()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("Connect, then run a query to see results here.")
                .into_any_element()
        } else {
            self.render_results(tab, cx).into_any_element()
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .on_action(cx.listener(|this, _: &CopySelection, window, cx| {
                this.copy_selection(window, cx);
            }))
            .child(self.render_tab_bar(cx))
            .child(
                v_resizable("query-split")
                    .child(
                        resizable_panel()
                            .size(px(300.))
                            .size_range(px(160.)..px(900.))
                            .flex_none()
                            .child(self.render_editor(cx)),
                    )
                    .child(body),
            )
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
            .child(content)
            .children(Root::render_dialog_layer(window, cx))
    }
}
