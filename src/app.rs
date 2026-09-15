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
use crate::config::{
    CompleteMode, Preferences, SavedConfig, SavedTab, TabsManifest, UiDensity, FONT_FAMILIES,
    GRID_ROW_HEIGHT_MAX, GRID_ROW_HEIGHT_MIN, THEME_LIST,
};
use crate::conn_picker::{PendingPick, PickAfter};
use crate::db::SharedSession;
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
use gpui_kit::component::searchable_list::{SearchableListItem, SearchableVec};
use gpui_kit::component::select::{SelectEvent, SelectState};
use gpui_kit::component::slider::{SliderEvent, SliderState, SliderValue};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableEvent, TableState};
use gpui_kit::component::tree::{tree, TreeState};
use gpui_kit::component::Size;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit_assets::IconName as KitIcon;

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
        OpenSettings,
        OpenAbout,
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
    RefreshMeta,
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
                ConnMenuOp::RefreshMeta => this.refresh_meta(&conn_id, cx),
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

/// Which run currently owns a tab's `busy` flag, so the Run and Script
/// buttons can each show their own spinner.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RunKind {
    /// A single statement at the cursor (`run_sql`).
    Statement,
    /// A buffer/`@` script run (`run_script`).
    Script,
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
    /// Which run owns `busy` (None when idle). Drives the per-button spinners.
    pub(crate) run_kind: Option<RunKind>,
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
    /// Native bind values for `last_sql`, retained so an export can re-execute
    /// the query on its own session. In-memory only, never persisted.
    pub(crate) last_binds: Vec<crate::db::BindParam>,
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

impl QueryTab {
    /// Replace the retained bind values, wiping the previous ones (they may
    /// hold sensitive literals and are only kept for export re-execution).
    pub(crate) fn set_last_binds(&mut self, binds: &[crate::db::BindParam]) {
        self.wipe_last_binds();
        self.last_binds = binds.to_vec();
    }

    /// Zeroize and drop the retained bind values.
    pub(crate) fn wipe_last_binds(&mut self) {
        use zeroize::Zeroize as _;
        for b in &mut self.last_binds {
            b.value.zeroize();
        }
        self.last_binds.clear();
    }
}

impl Drop for QueryTab {
    fn drop(&mut self) {
        self.wipe_last_binds();
    }
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

/// A (label, value) choice for the font-family `Select`, so "Theme default"
/// can map to the empty string while still showing a friendly label.
#[derive(Clone)]
pub(crate) struct ChoiceItem {
    pub(crate) label: SharedString,
    pub(crate) value: SharedString,
}

impl SearchableListItem for ChoiceItem {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &Self::Value {
        &self.value
    }
}

/// Entity handles the Settings dialog seeds on open. Gathered by the caller —
/// via direct field access when the view is already leased (menu About, the
/// sidebar gear), or `view.read` when it is not — so `open_settings_dialog`
/// never reads the view itself (a nested read under a lease panics).
pub struct SettingsControls {
    pub(crate) result_cap_input: Entity<InputState>,
    pub(crate) grid_density_slider: Entity<SliderState>,
    pub(crate) query_timeout_input: Entity<InputState>,
    pub(crate) csv_delim_input: Entity<InputState>,
    pub(crate) metadata_ttl_input: Entity<InputState>,
    pub(crate) theme_select: Entity<SelectState<SearchableVec<SharedString>>>,
    pub(crate) font_select: Entity<SelectState<SearchableVec<ChoiceItem>>>,
    pub(crate) density_select: Entity<SelectState<SearchableVec<SharedString>>>,
    pub(crate) grid_row_height: u32,
}

/// Responsive level for the main-window toolbars/headers, derived from the
/// measured content width (`SqlHighlandView::main_width`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolbarSize {
    /// Full labels.
    Full,
    /// Action buttons collapse to icons; connection name shortened.
    Compact,
    /// Secondary actions move into a "⋯" overflow menu.
    Minimal,
}

