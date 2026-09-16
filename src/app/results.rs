//! Results-grid data and the table delegate.
//!
//! Buffered query pages, the held cursor handle, and the DataTable delegate.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use std::collections::BTreeSet;

use gpui_kit::component::scroll::Scrollbar;

use crate::sql::{SortDir, SortSpec};

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

/// Format a grid selection as clipboard text from `rows` (data cells only,
/// no row-number column, no header). `None` when nothing is selected or the
/// result is empty. Rows are CSV lines (delimiter honored); a column is one
/// field per line. Grid data column indices are `col_ix - 1` (col 0 is the
/// row number).
fn selection_text(
    selection: &GridSelection,
    rows: &[Vec<Option<SharedString>>],
    delim: char,
) -> Option<String> {
    match selection {
        GridSelection::None => None,
        GridSelection::Rows(sel) => {
            let lines: Vec<String> = sel
                .iter()
                .filter_map(|&r| rows.get(r))
                .map(|row| crate::model::csv_row_with(row.iter().map(|c| c.as_deref()), delim))
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
        GridSelection::Column(col) => {
            let ci = col.checked_sub(1)?;
            let lines: Vec<String> = rows
                .iter()
                .map(|row| {
                    crate::model::csv_field(
                        row.get(ci).and_then(|c| c.as_deref()).unwrap_or(""),
                        delim,
                    )
                })
                .collect();
            (!lines.is_empty()).then(|| lines.join("\n"))
        }
    }
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
    /// Active native sort for this result (server-side ORDER BY), so the header
    /// can show the indicator after a re-run replaces the fetch.
    pub(crate) sort: Option<SortSpec>,
    /// Whether a native sort re-run is valid for the executed SQL (not a
    /// `FOR UPDATE` query or `DESCRIBE`).
    pub(crate) sortable: bool,
}

/// What's selected in the grid. One mode at a time: a set of whole rows (left
/// `#` strip clicks, Shift ranges, or select-all) or a single whole column. The
/// single "current cell" is owned by the library (clicking a cell sets its
/// cursor, so arrow keys navigate from there); its value is copied through
/// [`ResultsDelegate::cell_text`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum GridSelection {
    #[default]
    None,
    Rows(BTreeSet<usize>),
    Column(usize),
}

/// Table delegate over the shared [`FetchState`] of a tab's latest query.
pub(crate) struct ResultsDelegate {
    fetch: Option<Arc<FetchState>>,
    selection: GridSelection,
    /// Last clicked row: the anchor a Shift-click extends a range from.
    anchor: Option<usize>,
}

