//! Autocomplete provider: snapshot-based completion items, LSP mapping,
//! and the manual-completion trigger.
//!
//! Extracted from `app.rs` (refactor Phase 3); behavior unchanged.

use gpui::{Context, Window};

use crate::app::{SqlHighlandView, COMPLETE_LIMIT};
use crate::complete::{
    allows_empty_prefix, ambiguous_columns, build_alias_map, byte_to_lsp_pos, classify_context,
    detect_join_on, display_name, function_insert, insert_text_for, is_system_schema,
    is_trivia_position, join_condition_candidates, rank_candidates, resolve_qualifier, scope_label,
    short_comment, word_prefix, Candidate, CandidateKind, CompleteContext, ForeignKey, ScopeTable,
    EXPR_KEYWORDS, ORACLE_FUNCTIONS, ORACLE_KEYWORDS, PRED_FOLLOW, PRED_KEYWORDS, SELECT_FOLLOW,
    STMT_KEYWORDS,
};
use crate::session::lock;
use crate::sql::statement_at;

impl SqlHighlandView {
    // -- Autocomplete --------------------------------------------------------

    /// Build popup items for `text` at byte `offset`: returns
    /// (items, word_start, prefix). Pure snapshot read — safe from provider
    /// tasks and the manual shortcut alike. Empty items = no popup.
    /// Gating lives here (not the trigger) because only this path owns the
    /// buffer text; the trigger sees just the typed fragment. `force`
    /// (manual shortcut) skips the 2-char gate but never trivia.
    pub(crate) fn completion_items_for(
        &mut self,
        tab_id: &str,
        text: &str,
        offset: usize,
        force: bool,
    ) -> (Vec<lsp_types::CompletionItem>, usize, String) {
        let empty = (Vec::new(), offset, String::new());
        let Some(tab) = self.tabs.iter().find(|t| t.id == tab_id) else {
            return empty;
        };
        let conn_id = tab.connection_id.clone();
        if is_trivia_position(text, offset) {
            return empty;
        }
        let (prefix, word_start) = word_prefix(text, offset);
        // 2-char gate; a trailing dot forces the column list even empty, and
        // an empty prefix is allowed right after operand-expecting keywords
        // (`FROM |` lists tables immediately — the space-trigger path).
        if !force && prefix.len() < 2 {
            let dot_forced =
                word_start > 0 && text[..word_start.min(text.len())].trim_end().ends_with('.');
            if !dot_forced && !(prefix.is_empty() && allows_empty_prefix(text, offset)) {
                return empty;
            }
        }
        // Snapshot the cache (clone the Arc; never lock the session here).
        // Missing/stale cache kicks a background refresh; this request
        // completes from keywords + whatever is cached.
        let cache = conn_id.as_deref().and_then(|id| self.meta.get(id)).cloned();
        let stale = cache
            .as_ref()
            .map(|c| c.lock().map(|c| c.is_stale()).unwrap_or(true))
            .unwrap_or(true);
        if stale && conn_id.is_some() {
            // Bound tab with a cold cache: a refresh is possible (runs,
            // connects, and the manual trigger all call ensure_meta), so
            // the notice is honest. Unbound tabs skip it — no connection
            // means no refresh could ever clear it (stuck-status bug).
            self.status = "Loading suggestions…".into();
        }
        let stmt = statement_at(text, offset).unwrap_or_else(|| text.to_string());
        let aliases = build_alias_map(&stmt);
        // JOIN … ON with a fresh condition short-circuits everything else:
        // FK-derived conditions or no popup at all.
        let head_end = offset.min(text.len());
        if let Some((right_alias, right)) = detect_join_on(&text[..head_end], &aliases) {
            let fks: Vec<ForeignKey> = cache
                .as_ref()
                .and_then(|c| c.lock().ok().map(|c| c.fks.clone()))
                .unwrap_or_default();
            let cands = join_condition_candidates(&right_alias, &right, &aliases, &fks);
            if !cands.is_empty() {
                return (
                    Self::to_items(cands, text, word_start, offset),
                    word_start,
                    prefix,
                );
            }
            // No FK links the pair: fall through to Predicate (columns for a
            // hand-written condition) instead of an empty popup.
        }
        let is_seq = |n: &str| {
            cache
                .as_ref()
                .is_some_and(|c| c.lock().map(|c| c.is_sequence(n)).unwrap_or(false))
        };
        let ctx = classify_context(text, offset, &is_seq);
        let show_system = self.show_system;
        let mut cands: Vec<Candidate> = Vec::new();
        let usage_of = |conn: &Option<String>, label: &str| {
            conn.as_ref()
                .and_then(|id| {
                    self.usage
                        .get(&(id.clone(), label.to_ascii_uppercase()))
                        .copied()
                })
                .unwrap_or(0)
        };
        // Own schema (connected user): its objects rank above the shared
        // catalog, so a DBA login still sees their own tables first.
        let own_schema = self.own_schema_of(&conn_id);
        // Client-side mirror of the SQL filter (cache may predate a toggle,
        // or hold system rows from an unfiltered fetch): hide system owners
        // except the connected user's own schema.
        let hide_system = |owner: &str| {
            !show_system && !owner.eq_ignore_ascii_case(&own_schema) && is_system_schema(owner)
        };
        // In-scope tables as ScopeTables: single resolution shared by
        // column completion (with ambiguity info) and qualifier detail.
        // Deterministic alias order.
        let mut scope_order: Vec<String> = aliases.keys().cloned().collect();
        scope_order.sort();
        let scope_tables: Vec<ScopeTable> = if let Some(cache) = &cache {
            let cache = lock(cache);
            let mut seen = std::collections::HashSet::new();
            let mut out = Vec::new();
            for alias in &scope_order {
                let Some(tref) = aliases.get(alias) else {
                    continue;
                };
                let key = (
                    tref.owner.clone().unwrap_or_default().to_ascii_uppercase(),
                    tref.name.to_ascii_uppercase(),
                );
                if !seen.insert(key) {
                    continue;
                }
                let cols = cache.columns_for(tref.owner.as_deref(), &tref.name);
                out.push(ScopeTable {
                    owner: tref.owner.clone(),
                    table: tref.name.clone(),
                    cols,
                });
            }
            out
        } else {
            Vec::new()
        };
        // Shared builders: columns of in-scope tables, keyword lists,
        // function skeletons. Each context composes only what SQL allows.
        let column_detail = |col: &crate::metadata::ColumnMeta, scope_name: &str| -> String {
            let mut d = if col.data_type.is_empty() {
                "COLUMN".to_string()
            } else {
                col.data_type.clone()
            };
            d.push_str(" · ");
            d.push_str(scope_name);
            let c = short_comment(&col.comments);
            if !c.is_empty() {
                d.push_str(" — ");
                d.push_str(&c);
            }
            d
        };
        let push_scope_columns = |cands: &mut Vec<Candidate>| {
            let ambiguous = ambiguous_columns(&scope_tables);
            for t in &scope_tables {
                for col in &t.cols {
                    // Collision across scope tables: qualify so the insert
                    // is unambiguous SQL (`e.DEPTNO`, never bare `DEPTNO`).
                    let (label, owner_out) = if ambiguous.contains(&col.name.to_ascii_uppercase()) {
                        match scope_label(&t.owner, &t.table, &aliases) {
                            Some(scoped) => (format!("{scoped}.{}", col.name), t.owner.clone()),
                            None => (col.name.clone(), t.owner.clone()),
                        }
                    } else {
                        (col.name.clone(), t.owner.clone())
                    };
                    cands.push(Candidate {
                        label,
                        detail: column_detail(col, &t.table),
                        kind: CandidateKind::ColumnInScope,
                        owner: owner_out,
                        usage: usage_of(&conn_id, &col.name),
                    });
                }
            }
        };
        let push_keywords = |cands: &mut Vec<Candidate>, kws: &[&str]| {
            for kw in kws {
                cands.push(Candidate {
                    label: kw.to_string(),
                    detail: "KEYWORD".to_string(),
                    kind: CandidateKind::Keyword,
                    owner: None,
                    usage: 0,
                });
            }
        };
        let push_functions = |cands: &mut Vec<Candidate>| {
            for (name, sig) in ORACLE_FUNCTIONS {
                cands.push(Candidate {
                    label: function_insert(name),
                    detail: sig.to_string(),
                    kind: CandidateKind::Function,
                    owner: None,
                    usage: usage_of(&conn_id, name),
                });
            }
        };
        let push_sequences = |cands: &mut Vec<Candidate>| {
            let Some(cache) = &cache else {
                return;
            };
            let cache = lock(cache);
            for s in &cache.sequences {
                if hide_system(&s.owner) {
                    continue;
                }
                cands.push(Candidate {
                    label: s.name.clone(),
                    detail: format!("SEQUENCE · {}", s.owner),
                    kind: CandidateKind::Sequence,
                    owner: Some(s.owner.clone()),
                    usage: usage_of(&conn_id, &s.name),
                });
            }
        };
        match &ctx {
            // Handled above via detect_join_on — unreachable here.
            CompleteContext::JoinOn { .. } => {}
            CompleteContext::SequenceMember(_) => {
                for kw in ["NEXTVAL", "CURRVAL"] {
                    cands.push(Candidate {
                        label: kw.to_string(),
                        detail: "SEQUENCE".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                    });
                }
            }
            CompleteContext::ColumnOf(q) => {
                if let Some(tref) = resolve_qualifier(q, &aliases) {
                    let cols = cache.as_ref().map(|c| {
                        let c = lock(c);
                        c.columns_for(tref.owner.as_deref(), &tref.name)
                    });
                    if let Some(cols) = cols {
                        for col in cols {
                            cands.push(Candidate {
                                label: col.name.clone(),
                                detail: column_detail(&col, &tref.name),
                                kind: CandidateKind::ColumnInScope,
                                owner: tref.owner.clone(),
                                usage: usage_of(&conn_id, &col.name),
                            });
                        }
                    }
                }
            }
            CompleteContext::StatementStart => {
                push_keywords(&mut cands, STMT_KEYWORDS);
            }
            CompleteContext::AfterFrom => {
                // Tables only — keywords never follow FROM. Own-schema
                // tables show (and insert) bare: Oracle resolves
                // unqualified names to the connected schema first.
                if let Some(cache) = &cache {
                    let cache = lock(cache);
                    for t in &cache.tables {
                        if hide_system(&t.owner) {
                            continue;
                        }
                        let label = display_name(Some(&t.owner), &t.name, &own_schema);
                        cands.push(Candidate {
                            label,
                            detail: "TABLE".to_string(),
                            kind: CandidateKind::Table,
                            owner: Some(t.owner.clone()),
                            usage: usage_of(&conn_id, &t.name),
                        });
                    }
                }
            }
            CompleteContext::OwnerTables(owner) => {
                // `FROM owner.|` — that owner's tables, bare names.
                if let Some(cache) = &cache {
                    let cache = lock(cache);
                    for t in &cache.tables {
                        if !t.owner.eq_ignore_ascii_case(owner) {
                            continue;
                        }
                        cands.push(Candidate {
                            label: t.name.clone(),
                            detail: format!("TABLE · {}", t.owner),
                            kind: CandidateKind::Table,
                            owner: Some(t.owner.clone()),
                            usage: usage_of(&conn_id, &t.name),
                        });
                    }
                }
            }
            CompleteContext::SelectList => {
                push_scope_columns(&mut cands);
                push_functions(&mut cands);
                push_sequences(&mut cands);
                push_keywords(&mut cands, EXPR_KEYWORDS);
                push_keywords(&mut cands, SELECT_FOLLOW);
            }
            CompleteContext::Predicate => {
                push_scope_columns(&mut cands);
                push_functions(&mut cands);
                push_sequences(&mut cands);
                push_keywords(&mut cands, PRED_KEYWORDS);
                push_keywords(&mut cands, PRED_FOLLOW);
            }
            CompleteContext::BareWord => {
                // Ambiguous position: keywords + functions, minus the
                // function names (which complete as call skeletons below).
                for kw in ORACLE_KEYWORDS {
                    if ORACLE_FUNCTIONS.iter().any(|(n, _)| n == kw) {
                        continue;
                    }
                    cands.push(Candidate {
                        label: kw.to_string(),
                        detail: "KEYWORD".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                    });
                }
                push_functions(&mut cands);
            }
        }
        let ranked = rank_candidates(&prefix, cands, &own_schema, COMPLETE_LIMIT);
        if ranked.is_empty() {
            return empty;
        }
        (
            Self::to_items(ranked, text, word_start, offset),
            word_start,
            prefix,
        )
    }

