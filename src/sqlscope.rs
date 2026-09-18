//! Tree-sitter → [`ScopeForest`] extraction (Phase A of the autocomplete plan).
//!
//! GUI-gated like [`crate::sqlparse`]: it owns the parser, produces pure
//! [`ScopeForest`] data, and never touches the dictionary. The grammar is
//! generic/Postgres-flavored, so extraction is best-effort — an unparsable
//! buffer yields an empty forest and the completion provider keeps using its
//! lexical fallback.

use std::collections::HashMap;

use tree_sitter::Node;

use crate::complete::{DmlAnchor, DmlKind, Relation, RelationKind, Scope, ScopeForest};

/// Extract the scope forest for `text`. Empty when the buffer can't be
/// parsed (partial buffer, PL/SQL, Oracle-only syntax).
pub fn extract(text: &str) -> ScopeForest {
    let Some(tree) = crate::sqlparse::parse(text) else {
        return ScopeForest::default();
    };
    let root = tree.root_node();
    // CTE name → projection columns. v1 collects across the whole buffer;
    // name collisions across separate statements are rare and only affect
    // which columns are offered, never correctness of the SQL.
    let mut ctes: HashMap<String, Vec<String>> = HashMap::new();
    for node in descendants(root) {
        if node.kind() == "cte" {
            if let Some((name, cols)) = read_cte(node, text) {
                ctes.insert(name.to_ascii_lowercase(), cols);
            }
        }
    }
    let mut scopes: Vec<Scope> = Vec::new();
    // Grammar shape: a `select` node spans only the select list; its `from`,
    // `where`, … are siblings under the same parent (`statement` or
    // `subquery`). So a scope is built from a `select` plus its siblings.
    // Selects inside a CTE definition are skipped — their columns feed the
    // CTE relation via `read_cte`, and they must not leak into the query
    // scope.
    for node in descendants(root) {
        if node.kind() == "select" && !has_ancestor_kind(node, "cte") {
            let depth = count_ancestors(node, "subquery");
            scopes.push(build_scope(node, text, &ctes, depth));
        }
    }
    scopes.sort_by_key(|s| s.start);

    // DML positions (INSERT column list, UPDATE SET) carry their own target.
    let mut dml = Vec::new();
    for node in descendants(root) {
        match node.kind() {
            "insert" => dml.extend(insert_anchor(node, text, &ctes)),
            "update" => dml.extend(update_anchor(node, text, &ctes)),
            _ => {}
        }
    }
    dml.sort_by_key(|a| a.start);

    ScopeForest { scopes, dml }
}

/// `INSERT INTO t (col, …) …` → the column-list span + target `t`.
fn insert_anchor(cte: Node, text: &str, ctes: &HashMap<String, Vec<String>>) -> Option<DmlAnchor> {
    let target_obj = named_children(cte)
        .into_iter()
        .find(|c| c.kind() == "object_reference")?;
    let target = object_ref_relation(target_obj, String::new(), text, ctes)?;
    // The first `list` whose members are `column` nodes is the column list.
    let list = named_children(cte).into_iter().find(|c| {
        c.kind() == "list"
            && named_children(*c)
                .first()
                .is_some_and(|m| m.kind() == "column")
    })?;
    Some(DmlAnchor {
        start: list.start_byte(),
        end: list.end_byte(),
        kind: DmlKind::InsertColumns,
        target,
    })
}

/// `UPDATE t SET …` → the SET-body span + target `t`.
fn update_anchor(cte: Node, text: &str, ctes: &HashMap<String, Vec<String>>) -> Option<DmlAnchor> {
    let rel = named_children(cte)
        .into_iter()
        .find(|c| c.kind() == "relation")?;
    let target = build_relation(rel, text, ctes)?;
    let set_kw = named_children(cte)
        .into_iter()
        .find(|c| c.kind() == "keyword_set")?;
    // The anchor ends where the assignments do: at `WHERE`, else the end of
    // the statement. (The grammar nests `where` inside the `update` node, so
    // running to the statement end would swallow the predicate — dropping its
    // keywords and any other tables' columns.)
    let end = named_children(cte)
        .into_iter()
        .find(|c| c.kind() == "where")
        .map(|w| w.start_byte())
        .or_else(|| cte.parent().map(|p| p.end_byte()))
        .unwrap_or_else(|| cte.end_byte());
    Some(DmlAnchor {
        start: set_kw.start_byte(),
        end,
        kind: DmlKind::UpdateSet,
        target,
    })
}

