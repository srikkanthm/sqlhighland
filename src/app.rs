//! UI: connections sidebar + SQL editor + results grid + status bar.
//!
//! Blocking Oracle calls run on the background executor; the view is only
//! ever mutated on the UI thread via `Context::spawn` handles.
//!
//! The single live session auto-switches on Connect. (`connected_id` tracks
//! which saved entry owns it — the seam for multi-session later.)

use std::sync::{Arc, Mutex};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Editor, EditorState, Input, InputContentType, InputState};
use gpui_kit::component::menu::{ContextMenuExt, PopupMenuItem};
use gpui_kit::component::resizable::{h_resizable, resizable_panel, v_resizable};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::table::{Column, DataTable, TableDelegate, TableState};
use gpui_kit::component::*;
use gpui_kit::*;
use gpui_kit_assets::IconName as KitIcon;
use sqlhighland::config::SavedConfig;
use sqlhighland::db::{FETCH_CAP, FETCH_CHUNK, DbClient, OracledbSession};
use sqlhighland::model::{ColumnInfo, ConnectionConfig};
use sqlhighland::sql::{format_sql, statement_at};

const DEFAULT_SQL: &str = "SELECT user, sysdate FROM dual;";

gpui_kit::actions!(sqlhighland, [RunQuery]);

/// Buffered query data shared between the view and the table delegate.
/// Rows only ever grow; a new query swaps in a fresh [`FetchState`].
struct ResultData {
    columns: Vec<ColumnInfo>,
    rows: Vec<Vec<Option<String>>>,
    elapsed_ms: u128,
    exhausted: bool,
    loading: bool,
    capped: bool,
}

/// Links one open server-side cursor to the grid: session for fetching,
/// buffered data for rendering, and the view for status updates.
struct FetchState {
    session: Arc<Mutex<OracledbSession>>,
    query_id: u64,
    chunk: usize,
    cap: usize,
    data: Mutex<ResultData>,
    view: WeakEntity<SqlHighlandView>,
}

/// Table delegate over the shared [`FetchState`] of the latest query.
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
}

impl TableDelegate for ResultsDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.with_data(|d| d.columns.len(), 0)
    }

    fn rows_count(&self, _: &App) -> usize {
        self.with_data(|d| d.rows.len(), 0)
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        let name = self.with_data(
            |d| d.columns.get(col_ix).map(|c| c.name.clone()),
            None,
        );
        let name = name.expect("column with no result");
        Column::new(name.clone(), name).width(px(180.))
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let cell = self.with_data(
            |d| {
                d.rows
                    .get(row_ix)
                    .and_then(|row| row.get(col_ix))
                    .cloned()
                    .flatten()
            },
            None,
        );
        match cell {
            Some(text) => div().child(text).into_any_element(),
            None => div()
                .text_color(rgb(0x7f849c))
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
                                    .extend(page.rows.into_iter().take(take));
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

            // Surface the outcome in the view (status line / error banner),
            // but only if this fetch still belongs to the visible query.
            fetch.view
                .update(cx, |view, cx| {
                    if !view
                        .table
                        .read(cx)
                        .delegate()
                        .is_current(&fetch)
                    {
                        return;
                    }
                    if applied {
                        view.result_meta = describe_fetch(&fetch).into();
                    } else if let Some(msg) = fetch_err {
                        // A failed page surfaces once; the next scroll retries.
                        view.error = Some(format!("Fetch failed: {msg}").into());
                    }
                    cx.notify();
                })
                .ok();
        })
        .detach();
    }
}

/// One-line status for the buffered data, e.g. `2,400 rows · 12 ms`.
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
    Connected(String),
    Failed(String),
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
    ix: usize,
    op: ConnMenuOp,
) -> PopupMenuItem {
    PopupMenuItem::new(label).icon(icon).on_click(move |_, window, cx| {
        view.update(cx, |this, cx| match op {
            ConnMenuOp::Connect => this.connect_row(ix, cx),
            ConnMenuOp::Disconnect => this.disconnect_session(cx),
            ConnMenuOp::Edit => this.start_edit(ix, window, cx),
            ConnMenuOp::Delete => this.delete_connection(ix, cx),
        })
        .ok();
    })
}

