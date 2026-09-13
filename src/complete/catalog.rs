//! Scope tables and hover/describe cards from the dictionary.
//!
//! Part of the completion engine (see `complete.rs`).

use super::*;

/// Column names (uppercase) appearing in more than one scope table.
/// Inserting those bare is ambiguous SQL — callers qualify them.
/// `scope` is `(owner, table, column_names...)` per in-scope table.
pub fn ambiguous_columns(scope: &[ScopeTable]) -> std::collections::HashSet<String> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for t in scope {
        for c in &t.cols {
            *counts.entry(c.name.to_ascii_uppercase()).or_insert(0) += 1;
        }
    }
    counts
        .into_iter()
        .filter(|(_, n)| *n > 1)
        .map(|(c, _)| c)
        .collect()
}

use crate::metadata::{ColumnMeta, MetadataCache};

/// Scope table entry for ambiguity analysis: owner, table, and its columns.
pub struct ScopeTable {
    pub owner: Option<String>,
    pub table: String,
    pub cols: Vec<ColumnMeta>,
}

/// A scope table's display label for qualified inserts: the alias as
/// written when one exists (`e`), else the table name itself. `table` must
/// be in scope (callers fall back to it when this returns `None`).
pub fn scope_label(
    owner: &Option<String>,
    table: &str,
    aliases: &HashMap<String, TableRef>,
) -> Option<String> {
    let mut names: Vec<&String> = aliases.keys().collect();
    names.sort();
    names.into_iter().find_map(|a| {
        let t = &aliases[a.as_str()];
        (t.name.eq_ignore_ascii_case(table) && owners_match(&t.owner, owner)).then(|| a.clone())
    })
}

/// Display name for an object: bare `name` when `owner` is the connected
/// user's own schema (Oracle resolves unqualified names to it first, so the
/// prefix is noise), else `OWNER.name`. Used for suggestion labels/inserts,
/// hover titles, and DESCRIBE statements alike.
pub fn display_name(owner: Option<&str>, name: &str, own_schema: &str) -> String {
    match owner {
        Some(o) if !o.is_empty() && !o.eq_ignore_ascii_case(own_schema) => {
            format!("{o}.{name}")
        }
        _ => name.to_string(),
    }
}

/// Hover card markdown for the word under the cursor. `qualifier` is the
/// `x` in `x.word` (None for bare words); `aliases` maps the current
/// statement's aliases. Returns None when nothing reliable can be said
/// (unknown object, ambiguous bare column, system object while hidden).
/// Pure and snapshot-driven — the provider calls it with cached data.
pub fn hover_markdown(
    word: &str,
    qualifier: Option<&str>,
    aliases: &HashMap<String, TableRef>,
    cache: &MetadataCache,
    show_system: bool,
    own_schema: &str,
) -> Option<String> {
    if word.is_empty() {
        return None;
    }
    let hidden = |owner: &str| {
        !show_system && !owner.eq_ignore_ascii_case(own_schema) && is_system_schema(owner)
    };
    // Qualified: `alias.column`, `owner.table` (cursor on the table), or
    // `owner.table.column`. Alias columns first; a bare owner never lives
    // in the alias map, so `SYSTEM.EMPLOYEES` resolves directly.
    if let Some(q) = qualifier {
        if let Some(tref) = resolve_qualifier(q, aliases) {
            let cols = cache.columns_for(tref.owner.as_deref(), &tref.name);
            if !cols.is_empty() {
                if let Some(col) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(word)) {
                    return Some(column_card(
                        tref.owner.as_deref().unwrap_or(""),
                        &tref.name,
                        col,
                        own_schema,
                    ));
                }
                // Qualifier resolves to a real table but the word isn't its
                // column (cursor on the table part): table card.
                return Some(table_card(
                    tref.owner.as_deref(),
                    &tref.name,
                    &cols,
                    own_schema,
                ));
            }
        }
        let (q_owner, q_table) = split_dotted(q);
        match (q_owner, q_table) {
            // Single-segment qualifier + word: `SYSTEM.|EMPLOYEES`.
            (None, _) => {
                let cols = cache.columns_for(Some(q), word);
                if !cols.is_empty() {
                    return Some(table_card(Some(q), word, &cols, own_schema));
                }
            }
            // Dotted qualifier + word: `scott.emp.|ename` → column card.
            (Some(o), t) => {
                let cols = cache.columns_for(Some(&o), &t);
                if let Some(col) = cols.iter().find(|c| c.name.eq_ignore_ascii_case(word)) {
                    return Some(column_card(&o, &t, col, own_schema));
                }
            }
        }
        return None;
    }
    // Bare word: prefer the qualifier's own table, then own-schema, then a
    // unique visible match. Never guess across several schemas.
    let mut matches: Vec<(&String, &String)> = cache
        .tables
        .iter()
        .filter(|t| t.name.eq_ignore_ascii_case(word) && !hidden(&t.owner))
        .map(|t| (&t.owner, &t.name))
        .collect();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        let (owner, name) = matches[0];
        let cols = cache.columns_for(Some(owner), name);
        return Some(table_card(Some(owner), name, &cols, own_schema));
    }
    // Bare column: unique across in-scope tables only.
    let mut holders: Vec<(String, String, ColumnMeta)> = Vec::new();
    for tref in aliases.values() {
        for col in cache.columns_for(tref.owner.as_deref(), &tref.name) {
            if col.name.eq_ignore_ascii_case(word) {
                holders.push((
                    tref.owner.clone().unwrap_or_default(),
                    tref.name.clone(),
                    col,
                ));
            }
        }
    }
    holders.sort_by(|a, b| (&a.0, &a.1, &a.2.name).cmp(&(&b.0, &b.1, &b.2.name)));
    holders.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1 && a.2.name == b.2.name);
    if holders.len() == 1 {
        let (owner, table, col) = &holders[0];
        return Some(column_card(owner, table, col, own_schema));
    }
    None
}