    /// Map ranked candidates to popup items with explicit edit ranges (see
    /// the sticky-`trigger_start_offset` note in `completion_items_for`).
    fn to_items(
        ranked: Vec<Candidate>,
        text: &str,
        word_start: usize,
        offset: usize,
    ) -> Vec<lsp_types::CompletionItem> {
        // Explicit edit range per item: the kit falls back to its sticky
        // `trigger_start_offset` otherwise, which survives across accepts
        // and replaces the whole buffer on the second completion.
        let (s_line, s_char) = byte_to_lsp_pos(text, word_start);
        let (e_line, e_char) = byte_to_lsp_pos(text, offset);
        ranked
            .into_iter()
            .enumerate()
            .map(|(ix, c)| lsp_types::CompletionItem {
                label: c.label.clone(),
                detail: Some(c.detail),
                kind: Some(match c.kind {
                    CandidateKind::JoinCondition => lsp_types::CompletionItemKind::SNIPPET,
                    CandidateKind::ColumnInScope | CandidateKind::Column => {
                        lsp_types::CompletionItemKind::FIELD
                    }
                    CandidateKind::Table => lsp_types::CompletionItemKind::CLASS,
                    CandidateKind::Sequence => lsp_types::CompletionItemKind::VALUE,
                    CandidateKind::Function => lsp_types::CompletionItemKind::FUNCTION,
                    CandidateKind::Keyword => lsp_types::CompletionItemKind::KEYWORD,
                }),
                sort_text: Some(format!("{ix:04}")),
                text_edit: Some(lsp_types::CompletionTextEdit::Edit(lsp_types::TextEdit {
                    range: lsp_types::Range {
                        start: lsp_types::Position {
                            line: s_line,
                            character: s_char,
                        },
                        end: lsp_types::Position {
                            line: e_line,
                            character: e_char,
                        },
                    },
                    new_text: insert_text_for(c.kind, &c.label),
                })),
                ..Default::default()
            })
            .collect()
    }

