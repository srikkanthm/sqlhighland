//! Server-side (native) result ordering.
//!
//! Part of the `sql` module (see `sql.rs`). Clicking a grid column header
//! re-runs the query wrapped in an `ORDER BY` so Oracle does the sort, rather
//! than sorting the buffered page in memory. Pure and dependency-free.

/// Sort direction for a native `ORDER BY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

impl SortDir {
    pub fn sql(self) -> &'static str {
        match self {
            SortDir::Asc => "ASC",
            SortDir::Desc => "DESC",
        }
    }
}

/// One sorted column: its **select-list position** (1-based, matching the grid's
/// data columns) and direction. Position is used in the generated `ORDER BY`
/// because it is robust to duplicate column names and to expression columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortSpec {
    pub col: usize,
    pub dir: SortDir,
}

/// Wrap `base` so the server sorts by `spec.col`: `SELECT * FROM ( <base> )
/// ORDER BY <col> ASC|DESC`. Strips a trailing `;` / `/` script terminator.
///
/// Only valid for statements that can appear in an inline view; callers must
/// gate on [`is_sortable_sql`].
pub fn order_by(base: &str, spec: SortSpec) -> String {
    let core = base.trim();
    let core = core.strip_suffix(';').unwrap_or(core);
    let core = core.trim_end();
    let core = core.strip_suffix('/').unwrap_or(core);
    let core = core.trim_end();
    format!(
        "SELECT * FROM (\n{core}\n) ORDER BY {} {}",
        spec.col,
        spec.dir.sql()
    )
}

/// Whether a native `ORDER BY` wrap is valid for `sql`.
///
/// `FOR UPDATE` cannot appear in an inline view (ORA-02014), and `DESCRIBE` is
/// a SQL\*Plus client command the driver rewrites — neither can be wrapped.
pub fn is_sortable_sql(sql: &str) -> bool {
    !sql.to_ascii_lowercase().contains("for update") && !crate::db::is_describe_statement(sql)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_with_position_and_direction() {
        let sql = order_by(
            "SELECT a, b FROM t WHERE x = 1",
            SortSpec {
                col: 2,
                dir: SortDir::Desc,
            },
        );
        assert_eq!(
            sql,
            "SELECT * FROM (\nSELECT a, b FROM t WHERE x = 1\n) ORDER BY 2 DESC"
        );
    }

    #[test]
    fn strips_terminators() {
        let spec = SortSpec {
            col: 1,
            dir: SortDir::Asc,
        };
        assert_eq!(
            order_by("SELECT 1 FROM dual;", spec),
            "SELECT * FROM (\nSELECT 1 FROM dual\n) ORDER BY 1 ASC"
        );
        assert_eq!(
            order_by("SELECT 1 FROM dual\n/\n", spec),
            "SELECT * FROM (\nSELECT 1 FROM dual\n) ORDER BY 1 ASC"
        );
    }

    #[test]
    fn sortability_rules() {
        assert!(is_sortable_sql("SELECT * FROM emp"));
        assert!(!is_sortable_sql("SELECT * FROM emp FOR UPDATE"));
        assert!(!is_sortable_sql("select * from emp for update"));
        assert!(!is_sortable_sql("DESCRIBE emp"));
    }
}
