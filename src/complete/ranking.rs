//! Candidate keyword/function tables and ranking.
//!
//! Part of the completion engine (see `complete.rs`).

use super::*;

/// Statement starters for empty statement heads.
pub const STMT_KEYWORDS: &[&str] = &[
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "DROP", "ALTER", "TRUNCATE",
    "DESCRIBE", "EXPLAIN", "COMMIT", "ROLLBACK", "BEGIN", "DECLARE",
];

/// Expression keywords for select lists.
pub const EXPR_KEYWORDS: &[&str] = &[
    "DISTINCT", "ALL", "CASE", "WHEN", "THEN", "ELSE", "NOT", "NULL",
];

/// Predicate keywords for WHERE/GROUP/ORDER/HAVING/ON conditions.
pub const PRED_KEYWORDS: &[&str] = &[
    "AND", "OR", "NOT", "IN", "LIKE", "BETWEEN", "EXISTS", "IS", "NULL", "CASE", "WHEN",
];

/// Clause-transition keywords valid right after a select list
/// (`SELECT * fro|` must offer FROM — strictness is about the prefix
/// matching, not about hiding transitions).
pub const SELECT_FOLLOW: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "LIMIT",
    "UNION",
    "INTERSECT",
    "MINUS",
    "INTO",
    "FETCH",
];

/// Clause-transition keywords valid after a predicate
/// (`WHERE x=1 ord|` must offer ORDER).
pub const PRED_FOLLOW: &[&str] = &[
    "ORDER",
    "GROUP",
    "HAVING",
    "LIMIT",
    "UNION",
    "INTERSECT",
    "MINUS",
    "FETCH",
    "OFFSET",
];

/// Oracle keywords worth completing (v1 set — statements, clauses, common
/// functions, sequence pseudo-columns). Uppercase; matching is case-insensitive.
pub const ORACLE_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "OUTER",
    "ON",
    "GROUP",
    "BY",
    "ORDER",
    "HAVING",
    "UNION",
    "UNION ALL",
    "MINUS",
    "INTERSECT",
    "INSERT",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE",
    "MERGE",
    "USING",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "CASE",
    "AND",
    "OR",
    "NOT",
    "IN",
    "EXISTS",
    "BETWEEN",
    "LIKE",
    "IS",
    "NULL",
    "DISTINCT",
    "ALL",
    "AS",
    "ASC",
    "DESC",
    "WITH",
    "CONNECT",
    "START",
    "PRIOR",
    "SIBLINGS",
    "ROWNUM",
    "ROWID",
    "NEXTVAL",
    "CURRVAL",
    "SYSDATE",
    "SYSTIMESTAMP",
    "DUAL",
    "COMMIT",
    "ROLLBACK",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "TABLE",
    "VIEW",
    "INDEX",
    "SEQUENCE",
    "DESCRIBE",
    "EXPLAIN",
    "GRANT",
    "ORDER",
    "SIBLINGS",
    "BEGIN",
    "DECLARE",
    "LIMIT",
    "OFFSET",
    "FETCH",
];

/// Built-in functions: `(NAME, signature shown in the detail pane)`.
/// Labels complete as `NAME()` — the kit has no snippet engine (`$1` would
/// insert literally) and no post-accept hook, so the cursor lands after the
/// closing paren (one Left-arrow into the args). True tab-stops need kit
/// support. Names here must not repeat in [`ORACLE_KEYWORDS`].
pub const ORACLE_FUNCTIONS: &[(&str, &str)] = &[
    ("ADD_MONTHS", "ADD_MONTHS(date, n)"),
    ("AVG", "AVG([DISTINCT | ALL] expr)"),
    ("CAST", "CAST(expr AS type)"),
    ("COALESCE", "COALESCE(expr, …)"),
    ("COUNT", "COUNT(* | [DISTINCT | ALL] expr)"),
    ("DECODE", "DECODE(expr, search, result [, …] [, default])"),
    ("DENSE_RANK", "DENSE_RANK() OVER (…)"),
    ("EXTRACT", "EXTRACT(field FROM src)"),
    ("INITCAP", "INITCAP(char)"),
    ("INSTR", "INSTR(str, substr [, pos [, nth]])"),
    ("LAST_DAY", "LAST_DAY(date)"),
    ("LENGTH", "LENGTH(char)"),
    (
        "LISTAGG",
        "LISTAGG(expr [, delim]) WITHIN GROUP (ORDER BY …)",
    ),
    ("LOWER", "LOWER(char)"),
    ("MAX", "MAX([DISTINCT | ALL] expr)"),
    ("MIN", "MIN([DISTINCT | ALL] expr)"),
    ("MOD", "MOD(n, m)"),
    ("MONTHS_BETWEEN", "MONTHS_BETWEEN(d1, d2)"),
    ("NULLIF", "NULLIF(expr1, expr2)"),
    ("NVL", "NVL(expr1, expr2)"),
    ("NVL2", "NVL2(expr, v1, v2)"),
    ("RANK", "RANK() OVER (…)"),
    ("ROUND", "ROUND(n [, m])"),
    ("ROW_NUMBER", "ROW_NUMBER() OVER (…)"),
    ("SUBSTR", "SUBSTR(char, pos [, len])"),
    ("SUM", "SUM([DISTINCT | ALL] expr)"),
    ("TO_CHAR", "TO_CHAR(n | date [, fmt [, nls]])"),
    ("TO_DATE", "TO_DATE(char [, fmt [, nls]])"),
    ("TO_NUMBER", "TO_NUMBER(char [, fmt [, nls]])"),
    ("TRIM", "TRIM([LEAD|TRAIL|BOTH] [char] FROM src)"),
    ("TRUNC", "TRUNC(n [, m] | date [, fmt])"),
    ("UPPER", "UPPER(char)"),
];