fn build_scope(
    select: Node,
    text: &str,
    ctes: &HashMap<String, Vec<String>>,
    depth: usize,
) -> Scope {
    // Relations live in the `from` sibling(s) under the same parent as this
    // `select`; projection comes from the `select` node itself.
    let mut relations = Vec::new();
    if let Some(parent) = select.parent() {
        for child in named_children(parent) {
            if child.kind() == "from" {
                relations.extend(relations_of(child, text, ctes));
            }
        }
    }
    // `ORDER BY` lives inside `from` (or the statement with no FROM). Its span
    // runs to the end of the statement so a cursor in trailing whitespace is
    // still inside; Oracle allows projection aliases only here.
    let scope_end = select
        .parent()
        .map(|p| p.end_byte())
        .unwrap_or_else(|| select.end_byte());
    let order_group = clause_spans(select, "order_by")
        .into_iter()
        .map(|(start, _)| (start, scope_end))
        .collect();
    Scope {
        start: select.start_byte(),
        end: scope_end,
        depth,
        relations,
        projection: projection_of(select, text),
        order_group,
    }
}

/// Byte spans of `kind` clauses belonging to `select`: a direct child of its
/// parent, or a child of the parent's `from` (where the grammar nests
/// `where`/`order_by`/`group_by`). Inner selects live under `subquery`
/// relations, so this never captures another scope's clauses.
fn clause_spans(select: Node, kind: &str) -> Vec<(usize, usize)> {
    let Some(parent) = select.parent() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for child in named_children(parent) {
        if child.kind() == kind {
            out.push((child.start_byte(), child.end_byte()));
        } else if child.kind() == "from" {
            for inner in named_children(child) {
                if inner.kind() == kind {
                    out.push((inner.start_byte(), inner.end_byte()));
                }
            }
        }
    }
    out
}

fn has_ancestor_kind(node: Node, kind: &str) -> bool {
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == kind {
            return true;
        }
        cur = n.parent();
    }
    false
}

fn count_ancestors(node: Node, kind: &str) -> usize {
    let mut count = 0;
    let mut cur = node.parent();
    while let Some(n) = cur {
        if n.kind() == kind {
            count += 1;
        }
        cur = n.parent();
    }
    count
}

/// Relations from a `from` node: each direct `relation`, plus the `relation`
/// inside every `join`-family child.
fn relations_of(from: Node, text: &str, ctes: &HashMap<String, Vec<String>>) -> Vec<Relation> {
    let mut out = Vec::new();
    for child in named_children(from) {
        match child.kind() {
            "relation" => out.extend(build_relation(child, text, ctes)),
            "join" | "cross_join" | "lateral_join" | "lateral_cross_join" => {
                for inner in named_children(child) {
                    if inner.kind() == "relation" {
                        out.extend(build_relation(inner, text, ctes));
                    }
                }
            }
            _ => {}
        }
    }
    out
}

fn build_relation(rel: Node, text: &str, ctes: &HashMap<String, Vec<String>>) -> Option<Relation> {
    let alias = rel
        .child_by_field_name("alias")
        .map(|n| text_of(n, text).to_string())
        .unwrap_or_default();

    // `(relation (subquery (select …)) alias: …)`
    if let Some(sub) = named_children(rel)
        .into_iter()
        .find(|c| c.kind() == "subquery")
    {
        let columns = first_descendant(sub, "select")
            .map(|sel| projection_of(sel, text))
            .unwrap_or_default();
        let name = if alias.is_empty() {
            "subquery".to_string()
        } else {
            alias.clone()
        };
        return Some(Relation {
            alias: name.clone(),
            owner: None,
            name,
            kind: RelationKind::Subquery,
            columns,
        });
    }

    // `(relation (object_reference name: (identifier) …) alias: …)`
    let obj = named_children(rel)
        .into_iter()
        .find(|c| c.kind() == "object_reference")?;
    object_ref_relation(obj, alias, text, ctes)
}

/// A relation from a bare `object_reference` (used by `from` relations and by
/// DML targets, which have no `relation` wrapper).
fn object_ref_relation(
    obj: Node,
    alias: String,
    text: &str,
    ctes: &HashMap<String, Vec<String>>,
) -> Option<Relation> {
    let ids: Vec<String> = descendants(obj)
        .filter(|n| matches!(n.kind(), "identifier" | "quoted_identifier"))
        .map(|n| text_of(n, text).trim_matches('"').to_string())
        .collect();
    let (owner, name) = match ids.as_slice() {
        [] => return None,
        [only] => (None, only.clone()),
        [.., owner, name] => (Some(owner.clone()), name.clone()),
    };
    let alias = if alias.is_empty() {
        name.clone()
    } else {
        alias
    };
    if let Some(columns) = ctes.get(&name.to_ascii_lowercase()) {
        return Some(Relation {
            alias,
            owner: None,
            name,
            kind: RelationKind::Cte,
            columns: columns.clone(),
        });
    }
    Some(Relation {
        alias,
        owner,
        name,
        kind: RelationKind::Table,
        columns: Vec::new(),
    })
}

