//! Autocomplete provider: snapshot-based completion items, LSP mapping,
//! and the manual-completion trigger.
//!
//! Extracted from `app.rs` (refactor Phase 3); behavior unchanged.

use gpui::{Context, Window};

use std::collections::HashMap;

use crate::app::{SqlHighlandView, COMPLETE_LIMIT};
use crate::complete::{
    allows_empty_prefix, ambiguous_columns, build_alias_map, byte_to_lsp_pos, classify_context,
    detect_join_on, display_name, function_insert, insert_text_for, is_trivia_position,
    join_condition_candidates, owners_match, rank_candidates, resolve_qualifier, scope_label,
    select_list_is_empty, short_comment, word_prefix, Candidate, CandidateKind, CompleteContext,
    DmlKind, ForeignKey, FromOrigin, Relation, RelationKind, ScopeForest, ScopeTable, TableRef,
    CASE_CONDITION_KEYWORDS, CASE_RESULT_KEYWORDS, CASE_START_KEYWORDS, FROM_FOLLOW, INTO_FOLLOW,
    JOIN_FOLLOW, MERGE_FOLLOW, SUBQUERY_START_KEYWORDS, UPDATE_FOLLOW, USING_FOLLOW,
    WINDOW_KEYWORDS,
};
use crate::metadata::{ColumnMeta, SharedCache};
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
        let structural = tab.scope.clone().filter(|s| !s.is_empty());
        // Any request that ends up empty must clear the accept flag; the
        // non-empty returns below set it. Cleared here so every early return
        // (trivia, short prefix, …) leaves it false.
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.pending_completion = false;
        }
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
        // Structural scope from the last debounced pass (None/empty → lexical
        // fallback below). Cloned only after the cheap gates above, since the
        // completion callback runs synchronously on the UI thread.
        // Snapshot the cache (clone the Arc; never lock the session here).
        // Missing/stale cache kicks a background refresh; this request
        // completes from keywords + whatever is cached.
        let cache = conn_id
            .as_deref()
            .and_then(|id| self.browser.meta.get(id))
            .cloned();
        let stale = cache
            .as_ref()
            .map(|c| lock(c).is_stale(self.metadata_ttl))
            .unwrap_or(true);
        if stale && conn_id.is_some() {
            // Bound tab with a cold cache: a refresh is possible (runs,
            // connects, and the manual trigger all call ensure_meta), so
            // the notice is honest. Unbound tabs skip it — no connection
            // means no refresh could ever clear it (stuck-status bug).
            self.status = "Loading suggestions…".into();
        }
        // Scope-correct aliases + in-scope tables when the structural pass has
        // run; otherwise the lexical statement scan (unchanged behavior). A
        // structural gap (no relations resolved at this offset) also falls back
        // rather than suppressing columns.
        let (aliases, scope_tables) = match structural.as_ref() {
            Some(forest) => {
                let (aliases, tables) = scope_from_forest(forest, offset, &cache);
                if aliases.is_empty() && tables.is_empty() {
                    let stmt = statement_at(text, offset).unwrap_or_else(|| text.to_string());
                    let aliases = build_alias_map(&stmt);
                    let tables = lexical_scope_tables(&aliases, &cache);
                    (aliases, tables)
                } else {
                    (aliases, tables)
                }
            }
            None => {
                let stmt = statement_at(text, offset).unwrap_or_else(|| text.to_string());
                let aliases = build_alias_map(&stmt);
                let tables = lexical_scope_tables(&aliases, &cache);
                (aliases, tables)
            }
        };
        // JOIN … ON: FK-derived conditions, minus any already typed. When none
        // remain (or no FK links the pair), fall through to Predicate.
        let head_end = offset.min(text.len());
        if let Some((right_alias, right, tail_start)) = detect_join_on(&text[..head_end], &aliases)
        {
            let fks: Vec<ForeignKey> = cache
                .as_ref()
                .map(|c| lock(c).fks.clone())
                .unwrap_or_default();
            let cands = join_condition_candidates(&right_alias, &right, &aliases, &fks);
            let typed = normalize_condition(&text[tail_start.min(head_end)..head_end]);
            let cands: Vec<Candidate> = cands
                .into_iter()
                .filter(|c| !typed.contains(&normalize_condition(&c.label)))
                .collect();
            if !cands.is_empty() {
                if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
                    tab.pending_completion = true;
                }
                return (
                    Self::to_items(cands, text, word_start, offset),
                    word_start,
                    prefix,
                );
            }
            // No FK links the pair: fall through to Predicate (columns for a
            // hand-written condition) instead of an empty popup.
        }
        let dialect = self.engine_of(&conn_id).dialect();
        // Sequence pseudo-columns (`seq.`) only exist where the dialect has
        // them; otherwise a qualified prefix resolves as a normal table.
        let is_seq = |n: &str| {
            dialect.uses_sequence_pseudocolumns()
                && cache.as_ref().is_some_and(|c| lock(c).is_sequence(n))
        };
        let ctx = classify_context(text, offset, &is_seq);
        // A qualifier that names a package completes its members (`pkg.`).
        let ctx = match ctx {
            CompleteContext::ColumnOf(q)
                if cache.as_ref().is_some_and(|c| {
                    let last = q.rsplit('.').next().unwrap_or(&q);
                    lock(c).is_package_name(&last.to_ascii_uppercase())
                }) =>
            {
                CompleteContext::PackageMember(q)
            }
            other => other,
        };
        let show_system = self.show_system;
        let mut cands: Vec<Candidate> = Vec::new();
        let usage_of = |conn: &Option<String>, label: &str| {
            conn.as_ref()
                .and_then(|id| {
                    self.browser
                        .usage
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
            !show_system
                && !owner.eq_ignore_ascii_case(&own_schema)
                && dialect.is_system_schema(owner)
        };
        // In-scope tables as ScopeTables (columns, alias detail) come from
        // `aliases` + `scope_tables`, built above from the structural scope
        // when available.
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
            // `scope_tables` is nearest-scope-first, so the enumerate index is
            // the proximity used as a ranking tie-break.
            for (depth, t) in scope_tables.iter().enumerate() {
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
                        insert: None,
                        label,
                        detail: column_detail(col, &t.table),
                        kind: CandidateKind::ColumnInScope,
                        owner: owner_out,
                        usage: usage_of(&conn_id, &col.name),
                        depth: depth.min(u8::MAX as usize) as u8,
                    });
                }
            }
        };
        let push_keywords = |cands: &mut Vec<Candidate>, kws: &[&str]| {
            for kw in kws {
                cands.push(Candidate {
                    insert: None,
                    label: kw.to_string(),
                    detail: "KEYWORD".to_string(),
                    kind: CandidateKind::Keyword,
                    owner: None,
                    usage: 0,
                    depth: 0,
                });
            }
        };
        let push_functions = |cands: &mut Vec<Candidate>| {
            for (name, sig) in dialect.functions() {
                cands.push(Candidate {
                    insert: None,
                    label: function_insert(name),
                    detail: sig.to_string(),
                    kind: CandidateKind::Function,
                    owner: None,
                    usage: usage_of(&conn_id, name),
                    depth: 0,
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
                    insert: None,
                    label: s.name.clone(),
                    detail: format!("SEQUENCE · {}", s.owner),
                    kind: CandidateKind::Sequence,
                    owner: Some(s.owner.clone()),
                    usage: usage_of(&conn_id, &s.name),
                    depth: 0,
                });
            }
        };
        // Empty select list: offer tables (and CTE names) as `* FROM t`
        // templates, so the query is populated and columns are available from
        // the next completion on. Deduped by label (table + synonym clashes).
        let push_table_templates = |cands: &mut Vec<Candidate>| {
            let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
            if let Some(cache) = &cache {
                let cache = lock(cache);
                for t in &cache.tables {
                    if hide_system(&t.owner) {
                        continue;
                    }
                    let label = display_name(Some(&t.owner), &t.name, &own_schema);
                    if !seen.insert(label.to_ascii_uppercase()) {
                        continue;
                    }
                    let target = qualified_ident(Some(&t.owner), &t.name, &own_schema);
                    cands.push(Candidate {
                        insert: Some(format!("* FROM {target} ")),
                        label,
                        detail: kind_label(t.kind).to_string(),
                        kind: CandidateKind::Table,
                        owner: Some(t.owner.clone()),
                        usage: usage_of(&conn_id, &t.name),
                        depth: 0,
                    });
                }
            }
            if let Some(forest) = structural.as_ref() {
                for name in &forest.ctes {
                    if !seen.insert(name.to_ascii_uppercase()) {
                        continue;
                    }
                    cands.push(Candidate {
                        insert: Some(format!("* FROM {name} ")),
                        label: name.clone(),
                        detail: "CTE".to_string(),
                        kind: CandidateKind::Table,
                        owner: None,
                        usage: 0,
                        depth: 0,
                    });
                }
            }
        };
        // DML positions: an INSERT column list or UPDATE SET body offers the
        // target table's columns only (plus expressions for UPDATE), instead
        // of the generic context's keyword noise.
        if let Some(anchor) = structural.as_ref().and_then(|s| s.dml_at(offset)) {
            let mut dml: Vec<Candidate> = relation_columns(&anchor.target, &cache)
                .into_iter()
                .map(|col| Candidate {
                    insert: None,
                    label: col.name.clone(),
                    detail: column_detail(&col, &anchor.target.name),
                    kind: CandidateKind::ColumnInScope,
                    owner: anchor.target.owner.clone(),
                    usage: usage_of(&conn_id, &col.name),
                    depth: 0,
                })
                .collect();
            if anchor.kind == DmlKind::UpdateSet {
                // The SET right-hand/left-hand sides are expressions.
                push_functions(&mut dml);
            }
            let ranked = rank_candidates(&prefix, dml, &own_schema, COMPLETE_LIMIT);
            if !ranked.is_empty() {
                return (
                    Self::to_items(ranked, text, word_start, offset),
                    word_start,
                    prefix,
                );
            }
            // Target unresolvable (unknown table): fall through to the generic
            // context rather than showing nothing.
        }
        match &ctx {
            // Handled above via detect_join_on — unreachable here.
            CompleteContext::JoinOn { .. } => {}
            CompleteContext::SequenceMember(_) => {
                for kw in dialect.sequence_members() {
                    cands.push(Candidate {
                        insert: None,
                        label: kw.to_string(),
                        detail: "SEQUENCE".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                        depth: 0,
                    });
                }
            }
            CompleteContext::PackageMember(q) => {
                // `pkg.` — the package's callable members, inserted as calls.
                let (owner, package) = crate::complete::split_dotted(q);
                if let Some(cache) = &cache {
                    let members = lock(cache).package_member_names(owner.as_deref(), &package);
                    for m in members {
                        cands.push(Candidate {
                            insert: None,
                            label: function_insert(&m),
                            detail: "PACKAGE MEMBER".to_string(),
                            kind: CandidateKind::Function,
                            owner: owner.clone(),
                            usage: usage_of(&conn_id, &m),
                            depth: 0,
                        });
                    }
                }
            }
            CompleteContext::ColumnOf(q) => {
                if let Some(tref) = resolve_qualifier(q, &aliases) {
                    // A CTE/subquery relation carries its own columns; a base
                    // table/view resolves through the dictionary cache.
                    let cols: Vec<ColumnMeta> = scope_tables
                        .iter()
                        .find(|t| {
                            t.table.eq_ignore_ascii_case(&tref.name)
                                && owners_match(&t.owner, &tref.owner)
                        })
                        .map(|t| t.cols.clone())
                        .or_else(|| {
                            cache
                                .as_ref()
                                .map(|c| lock(c).columns_for(tref.owner.as_deref(), &tref.name))
                        })
                        .unwrap_or_default();
                    for col in cols {
                        cands.push(Candidate {
                            insert: None,
                            label: col.name.clone(),
                            detail: column_detail(&col, &tref.name),
                            kind: CandidateKind::ColumnInScope,
                            owner: tref.owner.clone(),
                            usage: usage_of(&conn_id, &col.name),
                            depth: 0,
                        });
                    }
                }
            }
            CompleteContext::StatementStart => {
                push_keywords(&mut cands, dialect.statement_starters());
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
                            insert: None,
                            label,
                            detail: kind_label(t.kind).to_string(),
                            kind: CandidateKind::Table,
                            owner: Some(t.owner.clone()),
                            usage: usage_of(&conn_id, &t.name),
                            depth: 0,
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
                            insert: None,
                            label: t.name.clone(),
                            detail: format!("{} · {}", kind_label(t.kind), t.owner),
                            kind: CandidateKind::Table,
                            owner: Some(t.owner.clone()),
                            usage: usage_of(&conn_id, &t.name),
                            depth: 0,
                        });
                    }
                }
            }
            CompleteContext::SelectList => {
                push_scope_columns(&mut cands);
                push_functions(&mut cands);
                push_sequences(&mut cands);
                push_keywords(&mut cands, dialect.expr_keywords());
                if select_list_is_empty(text, offset) {
                    // No projection yet: the useful move is to pick a table
                    // (templates insert `* FROM t`). Clause transitions
                    // (`FROM`/`WHERE`/…) are invalid here, so they're dropped.
                    push_table_templates(&mut cands);
                } else {
                    push_keywords(&mut cands, dialect.select_follow());
                }
            }
            CompleteContext::Predicate => {
                push_scope_columns(&mut cands);
                push_functions(&mut cands);
                push_sequences(&mut cands);
                push_keywords(&mut cands, dialect.predicate_keywords());
                push_keywords(&mut cands, dialect.predicate_follow());
                // In ORDER BY, the select-list aliases are valid too (Oracle
                // allows aliases only there). Skip names already offered as
                // columns (redundant).
                if let Some(forest) = structural
                    .as_ref()
                    .filter(|s| s.allows_projection_alias(offset))
                {
                    let columns: std::collections::HashSet<String> = scope_tables
                        .iter()
                        .flat_map(|t| t.cols.iter().map(|c| c.name.to_ascii_uppercase()))
                        .collect();
                    for alias in forest.projection_at(offset) {
                        if columns.contains(&alias.to_ascii_uppercase()) {
                            continue;
                        }
                        cands.push(Candidate {
                            insert: None,
                            label: alias.clone(),
                            detail: "SELECT alias".to_string(),
                            kind: CandidateKind::Column,
                            owner: None,
                            usage: usage_of(&conn_id, alias),
                            depth: 0,
                        });
                    }
                }
            }
            CompleteContext::FromTail(origin) => {
                // Past a table reference: only the continuations valid for the
                // introducing clause (never statement starters/DDL/functions).
                let kws: &[&str] = match origin {
                    FromOrigin::From => FROM_FOLLOW,
                    FromOrigin::Join => JOIN_FOLLOW,
                    FromOrigin::Update => UPDATE_FOLLOW,
                    FromOrigin::Into => INTO_FOLLOW,
                    FromOrigin::Merge => MERGE_FOLLOW,
                    FromOrigin::Using => USING_FOLLOW,
                    // DDL table tail: no meaningful keyword continuation.
                    FromOrigin::Table => &[],
                };
                push_keywords(&mut cands, kws);
            }
            CompleteContext::CaseStart => {
                push_keywords(&mut cands, CASE_START_KEYWORDS);
            }
            CompleteContext::CaseCondition => {
                // A WHEN condition is a predicate; `THEN` closes it. Functions
                // are only offered once the user types (the full catalog would
                // crowd out the case keywords at an empty prefix). No clause
                // transitions here.
                push_scope_columns(&mut cands);
                if !prefix.is_empty() {
                    push_functions(&mut cands);
                }
                push_keywords(&mut cands, CASE_CONDITION_KEYWORDS);
            }
            CompleteContext::CaseResult => {
                // A THEN/ELSE result is an expression; the case keywords that
                // follow a result close/branch it. Functions as above.
                push_scope_columns(&mut cands);
                if !prefix.is_empty() {
                    push_functions(&mut cands);
                }
                push_keywords(&mut cands, CASE_RESULT_KEYWORDS);
            }
            CompleteContext::SubqueryStart => {
                push_keywords(&mut cands, SUBQUERY_START_KEYWORDS);
            }
            CompleteContext::CastType => {
                // `CAST(x AS |` — the dialect's data types only.
                for ty in dialect.data_types() {
                    cands.push(Candidate {
                        insert: None,
                        label: ty.to_string(),
                        detail: "TYPE".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                        depth: 0,
                    });
                }
            }
            CompleteContext::WindowClause => {
                push_keywords(&mut cands, WINDOW_KEYWORDS);
            }
            CompleteContext::UsingColumns => {
                // Columns common to every joined relation (intersection by
                // name), in the first relation's order.
                if let Some(first) = scope_tables.first() {
                    let mut common: Vec<&String> = first
                        .cols
                        .iter()
                        .map(|c| &c.name)
                        .filter(|name| {
                            scope_tables[1..]
                                .iter()
                                .all(|t| t.cols.iter().any(|c| c.name.eq_ignore_ascii_case(name)))
                        })
                        .collect();
                    common.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
                    for name in common {
                        cands.push(Candidate {
                            insert: None,
                            label: name.clone(),
                            detail: "USING".to_string(),
                            kind: CandidateKind::ColumnInScope,
                            owner: None,
                            usage: usage_of(&conn_id, name),
                            depth: 0,
                        });
                    }
                }
            }
            CompleteContext::BareWord => {
                // Ambiguous position: keywords + functions, minus the
                // function names (which complete as call skeletons below).
                for kw in dialect.keywords() {
                    if dialect.functions().iter().any(|(n, _)| n == kw) {
                        continue;
                    }
                    cands.push(Candidate {
                        insert: None,
                        label: kw.to_string(),
                        detail: "KEYWORD".to_string(),
                        kind: CandidateKind::Keyword,
                        owner: None,
                        usage: 0,
                        depth: 0,
                    });
                }
                push_functions(&mut cands);
            }
        }
        let ranked = rank_candidates(&prefix, cands, &own_schema, COMPLETE_LIMIT);
        if ranked.is_empty() {
            return empty;
        }
        if let Some(tab) = self.tabs.iter_mut().find(|t| t.id == tab_id) {
            tab.pending_completion = true;
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
                    new_text: c
                        .insert
                        .clone()
                        .unwrap_or_else(|| insert_text_for(c.kind, &c.label)),
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
            *self.browser.usage.entry(key).or_insert(0) += 1;
        }
        if self.browser.usage.len() > 5000 {
            self.browser.usage.clear();
        }
        // Persist for the next session; a failure surfaces on the status bar.
        if let Err(e) = crate::config::save_usage(&self.browser.usage) {
            self.status = format!("Usage save failed: {e:#}").into();
        }
    }
}