pub struct SqlHighlandView {
    session: Arc<Mutex<OracledbSession>>,
    connections: Vec<ConnectionConfig>,
    /// Index being edited in the dialog (`None` = adding a new one).
    editing: Option<usize>,
    sidebar_collapsed: bool,
    // Dialog form fields (entities persist across dialog open/close).
    name: Entity<InputState>,
    host: Entity<InputState>,
    port: Entity<InputState>,
    service: Entity<InputState>,
    user: Entity<InputState>,
    password: Entity<InputState>,
    editor: Entity<EditorState>,
    table: Entity<TableState<ResultsDelegate>>,
    /// Transient notice for the status bar ("Saved X", "Connecting…").
    status: SharedString,
    connected: bool,
    /// Id of the saved entry owning the live session (`None` = ad-hoc/none).
    connected_id: Option<String>,
    busy: bool,
    result_meta: SharedString,
    has_result: bool,
    error: Option<SharedString>,
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
        let editor = cx.new(|cx| {
            EditorState::new(window, cx)
                .language("sql")
                .default_value(DEFAULT_SQL)
        });
        let table = cx.new(|cx| TableState::new(ResultsDelegate::empty(), window, cx));

        // Cmd+Enter runs the statement under the cursor. Scoped to the
        // editor's own `Input` key context; the handler double-checks focus.
        cx.bind_keys([KeyBinding::new("cmd-enter", RunQuery, Some("Input"))]);

