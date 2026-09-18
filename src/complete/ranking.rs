//! Candidate keyword/function tables and ranking.
//!
//! Part of the completion engine (see `complete.rs`).

use super::*;

/// Statement starters for empty statement heads. Multi-word where Oracle
/// requires it (`INSERT INTO`, `DELETE FROM`, `MERGE INTO`).
pub const STMT_KEYWORDS: &[&str] = &[
    "SELECT",
    "WITH",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "MERGE INTO",
    "CREATE",
    "DROP",
    "ALTER",
    "TRUNCATE",
    "DESCRIBE",
    "EXPLAIN",
    "COMMIT",
    "ROLLBACK",
    "BEGIN",
    "DECLARE",
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
    "GROUP BY",
    "ORDER BY",
    "ORDER SIBLINGS BY",
    "HAVING",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "MINUS",
    "INTO",
    "FETCH FIRST",
    "FETCH NEXT",
    "OFFSET",
    "FOR UPDATE",
];

/// Clause-transition keywords valid after a predicate
/// (`WHERE x=1 ord|` must offer ORDER BY).
pub const PRED_FOLLOW: &[&str] = &[
    "GROUP BY",
    "ORDER BY",
    "ORDER SIBLINGS BY",
    "HAVING",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "MINUS",
    "RETURNING",
    "FETCH FIRST",
    "FETCH NEXT",
    "OFFSET",
    "FOR UPDATE",
];

/// Continuations valid right after a table reference in `FROM`/`DELETE FROM`
/// (`FROM t |`): the query/DML transitions, never statement starters or DDL.
/// Join keywords are the true Oracle combinations, not `LEFT` + `JOIN`.
pub const FROM_FOLLOW: &[&str] = &[
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "ORDER SIBLINGS BY",
    "HAVING",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
    "CROSS JOIN",
    "NATURAL JOIN",
    "NATURAL INNER JOIN",
    "NATURAL LEFT JOIN",
    "NATURAL RIGHT JOIN",
    "NATURAL FULL JOIN",
    "CONNECT BY",
    "START WITH",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "MINUS",
    "FETCH FIRST",
    "FETCH NEXT",
    "OFFSET",
    "FOR UPDATE",
];

/// `FROM_FOLLOW` plus the join-only keywords, after the right side of a
/// `JOIN` (`JOIN t |` → also `ON`/`USING`).
pub const JOIN_FOLLOW: &[&str] = &[
    "ON",
    "USING",
    "WHERE",
    "GROUP BY",
    "ORDER BY",
    "ORDER SIBLINGS BY",
    "HAVING",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
    "CROSS JOIN",
    "NATURAL JOIN",
    "NATURAL INNER JOIN",
    "NATURAL LEFT JOIN",
    "NATURAL RIGHT JOIN",
    "NATURAL FULL JOIN",
    "CONNECT BY",
    "START WITH",
    "UNION",
    "UNION ALL",
    "INTERSECT",
    "MINUS",
    "FETCH FIRST",
    "FETCH NEXT",
    "OFFSET",
    "FOR UPDATE",
];

/// After `UPDATE t |`: the assignment clause.
pub const UPDATE_FOLLOW: &[&str] = &["SET"];

/// After `INSERT INTO t |`: the row source.
pub const INTO_FOLLOW: &[&str] = &["VALUES", "SELECT"];

/// After `MERGE INTO t |`.
pub const MERGE_FOLLOW: &[&str] = &["USING"];

/// After `MERGE … USING t |`.
pub const USING_FOLLOW: &[&str] = &["ON"];

