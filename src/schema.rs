//! Engine-agnostic schema-browser model + provider trait.
//!
//! The sidebar tree renders [`SchemaTree`] only — never SQL, never
//! dictionary views. Each engine implements [`SchemaProvider`] over its own
//! cached snapshot; v1 ships the Oracle provider over [`MetadataCache`].
//! GUI-free: pure data + filtering, unit-tested here.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::complete::{display_name, is_system_schema};
use crate::metadata::{MetadataCache, TableKind};

/// Database engine. Only Oracle exists today; the enum (plus [`SchemaProvider`])
/// is the seam the second engine plugs into. Serde-defaults to Oracle so
/// saved connections load unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DbEngine {
    #[default]
    Oracle,
}

impl DbEngine {
    /// Display label for the connection dialog's Database type row.
    /// New variants add their label here (and a pill next to Oracle's).
    pub fn label(self) -> &'static str {
        match self {
            DbEngine::Oracle => "Oracle",
        }
    }
}

/// One schema (owner) with its objects grouped for the tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaGroup {
    pub name: String,
    pub tables: Vec<SchemaObject>,
    pub views: Vec<SchemaObject>,
    pub sequences: Vec<String>,
}

/// A table or view with its columns (already uppercased by the fetch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaObject {
    pub name: String,
    pub columns: Vec<SchemaColumn>,
}

/// Lean column for tree display (full [`crate::metadata::ColumnMeta`]
/// with types/comments stays in the cache for hover/cards).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaColumn {
    pub name: String,
    pub data_type: String,
}

/// Whole-connection tree: schemas in display order (own schema first).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaTree {
    pub schemas: Vec<SchemaGroup>,
}

/// Schema-browser backend. Implemented per engine; the UI calls only this.
pub trait SchemaProvider {
    /// Group cached objects into display-ordered schemas. `show_system`
    /// and `own_schema` apply the same visibility rules as completion.
    /// `own_only` keeps just the connected user's schema (browser mode:
    /// no schema level, no other users).
    fn tree(
        &self,
        cache: &MetadataCache,
        show_system: bool,
        own_schema: &str,
        own_only: bool,
    ) -> SchemaTree;
    /// Statement that describes an object when run. Tables/views use the
    /// native DESCRIBE (bare for own schema); sequences have no columns,
    /// so they get an `ALL_SEQUENCES` row instead of a zero-row DESCRIBE.
    fn describe_sql(
        &self,
        owner: &str,
        name: &str,
        own_schema: &str,
        kind: TableKind,
    ) -> String;
    /// Display title for an object (bare for own schema).
    fn object_title(&self, owner: &str, name: &str, own_schema: &str) -> String {
        display_name(Some(owner), name, own_schema)
    }
}

/// Oracle over the autocomplete dictionary cache. No new queries: tables,
/// views, columns, and sequences all come from [`MetadataCache`].
pub struct OracleProvider;

impl SchemaProvider for OracleProvider {
    fn tree(
        &self,
        cache: &MetadataCache,
        show_system: bool,
        own_schema: &str,
        own_only: bool,
    ) -> SchemaTree {
        let hidden = |owner: &str| {
            (!show_system && !owner.eq_ignore_ascii_case(own_schema) && is_system_schema(owner))
                || (own_only && !owner.eq_ignore_ascii_case(own_schema))
        };
        // Group tables/views by owner; sequences by owner.
        let mut tables: BTreeMap<String, Vec<SchemaObject>> = BTreeMap::new();
        let mut views: BTreeMap<String, Vec<SchemaObject>> = BTreeMap::new();
        for t in &cache.tables {
            if hidden(&t.owner) {
                continue;
            }
            let cols = cache
                .columns_for(Some(&t.owner), &t.name)
                .into_iter()
                .map(|c| SchemaColumn {
                    name: c.name,
                    data_type: c.data_type,
                })
                .collect();
            let obj = SchemaObject {
                name: t.name.clone(),
                columns: cols,
            };
            match t.kind {
                TableKind::Table => tables.entry(t.owner.clone()).or_default().push(obj),
                TableKind::View => views.entry(t.owner.clone()).or_default().push(obj),
                TableKind::Sequence => {}
            }
        }
        let mut seqs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for s in &cache.sequences {
            if hidden(&s.owner) {
                continue;
            }
            seqs.entry(s.owner.clone()).or_default().push(s.name.clone());
        }
        // Owners: union of all groups; own schema first, rest sorted.
        let mut owners: Vec<String> = tables
            .keys()
            .chain(views.keys())
            .chain(seqs.keys())
            .cloned()
            .collect();
        owners.sort();
        owners.dedup();
        let mut own = vec![];
        let mut rest = vec![];
        for o in owners {
            if o.eq_ignore_ascii_case(own_schema) {
                own.push(o);
            } else {
                rest.push(o);
            }
        }
        let mut schemas = Vec::with_capacity(own.len() + rest.len());
        for o in own.into_iter().chain(rest) {
            let mut g = SchemaGroup {
                name: o.clone(),
                tables: tables.remove(&o).unwrap_or_default(),
                views: views.remove(&o).unwrap_or_default(),
                sequences: seqs.remove(&o).unwrap_or_default(),
            };
            g.tables.sort_by(|a, b| a.name.cmp(&b.name));
            g.views.sort_by(|a, b| a.name.cmp(&b.name));
            g.sequences.sort();
            schemas.push(g);
        }
        SchemaTree { schemas }
    }

