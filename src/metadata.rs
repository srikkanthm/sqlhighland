//! Per-connection dictionary cache for autocomplete, hover, and the schema
//! browser.
//!
//! GUI-free: blocking fetchers take `&mut dyn DbClient` (a real session on the
//! background executor, fakes in tests) and are reached through the
//! engine-agnostic [`MetadataProvider`] seam ([`provider_for`]). The view owns
//! `HashMap<connection_id, Arc<Mutex<MetadataCache>>>` beside the session
//! pool; the completion provider only ever clones an `Arc` snapshot. The
//! Oracle dictionary SQL lives in the `*_blocking` fetchers.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::complete::{ForeignKey, SYSTEM_SCHEMAS};
use crate::db::DbClient;
use crate::schema::DbEngine;

/// Cap per dictionary query (protects huge schemas; highland-size DBs never
/// come close).
pub const DICT_MAX_ROWS: usize = 100_000;
/// Stale-after duration; the view refreshes past this on next trigger.
pub const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    Table,
    View,
    Sequence,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableId {
    pub owner: String,
    pub name: String,
    pub kind: TableKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    pub data_type: String,
    /// `ALL_COL_COMMENTS` text (may be empty). Shown in popup detail.
    pub comments: String,
}

#[derive(Debug, Clone, Default)]
pub struct MetadataCache {
    pub tables: Vec<TableId>,
    pub columns: HashMap<(String, String), Vec<ColumnMeta>>,
    pub sequences: Vec<TableId>,
    /// Referential constraints for JOIN … ON suggestions.
    pub fks: Vec<ForeignKey>,
    pub fetched_at: Option<Instant>,
    pub loading: bool,
}

impl MetadataCache {
    /// True when never fetched or older than [`CACHE_TTL`].
    pub fn is_stale(&self) -> bool {
        match self.fetched_at {
            None => true,
            Some(t) => t.elapsed() > CACHE_TTL,
        }
    }

    /// Uppercase sequence-name lookup for `seq.|` detection.
    pub fn is_sequence(&self, upper_name: &str) -> bool {
        self.sequences
            .iter()
            .any(|s| s.name.to_ascii_uppercase() == upper_name)
    }

    /// Columns for `(owner, table)` (both compared uppercase). `None` owner
    /// matches any owner with that table name (merged).
    pub fn columns_for(&self, owner: Option<&str>, table: &str) -> Vec<ColumnMeta> {
        let t = table.to_ascii_uppercase();
        match owner {
            Some(o) => {
                let o = o.to_ascii_uppercase();
                self.columns.get(&(o, t)).cloned().unwrap_or_default()
            }
            None => {
                let mut out = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for ((_, tbl), cols) in &self.columns {
                    if *tbl == t {
                        for c in cols {
                            if seen.insert(c.name.clone()) {
                                out.push(c.clone());
                            }
                        }
                    }
                }
                out.sort_by(|a, b| a.name.cmp(&b.name));
                out
            }
        }
    }
}

/// Snapshot shared with the completion provider (cheap `Arc` clone).
pub type SharedCache = Arc<Mutex<MetadataCache>>;

/// System-schema predicate on `owner_col` (dictionary owners are stored
/// uppercase). Empty when `include_system`. `own_schema` (connected user)
/// is always exempt. Composes with AND: callers add their own WHERE.
pub fn system_predicate(owner_col: &str, include_system: bool, own_schema: &str) -> String {
    if include_system {
        return String::new();
    }
    let mut s = format!("{owner_col} NOT IN (");
    for (i, schema) in SYSTEM_SCHEMAS.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('\'');
        s.push_str(schema);
        s.push('\'');
    }
    s.push_str(&format!(
        ") AND {owner_col} NOT LIKE 'APEX\\_%' ESCAPE '\\' AND {owner_col} NOT LIKE 'FLOWS\\_%' ESCAPE '\\'"
    ));
    let own = own_schema.to_ascii_uppercase().replace('\'', "''");
    if own.is_empty() {
        s
    } else {
        // Parenthesized: AND binds tighter than OR, but spell it out.
        format!("({s} OR {owner_col} = '{own}')")
    }
}

/// Back-compat wrapper: full WHERE clause for queries without one.
pub fn system_filter_sql(owner_col: &str, include_system: bool, own_schema: &str) -> String {
    let p = system_predicate(owner_col, include_system, own_schema);
    if p.is_empty() {
        String::new()
    } else {
        format!(" WHERE {p}")
    }
}