/// Projection names of a `select`. Each `select_expression` holds every
/// comma-separated `term`; a term's name is its explicit alias when present,
/// else the last identifier of its `field`/`object_reference`. Expressions
/// without a name (`COUNT(*)`, literals, arithmetic) are skipped.
fn projection_of(select: Node, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for child in named_children(select) {
        if child.kind() != "select_expression" {
            continue;
        }
        for term in named_children(child)
            .into_iter()
            .filter(|c| c.kind() == "term")
        {
            if let Some(name) = simple_term_name(term, text) {
                out.push(name);
            }
        }
    }
    out
}

/// The column a bare `term` selects: its explicit `AS alias` when present,
/// else the last identifier of its `field`/`object_reference` value. `None`
/// for `*`, function calls, literals, …
fn simple_term_name(term: Node, text: &str) -> Option<String> {
    if let Some(alias) = term.child_by_field_name("alias") {
        return Some(text_of(alias, text).trim_matches('"').to_string());
    }
    let value = named_children(term)
        .into_iter()
        .find(|c| matches!(c.kind(), "field" | "object_reference"))?;
    descendants(value)
        .filter(|n| matches!(n.kind(), "identifier" | "quoted_identifier"))
        .last()
        .map(|n| text_of(n, text).trim_matches('"').to_string())
}

/// `(cte (identifier) (keyword_as) (statement (select …)))` → (name, columns).
fn read_cte(cte: Node, text: &str) -> Option<(String, Vec<String>)> {
    let name = named_children(cte)
        .into_iter()
        .find(|c| matches!(c.kind(), "identifier" | "quoted_identifier"))?;
    let select = first_descendant(cte, "select")?;
    Some((
        text_of(name, text).trim_matches('"').to_string(),
        projection_of(select, text),
    ))
}

fn text_of<'a>(node: Node, text: &'a str) -> &'a str {
    text.get(node.byte_range()).unwrap_or("")
}

fn named_children(node: Node) -> Vec<Node> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).collect()
}

fn first_descendant<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
    descendants(node).find(|n| n.kind() == kind)
}

