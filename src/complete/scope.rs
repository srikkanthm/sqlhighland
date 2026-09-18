//! Structural query scope (Phase A of the autocomplete plan).
//!
//! Pure, GUI-free data produced by the tree-sitter extractor
//! (`crate::sqlscope`, GUI-gated) and consumed by the completion provider to
//! resolve *which* relations and projection names are visible at a cursor.
//! The extractor never supplies identifiers from the dictionary — only the
//! structure (aliases, owner/name, CTE/subquery column names).
//!
//! Fallback contract: an empty forest means "no structural information", and
//! the caller keeps using the lexical `build_alias_map` path unchanged.

use std::collections::HashSet;

/// What a relation in `FROM`/`JOIN` refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationKind {
    /// A base table or view (the dictionary distinguishes them).
    Table,
    /// A CTE defined by an enclosing `WITH`.
    Cte,
    /// An inline `(SELECT …)` subquery.
    Subquery,
}

/// One relation visible in a scope, keyed by the name it is referenced by
/// (`alias` when present, else the object name).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relation {
    /// The name it is referenced by as written (`e`, `dept`, `recent`).
    pub alias: String,
    /// Schema/owner for a dotted reference, when present.
    pub owner: Option<String>,
    /// The underlying object name (table/view/CTE name, or the subquery alias).
    pub name: String,
    pub kind: RelationKind,
    /// Column names for a CTE or subquery (projection names); empty for
    /// table/view relations, whose columns come from the dictionary cache.
    pub columns: Vec<String>,
}

impl Relation {
    /// Case-insensitive lookup key (unquoted identifiers fold).
    pub fn key(&self) -> String {
        self.alias.to_ascii_lowercase()
    }
}

/// One `SELECT`-level scope: its span, nesting depth (0 = outermost), the
/// relations in its `FROM`/`JOIN`, and its projection names.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Scope {
    pub start: usize,
    pub end: usize,
    pub depth: usize,
    pub relations: Vec<Relation>,
    pub projection: Vec<String>,
    /// Byte spans of this select's `ORDER BY` clause (to the end of its
    /// statement), where the projection aliases above are valid. Oracle allows
    /// aliases only in `ORDER BY`, not `GROUP BY`/`HAVING`/`WHERE`.
    pub order_group: Vec<(usize, usize)>,
}

/// All scopes found in a buffer, in document order.
#[derive(Debug, Clone, Default)]
pub struct ScopeForest {
    pub scopes: Vec<Scope>,
    /// DML positions (INSERT column list, UPDATE SET) with their target.
    pub dml: Vec<DmlAnchor>,
}

/// Which DML position an anchor marks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmlKind {
    /// Inside an `INSERT INTO t (col, …)` column list — column names only.
    InsertColumns,
    /// In an `UPDATE t SET …` body — columns and expressions.
    UpdateSet,
}

/// A region of a DML statement where only the target table's columns make
/// sense, plus the target relation itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmlAnchor {
    pub start: usize,
    pub end: usize,
    pub kind: DmlKind,
    pub target: Relation,
}

impl ScopeForest {
    pub fn is_empty(&self) -> bool {
        self.scopes.is_empty() && self.dml.is_empty()
    }

    /// The innermost scope whose span contains `offset` (strict). Prefer
    /// [`enclosing`](Self::enclosing) for completion, which tolerates a cursor
    /// in trailing whitespace past the parsed span.
    pub fn scope_at(&self, offset: usize) -> Option<&Scope> {
        self.scopes
            .iter()
            .filter(|s| s.start <= offset && offset <= s.end)
            .max_by_key(|s| s.depth)
    }

    /// The scope the cursor belongs to: the innermost one containing `offset`,
    /// or — when the cursor sits in trailing whitespace past the parsed span
    /// (very common right after typing `WHERE `) — the nearest preceding scope.
    /// Mirrors `sql::statement_at`'s trailing-space rule.
    pub fn enclosing(&self, offset: usize) -> Option<&Scope> {
        self.scope_at(offset).or_else(|| {
            self.scopes
                .iter()
                .filter(|s| s.start <= offset)
                .max_by_key(|s| (s.start, s.depth))
        })
    }

    /// Relations visible at `offset`: the innermost scope's first, then each
    /// enclosing scope's (correlated references), deduped by alias key with
    /// the nearest definition winning.
    pub fn visible_relations(&self, offset: usize) -> Vec<&Relation> {
        let Some(scope) = self.enclosing(offset) else {
            return Vec::new();
        };
        let mut scopes: Vec<&Scope> = self
            .scopes
            .iter()
            .filter(|s| s.start <= scope.start && scope.end <= s.end)
            .collect();
        scopes.sort_by_key(|s| std::cmp::Reverse(s.depth));
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for scope in scopes {
            for rel in &scope.relations {
                if seen.insert(rel.key()) {
                    out.push(rel);
                }
            }
        }
        out
    }

    /// Projection names of the innermost scope containing `offset`.
    pub fn projection_at(&self, offset: usize) -> &[String] {
        self.enclosing(offset)
            .map(|s| s.projection.as_slice())
            .unwrap_or(&[])
    }

    /// A CTE relation named `name` visible at `offset` (case-insensitive).
    pub fn cte_at(&self, offset: usize, name: &str) -> Option<&Relation> {
        self.visible_relations(offset)
            .into_iter()
            .find(|r| r.kind == RelationKind::Cte && r.name.eq_ignore_ascii_case(name))
    }

    /// The innermost DML anchor containing `offset`, if any.
    pub fn dml_at(&self, offset: usize) -> Option<&DmlAnchor> {
        self.dml
            .iter()
            .filter(|a| a.start <= offset && offset <= a.end)
            .min_by_key(|a| a.end.saturating_sub(a.start))
    }

