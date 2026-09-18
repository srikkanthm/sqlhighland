//! Pure autocomplete context logic: prefix extraction, qualifier detection,
//! alias-map building, candidate ranking/filtering.
//!
//! GUI-free and driver-free by design: the GPUI `CompletionProvider` (in
//! `app.rs`) calls into these helpers with buffer snapshots and cached
//! metadata. All Oracle folding rules (bare folds uppercase, quoted keeps
//! case) are honored here so ranking is deterministic and unit-testable.

use std::collections::HashMap;

/// A table/view reference: optional owner + name, case as stored
/// (dictionary returns uppercase for bare identifiers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRef {
    pub owner: Option<String>,
    pub name: String,
}

/// Completion context at the cursor. Strict by design: each position
/// offers only what SQL allows there — ranking never mixes kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteContext {
    /// Empty statement head — statement starters only.
    StatementStart,
    /// After `SELECT`/`DISTINCT`/`,`/`(`/operators in select scope —
    /// in-scope columns, functions, expression keywords. Never tables.
    SelectList,
    /// After `FROM`/`JOIN`/`INTO`/`UPDATE`/`TABLE`/`USING` with no table
    /// yet — tables only.
    AfterFrom,
    /// After `WHERE`/`GROUP`/`ORDER`/`HAVING`/`BY`/`AND`/`OR`/`SET`/`WHEN`/
    /// `ON` (started condition) — in-scope columns, functions. Never tables.
    Predicate,
    /// Past a table reference (`FROM t `, `UPDATE t `, …) — only the clause
    /// continuations valid for that statement (never statement starters/DDL).
    FromTail(FromOrigin),
    /// `owner.` after `FROM`/`JOIN` — tables of that owner (bare names).
    OwnerTables(String),
    /// After `alias.` or `table.` — columns of that object only.
    /// `qualifier` is the raw text before the dot (`e`, `emp`, `scott.emp`).
    ColumnOf(String),
    /// Qualifier is a known sequence — offer `NEXTVAL`/`CURRVAL`.
    SequenceMember(String),
    /// In `JOIN <right> [alias] ON |` with a fresh (operator-free) condition —
    /// offer FK-derived `left.col = right.col` conditions.
    JoinOn {
        right_alias: String,
        right: TableRef,
    },
    /// Ambiguous (after a complete identifier/literal, post-paren, misc
    /// keywords) — keywords + functions only. Never tables/columns.
    BareWord,
}

/// Candidate kinds for iconing/ranking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    JoinCondition,
    ColumnInScope,
    Table,
    Column,
    Sequence,
    Function,
    Keyword,
}

/// One completion row (provider maps this to `lsp_types::CompletionItem`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub label: String,
    pub detail: String,
    pub kind: CandidateKind,
    /// Owning schema when known (tables/sequences/qualified columns).
    /// Drives the own-schema ranking tier.
    pub owner: Option<String>,
    /// Usage count for recency/frequency boost (0 = never used).
    pub usage: u64,
    /// Scope proximity: 0 = innermost scope, higher = farther out. Breaks ties
    /// between otherwise-equal candidates so a correlated column resolves to
    /// the nearest definition. 0 for candidates with no scope.
    pub depth: u8,
}

impl Candidate {
    fn own_schema_first(&self, own: &str) -> u8 {
        if own.is_empty() {
            return 1;
        }
        match &self.owner {
            Some(o) if o.eq_ignore_ascii_case(own) => 0,
            _ => 1,
        }
    }
}

mod aliases;
mod catalog;
mod context;
mod dialect;
mod ranking;
mod scope;

// Re-exports preserve the public `crate::complete::…` API across the split.
pub use aliases::*;
pub use catalog::*;
pub use context::*;
pub use dialect::*;
pub use ranking::*;
pub use scope::*;

#[cfg(test)]
mod tests;
