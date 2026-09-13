//! Tab lifecycle: create, restore, open/save SQL files, drafts.
//!
//! One tab owns its editor, grid, and run state.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use super::lsp::{OracleCompleter, OracleDefiner, OracleHover, ShowDocumentHook};
use super::*;

impl SqlHighlandView {
    pub(super) fn make_tab(
        &mut self,
        spec: NewTabSpec,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
                if !params
                    .uri
                    .scheme()
                    .is_some_and(|s| s.as_str() == "oracle-describe")
                {
                    return false;
                }
                let mut segs = params
                    .uri
                    .path()
                    .as_str()
                    .split('/')
                    .filter(|s| !s.is_empty());
                let (Some(owner), Some(table)) = (segs.next(), segs.next()) else {
                    return false;
                };
                let tab_id = jump_tab_id.clone();
                view.update(cx, |this, cx| {
                    // Engine-owned statement (Oracle: DESCRIBE, bare for own
                    // schema). The provider — not the UI — knows the dialect.
                    let sql = match this
                        .tab_by_id(&tab_id)
                        .and_then(|t| t.connection_id.clone())
                    {
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
        let table = cx.new(|cx| {
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

    pub(super) fn restore_tabs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
    pub(super) fn open_viewer(
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
    pub(super) fn refresh_viewer(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn close_tab_now(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn request_close_tab(
        &mut self,
        tab_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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

    pub(super) fn check_external_change(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
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
        let dir = crate::fsutil::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
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
    pub(super) fn schedule_draft_save(&mut self, tab_id: &str, cx: &mut Context<Self>) {
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

    // -- App-global entry points (main.rs) -----------------------------------

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
}