impl ToolbarSize {
    pub(crate) fn for_width(width: f32) -> Self {
        if width >= 780.0 {
            Self::Full
        } else if width >= 460.0 {
            Self::Compact
        } else {
            Self::Minimal
        }
    }

    /// True unless the full labels fit.
    pub(crate) fn compact(self) -> bool {
        self != Self::Full
    }
}

/// Concrete sizes derived from the active [`UiDensity`].
#[derive(Clone, Copy)]
pub(crate) struct Density {
    pub(crate) is_compact: bool,
    /// Size passed to the kit's `TabBar`/`Tab`.
    pub(crate) tab_size: Size,
    /// Action/connection button size.
    pub(crate) button_size: Size,
    /// Form control size (dialog inputs, selects, switches, footer buttons).
    pub(crate) control_size: Size,
    /// Fixed width for labelled action buttons.
    pub(crate) action_button_w: f32,
    /// Gap between inline controls.
    pub(crate) gap: f32,
    /// Sidebar connection-row vertical padding.
    pub(crate) row_py: f32,
    /// Sidebar icon-button size.
    pub(crate) icon_button: f32,
    /// Live status dot diameter.
    pub(crate) dot: f32,
    /// Status bar height.
    pub(crate) status_h: f32,
    /// Pane inner padding.
    pub(crate) pane_pad: f32,
    /// Environment badge width (0 = content-sized).
    pub(crate) badge_w: f32,
    /// Dialog inner padding.
    pub(crate) dialog_pad: f32,
}

impl Density {
    pub(crate) fn for_level(level: UiDensity) -> Self {
        match level {
            UiDensity::Compact => Self {
                is_compact: true,
                tab_size: Size::Small,
                button_size: Size::XSmall,
                control_size: Size::Small,
                action_button_w: 80.0,
                gap: 4.0,
                row_py: 2.0,
                icon_button: 20.0,
                dot: 6.0,
                status_h: 26.0,
                pane_pad: 4.0,
                badge_w: 0.0,
                dialog_pad: 6.0,
            },
            UiDensity::Comfortable => Self {
                is_compact: false,
                tab_size: Size::Medium,
                button_size: Size::Small,
                control_size: Size::Medium,
                action_button_w: 104.0,
                gap: 8.0,
                row_py: 4.0,
                icon_button: 24.0,
                dot: 8.0,
                status_h: 32.0,
                pane_pad: 8.0,
                badge_w: 48.0,
                dialog_pad: 8.0,
            },
        }
    }

