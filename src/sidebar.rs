//! Connections sidebar surface: connection rows, the sidebar panel,
//! and collapse/expand state.
//!
//! Extracted from `app.rs` (refactor Phase 3); behavior unchanged.

use gpui::{Context, Window};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::Input;
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit_assets::IconName as KitIcon;

use crate::app::{
    conn_menu_item, env_tag, ConnMenuOp, DismissResults, NextTab, OpenSql, PrevTab, SaveSql,
    SaveSqlAs, SqlHighlandView,
};

impl SqlHighlandView {
    pub(crate) fn render_connection_row(
        &self,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let cfg = &self.connections[ix];
        let is_live = self.live.contains(&cfg.id);
        let view = cx.entity().downgrade();
        let conn_id = cfg.id.clone();
        let browser_open = self.browser.open.contains(&cfg.id);
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
            .child(
                div()
                    .id(("conn-status", ix))
                    .test_support()
                    .w(px(3.))
                    .rounded_full()
                    .bg(if is_live {
                        cx.theme().success
                    } else {
                        cx.theme().border
                    }),
            )
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
                            .when_some(env_tag(cfg.environment, cx), |this, tag| this.child(tag)),
                    ),
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
        v_flex().w_full().child(row).when(browser_open, |this| {
            let filter_row = self.browser.filters.get(&tree_conn).map(|f| {
                div()
                    .w_full()
                    .pt_1()
                    .pb_1()
                    .child(Input::new(f).w_full().h(px(18.)).text_xs())
            });
            // Fit the visible rows with a little breathing room: row
            // pixels vary a hair by font metrics, and any shortfall
            // overflows into a scrollbar, while a small surplus reads
            // as intentional padding. Capped: beyond it the tree owns
            // its scroll and the bar is legitimate.
            let mut rows = 0;
            if let Some(state) = self.browser.trees.get(&tree_conn) {
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

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                // Dismiss lives here too (same bubble-path reason):
                // dialogs sit outside every root, so modals are safe.
                .on_action(cx.listener(|this, _: &DismissResults, _, cx| {
                    let tab_id = this.active_tab().id.clone();
                    this.dismiss_results(&tab_id, cx);
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
            // Dismiss lives here too (same bubble-path reason):
            // dialogs sit outside every root, so modals are safe.
            .on_action(cx.listener(|this, _: &DismissResults, _, cx| {
                let tab_id = this.active_tab().id.clone();
                this.dismiss_results(&tab_id, cx);
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

    pub(crate) fn toggle_sidebar(
        &mut self,
        _: &ClickEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.set_sidebar_collapsed(!self.sidebar_collapsed, cx);
    }

    /// Shared by the collapse button/rail and the Cmd+B action.
    pub(crate) fn set_sidebar_collapsed(&mut self, collapsed: bool, cx: &mut Context<Self>) {
        self.sidebar_collapsed = collapsed;
        cx.notify();
    }
}
