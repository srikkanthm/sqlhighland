//! Rendering: sidebar tree, tabs, editor, results, status bar, menus' host.
//!
//! All `render_*` builders plus the `Render` impl for the view.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use std::rc::Rc;

use super::*;

impl SqlHighlandView {
    // -- Schema browser (see browser.rs) --------------------------------------

    /// Render one open connection's schema tree. Object rows click through
    /// to viewer tabs; folders toggle via the kit's own row handling.
    pub(crate) fn render_browser_tree(
        &self,
        conn_id: &str,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let Some(state) = self.browser.trees.get(conn_id).cloned() else {
            return div().into_any_element();
        };
        let view = cx.entity().downgrade();
        let conn = conn_id.to_string();
        // Shared by the row renderer and the context menu below (both
        // `move` closures, so each gets its own clone).
        let menu_view = view.clone();
        let menu_conn = conn.clone();
        tree(&state, move |_ix, entry, _selected, _window, cx| {
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
                crate::logging::warn(format!("unparsed schema-browser object row: {ids}"));
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
        })
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
        let d = self.density();
        TabBar::new("query-tabs")
            .w_full()
            // Underline tabs have no inner padding, so without this the first
            // tab (and its active underline) sits flush under the centered
            // split handle/grip. Match the editor's own inset so the first
            // tab lines up with the pane content.
            .pl(px(d.pane_pad))
            .with_size(d.tab_size)
            // Underline gives the active tab a primary-color bar; the default
            // filled variant resolves to `tab_active` (== background), which is
            // near-indistinguishable from the bar itself in most themes. Keep
            // the strip's usual `tab_bar` fill (Underline defaults to
            // transparent) so only the active-tab indicator changes.
            .with_variant(TabVariant::Underline)
            .bg(cx.theme().tab_bar)
            .prefix(self.render_tab_nav(cx))
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
                        .with_size(d.button_size)
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.request_close_tab(&tab_id, window, cx);
                        })),
                )
            }))
            .suffix(
                Button::new("tab-add")
                    .icon(KitIcon::Plus)
                    .ghost()
                    .with_size(d.button_size)
                    .tooltip("New tab")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.add_tab(None, String::new(), window, cx);
                    })),
            )
    }

    /// Back/forward buttons for the tab strip: move to the previous/next tab
    /// in display order. Rendered as the tab bar's prefix; disabled at the
    /// ends.
    fn render_tab_nav(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.density();
        let can_back = self.active > 0;
        let can_forward = self.active + 1 < self.tabs.len();
        h_flex()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .pr_1()
            .child(
                Button::new("tab-nav-back")
                    .icon(KitIcon::ArrowLeft)
                    .ghost()
                    .with_size(d.button_size)
                    .tooltip("Back")
                    .disabled(!can_back)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.nav_back(window, cx);
                    })),
            )
            .child(
                Button::new("tab-nav-forward")
                    .icon(KitIcon::ArrowRight)
                    .ghost()
                    .with_size(d.button_size)
                    .tooltip("Forward")
                    .disabled(!can_forward)
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.nav_forward(window, cx);
                    })),
            )
    }

    fn render_connection_picker(
        &self,
        size: ToolbarSize,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
        // Narrow widths: shorten the name (full name stays in the tooltip) and,
        // at the minimal level, drop the label and env pill entirely.
        let compact = size.compact();
        let minimal = matches!(size, ToolbarSize::Minimal);
        let d = self.density();
        let shown = if minimal {
            String::new()
        } else {
            shorten(&label, if compact { 16 } else { 40 })
        };
        let tooltip = format!("Connection for this tab — {label}");
        h_flex()
            .gap_1()
            .items_center()
            .flex_shrink_0()
            .child(div().size(px(d.dot)).rounded_full().bg(if live {
                cx.theme().success
            } else {
                cx.theme().muted_foreground
            }))
            .child(
                Button::new("conn-picker")
                    .outline()
                    .with_size(d.button_size)
                    .icon(KitIcon::Database)
                    .tooltip(tooltip)
                    .when(!minimal, |b| b.label(shown))
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
            .when(!minimal, |this| {
                this.when_some(env_tag(tab_env, d.badge_w, cx), |this, tag| this.child(tag))
            })
    }

    /// The "⋯" overflow menu shown at the narrowest toolbar level: the
    /// secondary actions that no longer fit as icon buttons.
    fn render_toolbar_more(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let d = self.density();
        let script = cx.entity().downgrade();
        let commit = cx.entity().downgrade();
        let rollback = cx.entity().downgrade();
        let format = cx.entity().downgrade();
        Button::new("toolbar-more")
            .ghost()
            .with_size(d.button_size)
            .icon(KitIcon::Ellipsis)
            .tooltip("More actions")
            .dropdown_menu(move |menu, _, _| {
                menu.item(
                    PopupMenuItem::new("Run as Script")
                        .icon(KitIcon::FileTerminal)
                        .on_click({
                            let view = script.clone();
                            move |_, window, cx| {
                                view.update(cx, |this, cx| {
                                    let tab_id = this.active_tab().id.clone();
                                    this.run_buffer_as_script(&tab_id, window, cx);
                                })
                                .ok();
                            }
                        }),
                )
                .item(PopupMenuItem::new("Commit").icon(KitIcon::Check).on_click({
                    let view = commit.clone();
                    move |_, _, cx| {
                        view.update(cx, |this, cx| this.commit_now(cx)).ok();
                    }
                }))
                .item(
                    PopupMenuItem::new("Rollback")
                        .icon(KitIcon::Undo2)
                        .on_click({
                            let view = rollback.clone();
                            move |_, _, cx| {
                                view.update(cx, |this, cx| this.rollback_now(cx)).ok();
                            }
                        }),
                )
                .item(
                    PopupMenuItem::new("Format")
                        .icon(KitIcon::WandSparkles)
                        .on_click({
                            let view = format.clone();
                            move |_, window, cx| {
                                view.update(cx, |this, cx| this.format_now(window, cx)).ok();
                            }
                        }),
                )
            })
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
        // Responsive toolbar level (measured main width): full labels, icon
        // collapse, or secondary actions in the "⋯" overflow menu.
        let size = self.toolbar_size();
        let compact = size.compact();
        let minimal = matches!(size, ToolbarSize::Minimal);
        let d = self.density();
        v_flex()
            .id("query-section")
            .size_full()
            .gap(px(d.gap))
            .p(px(d.pane_pad))
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
                    .w_full()
                    .min_w_0()
                    .gap(px(d.gap))
                    .items_center()
                    .when(pending, |this| {
                        this.child(
                            h_flex()
                                .gap_1()
                                .items_center()
                                .child(div().size(px(d.dot)).rounded_full().bg(cx.theme().warning))
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
                            .with_size(d.button_size)
                            .icon(KitIcon::Play)
                            .tooltip("Run selection or statement at cursor (⌘↵)")
                            .when(!compact, |b| b.w(px(d.action_button_w)).label("Run"))
                            .loading(tab.busy && tab.run_kind == Some(RunKind::Statement))
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.run_at_cursor(window, cx);
                            })),
                    )
                    .when(!minimal, |this| {
                        this.child(
                            Button::new("run-script")
                                .info()
                                .with_size(d.button_size)
                                .icon(KitIcon::FileTerminal)
                                .tooltip("Run buffer as script (⇧⌘↵)")
                                .when(!compact, |b| b.w(px(d.action_button_w)).label("Script"))
                                .loading(tab.busy && tab.run_kind == Some(RunKind::Script))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    let tab_id = this.active_tab().id.clone();
                                    this.run_buffer_as_script(&tab_id, window, cx);
                                })),
                        )
                        .child(
                            Button::new("commit")
                                .success()
                                .with_size(d.button_size)
                                .icon(KitIcon::Check)
                                .tooltip("Commit transaction (⇧⌘C)")
                                .when(!compact, |b| b.w(px(d.action_button_w)).label("Commit"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.commit_now(cx);
                                })),
                        )
                        .child(
                            Button::new("rollback")
                                .danger()
                                .with_size(d.button_size)
                                .icon(KitIcon::Undo2)
                                .tooltip("Roll back transaction (⇧⌘R)")
                                .when(!compact, |b| b.w(px(d.action_button_w)).label("Rollback"))
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.rollback_now(cx);
                                })),
                        )
                        .child(
                            Button::new("format")
                                .warning()
                                .with_size(d.button_size)
                                .icon(KitIcon::WandSparkles)
                                .tooltip("Format SQL (⇧⌥F)")
                                .when(!compact, |b| b.w(px(d.action_button_w)).label("Format"))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.format_now(window, cx);
                                })),
                        )
                    })
                    .when(minimal, |this| this.child(self.render_toolbar_more(cx)))
                    // Cancel lives in the status bar while a run/export is in
                    // flight (see `render_status_bar`), so the toolbar no
                    // longer carries one.
                    .child(div().flex_1())
                    .child(self.render_connection_picker(size, cx)),
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
                    .child(Editor::new(&editor).size_full()),
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
            let clear_view = view.clone();
            let clear_tab = tab_id.clone();
            // "Count rows" only makes sense for a settled, wrapable query
            // result (not DESCRIBE / object viewers).
            let can_count = tab.has_result
                && !tab.busy
                && !tab.exporting
                && crate::sql::statement_kind(&tab.last_sql) == crate::sql::StatementKind::Query
                && !crate::db::is_describe_statement(&tab.last_sql);
            let d = self.density();
            v_flex()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .child(
                    div()
                        .id("grid-area")
                        .flex_1()
                        .min_w_0()
                        .min_h_0()
                        .overflow_hidden()
                        .p(px(d.pane_pad))
                        // Click on the grid background (not a cell/header —
                        // those stop propagation) clears the selection.
                        .on_click(move |_, _, cx: &mut App| {
                            clear_view
                                .update(cx, |this, cx| {
                                    this.clear_grid_selection(&clear_tab, cx);
                                })
                                .ok();
                        })
                        .context_menu(move |menu, _, _| {
                            let mut menu = menu;
                            if can_count {
                                let count_view = view.clone();
                                let count_tab = tab_id.clone();
                                menu = menu
                                    .item(PopupMenuItem::new("Count rows").on_click(
                                        move |_, _, cx| {
                                            count_view
                                                .update(cx, |this, cx| {
                                                    this.start_count_rows(&count_tab, cx);
                                                })
                                                .ok();
                                        },
                                    ))
                                    .separator();
                            }
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
                        .child(render_tab_table(&tab.table, self.grid_row_height)),
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
        let d = self.density();
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
            .p(px(d.pane_pad))
            .gap(px(d.gap))
            // No focus/context/copy plumbing by design: the message below
            // is a real (read-only) text area — caret, selection, and the
            // kit's native Input-context Cmd+C all come free.
            .child(
                h_flex()
                    .gap(px(d.gap))
                    .items_center()
                    .child(div().text_color(accent).child(icon))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .text_color(accent)
                            .child(title),
                    )
                    .child(div().flex_1()),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .p(px(d.pane_pad))
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

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tab = self.active_tab();
        let fetching = tab
            .table
            .read(cx)
            .delegate()
            .with_data(|d| d.loading, false);
        let left = if tab.busy {
            match tab.run_started {
                // A fixed-width clock: `Running… 0:00:07` keeps the same width
                // for the life of the run, so the Cancel button beside it never
                // shifts (and it stays readable past an hour).
                Some(started) => format!("Running… {}", elapsed_clock(started.elapsed().as_secs())),
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
        let minimal = matches!(self.toolbar_size(), ToolbarSize::Minimal);
        let compact = self.toolbar_size().compact();
        let d = self.density();
        // Bottom-pane actions moved here from the old results toolbar row so
        // the grid loses a full row of chrome. Export only applies to a grid
        // result; Dismiss appears whenever a bottom pane is showing. While a
        // run/export is in flight the only meaningful action is Cancel, so
        // Export/Dismiss (which target settled data) stand down.
        let running = tab.busy || tab.exporting;
        let settled = !running;
        let show_export = settled && tab.output.is_none() && tab.has_result;
        let show_dismiss = settled && !tab.hide_results && (tab.has_result || tab.output.is_some());
        let export_view = cx.entity().downgrade();
        let export_tab = tab.id.clone();
        let dismiss_tab = tab.id.clone();
        let cancel_tab = tab.id.clone();
        let cancel_exporting = tab.exporting;
        // Fixed label width while a run/export ticks, so the Cancel button
        // beside it never shifts: the clock is fixed-length but proportional
        // digits still vary in width, and the export row count grows.
        let left_w = if running { Some(px(140.)) } else { None };
        h_flex()
            .w_full()
            .min_w_0()
            .flex_none()
            .gap(px(d.gap))
            .px(px(d.pane_pad))
            .h(px(d.status_h))
            .items_center()
            .bg(cx.theme().status_bar)
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .when_some(left_w, |this, w| this.w(w))
                    .child(left),
            )
            .when(running, |this| {
                let tip = if cancel_exporting {
                    "Stop the export — the partial file is discarded"
                } else {
                    "Stop waiting — the server finishes in the background and its results are discarded"
                };
                this.child(
                    Button::new("status-cancel")
                        .danger()
                        .with_size(d.button_size)
                        .icon(KitIcon::X)
                        .tooltip(tip)
                        .when(!compact, |b| b.label("Cancel"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if cancel_exporting {
                                this.cancel_export(&cancel_tab, cx);
                            } else {
                                this.cancel_run(&cancel_tab, cx);
                            }
                        })),
                )
            })
            .child(div().flex_1())
            .when(!minimal, |this| {
                this.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(cx.theme().muted_foreground)
                        .child(self.status.clone()),
                )
            })
            .child(div().flex_shrink_0().size(px(d.dot)).rounded_full().bg(dot))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .child(right),
            )
            .when(show_export, |this| {
                this.child(
                    Button::new("status-export")
                        .ghost()
                        .with_size(d.button_size)
                        .icon(KitIcon::Download)
                        .tooltip("Export all result rows to CSV or Excel")
                        .dropdown_menu(move |menu, _, _| {
                            let mut menu = menu.max_h(px(320.)).scrollable(true);
                            for fmt in [ExportFormat::Csv, ExportFormat::Xlsx] {
                                let view = export_view.clone();
                                let tab_id = export_tab.clone();
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
                )
            })
            .when(show_dismiss, |this| {
                this.child(
                    Button::new("status-dismiss")
                        .ghost()
                        .with_size(d.button_size)
                        .icon(KitIcon::X)
                        .tooltip("Dismiss results (⌘J)")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.dismiss_results(&dismiss_tab, cx);
                        })),
                )
            })
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
        // Responsive: name truncates, the kind tag/connection drop out at the
        // narrowest level, and the buttons collapse to icons.
        let size = self.toolbar_size();
        let compact = size.compact();
        let minimal = matches!(size, ToolbarSize::Minimal);
        let d = self.density();
        h_flex()
            .w_full()
            .min_w_0()
            .h(px(d.viewer_h()))
            .gap(px(d.gap))
            .px(px(d.pane_pad))
            .items_center()
            .bg(cx.theme().tab_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(KitIcon::Database)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_semibold()
                    .child(tab.name.clone()),
            )
            .when(!minimal, |this| {
                this.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(kind_tag),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .max_w(px(180.))
                        .text_color(cx.theme().muted_foreground)
                        .child(self.connection_name(&tab.connection_id)),
                )
            })
            .child(
                Button::new("viewer-refresh")
                    .secondary()
                    .with_size(d.button_size)
                    .icon(KitIcon::RefreshCw)
                    .tooltip("Re-run DESCRIBE")
                    .when(!compact, |b| b.label("Refresh"))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.refresh_viewer(&refresh_id, window, cx);
                    })),
            )
            .when(tab.busy, |this| {
                let cancel_id = tab.id.clone();
                this.child(
                    Button::new("viewer-cancel")
                        .danger()
                        .with_size(d.button_size)
                        .icon(KitIcon::X)
                        .tooltip("Stop the description")
                        .when(!compact, |b| b.label("Cancel"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.cancel_run(&cancel_id, cx);
                        })),
                )
            })
    }

    fn render_main(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let tab = self.active_tab();
        // Default editor height: half the window. `.size()` is initial-only,
        // so this seeds the query split's first layout (clamped to its range);
        // the kit keeps panels proportional as the window is resized.
        let editor_h = (window.viewport_size().height.as_f32() * 0.5).clamp(160.0, 900.0);
        // Object-viewer tabs (schema browser): grid only, no editor.
        // Layout mirrors the query split (resizable panel + body) on
        // purpose: the grid is virtualized and needs the resizable's
        // definite pixel sizing — a pure flex chain collapses its body
        // to zero rows while fixed-height siblings still paint.
        let content: AnyElement = if matches!(tab.kind, TabKind::Viewer { .. }) {
            let viewer_h = self.density().viewer_h();
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
            div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(
                    v_resizable("viewer-split")
                        .child(
                            resizable_panel()
                                .size(px(viewer_h))
                                .size_range(px(viewer_h)..px(200.))
                                .flex_none()
                                .child(self.render_viewer_header(tab, cx)),
                        )
                        .child(body),
                )
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
                div()
                    .flex_1()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        v_resizable("query-split")
                            // Owned state: keyboard steps (`resize_panel`) and
                            // mouse drags share it, so neither fights the other.
                            // The panel's `.size()` below is initial-only.
                            .with_state(&self.editor_split)
                            .with_handle_appearance(resize_grip())
                            .child(
                                resizable_panel()
                                    .size(px(editor_h))
                                    .size_range(px(160.)..px(900.))
                                    // Grow 0 holds the editor at its size on
                                    // tall windows; shrink 1 lets it give room
                                    // back to the grid when the window is short.
                                    .flex_grow_0()
                                    .flex_shrink_1()
                                    .child(self.render_editor(cx)),
                            )
                            .child(body),
                    )
                    .into_any_element()
            }
        };

        v_flex()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            // Measure the content width each frame and flip the responsive
            // toolbar level when it crosses a threshold (hysteresis-free: the
            // level is a pure function of the width, so it settles).
            .on_prepaint({
                let width_view = cx.entity().downgrade();
                move |bounds, _, cx| {
                    let width = bounds.size.width.as_f32();
                    width_view
                        .update(cx, |this, cx| {
                            let before = this.toolbar_size();
                            this.main_width.set(width);
                            if this.toolbar_size() != before {
                                cx.notify();
                            }
                        })
                        .ok();
                }
            })
            .on_action(cx.listener(|this, _: &CopySelection, window, cx| {
                this.copy_selection(window, cx);
            }))
            .on_action(cx.listener(|this, _: &SelectAllRows, _, cx| {
                this.select_all_rows(cx);
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
}

/// Shorten a label to at most `max` characters, appending an ellipsis when
/// truncated. Used to keep connection names from pushing toolbars wide.
fn shorten(label: &str, max: usize) -> String {
    if label.chars().count() <= max {
        label.to_string()
    } else {
        let mut s: String = label.chars().take(max.saturating_sub(1)).collect();
        s.push('…');
        s
    }
}

/// Elapsed seconds as a fixed-width `HH:MM:SS`. The constant width keeps the
/// status bar's `Running… <clock>` label from resizing (and moving the Cancel
/// button) as time passes, while staying readable past an hour.
fn elapsed_clock(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
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
pub(crate) fn env_tag(env: Environment, width: f32, cx: &App) -> Option<AnyElement> {
    let label = env.label()?;
    let color = env_color(env, cx)?;
    Some(
        div()
            .when(width > 0.0, |e| e.w(px(width)))
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

/// Painted grip for a resize divider: a transparent ~6px strip centered on
/// the boundary carrying a small pill that brightens on hover and while
/// dragging. The kit's built-in handle only paints a 1px hairline; this keeps
/// the same hit area (the kit still owns the drag) and changes only the paint.
fn resize_grip() -> gpui_kit::base::ResizeHandleRenderer {
    Rc::new(|ctx, _window, cx| {
        // Horizontal axis == a vertical divider between side-by-side panels.
        let vertical_divider = ctx.axis().is_horizontal();
        let weak = cx.theme().border;
        let strong = cx.theme().ring;
        let wash = cx.theme().accent.opacity(0.15);

        let mut strip = div()
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .group_hover("handle", move |this| this.bg(wash));
        let mut grip = div()
            .rounded_full()
            .bg(if ctx.is_active() { strong } else { weak })
            .group_hover("handle", move |this| this.bg(strong));
        if vertical_divider {
            strip = strip.relative().left(px(-3.)).w(px(6.)).h_full();
            grip = grip.w(px(2.)).h(px(24.));
        } else {
            strip = strip.relative().top(px(-3.)).h(px(6.)).w_full();
            grip = grip.h(px(2.)).w(px(24.));
        }
        Some(strip.child(grip).into_any_element())
    })
}

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
                .with_state(&self.main_split)
                .with_handle_appearance(resize_grip())
                // Record only real drags (this callback fires on mouse-up, not
                // for our programmatic re-pins), so the remembered width is
                // the user's choice.
                .on_resize({
                    let view = cx.entity().downgrade();
                    move |state, _, cx| {
                        let w: f32 = state
                            .read(cx)
                            .sizes()
                            .first()
                            .map(|p| (*p).into())
                            .unwrap_or(0.0);
                        if w > 0.0 {
                            view.update(cx, |this, _| this.sidebar_width = w).ok();
                        }
                    }
                })
                .child(
                    resizable_panel()
                        .size(px(self.sidebar_width))
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
            // Re-pin the fixed sidebar after any container-size change. The
            // kit rescales a fixed panel proportionally with its flexible
            // sibling (which is why a fresh launch could show a fat sidebar);
            // this restores the remembered width once the frame has laid out.
            .on_prepaint({
                let view = cx.entity().downgrade();
                move |bounds, window, cx| {
                    let width = bounds.size.width.as_f32();
                    let scheduled = view
                        .update(cx, |this, _| {
                            if (this.last_window_w.get() - width).abs() <= 0.5 {
                                return None;
                            }
                            this.last_window_w.set(width);
                            Some((this.main_split.clone(), this.sidebar_width))
                        })
                        .ok()
                        .flatten();
                    let Some((state, sidebar_width)) = scheduled else {
                        return;
                    };
                    window.defer(cx, move |window, cx| {
                        state.update(cx, |s, cx| {
                            let cur: f32 = s.sizes().first().map(|p| (*p).into()).unwrap_or(0.0);
                            if (cur - sidebar_width).abs() > 0.5 {
                                s.resize_panel(0, px(sidebar_width), window, cx);
                            }
                        });
                    });
                }
            })
            .child(
                v_flex()
                    .size_full()
                    .child(TitleBar::new())
                    .child(div().flex_1().min_h_0().child(content)),
            )
            .children(Root::render_dialog_layer(window, cx))
    }
}

#[cfg(test)]
mod clock_tests {
    // Import explicitly: a `use super::*` glob would pull in GPUI's `test`
    // macro and shadow Rust's built-in `#[test]`.
    use super::elapsed_clock;

    #[test]
    fn elapsed_clock_is_fixed_width() {
        assert_eq!(elapsed_clock(0), "00:00:00");
        assert_eq!(elapsed_clock(7), "00:00:07");
        assert_eq!(elapsed_clock(59), "00:00:59");
        assert_eq!(elapsed_clock(60), "00:01:00");
        assert_eq!(elapsed_clock(3599), "00:59:59");
        assert_eq!(elapsed_clock(3600), "01:00:00");
        assert_eq!(elapsed_clock(3661), "01:01:01");
        assert_eq!(elapsed_clock(86399), "23:59:59");
        assert_eq!(elapsed_clock(360_000), "100:00:00");
    }
}
