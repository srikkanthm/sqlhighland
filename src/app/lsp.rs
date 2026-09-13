//! Editor LSP providers (definition, hover, completion).
//!
//! One weak view handle + tab id each; resolve everything live.
//!
//! Split out of `app.rs`; the inherent impl is re-opened here so the
//! view's behavior is unchanged (`impl` blocks may live in any module).

use super::*;

/// The Cmd-click document hook: answers `oracle-describe:` URIs by
/// opening a viewer tab. Named type for clippy's complexity lint.
pub(super) type ShowDocumentHook =
    Rc<dyn Fn(&lsp_types::ShowDocumentParams, &mut Window, &mut App) -> bool>;

/// Oracle definition provider for one tab: Cmd-hover underlines a table
/// word, Cmd-click jumps to its DESCRIBE output. Table-only (v1, same
/// rule as the hover table cards). Resolution is snapshot-only like the
/// other providers; the actual DESCRIBE run happens in the
/// `show_document` host hook below, which owns a `Context` + `Window`.
pub(super) struct OracleDefiner {
    pub(super) view: WeakEntity<SqlHighlandView>,
    pub(super) tab_id: String,
}

impl DefinitionProvider for OracleDefiner {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Vec<lsp_types::LocationLink>>> {
        let snapshot = text.to_string();
        let out = self.view.update(cx, |this, _| {
            let tab = this.tabs.iter().find(|t| t.id == self.tab_id)?;
            let conn_id = tab.connection_id.clone();
            if is_trivia_position(&snapshot, offset) {
                return None;
            }
            let (word, start) = word_at(&snapshot, offset);
            if word.is_empty() {
                return None;
            }
            let stmt = statement_at(&snapshot, offset).unwrap_or_else(|| snapshot.clone());
            let aliases = build_alias_map(&stmt);
            let qualifier = qualifier_before(&snapshot, start);
            let cache = conn_id
                .as_deref()
                .and_then(|id| this.meta.get(id))
                .cloned()?;
            let cache = lock(&cache);
            let tgt = describe_target(
                &word,
                qualifier.as_deref(),
                &aliases,
                &cache,
                this.show_system,
                &this.own_schema_of(&conn_id),
            );
            let (owner, table) = tgt?;
            let owner = owner?;
            // Single slash: `oracle-describe:/OWNER/TABLE` keeps both
            // segments in the path. (`://` would parse OWNER as the
            // authority/host, leaving the path with TABLE only.)
            let uri: lsp_types::Uri = format!("oracle-describe:/{owner}/{table}").parse().ok()?;
            let (sl, sc) = byte_to_lsp_pos(&snapshot, start);
            let (el, ec) = byte_to_lsp_pos(&snapshot, start + word.len());
            let origin = lsp_types::Range {
                start: lsp_types::Position {
                    line: sl,
                    character: sc,
                },
                end: lsp_types::Position {
                    line: el,
                    character: ec,
                },
            };
            let zero = lsp_types::Position {
                line: 0,
                character: 0,
            };
            Some(lsp_types::LocationLink {
                origin_selection_range: Some(origin),
                target_uri: uri,
                target_range: lsp_types::Range {
                    start: zero,
                    end: zero,
                },
                target_selection_range: lsp_types::Range {
                    start: zero,
                    end: zero,
                },
            })
        });
        match out {
            Ok(Some(link)) => Task::ready(Ok(vec![link])),
            _ => Task::ready(Ok(vec![])),
        }
    }
}

/// Oracle hover provider for one tab: table cards (columns) and column
/// cards (type/table/comment) from the cached dictionary. Snapshot-only —
/// same entity-lease rule as completions: the editor is mutably leased
/// along the hover path, so only the `&Rope` plus view-owned state.
pub(super) struct OracleHover {
    pub(super) view: WeakEntity<SqlHighlandView>,
    pub(super) tab_id: String,
}

impl HoverProvider for OracleHover {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Option<lsp_types::Hover>>> {
        let snapshot = text.to_string();
        let out = self.view.update(cx, |this, _| {
            let tab = this.tabs.iter().find(|t| t.id == self.tab_id)?;
            let conn_id = tab.connection_id.clone();
            if is_trivia_position(&snapshot, offset) {
                return None;
            }
            let (word, start) = word_at(&snapshot, offset);
            if word.is_empty() {
                return None;
            }
            let stmt = statement_at(&snapshot, offset).unwrap_or_else(|| snapshot.clone());
            let aliases = build_alias_map(&stmt);
            let qualifier = qualifier_before(&snapshot, start);
            let cache = conn_id
                .as_deref()
                .and_then(|id| this.meta.get(id))
                .cloned()?;
            let md = {
                let c = lock(&cache);
                hover_markdown(
                    &word,
                    qualifier.as_deref(),
                    &aliases,
                    &c,
                    this.show_system,
                    &this.own_schema_of(&conn_id),
                )
            }?;
            Some(lsp_types::Hover {
                contents: lsp_types::HoverContents::Markup(lsp_types::MarkupContent {
                    kind: lsp_types::MarkupKind::Markdown,
                    value: md,
                }),
                range: None,
            })
        });
        match out {
            Ok(Some(h)) => Task::ready(Ok(Some(h))),
            _ => Task::ready(Ok(None)),
        }
    }
}

/// Oracle suggestion provider for one tab. Holds only a weak view handle +
/// tab id and resolves everything live (connection, cache snapshot, prefs),
/// so tab rebinding needs no reinstall.
pub(super) struct OracleCompleter {
    pub(super) view: WeakEntity<SqlHighlandView>,
    pub(super) tab_id: String,
}

impl CompletionProvider for OracleCompleter {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _trigger: lsp_types::CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<lsp_types::CompletionResponse>> {
        // NOTE: the editor entity is mutably leased for the whole trigger
        // path (`handle_completion_trigger` runs inside its update), so this
        // must NEVER read the editor entity — only the passed-in Rope plus
        // view-owned state (different entity, safe to touch).
        let snapshot = text.to_string();
        let out = self.view.update(cx, |this, _| {
            this.completion_items_for(&self.tab_id, &snapshot, offset, false)
        });
        match out {
            Ok((items, _, _)) => Task::ready(Ok(lsp_types::CompletionResponse::Array(items))),
            Err(_) => Task::ready(Ok(lsp_types::CompletionResponse::Array(Vec::new()))),
        }
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, cx: &mut App) -> bool {
        // Same lease rule as above: view-owned flag only, no editor access.
        // Shape-gating (prefix length, dot, trivia) happens in completions(),
        // which owns the full buffer text.
        let last = new_text.chars().last();
        let wordy =
            last.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '#');
        // Single space also fires: `FROM |` should offer tables immediately.
        // (Multi-char pastes never trigger; newlines never trigger.)
        if last != Some('.') && !wordy && new_text != " " {
            return false;
        }
        // Manual mode: the shortcut path presents directly; never auto-fire.
        self.view
            .update(cx, |this, _| this.complete_auto)
            .unwrap_or(false)
    }
}