/// Oracle keywords worth completing. Multi-word clauses/joins are single
/// entries (the true Oracle combinations), so accepting one inserts the whole
/// phrase; matching stays case-insensitive.
pub const ORACLE_KEYWORDS: &[&str] = &[
    "SELECT",
    "FROM",
    "WHERE",
    "JOIN",
    "INNER JOIN",
    "LEFT JOIN",
    "LEFT OUTER JOIN",
    "RIGHT JOIN",
    "RIGHT OUTER JOIN",
    "FULL JOIN",
    "FULL OUTER JOIN",
    "CROSS JOIN",
    "NATURAL JOIN",
    "NATURAL INNER JOIN",
    "NATURAL LEFT JOIN",
    "NATURAL RIGHT JOIN",
    "NATURAL FULL JOIN",
    "ON",
    "USING",
    "GROUP BY",
    "ORDER BY",
    "ORDER SIBLINGS BY",
    "HAVING",
    "CONNECT BY",
    "START WITH",
    "UNION",
    "UNION ALL",
    "MINUS",
    "INTERSECT",
    "INSERT INTO",
    "INTO",
    "VALUES",
    "UPDATE",
    "SET",
    "DELETE FROM",
    "MERGE INTO",
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
    "PRIOR",
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
    "BEGIN",
    "DECLARE",
    "OFFSET",
    "FETCH FIRST",
    "FETCH NEXT",
    "FOR UPDATE",
    "LEVEL",
    "OVER",
    "PARTITION BY",
    "ROWS",
    "RANGE",
    "PRECEDING",
    "FOLLOWING",
    "UNBOUNDED",
    "CURRENT",
    "ROW",
    "PIVOT",
    "UNPIVOT",
    "MODEL",
    "KEEP",
    "RETURNING",
    "CONSTRAINT",
    "PRIMARY",
    "FOREIGN",
    "REFERENCES",
    "UNIQUE",
    "CHECK",
    "DEFAULT",
    "USER",
    "LATERAL",
    "CYCLE",
    "NOCYCLE",
    "CONNECT_BY_ROOT",
];

/// Built-in Oracle data types (for cast contexts and DDL). Kept separate from
/// keywords/functions; multi-word types carry their full spelling.
pub const ORACLE_DATA_TYPES: &[&str] = &[
    "BFILE",
    "BINARY_DOUBLE",
    "BINARY_FLOAT",
    "BLOB",
    "BOOLEAN",
    "CHAR",
    "CLOB",
    "DATE",
    "FLOAT",
    "INTERVAL DAY TO SECOND",
    "INTERVAL YEAR TO MONTH",
    "LONG",
    "LONG RAW",
    "NCHAR",
    "NCLOB",
    "NUMBER",
    "NVARCHAR2",
    "RAW",
    "ROWID",
    "TIMESTAMP",
    "TIMESTAMP WITH LOCAL TIME ZONE",
    "TIMESTAMP WITH TIME ZONE",
    "UROWID",
    "VARCHAR2",
    "XMLTYPE",
];

