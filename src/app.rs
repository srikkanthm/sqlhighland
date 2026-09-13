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

use zeroize::Zeroizing;

use crate::bind_dialog::PendingBind;
use crate::complete::{
    build_alias_map, byte_to_lsp_pos, describe_target, hover_markdown, is_trivia_position,
    qualifier_before, word_at,
};
use crate::config::{CompleteMode, Preferences, SavedConfig, SavedTab, TabsManifest};
use crate::conn_picker::{PendingPick, PickAfter};
use crate::db::{DbClient, OracledbSession};
use crate::filetab::{self, FileStamp};
use crate::metadata::SharedCache;
use crate::model::{
    csv_row, tab_name_from_sql, ColumnInfo, ConnectionConfig, Environment, OracleRole,
    PasswordMode, ServiceKind,
};
use crate::run::file_stem;
use crate::schema::{DbEngine, OracleProvider, SchemaProvider as _};
use crate::session::{lock, SessionPool};
use crate::sql::{format_sql, line_at, parse_at_directive, statement_at, statement_at_range};
use gpui_kit::base::SelectableText;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{
    CompletionProvider, DefinitionProvider, Editor, EditorState, HoverProvider, InputEvent,
    InputState, Textarea, TextareaState,
};
use gpui_kit::component::list::ListItem;
use gpui_kit::component::menu::{ContextMenuExt, DropdownMenu, PopupMenuItem};
use gpui_kit::component::resizable::ResizableState;
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::tree::{tree, TreeState};
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

// Submodules: the view's impl is split by responsibility so app.rs stays
// navigable. Inherent `impl SqlHighlandView` blocks may live in any module.
mod actions;
mod connections;
mod lsp;
mod render;
mod results;
mod tabs;

// Re-exported under `crate::app::…` for sibling modules (run, sidebar,
// dialogs) that import them by that path.
pub(crate) use render::{env_color, env_tag};
pub(crate) use results::{
    describe_fetch, render_tab_table, to_shared, FetchState, ResultData, ResultsDelegate,
};

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

/// The main application view: connections, tabs, sessions, and rendering.
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
    pub(crate) defines:
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// Run waiting on the bind dialog (cleared on submit or cancel).
    pub(crate) pending_bind: Option<PendingBind>,
    /// Run waiting on the connection picker (cleared on pick or cancel).
    pub(crate) pending_pick: Option<PendingPick>,
    /// Session-unlocked passwords, per connection id. Memory only, never
    /// persisted: Ask mode and Keychain-miss prompts land here, and every
    /// connect/run path prefers them over whatever is stored. Values are
    /// [`Zeroizing`], so removing a connection (or dropping the view) wipes
    /// the secret from memory.
    pub(crate) unlocked: std::collections::HashMap<String, Zeroizing<String>>,
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
    pub(crate) browser_expanded:
        std::collections::HashMap<String, std::collections::HashSet<String>>,
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
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("password")
                .masked(true)
        });
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
}

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