    /// Manual trigger (ctrl-space): compute synchronously and present.
    /// Kicks a metadata refresh first when the cache is stale so the next
    /// keystroke completes from data.
    pub(crate) fn trigger_complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.editor_focused(window, cx) {
            return;
        }
        let tab_id = self.active_tab().id.clone();
        let conn_id = self.active_tab().connection_id.clone();
        if let Some(id) = conn_id {
            self.ensure_meta(&id, cx);
        }
        let text = self.active_tab().editor.read(cx).value().to_string();
        let cursor = self.active_tab().editor.read(cx).cursor();
        let (items, start, prefix) = self.completion_items_for(&tab_id, &text, cursor, true);
        if items.is_empty() {
            return;
        }
        self.active_tab().editor.update(cx, |editor, cx| {
            editor.present_completion_items(start, prefix, items, cx);
        });
    }
}

impl SqlHighlandView {
    /// Bump usage counts for tables named in an executed statement so
    /// future rankings prefer working objects. Bounded: cleared past 5k.
    pub(crate) fn bump_usage(&mut self, conn_id: &str, sql: &str) {
        let map = build_alias_map(sql);
        if map.is_empty() {
            return;
        }
        for tref in map.values() {
            let key = (conn_id.to_string(), tref.name.to_ascii_uppercase());
            *self.usage.entry(key).or_insert(0) += 1;
        }
        if self.usage.len() > 5000 {
            self.usage.clear();
        }
    }
}
