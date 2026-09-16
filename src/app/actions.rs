//! View actions: run, commit/rollback, format, copy, zoom, dismiss.
//!
//! Invoked from keybindings, menus, and rendered buttons.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use super::*;

impl SqlHighlandView {
    // -- Query ------------------------------------------------------------------

    /// True when the query editor (not a dialog field) holds focus.
    /// All keyboard shortcuts double-check this so Cmd+Enter etc. in a
    /// connection dialog field never fire actions behind the modal.
    pub(crate) fn editor_focused(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        window
            .focused(cx)
            .map(|h| h == self.active_tab().editor.read(cx).focus_handle(cx))
            .unwrap_or(false)
    }

    /// Run the selected text (Cmd+Enter / Run button) when there is a
    /// selection; otherwise the statement under the cursor. With a single
    /// statement in the buffer, caret position is ignored.
    pub(super) fn run_at_cursor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_focused(window, cx) {
            return;
        }
        // A selection wins: run exactly what the user selected (trimmed), so a
        // fragment like `select * from usr` picked out of a longer statement
        // runs on its own. `start_run` still routes `@`-directives and handles
        // `&`/`&&` substitution and `:binds`.
        let selected = self
            .active_tab()
            .editor
            .read(cx)
            .selected_text()
            .to_string();
        let selected = selected.trim();
        if !selected.is_empty() {
            let tab_id = self.active_tab().id.clone();
            let sql = selected.to_string();
            self.start_run(&tab_id, sql, window, cx);
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
    pub(super) fn commit_now(&mut self, cx: &mut Context<Self>) {
        self.finish_txn(true, cx);
    }

    /// Roll back the active tab's connection (same scope rules as commit).
    pub(super) fn rollback_now(&mut self, cx: &mut Context<Self>) {
        self.finish_txn(false, cx);
    }

    pub(super) fn finish_txn(&mut self, commit: bool, cx: &mut Context<Self>) {
        let Some(conn_id) = self.active_tab().connection_id.clone() else {
            self.tabs[self.active].output = Some(Output::error("Select a connection for this tab"));
            cx.notify();
            return;
        };
        let engine = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .map(|c| c.engine)
            .unwrap_or_default();
        let session = self.pool.get_or_create(&conn_id, engine);
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
    pub(super) fn format_now(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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

    // -- Suggestions / selection ---------------------------------------------

    /// Connected user's schema (for own-schema ranking); "" when unbound.
    pub(crate) fn own_schema_of(&self, conn_id: &Option<String>) -> String {
        conn_id
            .as_deref()
            .and_then(|id| self.connections.iter().find(|c| c.id == id))
            .map(|c| c.user.clone())
            .unwrap_or_default()
    }

    /// Copy the grid selection to the clipboard. Precedence: any native text
    /// selection (output pane / editor) wins; then the app-owned grid
    /// selection (rows or a whole column, honoring the CSV delimiter); then
    /// the kit's single cell/row as a keyboard-only fallback. Silent no-op
    /// with no selection.
    pub(super) fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Native text selection wins wherever it exists (output-pane message,
        // editor inputs): grid copy below is the fallback when nothing is
        // selected as text.
        let selected = gpui_kit::base::TextSelection::selected_text(window, cx);
        if !selected.is_empty() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            return;
        }
        let delim = crate::export::csv_delim(&Preferences::load().csv_delimiter);
        let tab = self.active_tab();
        let text = {
            let table = tab.table.read(cx);
            let delegate = table.delegate();
            delegate.copy_text(delim).or_else(|| {
                let cell = || {
                    table
                        .selected_cell()
                        .and_then(|(r, c)| delegate.cell_text(r, c))
                        .map(|s| s.to_string())
                };
                let row = || table.selected_row().and_then(|r| delegate.row_csv(r));
                // Keyboard-only selection (mouse input drives the app-owned
                // selection above).
                cell().or_else(row)
            })
        };
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// A data cell was clicked. The library's own handler makes it the current
    /// cell (so arrow keys navigate from it); here we drop any row/column
    /// selection and record the cell's row as the row anchor.
    pub(crate) fn grid_cell_click(&mut self, tab_id: &str, row_ix: usize, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        self.tabs[ix].table.update(cx, |table, cx| {
            table.delegate_mut().set_current_row(row_ix);
            cx.notify();
        });
    }

    /// Select one row from a click on the left `#` anchor column (Cmd toggles,
    /// Shift extends a range from the anchor).
    pub(crate) fn grid_row_click(
        &mut self,
        tab_id: &str,
        row_ix: usize,
        secondary: bool,
        shift: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        let row_count = self.tabs[ix].table.read(cx).delegate().rows_count(cx);
        self.tabs[ix].table.update(cx, |table, cx| {
            // Drop the kit's stale single-cell/row outline first so only the
            // app-owned highlight shows.
            table.clear_selection(cx);
            table
                .delegate_mut()
                .click_row(row_ix, secondary, shift, row_count);
            cx.notify();
        });
    }

    /// Select a whole column from a header click (all fetched values).
    pub(crate) fn grid_header_click(
        &mut self,
        tab_id: &str,
        col_ix: usize,
        cx: &mut Context<Self>,
    ) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        self.tabs[ix].table.update(cx, |table, cx| {
            table.clear_selection(cx);
            table.delegate_mut().select_column(col_ix);
            cx.notify();
        });
    }

    /// Select every buffered row (Cmd+A). Never fetches more pages.
    pub(crate) fn select_all_rows(&mut self, cx: &mut Context<Self>) {
        let tab_id = self.active_tab().id.clone();
        let Some(ix) = self.tab_index(&tab_id) else {
            return;
        };
        let row_count = self.tabs[ix].table.read(cx).delegate().rows_count(cx);
        self.tabs[ix].table.update(cx, |table, cx| {
            table.clear_selection(cx);
            table.delegate_mut().select_all_rows(row_count);
            cx.notify();
        });
    }

    /// Clear the grid selection (empty-area click).
    pub(crate) fn clear_grid_selection(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        self.tabs[ix].table.update(cx, |table, cx| {
            table.delegate_mut().clear_selection();
            table.clear_selection(cx);
            cx.notify();
        });
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
    pub(super) fn zoom_font(&mut self, delta: i32, cx: &mut Context<Self>) {
        let mut prefs = Preferences::load();
        prefs.font_size = (prefs.font_size as i32 + delta).clamp(10, 24) as u32;
        if let Err(e) = prefs.save() {
            self.status = format!("Preferences save failed: {e:#}").into();
        }
        crate::guitheme::apply_font_prefs(&prefs, cx);
        cx.notify();
    }

    /// Reset the editor font to the 13pt default.
    pub(super) fn zoom_font_reset(&mut self, cx: &mut Context<Self>) {
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
    pub(super) fn step_editor_h(&mut self, dir: i32, window: &mut Window, cx: &mut Context<Self>) {
        let cur: f32 = self
            .editor_split
            .read(cx)
            .sizes()
            .first()
            .map(|s| (*s).into())
            .unwrap_or(300.0);
        let next = (cur + dir as f32 * 48.0).clamp(160.0, 900.0);
        self.editor_split
            .update(cx, |s, cx| s.resize_panel(0, px(next), window, cx));
    }
}