    /// Whether projection aliases are valid at `offset`. Each recorded span
    /// runs to the end of its statement, so a cursor in trailing whitespace
    /// (`ORDER BY ␣`) is still inside. The enclosing lookup tolerates an
    /// offset just past the parsed span.
    pub fn allows_projection_alias(&self, offset: usize) -> bool {
        self.enclosing(offset)
            .is_some_and(|s| s.order_group.iter().any(|(a, _)| *a <= offset))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(alias: &str, name: &str, kind: RelationKind, columns: &[&str]) -> Relation {
        Relation {
            alias: alias.to_string(),
            owner: None,
            name: name.to_string(),
            kind,
            columns: columns.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn forest(scopes: Vec<Scope>) -> ScopeForest {
        ScopeForest {
            scopes,
            dml: Vec::new(),
        }
    }

    #[test]
    fn scope_at_picks_the_innermost() {
        let f = forest(vec![
            Scope {
                start: 0,
                end: 100,
                depth: 0,
                relations: vec![rel("e", "emp", RelationKind::Table, &[])],
                projection: vec![],
                order_group: vec![],
            },
            Scope {
                start: 20,
                end: 60,
                depth: 1,
                relations: vec![rel("d", "dept", RelationKind::Table, &[])],
                projection: vec![],
                order_group: vec![],
            },
        ]);
        assert_eq!(f.scope_at(30).unwrap().depth, 1);
        assert_eq!(f.scope_at(80).unwrap().depth, 0);
        assert!(f.scope_at(200).is_none());
    }

    #[test]
    fn visible_relations_include_enclosing_scopes_nearest_first() {
        let f = forest(vec![
            Scope {
                start: 0,
                end: 100,
                depth: 0,
                relations: vec![
                    rel("e", "emp", RelationKind::Table, &[]),
                    rel("d", "dept", RelationKind::Table, &[]),
                ],
                projection: vec![],
                order_group: vec![],
            },
            Scope {
                start: 20,
                end: 60,
                depth: 1,
                // Same alias `e` shadows the outer one with a different table.
                relations: vec![rel("e", "employee", RelationKind::Table, &[])],
                projection: vec![],
                order_group: vec![],
            },
        ]);
        let vis = f.visible_relations(30);
        let names: Vec<&str> = vis.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["employee", "dept"],
            "inner `e` shadows outer; `d` still visible"
        );
        // Outside the subquery, the outer definitions apply.
        let outer: Vec<&str> = f
            .visible_relations(80)
            .iter()
            .map(|r| r.name.as_str())
            .collect();
        assert_eq!(outer, ["emp", "dept"]);
    }

    #[test]
    fn projection_and_cte_lookup() {
        let f = forest(vec![Scope {
            start: 0,
            end: 50,
            depth: 0,
            relations: vec![rel("recent", "recent", RelationKind::Cte, &["id", "total"])],
            projection: vec!["id".to_string()],
            order_group: vec![],
        }]);
        assert_eq!(f.projection_at(10), ["id"]);
        let cte = f.cte_at(10, "RECENT").expect("CTE visible");
        assert_eq!(cte.columns, ["id", "total"]);
        assert!(f.cte_at(10, "orders").is_none());
    }

    #[test]
    fn dml_at_picks_the_containing_anchor() {
        let f = ScopeForest {
            scopes: Vec::new(),
            dml: vec![
                DmlAnchor {
                    start: 10,
                    end: 30,
                    kind: DmlKind::UpdateSet,
                    target: rel("emp", "emp", RelationKind::Table, &[]),
                },
                DmlAnchor {
                    start: 12,
                    end: 20,
                    kind: DmlKind::InsertColumns,
                    target: rel("t", "t", RelationKind::Table, &[]),
                },
            ],
        };
        assert_eq!(f.dml_at(15).unwrap().kind, DmlKind::InsertColumns);
        assert_eq!(f.dml_at(25).unwrap().kind, DmlKind::UpdateSet);
        assert!(f.dml_at(5).is_none());
        // An empty forest (no scopes, no dml) reports empty.
        assert!(ScopeForest::default().is_empty());
    }

    #[test]
    fn projection_aliases_only_inside_order_or_group() {
        let f = forest(vec![Scope {
            start: 0,
            end: 100,
            depth: 0,
            relations: vec![rel("e", "emp", RelationKind::Table, &[])],
            projection: vec!["n".to_string()],
            order_group: vec![(60, 90)],
        }]);
        assert!(f.allows_projection_alias(70), "inside ORDER BY");
        assert!(
            !f.allows_projection_alias(10),
            "select list is not ORDER BY"
        );
        assert_eq!(f.projection_at(70), ["n"]);
    }

    #[test]
    fn enclosing_tolerates_a_cursor_past_the_parsed_span() {
        // Tree-sitter spans end at the last token, so a cursor in trailing
        // whitespace (`... WHERE ␣`) sits past `end`; the scope must still
        // resolve (the reported "columns missing after WHERE" bug).
        let f = forest(vec![Scope {
            start: 0,
            end: 10,
            depth: 0,
            relations: vec![rel("e", "emp", RelationKind::Table, &[])],
            projection: vec!["n".to_string()],
            order_group: vec![(5, 10)],
        }]);
        assert_eq!(
            f.visible_relations(12).len(),
            1,
            "trailing offset still scoped"
        );
        assert_eq!(f.projection_at(12), ["n"]);
        assert_eq!(f.enclosing(12).unwrap().depth, 0);
        assert!(f.allows_projection_alias(12));
    }
}
