//! Results-grid data and the table delegate.
//!
//! Buffered query pages, the held cursor handle, and the DataTable delegate.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use super::*;

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
    pub(crate) session: SharedSession,
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
    pub(crate) fn empty() -> Self {
        Self { fetch: None }
    }

    pub(crate) fn set_fetch(&mut self, fetch: Option<Arc<FetchState>>) {
        self.fetch = fetch;
    }

    pub(crate) fn is_current(&self, fetch: &Arc<FetchState>) -> bool {
        match &self.fetch {
            Some(current) => Arc::ptr_eq(current, fetch),
            None => false,
        }
    }

    pub(crate) fn with_data<R>(&self, f: impl FnOnce(&ResultData) -> R, default: R) -> R {
        match &self.fetch {
            Some(fetch) => f(&lock(&fetch.data)),
            None => default,
        }
    }

    /// Display text of one cell for copying. SQL NULL copies as empty.
    /// `col_ix` is a grid index: 0 is the row-number column, data follows.
    /// Returns `None` only when the indices are out of range.
    pub(crate) fn cell_text(&self, row_ix: usize, col_ix: usize) -> Option<SharedString> {
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
    pub(crate) fn row_csv(&self, row_ix: usize) -> Option<String> {
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
            return Column::new("col-rownum", "#")
                .width(px(52.))
                .text_right()
                .paddings(compact_cell_pad());
        }
        let name = self.with_data(|d| d.columns.get(col_ix - 1).map(|c| c.name.clone()), None);
        // DB-shaped data must never crash the grid: a 0-column result or
        // a virtualized-grid race yields a positional placeholder instead
        // of panicking the app (and its unsaved drafts) away.
        let name = name.unwrap_or_else(|| format!("col{col_ix}"));
        // Key by position, not name: duplicate column names (common in
        // SELECT * joins) would otherwise collide element identities,
        // breaking reconciliation and defeating column virtualization.
        Column::new(format!("col-{col_ix}"), name)
            .width(px(180.))
            .paddings(compact_cell_pad())
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
        // `h_full + items_center`: the kit's header wrapper centers a
        // content-sized child, but a full-height one defeats it and the
        // label sticks to the top (visible once the grid density grows the
        // rows). The row-number column's header hugs the same edge as its
        // right-aligned body cells.
        div()
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .when(col_ix == 0, |this| this.justify_end())
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
        // Every branch fills the (possibly tall) row and centers vertically.
        if col_ix == 0 {
            return div()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_end()
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
            Some(text) => div()
                .h_full()
                .flex()
                .items_center()
                .truncate()
                .child(text)
                .into_any_element(),
            None => div()
                .h_full()
                .flex()
                .items_center()
                .text_color(cx.theme().muted_foreground)
                .child("NULL")
                .into_any_element(),
        }
    }

    fn has_more(&self, _: &App) -> bool {
        self.with_data(|d| !d.exhausted && !d.capped && !d.loading, false)
    }

    fn load_more_threshold(&self) -> usize {
        // One page of look-ahead: prefetch the next page just before the user
        // reaches the bottom, without over-buffering. This must scale with the
        // configured fetch size — a fixed 200 was fine for the old 1000-row
        // pages but is 4 pages ahead at the 50-row default (it buffered ~300
        // rows on the first frame). Falls back to the kit default before a
        // fetch exists.
        self.fetch.as_ref().map(|f| f.chunk).unwrap_or(20)
    }

    fn load_more(&mut self, _: &mut Window, cx: &mut Context<TableState<Self>>) {
        let Some(fetch) = self.fetch.clone() else {
            return;
        };
        {
            let mut data = lock(&fetch.data);
            if data.loading || data.exhausted || data.capped {
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
                                data.exhausted = page.exhausted;
                                data.capped = data.rows.len() >= fetch.cap && !page.exhausted;
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

/// Compact cell padding for the results grid: pairs with the custom row height
/// in [`render_tab_table`] so the size table's medium defaults don't widen the
/// cells back out.
fn compact_cell_pad() -> gpui::Edges<gpui::Pixels> {
    gpui::Edges {
        top: px(0.),
        bottom: px(0.),
        left: px(5.),
        right: px(5.),
    }
}

pub(crate) fn render_tab_table(
    table: &Entity<TableState<ResultsDelegate>>,
    row_height: u32,
) -> impl IntoElement {
    div()
        .size_full()
        .min_w_0()
        .overflow_hidden()
        // A custom `Size` only sets the row height; the cell text size is
        // inherited, so pin it here to keep the compact rows from growing.
        .text_sm()
        .child(
            DataTable::new(table)
                .with_size(gpui_kit::component::Size::Size(px(row_height as f32)))
                .stripe(true),
        )
}