/// Pre-order DFS over a node's descendants (iterative: trees can be deep).
fn descendants<'a>(node: Node<'a>) -> impl Iterator<Item = Node<'a>> {
    let mut stack: Vec<Node<'a>> = named_children(node);
    stack.reverse();
    std::iter::from_fn(move || {
        let next = stack.pop()?;
        let mut children = named_children(next);
        children.reverse();
        stack.extend(children);
        Some(next)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(rels: &[Relation]) -> Vec<&str> {
        rels.iter().map(|r| r.name.as_str()).collect()
    }

    #[test]
    fn from_and_join_relations_with_aliases() {
        let sql = "SELECT e.name, d.name FROM emp e JOIN dept d ON e.deptno = d.deptno";
        let f = extract(sql);
        let scope = f.scope_at(sql.find("ON").unwrap()).unwrap();
        assert_eq!(names(&scope.relations), ["emp", "dept"]);
        let aliases: Vec<&str> = scope.relations.iter().map(|r| r.alias.as_str()).collect();
        assert_eq!(aliases, ["e", "d"]);
        assert!(scope
            .relations
            .iter()
            .all(|r| r.kind == RelationKind::Table && r.columns.is_empty()));
    }

    #[test]
    fn schema_qualified_relation_keeps_owner() {
        let sql = "SELECT * FROM scott.emp";
        let f = extract(sql);
        let scope = f.scope_at(sql.find("emp").unwrap()).unwrap();
        let rel = &scope.relations[0];
        assert_eq!(rel.owner.as_deref(), Some("scott"));
        assert_eq!(rel.name, "emp");
        assert_eq!(rel.alias, "emp");
    }

    #[test]
    fn subquery_relation_carries_projection_columns() {
        let sql = "SELECT * FROM (SELECT a, b AS bb FROM t1) sub WHERE sub.a = 1";
        let f = extract(sql);
        // Outer scope: the subquery relation with its projection columns.
        let outer = f.scope_at(sql.find("WHERE").unwrap()).unwrap();
        let sub = &outer.relations[0];
        assert_eq!(sub.kind, RelationKind::Subquery);
        assert_eq!(sub.alias, "sub");
        assert_eq!(sub.columns, ["a", "bb"]);
        // Inner scope exists too with its own relation.
        let inner = f.scope_at(sql.find("t1").unwrap()).unwrap();
        assert_eq!(names(&inner.relations), ["t1"]);
        assert!(inner.depth > outer.depth);
    }

    #[test]
    fn cte_columns_resolve_from_the_cte_body() {
        let sql = "WITH recent AS (SELECT id, total FROM orders WHERE total > 100) \
                   SELECT id FROM recent";
        let f = extract(sql);
        let from_recent = sql.rfind("recent").unwrap();
        let cte = f.cte_at(from_recent, "recent").expect("CTE visible");
        assert_eq!(cte.columns, ["id", "total"]);
        let scope = f.scope_at(from_recent).unwrap();
        assert_eq!(names(&scope.relations), ["recent"]);
    }

    #[test]
    fn projection_aliases_and_plain_names() {
        let sql = "SELECT ename AS n, sal, COUNT(*) FROM emp";
        let f = extract(sql);
        let scope = f.scope_at(sql.find("FROM").unwrap()).unwrap();
        assert_eq!(scope.projection, ["n", "sal"]);
    }

    #[test]
    fn nested_scopes_do_not_leak_inner_aliases_outward() {
        let sql = "SELECT * FROM emp e WHERE EXISTS (SELECT 1 FROM dept d WHERE d.x = e.x)";
        let f = extract(sql);
        // Outside the subquery only `e` is visible.
        let outer = f.visible_relations(sql.find("SELECT *").unwrap());
        let outer_names: Vec<&str> = outer.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(outer_names, ["emp"]);
        // Inside the subquery both `d` and the correlated `e` are visible.
        let inner = f.visible_relations(sql.find("d.x").unwrap());
        let inner_names: Vec<&str> = inner.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(inner_names, ["dept", "emp"]);
    }

    #[test]
    fn empty_input_yields_empty_forest() {
        assert!(extract("").is_empty());
        assert!(extract("-- just a comment").is_empty());
    }

    #[test]
    fn insert_column_list_anchor_targets_the_table() {
        let sql = "INSERT INTO emp (empno, ename) VALUES (1, 'x')";
        let f = extract(sql);
        let anchor = f
            .dml_at(sql.find("ename").unwrap())
            .expect("insert anchor in the column list");
        assert_eq!(anchor.kind, DmlKind::InsertColumns);
        assert_eq!(anchor.target.name, "emp");
        // The VALUES list is not a column-list anchor.
        assert!(f.dml_at(sql.find("VALUES").unwrap() + 7).is_none());
    }

    #[test]
    fn update_set_anchor_targets_the_table() {
        let sql = "UPDATE emp SET sal = 1 WHERE id = 2";
        let f = extract(sql);
        let anchor = f.dml_at(sql.find("sal").unwrap()).expect("update anchor");
        assert_eq!(anchor.kind, DmlKind::UpdateSet);
        assert_eq!(anchor.target.name, "emp");
        // The table position (before SET) is not anchored.
        assert!(f.dml_at(sql.find("emp").unwrap()).is_none());
        // The anchor stops at WHERE: the predicate is a normal context (its
        // keywords and other tables must stay available).
        assert!(
            f.dml_at(sql.find("id").unwrap()).is_none(),
            "WHERE must not be inside the SET anchor"
        );
    }

    #[test]
    fn order_by_span_is_recorded_for_projection_aliases() {
        let sql = "SELECT ename AS n FROM emp ORDER BY n";
        let f = extract(sql);
        let order = sql.find("ORDER").unwrap();
        assert!(f.allows_projection_alias(order + 8), "inside ORDER BY");
        assert!(
            !f.allows_projection_alias(sql.find("FROM").unwrap()),
            "not in the select list"
        );
        assert_eq!(f.projection_at(order + 8), ["n"]);

        // Trailing whitespace after the clause is still inside it (the span
        // runs to the end of the statement).
        let trailing = "SELECT ename AS n FROM emp ORDER BY ";
        let ft = extract(trailing);
        assert!(
            ft.allows_projection_alias(trailing.len()),
            "cursor after `ORDER BY ` still offers aliases"
        );
    }
}