impl ResultsDelegate {
    pub(crate) fn empty() -> Self {
        Self {
            fetch: None,
            selection: GridSelection::None,
            anchor: None,
        }
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

    /// Clear the grid selection (run start, Escape, empty-area click).
    pub(crate) fn clear_selection(&mut self) {
        self.selection = GridSelection::None;
        self.anchor = None;
    }

    /// A data cell was clicked: drop any row/column selection (the library
    /// owns the single current cell), and make the cell's row the current row —
    /// the anchor a later Shift-click on the row strip extends from.
    pub(crate) fn set_current_row(&mut self, row_ix: usize) {
        self.selection = GridSelection::None;
        self.anchor = Some(row_ix);
    }

    /// Apply a left-`#`-column click. `secondary` (Cmd/Ctrl) toggles the row;
    /// `shift` replaces the set with the range from the previous anchor;
    /// otherwise the clicked row becomes the only selection. `row_count` bounds
    /// a Shift range so it never selects past the fetched rows.
    pub(crate) fn click_row(
        &mut self,
        row_ix: usize,
        secondary: bool,
        shift: bool,
        row_count: usize,
    ) {
        if shift {
            if let Some(anchor) = self.anchor {
                let (lo, hi) = if anchor <= row_ix {
                    (anchor, row_ix)
                } else {
                    (row_ix, anchor)
                };
                let hi = hi.min(row_count.saturating_sub(1));
                self.selection = GridSelection::Rows((lo..=hi).collect());
                return;
            }
        }
        if secondary {
            let mut rows = match std::mem::take(&mut self.selection) {
                GridSelection::Rows(rows) => rows,
                _ => BTreeSet::new(),
            };
            if !rows.insert(row_ix) {
                rows.remove(&row_ix);
            }
            self.selection = if rows.is_empty() {
                GridSelection::None
            } else {
                GridSelection::Rows(rows)
            };
        } else {
            self.selection = GridSelection::Rows(BTreeSet::from([row_ix]));
        }
        self.anchor = Some(row_ix);
    }

    /// Select a whole column (header click).
    pub(crate) fn select_column(&mut self, col_ix: usize) {
        self.selection = GridSelection::Column(col_ix);
        self.anchor = None;
    }

    /// Select every buffered row (Cmd+A). No extra pages are fetched.
    pub(crate) fn select_all_rows(&mut self, row_count: usize) {
        self.selection = if row_count == 0 {
            GridSelection::None
        } else {
            GridSelection::Rows((0..row_count).collect())
        };
        self.anchor = None;
    }

    pub(crate) fn is_row_selected(&self, row_ix: usize) -> bool {
        matches!(&self.selection, GridSelection::Rows(rows) if rows.contains(&row_ix))
    }

    /// Column selection covers data columns only (col 0 is the row number).
    pub(crate) fn is_column_selected(&self, col_ix: usize) -> bool {
        col_ix >= 1 && matches!(self.selection, GridSelection::Column(c) if c == col_ix)
    }

    /// Clipboard text for the current selection, or `None` when nothing is
    /// selected. Rows are CSV lines joined by newlines (no header); a column is
    /// one value per line. `delim` is the configured CSV delimiter.
    pub(crate) fn copy_text(&self, delim: char) -> Option<String> {
        self.with_data(|d| selection_text(&self.selection, &d.rows, delim), None)
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
        //
        // Size the column to fit its header name (never below the default), so
        // the name is fully visible without a text-system measurement here
        // (`column` runs per column per frame and has no window). Over-estimate
        // the glyph width deliberately; a little extra column beats a clipped
        // name.
        let width = (name.chars().count() as f32 * 9.0 + 28.0).clamp(180.0, 800.0);
        Column::new(format!("col-{col_ix}"), name)
            .width(px(width))
            .paddings(compact_cell_pad())
    }

    /// Row wrapper. The library's own row click (`row_selectable`) is off, so
    /// its thin left row-header strip bubbles here: clicking the strip selects
    /// the row (Shift extends a range, Cmd toggles), matching the old
    /// single-row strip behavior extended to many. Clicks on data cells stop
    /// propagation before they reach the row.
    ///
    /// The row paints its own full-width background band (base/stripe, then
    /// the selection tint). Besides keeping the selection band continuous
    /// across the cells' horizontal padding, this covers the library's row
    /// hover wash, so hovering never changes the row.
    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let mut row = div().id(("row", row_ix));
        let row_count = self.with_data(|d| d.rows.len(), 0);
        if row_ix < row_count {
            // The whole row is a click target (the strip selects it), and the
            // pointer cursor lives on the row so moving across cells and the
            // inter-cell padding doesn't flicker between cursors.
            row = row.cursor_pointer();
            let base = if row_ix % 2 != 0 {
                cx.theme().tokens.table_even
            } else {
                cx.theme().tokens.table
            };
            row = row.child(div().absolute().inset_0().bg(base));
            if self.is_row_selected(row_ix) {
                let sel = cx.theme().tokens.table_active;
                row = row.child(div().absolute().inset_0().bg(sel));
            }
            if let Some(f) = &self.fetch {
                let view = f.view.clone();
                let tab_id = f.tab_id.clone();
                row = row.on_click(move |event, _window, cx: &mut App| {
                    cx.stop_propagation();
                    let m = event.modifiers();
                    view.update(cx, |this, cx| {
                        this.grid_row_click(&tab_id, row_ix, m.secondary(), m.shift, cx);
                    })
                    .ok();
                });
            }
        }
        row
    }