/// Resolve a hovered word to a describable table: returns
/// (owner, table). Table-only (v1): column words resolve to `None` — a
/// Cmd-click on a column is not a table jump. Mirrors the table-card
/// conditions of [`hover_markdown`] so the Cmd-hover underline and the
/// hover card agree on what is jumpable.
pub fn describe_target(
    word: &str,
    qualifier: Option<&str>,
    aliases: &HashMap<String, TableRef>,
    cache: &MetadataCache,
    show_system: bool,
    own_schema: &str,
) -> Option<(Option<String>, String)> {
    if word.is_empty() {
        return None;
    }
    let hidden = |owner: &str| {
        !show_system && !owner.eq_ignore_ascii_case(own_schema) && is_system_schema(owner)
    };
    if let Some(q) = qualifier {
        if let Some(tref) = resolve_qualifier(q, aliases) {
            let cols = cache.columns_for(tref.owner.as_deref(), &tref.name);
            if !cols.is_empty() {
                // Cursor on the table part (word is not one of its columns).
                if !cols.iter().any(|c| c.name.eq_ignore_ascii_case(word)) {
                    return Some((tref.owner.clone(), tref.name.clone()));
                }
                return None;
            }
            // Empty: not an alias hit (`resolve_qualifier` always returns
            // `Some`) — fall through to direct owner.table resolution below.
        }
        let (q_owner, _q_table) = split_dotted(q);
        if q_owner.is_none() {
            // `owner.table` with the cursor on the table: direct resolve,
            // bypassing the system filter like the hover card.
            if !cache.columns_for(Some(q), word).is_empty() {
                return Some((Some(q.to_string()), word.to_string()));
            }
        }
        return None;
    }
    // Bare word: unique visible table only — never guess across schemas.
    let mut matches: Vec<(&String, &String)> = cache
        .tables
        .iter()
        .filter(|t| t.name.eq_ignore_ascii_case(word) && !hidden(&t.owner))
        .map(|t| (&t.owner, &t.name))
        .collect();
    matches.sort();
    matches.dedup();
    if matches.len() == 1 {
        let (owner, name) = matches[0];
        return Some((Some(owner.clone()), name.clone()));
    }
    None
}

/// `**COL** · TYPE · TABLE [· OWNER]` + comment. Shared by qualified
/// and unique bare column cards. Own-schema tables show bare.
fn column_card(owner: &str, table: &str, col: &ColumnMeta, own_schema: &str) -> String {
    let mut md = format!(
        "**{}** · {}",
        col.name,
        if col.data_type.is_empty() {
            "COLUMN".to_string()
        } else {
            col.data_type.clone()
        }
    );
    md.push_str(&format!("\n\n{table}"));
    if !owner.is_empty() && !owner.eq_ignore_ascii_case(own_schema) {
        md.push_str(&format!(" · {owner}"));
    }
    let c = short_comment(&col.comments);
    if !c.is_empty() {
        md.push_str(&format!("\n\n{c}"));
    }
    md
}

/// `**OWNER.TABLE** — TABLE` (bare `**TABLE**` for the connected user's
/// own schema) + up to 30 `COL — TYPE — comment` lines.
fn table_card(owner: Option<&str>, table: &str, cols: &[ColumnMeta], own_schema: &str) -> String {
    let mut md = String::from("**");
    md.push_str(&display_name(owner, table, own_schema));
    md.push_str("** — TABLE");
    let shown = cols.len().min(30);
    for col in &cols[..shown] {
        md.push_str(&format!(
            "\n{} — {}",
            col.name,
            if col.data_type.is_empty() {
                "?"
            } else {
                col.data_type.as_str()
            }
        ));
        let c = short_comment(&col.comments);
        if !c.is_empty() {
            md.push_str(&format!(" — {c}"));
        }
    }
    if cols.len() > shown {
        md.push_str(&format!("\n… +{} more", cols.len() - shown));
    }
    md
}
/// One-line popup detail for a column comment: trimmed, single-spaced,
/// capped at 80 chars.
pub fn short_comment(comment: &str) -> String {
    let one: String = comment.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = one.trim();
    if trimmed.chars().count() <= 80 {
        trimmed.to_string()
    } else {
        format!("{}…", trimmed.chars().take(79).collect::<String>())
    }
}
