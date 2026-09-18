//! Per-engine SQL dialect rules and catalogs for completion.
//!
//! The completion core is dialect-agnostic logic; everything engine-specific
//! (keywords, functions, folding, system schemas, sequence syntax) lives
//! behind [`Dialect`]. v1 ships [`OracleDialect`]; a second engine is a new
//! impl plus a `DbEngine::dialect()` arm, not a change to the engine.

use super::ranking::{
    is_system_schema, EXPR_KEYWORDS, ORACLE_DATA_TYPES, ORACLE_FUNCTIONS, ORACLE_KEYWORDS,
    PRED_FOLLOW, PRED_KEYWORDS, SELECT_FOLLOW, STMT_KEYWORDS, SYSTEM_SCHEMAS,
};

/// How a dialect folds a bare (unquoted) identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fold {
    /// Oracle: `emp` → `EMP`.
    Upper,
    /// Postgres: `EMP` → `emp`.
    Lower,
    /// Case-insensitive engines (e.g. SQL Server default collation).
    Preserve,
}

/// Engine-specific completion rules and catalogs.
///
/// Data is returned as `'static` slices so callers can hold it without
/// borrowing the dialect. Prefer adding a method here over branching on the
/// engine elsewhere.
pub trait Dialect: Sync {
    /// Human label for diagnostics/tests.
    fn name(&self) -> &'static str;

    /// Fold applied to bare identifiers.
    fn fold(&self) -> Fold;

    /// Identifier quote character (`"` for Oracle/Postgres, backtick for
    /// MySQL, brackets for SQL Server).
    fn identifier_quote(&self) -> char {
        '"'
    }

    /// Whether `$tag$ … $tag$` string quoting is recognized.
    fn supports_dollar_quoting(&self) -> bool {
        false
    }

    /// Statements that may start an empty statement head.
    fn statement_starters(&self) -> &'static [&'static str];

    /// Keywords valid in a select list.
    fn expr_keywords(&self) -> &'static [&'static str];

    /// Keywords valid in a predicate (WHERE/GROUP/ORDER/HAVING/ON).
    fn predicate_keywords(&self) -> &'static [&'static str];

    /// Clause transitions valid after a select list.
    fn select_follow(&self) -> &'static [&'static str];

    /// Clause transitions valid after a predicate.
    fn predicate_follow(&self) -> &'static [&'static str];

    /// All completable keywords (superset of the context lists).
    fn keywords(&self) -> &'static [&'static str];

    /// Built-in functions: `(NAME, signature shown in the detail pane)`.
    fn functions(&self) -> &'static [(&'static str, &'static str)];

    /// Data types for cast/DDL contexts. Cataloged now, but not yet surfaced
    /// by the provider (a cast context is Phase F); empty when unsupported.
    fn data_types(&self) -> &'static [&'static str] {
        &[]
    }

    /// System schemas hidden unless the user opts in.
    fn system_schemas(&self) -> &'static [&'static str];

    /// Versioned/prefixed system-schema families (`APEX_…`, `FLOWS_…`).
    fn system_schema_prefixes(&self) -> &'static [&'static str] {
        &[]
    }

    /// True for a system schema (exact or prefixed).
    fn is_system_schema(&self, owner: &str) -> bool;

    /// Schemas whose objects may be referenced unqualified and should rank
    /// first. Oracle resolves to the connected user; Postgres to
    /// `search_path`. Empty when nothing is preferred.
    fn preferred_schemas(&self, connected_user: &str) -> Vec<String> {
        if connected_user.is_empty() {
            Vec::new()
        } else {
            vec![connected_user.to_string()]
        }
    }

    /// Members offered after `<sequence>.` (Oracle `NEXTVAL`/`CURRVAL`).
    /// Empty when the dialect has no sequence pseudo-columns.
    fn sequence_members(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether `<sequence>.|` should suggest sequence pseudo-columns.
    fn uses_sequence_pseudocolumns(&self) -> bool {
        !self.sequence_members().is_empty()
    }
}

/// Oracle dialect over the shipped catalog.
pub struct OracleDialect;

impl Dialect for OracleDialect {
    fn name(&self) -> &'static str {
        "Oracle"
    }

    fn fold(&self) -> Fold {
        Fold::Upper
    }

    fn statement_starters(&self) -> &'static [&'static str] {
        STMT_KEYWORDS
    }

    fn expr_keywords(&self) -> &'static [&'static str] {
        EXPR_KEYWORDS
    }

    fn predicate_keywords(&self) -> &'static [&'static str] {
        PRED_KEYWORDS
    }

    fn select_follow(&self) -> &'static [&'static str] {
        SELECT_FOLLOW
    }

    fn predicate_follow(&self) -> &'static [&'static str] {
        PRED_FOLLOW
    }

    fn keywords(&self) -> &'static [&'static str] {
        ORACLE_KEYWORDS
    }

    fn functions(&self) -> &'static [(&'static str, &'static str)] {
        ORACLE_FUNCTIONS
    }

    fn data_types(&self) -> &'static [&'static str] {
        ORACLE_DATA_TYPES
    }

    fn system_schemas(&self) -> &'static [&'static str] {
        SYSTEM_SCHEMAS
    }

    fn system_schema_prefixes(&self) -> &'static [&'static str] {
        &["APEX_", "FLOWS_"]
    }

    fn is_system_schema(&self, owner: &str) -> bool {
        is_system_schema(owner)
    }

    fn sequence_members(&self) -> &'static [&'static str] {
        &["NEXTVAL", "CURRVAL"]
    }
}

/// The Oracle dialect singleton.
pub fn oracle() -> &'static dyn Dialect {
    &OracleDialect
}