    fn describe_sql(&self, owner: &str, name: &str, own_schema: &str, kind: TableKind) -> String {
        match kind {
            TableKind::Sequence => format!(
                "SELECT sequence_name AS \"Name\", min_value AS \"Min\", \
                    max_value AS \"Max\", increment_by AS \"Increment\", \
                    last_number AS \"Last\" \
                 FROM all_sequences WHERE sequence_owner = '{}' AND sequence_name = '{}'",
                owner.replace('\'', "''"),
                name.replace('\'', "''"),
            ),
            TableKind::Table | TableKind::View => {
                format!("DESCRIBE {}", display_name(Some(owner), name, own_schema))
            }
        }
    }
}

/// Parse a browser object node id (`o:{schema}:{T|V|S}:{object}`) back
/// into its parts. Column (`c:…`), folder, and malformed ids return None.
/// Shared by row clicks and the context menu so both resolve identically.
pub fn parse_object_id(id: &str) -> Option<(String, String, TableKind)> {
    let rest = id.strip_prefix("o:")?;
    let mut parts = rest.split(':');
    let (schema, kind, first) = (parts.next()?, parts.next()?, parts.next()?);
    let kind = match kind {
        "T" => TableKind::Table,
        "V" => TableKind::View,
        "S" => TableKind::Sequence,
        _ => return None,
    };
    // Rejoin defensively so a `:` inside a quoted name never misresolves.
    let mut name = first.to_string();
    for p in parts {
        name.push(':');
        name.push_str(p);
    }
    Some((schema.to_string(), name, kind))
}