        Self {
            session: Arc::new(Mutex::new(OracledbSession::new())),
            connections,
            editing: None,
            sidebar_collapsed: false,
            name,
            host,
            port,
            service,
            user,
            password,
            editor,
            table,
            status: "Disconnected".into(),
            connected: false,
            connected_id: None,
            busy: false,
            result_meta: "".into(),
            has_result: false,
            error: None,
        }
    }

    fn form_config(&self, cx: &App) -> ConnectionConfig {
        // Preserve the edited entry's id so the live session keeps matching.
        // New entries get theirs at save time.
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
        window.open_dialog(cx, move |dialog, _, _| {
            let save_view = view.clone();
            dialog
                .title(title.clone())
                .w(px(400.))
                .child(
                    v_flex()
                        .gap_2()
                        .w_full()
                        .child(dialog_field("Name", &name, false))
                        .child(dialog_field("Host", &host, false))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(div().flex_1().child(dialog_field("Port", &port, false)))
                                .child(
                                    div()
                                        .flex_1()
                                        .child(dialog_field("Service", &service, false)),
                                ),
                        )
                        .child(dialog_field("User", &user, false))
                        .child(dialog_field("Password", &password, true)),
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
                                save_view.update(cx, |this, cx| {
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
        if self.connected_id.as_deref() == Some(removed.id.as_str()) {
            if let Ok(mut guard) = self.session.lock() {
                guard.disconnect();
            }
            self.connected = false;
            self.connected_id = None;
        }
        self.persist();
        self.status = format!("Deleted {}", removed.name).into();
        cx.notify();
    }

    // -- Session ------------------------------------------------------------

    fn connect_with(&mut self, cfg: ConnectionConfig, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.status = format!("Connecting to {}…", cfg.connect_string()).into();
        self.error = None;
        cx.notify();

        let session = self.session.clone();
        let bg = cx.background_executor().clone();
        let conn_id = cfg.id.clone();
        cx.spawn(async move |view, cx| {
            let outcome = bg
                .spawn(async move {
                    let mut guard = session.lock().expect("session lock");
                    match guard.connect(&cfg) {
                        Ok(()) => WorkOutcome::Connected(cfg.connect_string()),
                        Err(e) => WorkOutcome::Failed(e.to_string()),
                    }
                })
                .await;
            view.update(cx, |this, cx| {
                this.busy = false;
                match outcome {
                    WorkOutcome::Connected(connect_string) => {
                        this.status = format!("Connected to {connect_string}").into();
                        this.connected = true;
                        this.connected_id = Some(conn_id);
                    }
                    WorkOutcome::Failed(msg) => {
                        this.status = "Connection failed".into();
                        this.connected = false;
                        this.connected_id = None;
                        this.error = Some(msg.into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn disconnect_session(&mut self, cx: &mut Context<Self>) {
        if let Ok(mut guard) = self.session.lock() {
            guard.disconnect();
        }
        self.connected = false;
        self.connected_id = None;
        self.status = "Disconnected".into();
        cx.notify();
    }

    fn connect_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.connections.len() {
            return;
        }
        let cfg = self.connections[ix].clone();
        self.connect_with(cfg, cx);
    }

    fn toggle_sidebar(&mut self, _: &ClickEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    // -- Query --------------------------------------------------------------

    /// Run the statement under the cursor (Cmd+Enter) or via the Run button.
    /// With a single statement in the buffer, caret position is ignored.
    fn run_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Scope the shortcut to the query editor so Cmd+Enter in a
        // connection dialog field doesn't fire a query behind the modal.
        let editor_focused = window
            .focused(cx)
            .map(|h| h == self.editor.read(cx).focus_handle(cx))
            .unwrap_or(false);
        if !editor_focused {
            return;
        }
        let text = self.editor.read(cx).value().to_string();
        let cursor = self.editor.read(cx).cursor();
        match statement_at(&text, cursor) {
            Some(sql) => self.run_sql(sql, cx),
            None => {
                self.result_meta = "No statement at cursor".into();
                cx.notify();
            }
        }
    }

    fn run_sql(&mut self, sql: String, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        if !self.connected {
            self.error = Some("Not connected — connect first".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.error = None;
        self.result_meta = "".into();
        cx.notify();

        let session = self.session.clone();
        let bg = cx.background_executor().clone();
        cx.spawn(async move |view, cx| {
            let started = std::time::Instant::now();
            let outcome = bg
                .spawn(async move {
                    let mut session = session.lock().expect("session lock");
                    session.start_query(&sql, FETCH_CHUNK)
                })
                .await;
            let elapsed_ms = started.elapsed().as_millis();
            view.update(cx, |this, cx| {
                this.busy = false;
                match outcome {
                    Ok((columns, page, query_id)) => {
                        let fetch = Arc::new(FetchState {
                            session: this.session.clone(),
                            query_id,
                            chunk: FETCH_CHUNK,
                            cap: FETCH_CAP,
                            data: Mutex::new(ResultData {
                                columns,
                                rows: page.rows,
                                elapsed_ms,
                                exhausted: page.exhausted,
                                loading: false,
                                capped: false,
                            }),
                            view: view.clone(),
                        });
                        this.result_meta = describe_fetch(&fetch).into();
                        this.has_result = true;
                        this.table.update(cx, |table, cx| {
                            table.delegate_mut().set_fetch(Some(fetch));
                            table.refresh(cx);
                        });
                    }
                    Err(e) => {
                        this.error = Some(e.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn format_query(&mut self, _: &ClickEvent, window: &mut Window, cx: &mut Context<Self>) {
        let raw = self.editor.read(cx).value().to_string();
        let formatted = format_sql(&raw);
        self.editor.update(cx, |editor, cx| {
            editor.set_value(formatted, window, cx);
        });
        cx.notify();
    }

    // -- Render ---------------------------------------------------------------

    fn render_connection_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = &self.connections[ix];
        let is_live = self.connected_id.as_deref() == Some(cfg.id.as_str());
        let dot = if is_live { rgb(0xa6e3a1) } else { rgb(0x585b70) };
        let view = cx.entity().downgrade();
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
                menu.item(conn_menu_item(connect_label, connect_icon, view.clone(), ix, connect_op))
                    .item(conn_menu_item("Edit…", KitIcon::SquarePen, view.clone(), ix, ConnMenuOp::Edit))
                    .separator()
                    .item(conn_menu_item("Delete", KitIcon::X, view.clone(), ix, ConnMenuOp::Delete))
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
                                    .text_color(rgb(0x7f849c))
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
                .border_color(rgb(0x313244))
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
                    .border_color(rgb(0x313244))
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
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.start_add(window, cx);
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

    fn render_editor(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .id("query-section")
            .size_full()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(rgb(0x313244))
            .on_action(cx.listener(|this, _: &RunQuery, window, cx| {
                this.run_at_cursor(window, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_sm().child("Query"))
                    .child(div().flex_1())
                    .child(
                        Button::new("format")
                            .label("Format")
                            .on_click(cx.listener(Self::format_query)),
                    )
                    .child(
                        Button::new("run")
                            .primary()
                            .label("Run")
                            .loading(self.busy)
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run_at_cursor(window, cx);
                            })),
                    ),
            )
            .child(
                div()
                    .min_h_0()
                    .flex_1()
                    .child(Editor::new(&self.editor).size_full()),
            )
    }

    fn render_results(&self) -> impl IntoElement {
        if let Some(err) = &self.error {
            v_flex()
                .flex_1()
                .p_2()
                .gap_2()
                .child(
                    div()
                        .p_2()
                        .rounded_md()
                        .bg(rgb(0x3d1620))
                        .text_sm()
                        .text_color(rgb(0xf38ba8))
                        .child(err.clone()),
                )
                .child(self.render_table())
        } else {
            div().flex_1().p_2().child(self.render_table())
        }
    }

    fn render_table(&self) -> impl IntoElement {
        div().size_full().child(DataTable::new(&self.table))
    }

    fn render_status_bar(&self, cx: &App) -> impl IntoElement {
        // Action status on the left, connection status on the right.
        // The connection label has a single source (`status`), set only by
        // the connect/disconnect paths — never duplicated.
        let fetching = self
            .table
            .read(cx)
            .delegate()
            .with_data(|d| d.loading, false);
        let left = if self.busy {
            "Running…".to_string()
        } else if fetching {
            "Fetching more…".to_string()
        } else {
            self.result_meta.to_string()
        };
        let dot = if self.connected {
            rgb(0xa6e3a1)
        } else {
            rgb(0x585b70)
        };
        h_flex()
            .gap_2()
            .px_2()
            .h(px(28.))
            .items_center()
            .border_t_1()
            .border_color(rgb(0x313244))
            .text_xs()
            .child(div().text_color(rgb(0x7f849c)).child(left))
            .child(div().flex_1())
            .child(div().size(px(8.)).rounded_full().bg(dot))
            .child(div().text_color(rgb(0x7f849c)).child(self.status.clone()))
    }

    fn render_main(&self, cx: &mut Context<Self>) -> AnyElement {
        let body: AnyElement = if !self.has_result && self.error.is_none() {
            div()
                .flex_1()
                .items_center()
                .justify_center()
                .text_sm()
                .text_color(rgb(0x7f849c))
                .child("Connect, then run a query to see results here.")
                .into_any_element()
        } else {
            self.render_results().into_any_element()
        };

        v_flex()
            .flex_1()
            .h_full()
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
}

fn dialog_field(
    label: impl Into<SharedString>,
    state: &Entity<InputState>,
    password: bool,
) -> impl IntoElement {
    let label: SharedString = label.into();
    let mut input = Input::new(state).w_full();
    if password {
        input = input.content_type(InputContentType::Password);
    }
    v_flex()
        .gap_1()
        .child(div().text_xs().text_color(rgb(0x7f849c)).child(label))
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
            .child(content)
            .children(Root::render_dialog_layer(window, cx))
    }
}