/// Insert text for a function: call form with empty args.
pub fn function_insert(name: &str) -> String {
    format!("{name}()")
}

/// System schemas hidden by default (toggleable). Covers the Oracle
/// catalogs a DBA account sees but never queries: core, spatial/text,
/// vault/audit/label-security, GSM, APEX/FLOWS, and maintenance accounts.
/// Shared with `metadata.rs`, which excludes them at the SQL level.
pub const SYSTEM_SCHEMAS: &[&str] = &[
    "SYS",
    "SYSTEM",
    "XDB",
    "MDSYS",
    "MDDATA",
    "CTXSYS",
    "ORDSYS",
    "ORDDATA",
    "ORDPLUGINS",
    "OLAPSYS",
    "SI_INFORMTN_SCHEMA",
    "WMSYS",
    "DBSNMP",
    "OUTLN",
    "EXFSYS",
    "DIP",
    "TSMSYS",
    "DMSYS",
    "ODM",
    "ODM_MTR",
    "ANONYMOUS",
    "APPQOSSYS",
    "AUDSYS",
    "DVSYS",
    "DVF",
    "DV_MONITOR",
    "LBACSYS",
    "GSMADMIN_INTERNAL",
    "GSMCATUSER",
    "GSMROOTUSER",
    "GSMUSER",
    "REMOTE_SCHEDULER_AGENT",
    "SYSBACKUP",
    "SYSDG",
    "SYSKM",
    "SYSRAC",
    "ORACLE_OCM",
    "SPATIAL_WFS_ADMIN_USR",
    "SPATIAL_CSW_ADMIN_USR",
    "XS$NULL",
    "OJVMSYS",
    "SYSMAN",
];

/// Versioned/prefixed system families (`APEX_240200`, `FLOWS_300100`, …).
pub fn is_system_schema_prefix(owner_upper: &str) -> bool {
    owner_upper.starts_with("APEX_") || owner_upper.starts_with("FLOWS_")
}

pub fn is_system_schema(owner: &str) -> bool {
    let up = owner.to_ascii_uppercase();
    is_system_schema_prefix(&up) || SYSTEM_SCHEMAS.contains(&up.as_str())
}

/// Rank + filter candidates for `prefix` (case-insensitive). Order:
/// kind rank, exact match, own-schema tier, prefix quality, usage, length,
/// label. `own_schema` is the connected user ("" = no preference).
/// Empty prefix returns kind order capped at `limit`.
pub fn rank_candidates(
    prefix: &str,
    mut cands: Vec<Candidate>,
    own_schema: &str,
    limit: usize,
) -> Vec<Candidate> {
    let pre = prefix.to_lowercase();
    cands.retain(|c| {
        if pre.is_empty() {
            return true;
        }
        let l = c.label.to_lowercase();
        l.starts_with(&pre) || l.contains(&pre)
    });
    cands.sort_by(|a, b| {
        let ka = kind_rank(a.kind);
        let kb = kind_rank(b.kind);
        // Exact match (case-insensitive) beats everything within a kind:
        // typing `emp` should offer `EMP` before a well-worn `EMPLOYEE_AUDIT`.
        let ea = u8::from(!a.label.eq_ignore_ascii_case(&pre));
        let eb = u8::from(!b.label.eq_ignore_ascii_case(&pre));
        ka.cmp(&kb)
            .then_with(|| ea.cmp(&eb))
            .then_with(|| {
                a.own_schema_first(own_schema)
                    .cmp(&b.own_schema_first(own_schema))
            })
            .then_with(|| prefix_score(&a.label, &pre).cmp(&prefix_score(&b.label, &pre)))
            .then_with(|| b.usage.cmp(&a.usage))
            .then_with(|| a.label.len().cmp(&b.label.len()))
            .then_with(|| a.label.cmp(&b.label))
    });
    cands.truncate(limit);
    cands
}

fn kind_rank(k: CandidateKind) -> u8 {
    match k {
        CandidateKind::JoinCondition => 0,
        CandidateKind::ColumnInScope => 1,
        CandidateKind::Table => 2,
        CandidateKind::Column => 3,
        CandidateKind::Sequence => 4,
        CandidateKind::Function => 5,
        CandidateKind::Keyword => 6,
    }
}

/// 0 = prefix match, 1 = contains, 2 = no match (empty prefix = 0).
/// For qualified labels (`SCOTT.EMP`) the object part decides: typing `emp`
/// must prefer `SYSTEM.EMPLOYEES` over `SYS.MVIEW$_ADV_TEMP`.
fn prefix_score(label: &str, pre: &str) -> u8 {
    if pre.is_empty() {
        return 0;
    }
    // Don't split quoted labels (`"My.Table"` is one object).
    let short = if label.starts_with('"') && label.ends_with('"') && label.len() > 1 {
        label
    } else {
        label.rsplit('.').next().unwrap_or(label)
    };
    if short.to_lowercase().starts_with(pre) {
        0
    } else if label.to_lowercase().contains(pre) {
        1
    } else {
        2
    }
}