/// Narrow a tree to schemas/groups/objects matching `needle`
/// (case-insensitive contains). Columns never filter: a matching object
/// always shows its full column list. Empty needle returns everything.
pub fn filter_tree(tree: &SchemaTree, needle: &str) -> SchemaTree {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return tree.clone();
    }
    let hit = |s: &str| s.to_lowercase().contains(&needle);
    let mut out = SchemaTree::default();
    for g in &tree.schemas {
        let mut ng = SchemaGroup {
            name: g.name.clone(),
            tables: vec![],
            views: vec![],
            sequences: vec![],
        };
        let keep_group = hit(&g.name);
        let pick = |objs: &[SchemaObject], into: &mut Vec<SchemaObject>| {
            for obj in objs {
                if keep_group || hit(&obj.name) {
                    into.push(SchemaObject {
                        name: obj.name.clone(),
                        columns: obj.columns.clone(),
                    });
                }
            }
        };
        pick(&g.tables, &mut ng.tables);
        pick(&g.views, &mut ng.views);
        ng.sequences = g
            .sequences
            .iter()
            .filter(|s| keep_group || hit(s))
            .cloned()
            .collect();
        if keep_group || !ng.tables.is_empty() || !ng.views.is_empty() || !ng.sequences.is_empty() {
            out.schemas.push(ng);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::{ColumnMeta, TableId};

    fn cache() -> MetadataCache {
        let mut c = MetadataCache {
            tables: vec![
                TableId {
                    owner: "SYSTEM".into(),
                    name: "EMPLOYEES".into(),
                    kind: TableKind::Table,
                },
                TableId {
                    owner: "HR".into(),
                    name: "DEPT".into(),
                    kind: TableKind::Table,
                },
                TableId {
                    owner: "HR".into(),
                    name: "EMPVW".into(),
                    kind: TableKind::View,
                },
            TableId {
                owner: "SYS".into(),
                name: "DUALX".into(),
                kind: TableKind::Table,
            },
            ],
            ..Default::default()
        };
        c.columns.insert(
            ("SYSTEM".into(), "EMPLOYEES".into()),
            vec![ColumnMeta {
                name: "ID".into(),
                data_type: "NUMBER".into(),
                comments: "".into(),
            }],
        );
        c.columns.insert(
            ("HR".into(), "DEPT".into()),
            vec![ColumnMeta {
                name: "DEPTNO".into(),
                data_type: "NUMBER".into(),
                comments: "".into(),
            }],
        );
        c.sequences = vec![TableId {
            owner: "HR".into(),
            name: "DEPT_SEQ".into(),
            kind: TableKind::Sequence,
        }];
        c
    }

    #[test]
    fn tree_groups_kinds_and_orders_own_first() {
        let t = OracleProvider.tree(&cache(), false, "SYSTEM", false);
        assert_eq!(t.schemas.len(), 2);
        assert_eq!(t.schemas[0].name, "SYSTEM");
        assert_eq!(t.schemas[1].name, "HR");
        // SYS hidden by the system filter.
        assert!(!t.schemas.iter().any(|g| g.name == "SYS"));
        let sys = &t.schemas[0];
        assert_eq!(sys.tables.len(), 1);
        assert_eq!(sys.tables[0].name, "EMPLOYEES");
        assert_eq!(sys.tables[0].columns.len(), 1);
        assert!(sys.views.is_empty() && sys.sequences.is_empty());
        let hr = &t.schemas[1];
        assert_eq!(hr.tables.len(), 1);
        assert_eq!(hr.views.len(), 1);
        assert_eq!(hr.views[0].name, "EMPVW");
        assert_eq!(hr.sequences, vec!["DEPT_SEQ".to_string()]);
    }

    #[test]
    fn tree_show_system_includes_sys() {
        let t = OracleProvider.tree(&cache(), true, "SYSTEM", false);
        assert!(t.schemas.iter().any(|g| g.name == "SYS"));
    }

    #[test]
    fn tree_own_only_keeps_connected_schema() {
        let t = OracleProvider.tree(&cache(), false, "SYSTEM", true);
        assert_eq!(t.schemas.len(), 1);
        assert_eq!(t.schemas[0].name, "SYSTEM");
        assert_eq!(t.schemas[0].tables.len(), 1);
        // Case-insensitive owner match.
        let t = OracleProvider.tree(&cache(), false, "system", true);
        assert_eq!(t.schemas.len(), 1);
        // Unknown user sees nothing (never another schema).
        let t = OracleProvider.tree(&cache(), false, "NOBODY", true);
        assert!(t.schemas.is_empty());
    }

    #[test]
    fn filter_narrows_to_matches() {
        let t = OracleProvider.tree(&cache(), false, "SYSTEM", false);
        let f = filter_tree(&t, "dept");
        assert_eq!(f.schemas.len(), 1);
        assert_eq!(f.schemas[0].name, "HR");
        assert_eq!(f.schemas[0].tables.len(), 1);
        assert!(f.schemas[0].views.is_empty());
        assert_eq!(f.schemas[0].sequences, vec!["DEPT_SEQ".to_string()]);
        // Columns never filter: matching objects keep full column lists,
        // column-only needles match nothing.
        let f = filter_tree(&t, "dept");
        assert_eq!(f.schemas[0].tables[0].columns.len(), 1);
        assert!(filter_tree(&t, "deptno").schemas.is_empty());
        // Empty needle returns everything.
        assert_eq!(filter_tree(&t, "  ").schemas.len(), 2);
        // No match returns nothing.
        assert!(filter_tree(&t, "zzz").schemas.is_empty());
    }

    #[test]
    fn object_ids_round_trip() {
        use TableKind::*;
        assert_eq!(
            parse_object_id("o:HR:T:DEPT"),
            Some(("HR".to_string(), "DEPT".to_string(), Table))
        );
        assert_eq!(
            parse_object_id("o:HR:V:EMPVW"),
            Some(("HR".to_string(), "EMPVW".to_string(), View))
        );
        assert_eq!(parse_object_id("c:HR:DEPT:DEPTNO"), None);
        assert_eq!(parse_object_id("g:HR/Tables"), None);
        assert_eq!(parse_object_id("o:HR:X:WEIRD"), None);
        assert_eq!(parse_object_id("o:HR:T"), None);
    }

    #[test]
    fn describe_sql_bares_own_schema() {
        use TableKind::*;
        assert_eq!(
            OracleProvider.describe_sql("SYSTEM", "EMPLOYEES", "system", Table),
            "DESCRIBE EMPLOYEES"
        );
        assert_eq!(
            OracleProvider.describe_sql("HR", "DEPT", "SYSTEM", Table),
            "DESCRIBE HR.DEPT"
        );
        assert_eq!(
            OracleProvider.describe_sql("HR", "EMPVW", "SYSTEM", View),
            "DESCRIBE HR.EMPVW"
        );
        // Sequences query the catalog (DESCRIBE would return zero rows).
        let seq = OracleProvider.describe_sql("HR", "DEPT_SEQ", "HR", Sequence);
        assert!(seq.contains("all_sequences"), "{seq}");
        assert!(seq.contains("DEPT_SEQ"), "{seq}");
        assert_eq!(
            OracleProvider.object_title("SYSTEM", "EMPLOYEES", "SYSTEM"),
            "EMPLOYEES"
        );
    }
}