    /// Header cells: a single click selects the whole column (all fetched
    /// values); a double click sorts it (Asc -> Desc -> clear), re-running the
    /// query server-side. The kit's own header/sort path is bypassed (it cycles
    /// descending-first), and header labels are no longer drag-selectable —
    /// Cmd+C on the selected column replaces label copy. The `#` header (col 0)
    /// is inert.
    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let name = self.column(col_ix, cx).name;
        if col_ix == 0 {
            // Row-number header: right-aligned like its cells, not clickable.
            return div()
                .id(("col-th", col_ix))
                .test_support()
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_end()
                .font_bold()
                .child(div().truncate().child(name))
                .into_any_element();
        }
        // Data header. `pos` is the 1-based select-list position used by the
        // native ORDER BY (grid col 1 is data column 1).
        let pos = col_ix;
        let (sortable, active, view, tab_id) = match &self.fetch {
            Some(f) => (f.sortable, f.sort, Some(f.view.clone()), f.tab_id.clone()),
            None => (false, None, None, String::new()),
        };
        let indicator = active.filter(|s| s.col == pos).map(|s| s.dir);
        let next = match active {
            Some(s) if s.col == pos => match s.dir {
                SortDir::Asc => Some(SortSpec {
                    col: pos,
                    dir: SortDir::Desc,
                }),
                SortDir::Desc => None,
            },
            _ => Some(SortSpec {
                col: pos,
                dir: SortDir::Asc,
            }),
        };
        // `h_full + items_center`: the kit's header wrapper centers a
        // content-sized child, but a full-height one defeats it and the
        // label sticks to the top (visible once the grid density grows the
        // rows).
        //
        // `min_w_0` + `.truncate()` on the label ellipsizes a long name
        // (matching the body cells) instead of hard-clipping at the cell edge.
        let mut label = h_flex()
            .min_w_0()
            .gap_1()
            .items_center()
            .font_bold()
            .child(div().min_w_0().truncate().child(name));
        if let Some(dir) = indicator {
            label = label.child(
                Icon::new(match dir {
                    SortDir::Asc => KitIcon::ChevronUp,
                    SortDir::Desc => KitIcon::ChevronDown,
                })
                .size_3(),
            );
        }
        // A selected column tints only its body cells, never the header.
        div()
            .id(("col-th", col_ix))
            .test_support()
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .child(label)
            .when_some(view, |this, view| {
                this.cursor_pointer()
                    .on_click(move |event, _window, cx: &mut App| {
                        // The container's empty-area click must not clear a
                        // selection we just made here.
                        cx.stop_propagation();
                        if event.click_count() == 2 {
                            if sortable {
                                view.update(cx, |this, cx| this.sort_column(&tab_id, next, cx))
                                    .ok();
                            }
                        } else {
                            view.update(cx, |this, cx| {
                                this.grid_header_click(&tab_id, col_ix, cx);
                            })
                            .ok();
                        }
                    })
            })
            .into_any_element()
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Column selection tints its cells here; row selection is a full-width
        // band drawn by `render_tr`. A cell's own "current cell" highlight is
        // the library's (it owns click/arrow navigation).
        let col_selected = col_ix >= 1 && self.is_column_selected(col_ix);
        let (view, tab_id) = match &self.fetch {
            Some(f) => (Some(f.view.clone()), f.tab_id.clone()),
            None => (None, String::new()),
        };
        let inner = if col_ix == 0 {
            div()
                .h_full()
                .flex()
                .items_center()
                .justify_end()
                .text_color(cx.theme().muted_foreground)
                .child(format!("{}", row_ix + 1))
                .into_any_element()
        } else {
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
        };
        // The `#` (col 0) is inert: stop propagation so a click there doesn't
        // become the current cell. Data cells don't stop propagation — the
        // library's own handler sets the current cell, so arrow keys then
        // navigate from the clicked cell — but the app's row/column selection
        // is dropped first (the current cell's row becomes the row anchor).
        div()
            .id(format!("grid-cell:{row_ix}:{col_ix}"))
            .test_support()
            .relative()
            .w_full()
            .h_full()
            // A selected column highlights the full column width, matching the
            // library's full-cell current-cell highlight. The cell carries 5px
            // horizontal padding, so the tint is a negative-inset overlay that
            // reaches the cell edges (the cell's own overflow clips it). Drawn
            // before the content, so it sits behind the text.
            .when(col_selected, |this| {
                this.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .left(px(-5.))
                        .right(px(-5.))
                        .bg(cx.theme().tokens.table_active),
                )
            })
            .child(inner)
            .when_some(view, |this, view| {
                this.when(col_ix != 0, |this| this.cursor_pointer())
                    .when(col_ix == 0, |this| this.cursor_default())
                    .on_click(move |_event, _window, cx: &mut App| {
                        if col_ix == 0 {
                            cx.stop_propagation();
                            return;
                        }
                        view.update(cx, |this, cx| {
                            this.grid_cell_click(&tab_id, row_ix, cx);
                        })
                        .ok();
                    })
            })
    }

    fn has_more(&self, _: &App) -> bool {
        self.with_data(|d| !d.exhausted && !d.capped && !d.loading, false)
    }

    /// Trailing gutter for the table's overlay vertical scrollbar (16px wide).
    /// The kit's default is 12px, which leaves the last column's right edge
    /// under the scrollbar; reserve the full width plus a little air.
    fn render_last_empty_col(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .w(Scrollbar::width() + px(6.))
            .h_full()
            .flex_shrink_0()
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

#[cfg(test)]
mod selection_tests {
    // Import explicitly: a `use super::*` glob would pull in GPUI's `test`
    // macro (re-exported by `gpui_kit::*` under test-support) and shadow
    // Rust's built-in `#[test]`.
    use super::{selection_text, GridSelection, ResultsDelegate};
    use gpui_kit::SharedString;
    use std::collections::BTreeSet;

    fn sample_rows() -> Vec<Vec<Option<SharedString>>> {
        vec![
            vec![Some("a".into()), Some("1".into())],
            vec![Some("b".into()), None],
            vec![Some("c".into()), Some("3".into())],
        ]
    }

    #[test]
    fn click_row_replaces_toggles_and_ranges() {
        let mut d = ResultsDelegate::empty();
        d.click_row(2, false, false, 5);
        assert!(d.is_row_selected(2));
        assert!(!d.is_row_selected(1));
        // Cmd toggles rows on, then off.
        d.click_row(0, true, false, 5);
        assert!(d.is_row_selected(0) && d.is_row_selected(2));
        d.click_row(2, true, false, 5);
        assert!(d.is_row_selected(0) && !d.is_row_selected(2));
        // Shift extends from the last-clicked anchor (2, set by the toggle).
        d.click_row(3, false, true, 5);
        assert!(d.is_row_selected(2) && d.is_row_selected(3));
        assert!(!d.is_row_selected(1));
        // The range is clamped to the fetched rows.
        d.click_row(9, false, true, 5);
        assert!(d.is_row_selected(4) && !d.is_row_selected(5));
        // A plain click replaces the whole set.
        d.click_row(1, false, false, 5);
        assert!(d.is_row_selected(1) && !d.is_row_selected(0));
    }

    #[test]
    fn select_all_and_clear() {
        let mut d = ResultsDelegate::empty();
        d.select_all_rows(3);
        assert!((0..3).all(|r| d.is_row_selected(r)));
        d.clear_selection();
        assert!(!d.is_row_selected(0));
        // Selecting all of an empty result is a no-op, not an empty set.
        d.select_all_rows(0);
        assert!(d.copy_text(',').is_none());
    }

    #[test]
    fn column_selection_excludes_row_number() {
        let mut d = ResultsDelegate::empty();
        // Grid col 1 is the first data column.
        d.select_column(1);
        assert!(d.is_column_selected(1));
        assert!(!d.is_column_selected(0), "col 0 is the row number");
        // A row click leaves column mode.
        d.click_row(1, false, false, 3);
        assert!(!d.is_column_selected(1));
        assert!(d.is_row_selected(1));
    }

    #[test]
    fn cell_click_drops_selection_and_sets_row_anchor() {
        let mut d = ResultsDelegate::empty();
        d.select_column(1);
        d.select_all_rows(2);
        // A cell click clears the row/column selection (the library owns the
        // current cell) and makes the clicked row the anchor.
        d.set_current_row(1);
        assert!(!d.is_row_selected(0) && !d.is_row_selected(1));
        assert!(!d.is_column_selected(1));
        assert!(d.copy_text(',').is_none());
        // A later Shift-click on the row strip ranges from that row.
        d.click_row(3, false, true, 5);
        assert!(d.is_row_selected(1) && d.is_row_selected(2) && d.is_row_selected(3));
        assert!(!d.is_row_selected(0));
    }

    #[test]
    fn copy_text_rows_and_columns() {
        let rows = sample_rows();
        let sel = GridSelection::Rows(BTreeSet::from([0, 2]));
        assert_eq!(
            selection_text(&sel, &rows, ',').as_deref(),
            Some("a,1\nc,3")
        );
        // The configured delimiter is honored.
        assert_eq!(
            selection_text(&sel, &rows, ';').as_deref(),
            Some("a;1\nc;3")
        );
        // A column copies one value per line; NULL copies empty. Grid col 2 is
        // the second data column.
        let sel = GridSelection::Column(2);
        assert_eq!(selection_text(&sel, &rows, ',').as_deref(), Some("1\n\n3"));
        // Nothing selected / empty result copies nothing.
        assert_eq!(selection_text(&GridSelection::None, &rows, ','), None);
        assert_eq!(
            selection_text(&GridSelection::Rows(BTreeSet::from([0])), &[], ','),
            None
        );
    }

    #[test]
    fn column_copy_quotes_embedded_delimiter() {
        let rows = vec![
            vec![Some("a,b".into())],
            vec![Some("plain".into())],
            vec![Some("line\nbreak".into())],
        ];
        let sel = GridSelection::Column(1);
        assert_eq!(
            selection_text(&sel, &rows, ',').as_deref(),
            Some("\"a,b\"\nplain\n\"line\nbreak\"")
        );
    }
}