/// Fetch tables+views as `(owner, name, kind)` triples. `own_schema`
/// (connected user) is always exempt from the system filter.
pub fn fetch_tables_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<TableId>, crate::db::DbError> {
    let filter = system_filter_sql("owner", include_system, own_schema);
    let r = db.run_query(
        &format!(
            "SELECT owner, table_name, 'TABLE' FROM all_tables{filter} UNION ALL SELECT owner, view_name, 'VIEW' FROM all_views{filter}"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [Some(o), Some(t), Some(k)] => {
                let kind = if k.eq_ignore_ascii_case("VIEW") {
                    TableKind::View
                } else {
                    TableKind::Table
                };
                Some(TableId {
                    owner: o.clone(),
                    name: t.clone(),
                    kind,
                })
            }
            _ => None,
        })
        .collect())
}

/// Fetch columns grouped by `(OWNER, TABLE)` (uppercased keys), left-joined
/// to `ALL_COL_COMMENTS` for popup detail text.
pub fn fetch_columns_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<HashMap<(String, String), Vec<ColumnMeta>>, crate::db::DbError> {
    let filter = system_filter_sql("t.owner", include_system, own_schema);
    let r = db.run_query(
        &format!(
            "SELECT t.owner, t.table_name, t.column_name, t.data_type, c.comments \
               FROM all_tab_columns t \
               LEFT JOIN all_col_comments c \
                 ON c.owner = t.owner AND c.table_name = t.table_name \
                AND c.column_name = t.column_name{filter}"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    let mut map: HashMap<(String, String), Vec<ColumnMeta>> = HashMap::new();
    for row in &r.rows {
        if let [Some(o), Some(t), Some(c), dt, cm] = row.as_slice() {
            map.entry((o.to_ascii_uppercase(), t.to_ascii_uppercase()))
                .or_default()
                .push(ColumnMeta {
                    name: c.clone(),
                    data_type: dt.clone().unwrap_or_default(),
                    comments: cm.clone().unwrap_or_default(),
                });
        }
    }
    Ok(map)
}

/// Fetch referential constraints, grouped per constraint (composite keys
/// stay aligned by position). Filtered on the constrained table's owner.
pub fn fetch_fks_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<ForeignKey>, crate::db::DbError> {
    let pred = system_predicate("c.owner", include_system, own_schema);
    let scope = if pred.is_empty() {
        String::new()
    } else {
        format!(" AND ({pred})")
    };
    let r = db.run_query(
        &format!(
            "SELECT c.owner, c.constraint_name, a.table_name, a.column_name, \
                c.r_owner, c2.table_name, c2.column_name \
             FROM all_constraints c \
             JOIN all_cons_columns a \
               ON a.owner = c.owner AND a.constraint_name = c.constraint_name \
             JOIN all_cons_columns c2 \
               ON c2.owner = c.r_owner AND c2.constraint_name = c.r_constraint_name \
              AND c2.position = a.position \
            WHERE c.constraint_type = 'R'{scope} \
            ORDER BY c.owner, c.constraint_name, a.position"
        ),
        DICT_MAX_ROWS,
        &[],
    )?;
    // Group rows by (owner, constraint); ORDER BY keeps positions aligned.
    let mut order: Vec<(String, String)> = Vec::new();
    let mut groups: HashMap<(String, String), ForeignKey> = HashMap::new();
    for row in &r.rows {
        if let [Some(o), Some(name), Some(ft), Some(fc), Some(ro), Some(tt), Some(tc)] =
            row.as_slice()
        {
            let key = (o.clone(), name.clone());
            let entry = groups.entry(key.clone()).or_insert_with(|| {
                order.push(key);
                ForeignKey {
                    name: name.clone(),
                    from_owner: Some(o.clone()),
                    from_table: ft.clone(),
                    from_cols: Vec::new(),
                    to_owner: Some(ro.clone()),
                    to_table: tt.clone(),
                    to_cols: Vec::new(),
                }
            });
            entry.from_cols.push(fc.clone());
            entry.to_cols.push(tc.clone());
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect())
}

/// Fetch sequences as `(owner, name)` pairs.
pub fn fetch_sequences_blocking(
    db: &mut dyn DbClient,
    include_system: bool,
    own_schema: &str,
) -> Result<Vec<TableId>, crate::db::DbError> {
    let filter = system_filter_sql("sequence_owner", include_system, own_schema);
    let r = db.run_query(
        &format!("SELECT sequence_owner, sequence_name FROM all_sequences{filter}"),
        DICT_MAX_ROWS,
        &[],
    )?;
    Ok(r.rows
        .iter()
        .filter_map(|row| match row.as_slice() {
            [Some(o), Some(t)] => Some(TableId {
                owner: o.clone(),
                name: t.clone(),
                kind: TableKind::Sequence,
            }),
            _ => None,
        })
        .collect())
}

/// Engine-specific dictionary fetching for autocomplete, hover, and the
/// schema browser. The Oracle provider is the only implementation today; a
/// second engine supplies its own and a [`DbEngine`] arm in [`provider_for`],
/// so `browser.rs` never names a specific engine.
pub trait MetadataProvider: Send + Sync {
    fn fetch_tables(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError>;
    fn fetch_columns(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<HashMap<(String, String), Vec<ColumnMeta>>, crate::db::DbError>;
    fn fetch_fks(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<ForeignKey>, crate::db::DbError>;
    fn fetch_sequences(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError>;
}

/// Oracle dictionary provider: delegates to the `*_blocking` fetchers below.
pub struct OracleMetadata;

impl MetadataProvider for OracleMetadata {
    fn fetch_tables(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError> {
        fetch_tables_blocking(db, include_system, own_schema)
    }

    fn fetch_columns(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<HashMap<(String, String), Vec<ColumnMeta>>, crate::db::DbError> {
        fetch_columns_blocking(db, include_system, own_schema)
    }

    fn fetch_fks(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<ForeignKey>, crate::db::DbError> {
        fetch_fks_blocking(db, include_system, own_schema)
    }

    fn fetch_sequences(
        &self,
        db: &mut dyn DbClient,
        include_system: bool,
        own_schema: &str,
    ) -> Result<Vec<TableId>, crate::db::DbError> {
        fetch_sequences_blocking(db, include_system, own_schema)
    }
}

/// The dictionary provider for an engine. Add a match arm per [`DbEngine`].
pub fn provider_for(engine: DbEngine) -> &'static dyn MetadataProvider {
    match engine {
        DbEngine::Oracle => &OracleMetadata,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{BindParam, DbError};
    use crate::model::ColumnInfo;
    use crate::model::QueryResult;

    struct FakeDb {
        tables: QueryResult,
        columns: QueryResult,
        sequences: QueryResult,
        constraints: QueryResult,
    }

    fn qr(cols: &[&str], rows: &[Vec<Option<&str>>]) -> QueryResult {
        QueryResult {
            columns: cols
                .iter()
                .map(|c| ColumnInfo {
                    name: c.to_string(),
                    db_type: "DB_TYPE_VARCHAR2".to_string(),
                })
                .collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|c| c.map(str::to_string)).collect())
                .collect(),
            elapsed_ms: 0,
            truncated: false,
        }
    }

    impl DbClient for FakeDb {
        fn connect(&mut self, _: &crate::model::ConnectionConfig) -> Result<(), DbError> {
            Ok(())
        }
        fn is_connected(&self) -> bool {
            true
        }
        fn disconnect(&mut self) {}
        fn run_query(
            &mut self,
            sql: &str,
            _max: usize,
            _binds: &[BindParam],
        ) -> Result<QueryResult, DbError> {
            if sql.contains("all_tab_columns") {
                Ok(std::mem::replace(&mut self.columns, qr(&[], &[])))
            } else if sql.contains("all_sequences") {
                Ok(std::mem::replace(&mut self.sequences, qr(&[], &[])))
            } else if sql.contains("all_constraints") {
                Ok(std::mem::replace(&mut self.constraints, qr(&[], &[])))
            } else {
                Ok(std::mem::replace(&mut self.tables, qr(&[], &[])))
            }
        }
        fn exec(&mut self, _: &str, _: &[BindParam]) -> Result<(u64, u128), DbError> {
            Ok((0, 0))
        }
        fn commit(&mut self) -> Result<(), DbError> {
            Ok(())
        }
        fn rollback(&mut self) -> Result<(), DbError> {
            Ok(())
        }
    }

    fn fake() -> FakeDb {
        FakeDb {
            tables: qr(
                &["OWNER", "TABLE_NAME", "KIND"],
                &[
                    vec![Some("SCOTT"), Some("EMP"), Some("TABLE")],
                    vec![Some("SCOTT"), Some("DEPT"), Some("TABLE")],
                    vec![Some("SCOTT"), Some("EMPVW"), Some("VIEW")],
                    vec![None, Some("GHOST"), Some("TABLE")],
                ],
            ),
            columns: qr(
                &[
                    "OWNER",
                    "TABLE_NAME",
                    "COLUMN_NAME",
                    "DATA_TYPE",
                    "COMMENTS",
                ],
                &[
                    vec![
                        Some("SCOTT"),
                        Some("EMP"),
                        Some("EMPNO"),
                        Some("NUMBER"),
                        Some("employee id"),
                    ],
                    vec![
                        Some("SCOTT"),
                        Some("EMP"),
                        Some("ENAME"),
                        Some("VARCHAR2"),
                        None,
                    ],
                ],
            ),
            sequences: qr(
                &["SEQUENCE_OWNER", "SEQUENCE_NAME"],
                &[vec![Some("SCOTT"), Some("EMP_SEQ")]],
            ),
            constraints: qr(
                &[
                    "OWNER",
                    "CONSTRAINT_NAME",
                    "TABLE_NAME",
                    "COLUMN_NAME",
                    "R_OWNER",
                    "R_TABLE_NAME",
                    "R_COLUMN_NAME",
                ],
                &[
                    vec![
                        Some("SCOTT"),
                        Some("EMP_DEPT_FK"),
                        Some("EMP"),
                        Some("DEPTNO"),
                        Some("SCOTT"),
                        Some("DEPT"),
                        Some("DEPTNO"),
                    ],
                    // Composite key: two rows, one constraint.
                    vec![
                        Some("SCOTT"),
                        Some("COMP_FK"),
                        Some("A"),
                        Some("X"),
                        Some("SCOTT"),
                        Some("B"),
                        Some("X"),
                    ],
                    vec![
                        Some("SCOTT"),
                        Some("COMP_FK"),
                        Some("A"),
                        Some("Y"),
                        Some("SCOTT"),
                        Some("B"),
                        Some("Y"),
                    ],
                ],
            ),
        }
    }

    #[test]
    fn tables_skip_null_owner() {
        let mut db = fake();
        let t = fetch_tables_blocking(&mut db, true, "").unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].name, "EMP");
        assert_eq!(t[0].kind, TableKind::Table);
        assert_eq!(t[2].name, "EMPVW");
        assert_eq!(t[2].kind, TableKind::View);
    }

    #[test]
    fn provider_for_dispatches_by_engine() {
        // The browser only names the provider, never a concrete engine.
        let mut db = fake();
        let provider = provider_for(DbEngine::Oracle);
        assert_eq!(provider.fetch_tables(&mut db, true, "").unwrap().len(), 3);
        assert_eq!(
            provider.fetch_sequences(&mut db, true, "").unwrap().len(),
            1
        );
    }

    #[test]
    fn columns_group_by_upper_key() {
        let mut db = fake();
        let m = fetch_columns_blocking(&mut db, true, "").unwrap();
        let cols = m.get(&("SCOTT".to_string(), "EMP".to_string())).unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].data_type, "NUMBER");
        assert_eq!(cols[0].comments, "employee id");
        assert_eq!(cols[1].comments, "");
    }

    #[test]
    fn sequences_fetch() {
        let mut db = fake();
        let s = fetch_sequences_blocking(&mut db, true, "").unwrap();
        assert_eq!(s[0].name, "EMP_SEQ");
        assert_eq!(s[0].kind, TableKind::Sequence);
    }

    #[test]
    fn fks_group_composite_keys() {
        let mut db = fake();
        let fks = fetch_fks_blocking(&mut db, true, "").unwrap();
        assert_eq!(fks.len(), 2);
        assert_eq!(fks[0].name, "EMP_DEPT_FK");
        assert_eq!(fks[0].from_cols, vec!["DEPTNO"]);
        assert_eq!(fks[0].to_cols, vec!["DEPTNO"]);
        assert_eq!(fks[1].from_cols, vec!["X", "Y"]);
        assert_eq!(fks[1].to_cols, vec!["X", "Y"]);
    }

    #[test]
    fn system_filter_sql_builds() {
        let f = system_filter_sql("owner", false, "");
        assert!(f.contains("WHERE owner NOT IN ('SYS', 'SYSTEM'"), "{f}");
        assert!(f.contains("NOT LIKE 'APEX\\_%'"), "{f}");
        assert_eq!(system_filter_sql("owner", true, ""), "");
        // sequence_owner variant.
        assert!(system_filter_sql("sequence_owner", false, "").starts_with(" WHERE sequence_owner"));
        // Own schema is exempt even when system.
        let f = system_filter_sql("owner", false, "system");
        assert!(f.contains("OR owner = 'SYSTEM'"), "{f}");
    }

    #[test]
    fn cache_columns_for_merges_owners() {
        let mut db = fake();
        let mut cache = MetadataCache {
            columns: fetch_columns_blocking(&mut db, true, "").unwrap(),
            ..Default::default()
        };
        assert_eq!(cache.columns_for(Some("scott"), "emp").len(), 2);
        assert_eq!(cache.columns_for(None, "emp").len(), 2);
        assert!(cache.columns_for(None, "nope").is_empty());
        assert!(cache.is_stale());
        cache.fetched_at = Some(Instant::now());
        assert!(!cache.is_stale());
    }
}
