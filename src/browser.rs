//! Schema-browser surface: per-connection tree items, group builders,
//! background refresh, and expand/collapse state.
//!
//! Extracted from `app.rs` (refactor Phase 3); behavior unchanged.

use gpui::{Context, Window};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::tree::{TreeEvent, TreeItem, TreeState};
use gpui_kit::*;

use crate::app::SqlHighlandView;
use crate::schema::{OracleProvider, SchemaProvider as _};

impl SqlHighlandView {
    // -- Schema browser ---------------------------------------------------

    /// Tree node ids (per-connection trees, so no connection prefix):
    /// `s:{schema}` folder, `g:{schema}/{Tables|Views|Sequences}` folder,
    /// `o:{schema}/{T|V|S}/{object}` (click → viewer tab),
    /// `c:{schema}/{object}/{column}` leaf.
    pub(crate) fn browser_tree_items(&self, conn_id: &str, filter: &str) -> Vec<TreeItem> {
        let loading_item = |label: &str| {
            vec![
                TreeItem::new(format!("b:note:{label}"), label).disabled(true),
            ]
        };
        let Some(cache) = self.meta.get(conn_id) else {
            return loading_item("Loading schema…");
        };
        let Ok(cache) = cache.lock() else {
            return loading_item("Loading schema…");
        };
        if cache.loading
            && cache.tables.is_empty()
            && cache.columns.is_empty()
            && cache.sequences.is_empty()
        {
            return loading_item("Loading schema…");
        }
        let own = self
            .connections
            .iter()
            .find(|c| c.id == conn_id)
            .map(|c| c.user.clone())
            .unwrap_or_default();
        let tree = OracleProvider.tree(&cache, self.show_system, &own, false);
        let tree = crate::schema::filter_tree(&tree, filter);
        if tree.schemas.is_empty() {
            let label = if filter.trim().is_empty() {
                "No objects found"
            } else {
                "No matches"
            };
            return loading_item(label);
        }
        let expanded = self.browser_expanded.get(conn_id).cloned().unwrap_or_default();
        let exp = |id: &str| expanded.contains(id);
        // Own schema's groups sit at the root (no schema folder); every
        // other visible schema nests under "Other Users". The tree model
        // already orders own-first, so partition preserves display order.
        const OTHER_ID: &str = "u:users";
        let (own_schemas, other_schemas): (Vec<_>, Vec<_>) = tree
            .schemas
            .iter()
            .partition(|g| g.name.eq_ignore_ascii_case(&own));
        let mut roots = Vec::with_capacity(own_schemas.len() + 1);
        for g in own_schemas {
            roots.extend(Self::browser_group_items(g, &exp));
        }
        if !other_schemas.is_empty() {
            let mut users = Vec::with_capacity(other_schemas.len());
            for g in other_schemas {
                let sid = format!("s:{}", g.name);
                let groups = Self::browser_group_items(g, &exp);
                users.push(
                    TreeItem::new(sid.clone(), g.name.clone())
                        .children(groups)
                        .expanded(exp(&sid)),
                );
            }
            roots.push(
                TreeItem::new(OTHER_ID, format!("Other Users ({})", users.len()))
                    .children(users)
                    .expanded(exp(OTHER_ID)),
            );
        }
        roots
    }