    /// Height for the viewer header (no editor), aligned with the compact
    /// status bar plus room for its buttons.
    pub(crate) fn viewer_h(&self) -> f32 {
        self.status_h + if self.is_compact { 4.0 } else { 8.0 }
    }
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

/// State for the connection add/edit dialog: the in-progress `pending_*` option
/// pills, the stable form input entities (which persist across dialog opens),
/// and the keychain password snapshot. Split out of [`SqlHighlandView`] so the
/// view's field list stays navigable as features are added.
pub(crate) struct ConnectionDialogState {
    /// Index being edited (`None` = adding).
    pub(crate) editing: Option<usize>,
    /// Pending environment tag, set by start_add/start_edit, mutated by the
    /// dialog's pill row, read by save.
    pub(crate) pending_env: Environment,
    /// Same pattern for role / service-kind / SSL / password-mode rows.
    pub(crate) pending_role: OracleRole,
    pub(crate) pending_service_kind: ServiceKind,
    pub(crate) pending_ssl: bool,
    pub(crate) pending_password_mode: PasswordMode,
    /// Pending database engine. Only Oracle exists today, so the row is
    /// display-only — but the dialog owns the value like role/kind.
    pub(crate) pending_engine: DbEngine,
    /// Password field value when a Keychain-mode dialog opened (None
    /// otherwise). Keychain mode saves only *typed* changes: an untouched
    /// blank field keeps the stored entry instead of deleting it.
    pub(crate) password_snapshot: Option<String>,
    // Form fields (entities persist across dialog open/close).
    pub(crate) name: Entity<InputState>,
    pub(crate) host: Entity<InputState>,
    pub(crate) port: Entity<InputState>,
    pub(crate) service: Entity<InputState>,
    pub(crate) user: Entity<InputState>,
    pub(crate) password: Entity<InputState>,
}

/// Runs paused on a modal: the bind-variable dialog, the connection picker, or
/// the password prompt. At most one is set; each carries enough to resume.
pub(crate) struct PendingOps {
    /// Run waiting on the bind dialog (cleared on submit or cancel).
    pub(crate) bind: Option<PendingBind>,
    /// Run waiting on the connection picker (cleared on pick or cancel).
    pub(crate) pick: Option<PendingPick>,
    /// Password prompt in flight (cleared on submit or cancel). `run` is set
    /// when the prompt gates a query run rather than a plain connect.
    pub(crate) password: Option<PendingPassword>,
}

/// Per-connection autocomplete dictionary caches and schema-browser state.
pub(crate) struct BrowserState {
    /// Dictionary snapshots per connection id for autocomplete. Filled on the
    /// background executor; the provider only clones the `Arc`.
    pub(crate) meta: std::collections::HashMap<String, SharedCache>,
    /// Usage counts `(connection_id, UPPER_LABEL)` bumping executed table
    /// names; feeds completion ranking (recency/frequency boost).
    pub(crate) usage: std::collections::HashMap<(String, String), u64>,
    /// Open schema-browser connections.
    pub(crate) open: std::collections::HashSet<String>,
    /// Expanded tree node ids per connection (`s:{schema}`,
    /// `g:{schema}/{group}`, `o:{schema}/{T|V|S}/{object}`); the source of
    /// truth reapplied on every rebuild (filter/cache refresh), fed by
    /// `TreeEvent`s. Per-connection so identical schemas don't mirror.
    pub(crate) expanded: std::collections::HashMap<String, std::collections::HashSet<String>>,
    pub(crate) trees: std::collections::HashMap<String, Entity<TreeState>>,
    /// Client-side tree filter, one input per open connection (entity
    /// persists while open; Change rebuilds that connection's tree).
    pub(crate) filters: std::collections::HashMap<String, Entity<InputState>>,
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
    /// Measured width of the main content area, updated by `on_prepaint` on
    /// the main root. Drives the responsive toolbars/headers (`ToolbarSize`).
    pub(crate) main_width: std::cell::Cell<f32>,
    /// Owned splitter state for the query editor/results split. Held
    /// (not keyed) so keyboard height steps drive the same state the
    /// mouse drags — `ResizablePanel::size()` is initial-only, which is
    /// why an `editor_h` field never moved the panel.
    pub(crate) editor_split: Entity<ResizableState>,
    /// Owned splitter state for the sidebar | main split. Owned (not the
    /// kit's keyed state) so the fixed sidebar can be re-pinned to its
    /// remembered width when the window resizes: the kit otherwise rescales
    /// a fixed panel proportionally with its flexible sibling.
    pub(crate) main_split: Entity<ResizableState>,
    /// Remembered sidebar width (px). Updated on user drags via
    /// [`ResizablePanelEvent::Resized`], re-applied on window resize.
    pub(crate) sidebar_width: f32,
    /// Last full-window width seen, so the sidebar is re-pinned only when the
    /// container actually changed — never mid-drag.
    pub(crate) last_window_w: std::cell::Cell<f32>,
    /// Connection add/edit dialog: open form, pending option pills, and stable
    /// editor entities. See [`ConnectionDialogState`].
    pub(crate) dialog: ConnectionDialogState,
    /// Dialog open counter + the counter value when Settings opened.
    /// Cmd+, toggles Settings off only when no other dialog opened since
    /// (top must be Settings); otherwise Settings stacks on top instead
    /// of closing whatever is showing. Cells: every open site has only
    /// &self in some cases (dialog builders re-run every render).
    pub(crate) dialog_seq: std::cell::Cell<u64>,
    pub(crate) settings_seq: std::cell::Cell<Option<u64>>,
    /// Password prompt field (Ask mode / Keychain miss). Cleared on submit.
    pub(crate) pwd_prompt: Entity<InputState>,
    /// Transient notice for the status bar ("Saved X", "Connecting…").
    pub(crate) status: SharedString,
    /// `&&name` values defined this session, per connection id. Once defined,
    /// even `&name` reuses the value without prompting (SQL*Plus parity).
    pub(crate) defines:
        std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    /// In-flight operations waiting on a dialog (bind vars / picker /
    /// password prompt). At most one is set at a time.
    pub(crate) pending: PendingOps,
    /// Session-unlocked passwords, per connection id. Memory only, never
    /// persisted: Ask mode and Keychain-miss prompts land here, and every
    /// connect/run path prefers them over whatever is stored. Values are
    /// [`Zeroizing`], so removing a connection (or dropping the view) wipes
    /// the secret from memory.
    pub(crate) unlocked: std::collections::HashMap<String, Zeroizing<String>>,
    /// Per-connection dictionary caches, usage counts, and schema-browser
    /// state. See [`BrowserState`].
    pub(crate) browser: BrowserState,
    /// Suggestion popup fires while typing (vs manual shortcut only).
    /// Mirrors `Preferences.completion`; toggled in Settings.
    pub(crate) complete_auto: bool,
    /// Include SYS/SYSTEM/etc. objects in suggestions. Mirrors preferences.
    pub(crate) show_system: bool,
    /// Global interface density. Mirrors preferences; toggled in Settings.
    pub(crate) ui_density: UiDensity,
    /// Suggestions/dictionary cache TTL; `None` = never expires by time.
    /// Mirrors preferences and is updated live from the Settings field.
    pub(crate) metadata_ttl: Option<std::time::Duration>,
    /// Settings → Editor → Suggestions cache TTL field (minutes).
    pub(crate) metadata_ttl_input: Entity<InputState>,
    /// Show table/column detail cards on editor hover. Mirrors preferences.
    pub(crate) hover_details: bool,
    /// Results-grid row cap in effect (0 = unlimited). Mirrors the saved
    /// preference and is updated live from the Settings field.
    pub(crate) result_cap: usize,
    /// Results-grid row height in points (compactness). Mirrors the saved
    /// preference and is updated live from the Settings slider.
    pub(crate) grid_row_height: u32,
    /// Settings → Results density slider (row height).
    pub(crate) grid_density_slider: Entity<SliderState>,
    /// Settings → Results query timeout field (seconds; blank/0 = unlimited).
    pub(crate) query_timeout_input: Entity<InputState>,
    /// Settings → Results CSV delimiter field.
    pub(crate) csv_delim_input: Entity<InputState>,
    /// Settings → Themes searchable dropdown.
    pub(crate) theme_select: Entity<SelectState<SearchableVec<SharedString>>>,
    /// Settings → Editor → Font searchable dropdown.
    pub(crate) font_select: Entity<SelectState<SearchableVec<ChoiceItem>>>,
    /// Settings → Themes → interface density dropdown.
    pub(crate) density_select: Entity<SelectState<SearchableVec<SharedString>>>,
    /// Free-form results-grid row cap field (Settings → Results). `0` or
    /// empty means unlimited.
    pub(crate) result_cap_input: Entity<InputState>,
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
        // Settings → Results row cap. Blank/0 renders as unlimited.
        let result_cap_value = Preferences::load().result_cap;
        let result_cap_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("100000")
                .default_value(if result_cap_value == 0 {
                    String::new()
                } else {
                    result_cap_value.to_string()
                })
        });
        // Settings → Results density slider (row height in points).
        let grid_row_height = Preferences::load()
            .grid_row_height
            .clamp(GRID_ROW_HEIGHT_MIN, GRID_ROW_HEIGHT_MAX);
        let grid_density_slider = cx.new(|_| {
            SliderState::new()
                .min(GRID_ROW_HEIGHT_MIN as f32)
                .max(GRID_ROW_HEIGHT_MAX as f32)
                .step(1.)
                .default_value(SliderValue::Single(grid_row_height as f32))
        });
        // Settings → Results query timeout (seconds; blank/0 = unlimited).
        let timeout_secs = Preferences::load().query_timeout_secs;
        let query_timeout_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("60")
                .default_value(if timeout_secs == 0 {
                    String::new()
                } else {
                    timeout_secs.to_string()
                })
        });
        // Settings → Editor → Suggestions cache TTL (minutes; blank/0 =
        // never, i.e. refresh only on reconnect or manually).
        let ttl_secs = Preferences::load().metadata_ttl_secs;
        let metadata_ttl_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("0")
                .default_value(if ttl_secs == 0 {
                    String::new()
                } else {
                    (ttl_secs / 60).to_string()
                })
        });
        // Settings → Results CSV delimiter (single character; "tab" for tab).
        let delim = Preferences::load().csv_delimiter;
        let csv_delim_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(",")
                .default_value(crate::export::csv_delim_display(&delim))
        });
        // Settings → Themes searchable dropdown.
        let theme_items: Vec<SharedString> = THEME_LIST
            .iter()
            .map(|name| SharedString::from(*name))
            .collect();
        let theme_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(theme_items), None, window, cx).searchable(true)
        });
        // Settings → Editor → Font searchable dropdown.
        let font_items: Vec<ChoiceItem> = FONT_FAMILIES
            .iter()
            .map(|(label, value)| ChoiceItem {
                label: SharedString::from(*label),
                value: SharedString::from(*value),
            })
            .collect();
        let font_select = cx.new(|cx| {
            SelectState::new(SearchableVec::new(font_items), None, window, cx).searchable(true)
        });
        // Settings → Themes → interface density (two options).
        let density_items: Vec<SharedString> = [UiDensity::Compact, UiDensity::Comfortable]
            .iter()
            .map(|d| SharedString::from(d.label()))
            .collect();
        let density_select =
            cx.new(|cx| SelectState::new(SearchableVec::new(density_items), None, window, cx));

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
            // Starts wide; `on_prepaint` corrects it on the first frame.
            main_width: std::cell::Cell::new(1200.0),
            editor_split: cx.new(|_| ResizableState::default()),
            main_split: cx.new(|_| ResizableState::default()),
            sidebar_width: 264.0,
            last_window_w: std::cell::Cell::new(0.0),
            dialog: ConnectionDialogState {
                editing: None,
                pending_env: Environment::default(),
                pending_role: OracleRole::default(),
                pending_service_kind: ServiceKind::default(),
                pending_ssl: false,
                pending_password_mode: PasswordMode::default(),
                pending_engine: DbEngine::default(),
                password_snapshot: None,
                name,
                host,
                port,
                service,
                user,
                password,
            },
            dialog_seq: std::cell::Cell::new(0),
            settings_seq: std::cell::Cell::new(None),
            pwd_prompt,
            status: "".into(),
            defines: std::collections::HashMap::new(),
            pending: PendingOps {
                bind: None,
                pick: None,
                password: None,
            },
            unlocked: std::collections::HashMap::new(),
            browser: BrowserState {
                meta: std::collections::HashMap::new(),
                usage: std::collections::HashMap::new(),
                open: std::collections::HashSet::new(),
                expanded: std::collections::HashMap::new(),
                trees: std::collections::HashMap::new(),
                filters: std::collections::HashMap::new(),
            },
            complete_auto: prefs.completion == CompleteMode::Auto,
            show_system: prefs.show_system_schemas,
            ui_density: prefs.ui_density,
            metadata_ttl: prefs.metadata_ttl(),
            metadata_ttl_input,
            hover_details: prefs.hover_details,
            result_cap: prefs.result_cap,
            grid_row_height,
            grid_density_slider,
            query_timeout_input,
            csv_delim_input,
            theme_select,
            font_select,
            density_select,
            result_cap_input,
            _subs: Vec::new(),
        };
        // Follow the OS appearance while the theme mode is System. The
        // subscription lives as long as the view; the callback no-ops for
        // explicit Light/Dark preferences.
        let appearance_sub = window.observe_window_appearance(move |window, cx| {
            crate::guitheme::reapply_for_system_appearance(window, cx);
        });
        this._subs.push(appearance_sub);
        // Persist the free-form results row cap as it is edited. Empty or 0
        // means unlimited; non-numeric text is left unsaved (the field keeps
        // what was typed, but the preference is unchanged).
        {
            let cap_input = this.result_cap_input.clone();
            let cap_input_sub = cap_input.clone();
            let cap_sub = cx.subscribe_in(
                &cap_input,
                window,
                move |this, _, ev: &InputEvent, _, cx| {
                    if !matches!(
                        ev,
                        InputEvent::Change | InputEvent::Blur | InputEvent::PressEnter { .. }
                    ) {
                        return;
                    }
                    let text = cap_input_sub.read(cx).value().to_string();
                    let trimmed = text.trim();
                    let parsed = if trimmed.is_empty() {
                        Some(0usize)
                    } else if trimmed.bytes().all(|b| b.is_ascii_digit()) {
                        trimmed.parse::<usize>().ok()
                    } else {
                        None
                    };
                    if let Some(cap) = parsed {
                        // Apply immediately (the next run reads the view field),
                        // and persist for the next launch.
                        this.result_cap = cap;
                        let mut prefs = Preferences::load();
                        if prefs.result_cap != cap {
                            prefs.result_cap = cap;
                            let _ = prefs.save();
                        }
                    }
                },
            );
            this._subs.push(cap_sub);
        }
        // Grid density slider: preview live while dragging, persist on
        // release (avoids a preferences write per pixel).
        {
            let slider = this.grid_density_slider.clone();
            let sub = cx.subscribe(&slider, move |this, _, ev: &SliderEvent, cx| {
                let (value, persist) = match ev {
                    SliderEvent::Change(v) => (*v, false),
                    SliderEvent::Release(v) => (*v, true),
                };
                let height = (value.end().round() as i64)
                    .clamp(GRID_ROW_HEIGHT_MIN as i64, GRID_ROW_HEIGHT_MAX as i64)
                    as u32;
                if this.grid_row_height != height {
                    this.grid_row_height = height;
                    cx.notify();
                }
                if persist {
                    let mut prefs = Preferences::load();
                    if prefs.grid_row_height != height {
                        prefs.grid_row_height = height;
                        let _ = prefs.save();
                    }
                }
            });
            this._subs.push(sub);
        }
        // Query timeout field: blank/0 = unlimited, otherwise seconds.
        {
            let input = this.query_timeout_input.clone();
            let input_sub = input.clone();
            let sub = cx.subscribe_in(&input, window, move |_, _, ev: &InputEvent, _, cx| {
                if !matches!(
                    ev,
                    InputEvent::Change | InputEvent::Blur | InputEvent::PressEnter { .. }
                ) {
                    return;
                }
                let text = input_sub.read(cx).value().to_string();
                let trimmed = text.trim();
                let parsed = if trimmed.is_empty() {
                    Some(0u64)
                } else if trimmed.bytes().all(|b| b.is_ascii_digit()) {
                    trimmed.parse::<u64>().ok()
                } else {
                    None
                };
                if let Some(secs) = parsed {
                    let mut prefs = Preferences::load();
                    if prefs.query_timeout_secs != secs {
                        prefs.query_timeout_secs = secs;
                        let _ = prefs.save();
                    }
                }
            });
            this._subs.push(sub);
        }
        // Suggestions cache TTL field (minutes; blank/0 = never).
        {
            let input = this.metadata_ttl_input.clone();
            let input_sub = input.clone();
            let sub = cx.subscribe_in(&input, window, move |this, _, ev: &InputEvent, _, cx| {
                if !matches!(
                    ev,
                    InputEvent::Change | InputEvent::Blur | InputEvent::PressEnter { .. }
                ) {
                    return;
                }
                let text = input_sub.read(cx).value().to_string();
                let trimmed = text.trim();
                let parsed = if trimmed.is_empty() {
                    Some(0u64)
                } else if trimmed.bytes().all(|b| b.is_ascii_digit()) {
                    trimmed.parse::<u64>().ok()
                } else {
                    None
                };
                if let Some(minutes) = parsed {
                    let secs = minutes.saturating_mul(60);
                    this.metadata_ttl = if secs == 0 {
                        None
                    } else {
                        Some(std::time::Duration::from_secs(secs))
                    };
                    let mut prefs = Preferences::load();
                    if prefs.metadata_ttl_secs != secs {
                        prefs.metadata_ttl_secs = secs;
                        let _ = prefs.save();
                    }
                }
            });
            this._subs.push(sub);
        }
        // CSV delimiter field (single character; "tab" normalizes to a tab).
        {
            let input = this.csv_delim_input.clone();
            let input_sub = input.clone();
            let sub = cx.subscribe_in(&input, window, move |_, _, ev: &InputEvent, _, cx| {
                if !matches!(
                    ev,
                    InputEvent::Change | InputEvent::Blur | InputEvent::PressEnter { .. }
                ) {
                    return;
                }
                let text = input_sub.read(cx).value().to_string();
                let delim = crate::export::csv_delim_from_input(&text);
                let mut prefs = Preferences::load();
                if prefs.csv_delimiter != delim {
                    prefs.csv_delimiter = delim;
                    let _ = prefs.save();
                }
            });
            this._subs.push(sub);
        }
        // Theme dropdown: apply + persist on confirm.
        {
            let select = this.theme_select.clone();
            let sub = cx.subscribe_in(
                &select,
                window,
                move |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, window, cx| {
                    if let SelectEvent::Confirm(Some(name)) = ev {
                        let mut prefs = Preferences::load();
                        prefs.theme = name.to_string();
                        if let Err(e) = prefs.save() {
                            this.status = format!("Preferences save failed: {e:#}").into();
                        }
                        crate::guitheme::apply_preferences(&prefs, Some(window), cx);
                        cx.notify();
                    }
                },
            );
            this._subs.push(sub);
        }
        // Font dropdown: apply + persist on confirm.
        {
            let select = this.font_select.clone();
            let sub = cx.subscribe_in(
                &select,
                window,
                move |this, _, ev: &SelectEvent<SearchableVec<ChoiceItem>>, _, cx| {
                    if let SelectEvent::Confirm(Some(value)) = ev {
                        let mut prefs = Preferences::load();
                        prefs.font_family = value.to_string();
                        if let Err(e) = prefs.save() {
                            this.status = format!("Preferences save failed: {e:#}").into();
                        }
                        crate::guitheme::apply_font_prefs(&prefs, cx);
                        cx.notify();
                    }
                },
            );
            this._subs.push(sub);
        }
        // Interface-density dropdown: apply + persist on confirm.
        {
            let select = this.density_select.clone();
            let sub = cx.subscribe_in(
                &select,
                window,
                move |this, _, ev: &SelectEvent<SearchableVec<SharedString>>, _, cx| {
                    if let SelectEvent::Confirm(Some(label)) = ev {
                        let density = if label.as_ref() == UiDensity::Comfortable.label() {
                            UiDensity::Comfortable
                        } else {
                            UiDensity::Compact
                        };
                        let mut prefs = Preferences::load();
                        prefs.ui_density = density;
                        if let Err(e) = prefs.save() {
                            this.status = format!("Preferences save failed: {e:#}").into();
                        }
                        this.ui_density = density;
                        cx.notify();
                    }
                },
            );
            this._subs.push(sub);
        }
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

    /// Responsive level for the toolbars/headers, from the measured main width.
    pub(crate) fn toolbar_size(&self) -> ToolbarSize {
        ToolbarSize::for_width(self.main_width.get())
    }

    /// Concrete sizes for the active interface density.
    pub(crate) fn density(&self) -> Density {
        Density::for_level(self.ui_density)
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
                MenuItem::action("About SQLHighland", OpenAbout),
                MenuItem::Separator,
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