/// Built-in functions: `(NAME, signature shown in the detail pane)`.
/// Labels complete as `NAME()` — the kit has no snippet engine (`$1` would
/// insert literally) and no post-accept hook, so the cursor lands after the
/// closing paren (one Left-arrow into the args). True tab-stops need kit
/// support. Names here must not repeat in [`ORACLE_KEYWORDS`].
pub const ORACLE_FUNCTIONS: &[(&str, &str)] = &[
    ("ABS", "ABS(n)"),
    ("ADD_MONTHS", "ADD_MONTHS(date, n)"),
    ("ASCII", "ASCII(char)"),
    ("ASIN", "ASIN(n)"),
    ("ATAN", "ATAN(n)"),
    ("ATAN2", "ATAN2(n1, n2)"),
    ("AVG", "AVG([DISTINCT | ALL] expr)"),
    ("BITAND", "BITAND(expr1, expr2)"),
    ("CAST", "CAST(expr AS type)"),
    ("CEIL", "CEIL(n)"),
    ("CHR", "CHR(n)"),
    ("COALESCE", "COALESCE(expr, …)"),
    ("CONCAT", "CONCAT(str1, str2)"),
    ("COS", "COS(n)"),
    ("COUNT", "COUNT(* | [DISTINCT | ALL] expr)"),
    ("CUME_DIST", "CUME_DIST() OVER (…)"),
    ("DECODE", "DECODE(expr, search, result [, …] [, default])"),
    ("DENSE_RANK", "DENSE_RANK() OVER (…)"),
    ("EMPTY_BLOB", "EMPTY_BLOB()"),
    ("EMPTY_CLOB", "EMPTY_CLOB()"),
    ("EXP", "EXP(n)"),
    ("EXTRACT", "EXTRACT(field FROM src)"),
    ("FIRST_VALUE", "FIRST_VALUE(expr) OVER (…)"),
    ("FLOOR", "FLOOR(n)"),
    ("GREATEST", "GREATEST(expr [, expr] …)"),
    ("INITCAP", "INITCAP(char)"),
    ("INSTR", "INSTR(str, substr [, pos [, nth]])"),
    ("LAG", "LAG(expr [, offset [, default]]) OVER (…)"),
    ("LAST_DAY", "LAST_DAY(date)"),
    ("LAST_VALUE", "LAST_VALUE(expr) OVER (…)"),
    ("LEAD", "LEAD(expr [, offset [, default]]) OVER (…)"),
    ("LEAST", "LEAST(expr [, expr] …)"),
    ("LENGTH", "LENGTH(char)"),
    (
        "LISTAGG",
        "LISTAGG(expr [, delim]) WITHIN GROUP (ORDER BY …)",
    ),
    ("LN", "LN(n)"),
    ("LOG", "LOG(base, n)"),
    ("LOWER", "LOWER(char)"),
    ("LPAD", "LPAD(expr, n [, pad])"),
    ("LTRIM", "LTRIM(char [, set])"),
    ("MAX", "MAX([DISTINCT | ALL] expr)"),
    ("MEDIAN", "MEDIAN(expr)"),
    ("MIN", "MIN([DISTINCT | ALL] expr)"),
    ("MOD", "MOD(n, m)"),
    ("MONTHS_BETWEEN", "MONTHS_BETWEEN(d1, d2)"),
    ("NEXT_DAY", "NEXT_DAY(date, weekday)"),
    ("NTILE", "NTILE(n) OVER (…)"),
    ("NULLIF", "NULLIF(expr1, expr2)"),
    ("NVL", "NVL(expr1, expr2)"),
    ("NVL2", "NVL2(expr, v1, v2)"),
    (
        "PERCENTILE_CONT",
        "PERCENTILE_CONT(p) WITHIN GROUP (ORDER BY …)",
    ),
    (
        "PERCENTILE_DISC",
        "PERCENTILE_DISC(p) WITHIN GROUP (ORDER BY …)",
    ),
    ("PERCENT_RANK", "PERCENT_RANK() OVER (…)"),
    ("POWER", "POWER(n, m)"),
    ("RANK", "RANK() OVER (…)"),
    ("RATIO_TO_REPORT", "RATIO_TO_REPORT(expr) OVER (…)"),
    (
        "REGEXP_COUNT",
        "REGEXP_COUNT(src, pattern [, pos [, match]])",
    ),
    ("REGEXP_INSTR", "REGEXP_INSTR(src, pattern [, …])"),
    ("REGEXP_LIKE", "REGEXP_LIKE(src, pattern [, match])"),
    (
        "REGEXP_REPLACE",
        "REGEXP_REPLACE(src, pattern [, repl [, …]])",
    ),
    ("REGEXP_SUBSTR", "REGEXP_SUBSTR(src, pattern [, …])"),
    ("REPLACE", "REPLACE(char, search [, replacement])"),
    ("ROUND", "ROUND(n [, m])"),
    ("ROW_NUMBER", "ROW_NUMBER() OVER (…)"),
    ("RPAD", "RPAD(expr, n [, pad])"),
    ("RTRIM", "RTRIM(char [, set])"),
    ("SIGN", "SIGN(n)"),
    ("SIN", "SIN(n)"),
    ("SQRT", "SQRT(n)"),
    ("STDDEV", "STDDEV([DISTINCT | ALL] expr)"),
    ("SUBSTR", "SUBSTR(char, pos [, len])"),
    ("SUM", "SUM([DISTINCT | ALL] expr)"),
    ("TAN", "TAN(n)"),
    ("TO_BINARY_DOUBLE", "TO_BINARY_DOUBLE(expr)"),
    ("TO_BINARY_FLOAT", "TO_BINARY_FLOAT(expr)"),
    ("TO_CHAR", "TO_CHAR(n | date [, fmt [, nls]])"),
    ("TO_CLOB", "TO_CLOB(char | clob)"),
    ("TO_DATE", "TO_DATE(char [, fmt [, nls]])"),
    ("TO_LOB", "TO_LOB(long_column)"),
    ("TO_NUMBER", "TO_NUMBER(char [, fmt [, nls]])"),
    ("TO_TIMESTAMP", "TO_TIMESTAMP(char [, fmt [, nls]])"),
    ("TRANSLATE", "TRANSLATE(expr, from, to)"),
    ("TREAT", "TREAT(expr AS type)"),
    ("TRIM", "TRIM([LEAD|TRAIL|BOTH] [char] FROM src)"),
    ("TRUNC", "TRUNC(n [, m] | date [, fmt])"),
    ("UPPER", "UPPER(char)"),
    ("VARIANCE", "VARIANCE([DISTINCT | ALL] expr)"),
    ("VSIZE", "VSIZE(expr)"),
    ("WIDTH_BUCKET", "WIDTH_BUCKET(expr, min, max, buckets)"),
    ("XMLAGG", "XMLAGG(expr ORDER BY …)"),
    ("XMLFOREST", "XMLFOREST(expr AS name [, …])"),
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
            .then_with(|| a.depth.cmp(&b.depth))
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