/// Popup detail label for a suggested object kind.
fn kind_label(kind: crate::metadata::TableKind) -> &'static str {
    use crate::metadata::TableKind;
    match kind {
        TableKind::Table => "TABLE",
        TableKind::View => "VIEW",
        TableKind::Sequence => "SEQUENCE",
        TableKind::Synonym => "SYNONYM",
    }
}

/// A quoted identifier when the bare spelling wouldn't preserve the name
/// (a bare Oracle identifier folds to uppercase), else the name as-is.
fn quote_ident(name: &str) -> String {
    let bare = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_uppercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'));
    if bare {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// `[owner.]name` for a `FROM` target, baring the connected user's own schema
/// (Oracle resolves unqualified names to it first) and quoting each segment
/// only when needed.
fn qualified_ident(owner: Option<&str>, name: &str, own_schema: &str) -> String {
    match owner {
        Some(o) if !o.is_empty() && !o.eq_ignore_ascii_case(own_schema) => {
            format!("{}.{}", quote_ident(o), quote_ident(name))
        }
        _ => quote_ident(name),
    }
}

/// Case/whitespace-insensitive form of a condition, for "already typed"
/// checks (so `ON a.x=b.y AND` does not re-suggest `a.x = b.y`).
fn normalize_condition(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Columns of a relation: base tables/views from the dictionary cache, a
/// CTE/subquery from its own projection names.
fn relation_columns(rel: &Relation, cache: &Option<SharedCache>) -> Vec<ColumnMeta> {
    match rel.kind {
        RelationKind::Table => cache
            .as_ref()
            .map(|c| lock(c).columns_for(rel.owner.as_deref(), &rel.name))
            .unwrap_or_default(),
        RelationKind::Cte | RelationKind::Subquery => rel
            .columns
            .iter()
            .map(|name| ColumnMeta {
                name: name.clone(),
                data_type: String::new(),
                comments: String::new(),
            })
            .collect(),
    }
}

/// Aliases + in-scope tables from the structural scope at `offset`: relations
/// are nearest-scope-first (so a subquery alias shadows an outer one), and a
/// CTE/subquery relation carries its own projection columns.
fn scope_from_forest(
    forest: &ScopeForest,
    offset: usize,
    cache: &Option<SharedCache>,
) -> (HashMap<String, TableRef>, Vec<ScopeTable>) {
    let mut aliases = HashMap::new();
    let mut tables = Vec::new();
    for rel in forest.visible_relations(offset) {
        aliases.insert(
            rel.key(),
            TableRef {
                owner: rel.owner.clone(),
                name: rel.name.clone(),
            },
        );
        tables.push(ScopeTable {
            owner: rel.owner.clone(),
            table: rel.name.clone(),
            cols: relation_columns(rel, cache),
        });
    }
    (aliases, tables)
}

/// Lexical fallback: aliases from the statement scan, columns from the cache
/// (the pre-scope behavior, unchanged).
fn lexical_scope_tables(
    aliases: &HashMap<String, TableRef>,
    cache: &Option<SharedCache>,
) -> Vec<ScopeTable> {
    let mut order: Vec<String> = aliases.keys().cloned().collect();
    order.sort();
    let Some(cache) = cache else {
        return Vec::new();
    };
    let cache = lock(cache);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for alias in &order {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::complete::{Relation, Scope};
    use crate::metadata::{MetadataCache, TableId, TableKind};
    use std::sync::{Arc, Mutex};

    fn col(name: &str) -> ColumnMeta {
        ColumnMeta {
            name: name.to_string(),
            data_type: "NUMBER".to_string(),
            comments: String::new(),
        }
    }

    fn cache_with_emp() -> Option<SharedCache> {
        let mut cache = MetadataCache::default();
        cache.tables.push(TableId {
            owner: "SCOTT".to_string(),
            name: "EMP".to_string(),
            kind: TableKind::Table,
        });
        cache.columns.insert(
            ("SCOTT".to_string(), "EMP".to_string()),
            vec![col("EMPNO"), col("ENAME")],
        );
        Some(Arc::new(Mutex::new(cache)))
    }

    fn forest() -> ScopeForest {
        ScopeForest {
            scopes: vec![Scope {
                start: 0,
                end: 100,
                depth: 0,
                relations: vec![
                    Relation {
                        alias: "e".to_string(),
                        owner: Some("SCOTT".to_string()),
                        name: "EMP".to_string(),
                        kind: RelationKind::Table,
                        columns: Vec::new(),
                    },
                    Relation {
                        alias: "recent".to_string(),
                        owner: None,
                        name: "recent".to_string(),
                        kind: RelationKind::Cte,
                        columns: vec!["id".to_string(), "total".to_string()],
                    },
                ],
                projection: vec![],
                order_group: vec![],
            }],
            dml: Vec::new(),
            ctes: Vec::new(),
        }
    }

    #[test]
    fn scope_from_forest_maps_aliases_and_columns() {
        let cache = cache_with_emp();
        let (aliases, tables) = scope_from_forest(&forest(), 10, &cache);
        assert!(aliases.contains_key("e"));
        assert_eq!(aliases["e"].name, "EMP");
        assert!(aliases.contains_key("recent"));

        let emp = tables.iter().find(|t| t.table == "EMP").unwrap();
        let emp_cols: Vec<&str> = emp.cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(emp_cols, ["EMPNO", "ENAME"], "table columns from the cache");

        let cte = tables.iter().find(|t| t.table == "recent").unwrap();
        let cte_cols: Vec<&str> = cte.cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(cte_cols, ["id", "total"], "CTE columns from the scope");
    }

    #[test]
    fn relation_columns_reads_cache_or_scope() {
        let cache = cache_with_emp();
        let table = Relation {
            alias: "e".to_string(),
            owner: Some("SCOTT".to_string()),
            name: "EMP".to_string(),
            kind: RelationKind::Table,
            columns: Vec::new(),
        };
        let cols: Vec<String> = relation_columns(&table, &cache)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(cols, ["EMPNO", "ENAME"]);

        let cte = Relation {
            alias: "recent".to_string(),
            owner: None,
            name: "recent".to_string(),
            kind: RelationKind::Cte,
            columns: vec!["id".to_string()],
        };
        let cols: Vec<String> = relation_columns(&cte, &cache)
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(cols, ["id"]);
    }

    #[test]
    fn lexical_fallback_reads_columns_from_cache() {
        let cache = cache_with_emp();
        let mut aliases = HashMap::new();
        aliases.insert(
            "e".to_string(),
            TableRef {
                owner: Some("SCOTT".to_string()),
                name: "EMP".to_string(),
            },
        );
        let tables = lexical_scope_tables(&aliases, &cache);
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].cols.len(), 2);
    }
}