    /// Tables/Views/Sequences group items for one schema (shared by the
    /// single-schema root path and the per-schema folder path).
    pub(crate) fn browser_group_items(
        g: &crate::schema::SchemaGroup,
        exp: &impl Fn(&str) -> bool,
    ) -> Vec<TreeItem> {
        {
            let mut groups = Vec::with_capacity(3);
            for (group, objs) in [("Tables", &g.tables), ("Views", &g.views)] {
                if objs.is_empty() {
                    continue;
                }
                // Groups stay collapsed until opened: a fresh expand shows
                // only the three group rows, not hundreds of objects.
                let gid = format!("g:{}/{group}", g.name);
                let kind = group.as_bytes()[0] as char;
                let mut items = Vec::with_capacity(objs.len());
                for o in objs {
                    let oid = format!("o:{}:{kind}:{}", g.name, o.name);
                    let cols: Vec<TreeItem> = o
                        .columns
                        .iter()
                        .map(|c| {
                            TreeItem::new(
                                format!("c:{}:{}:{}", g.name, o.name, c.name),
                                format!("{} — {}", c.name, c.data_type),
                            )
                        })
                        .collect();
                    items.push(
                        TreeItem::new(oid.clone(), o.name.clone())
                            .children(cols)
                            .expanded(exp(&oid)),
                    );
                }
                groups.push(
                    TreeItem::new(gid.clone(), format!("{group} ({})", objs.len()))
                        .children(items)
                        .expanded(exp(&gid)),
                );
            }
            if !g.sequences.is_empty() {
                let gid = format!("g:{}/Sequences", g.name);
                let items: Vec<TreeItem> = g
                    .sequences
                    .iter()
                    .map(|s| {
                        TreeItem::new(format!("o:{}:S:{s}", g.name), s.clone())
                    })
                    .collect();
                groups.push(
                    TreeItem::new(gid.clone(), format!("Sequences ({})", g.sequences.len()))
                        .children(items)
                        .expanded(exp(&gid)),
                );
            }
            groups
        }
    }

    /// Rebuild one open browser tree from cache (filter + expansion kept).
    /// No-op for closed or untracked connections.
    pub(crate) fn refresh_browser(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        if !self.browser_open.contains(conn_id) {
            return;
        }
        let Some(tree) = self.browser_trees.get(conn_id).cloned() else {
            return;
        };
        let filter = self
            .browser_filters
            .get(conn_id)
            .map(|f| f.read(cx).value().to_string())
            .unwrap_or_default();
        let items = self.browser_tree_items(conn_id, &filter);
        tree.update(cx, |t, cx| t.set_items(items, cx));
    }

    /// Expand/collapse the schema tree under a connection. Expanding a
    /// dead connection auto-connects first; `ensure_meta` warms the
    /// dictionary and its completion hook rebuilds the tree on arrival.
    pub(crate) fn toggle_browser(&mut self, conn_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.browser_open.contains(conn_id) {
            self.browser_open.remove(conn_id);
            self.browser_trees.remove(conn_id);
            self.browser_expanded.remove(conn_id);
            self.browser_filters.remove(conn_id);
            cx.notify();
            return;
        }
        self.browser_open.insert(conn_id.to_string());
        if !self.live.contains(conn_id) {
            self.connect_connection(conn_id, window, cx);
        }
        self.ensure_meta(conn_id, cx);
        // Per-connection filter: keystrokes rebuild only this tree.
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter schema…"));
        let filter_in = filter.clone();
        let filter_conn = conn_id.to_string();
        let filter_sub = cx.subscribe_in(
            &filter_in,
            window,
            move |this: &mut Self, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.refresh_browser(&filter_conn, cx);
                    // Same as toggles: the container height is computed at
                    // render time from visible rows.
                    cx.notify();
                }
            },
        );
        self._subs.push(filter_sub);
        self.browser_filters.insert(conn_id.to_string(), filter);
        let filter = self
            .browser_filters
            .get(conn_id)
            .map(|f| f.read(cx).value().to_string())
            .unwrap_or_default();
        let items = self.browser_tree_items(conn_id, &filter);
        let state = cx.new(|cx| TreeState::new(cx).items(items));
        let sub_conn = conn_id.to_string();
        let sub = cx.subscribe(&state, move |this: &mut Self, _, event: &TreeEvent, cx| {
            // Expansion state only — no auto-scroll. (An auto-reveal that
            // pinned expanded nodes to the top used to live here; it moved
            // rows under the cursor mid-gesture and broke double-click
            // opens, so expansion leaves the scroll alone now.)
            match event {
                TreeEvent::Expanded(id) => {
                    this.browser_expanded
                        .entry(sub_conn.clone())
                        .or_default()
                        .insert(id.to_string());
                }
                TreeEvent::Collapsed(id) => {
                    if let Some(set) = this.browser_expanded.get_mut(&sub_conn) {
                        set.remove(id.as_ref());
                    }
                }
            };
            // Container height derives from visible rows (read at render),
            // so toggles must repaint the view, not just the tree.
            cx.notify();
        });
        self._subs.push(sub);
        self.browser_trees.insert(conn_id.to_string(), state);
        cx.notify();
    }
}
