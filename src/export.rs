//! Export result sets to CSV / XLSX.
//!
//! Pure builders over display values (same strings the grid shows):
//! SQL NULL renders as empty in both formats. CSV streams line by line
//! (uncapped exports stay lean); XLSX uses a constant-memory worksheet so
//! large exports don't balloon either.

use crate::model::csv_row;

/// CSV header line from column names.
pub fn csv_header_line(headers: &[String]) -> String {
    csv_row(headers.iter().map(|h| Some(h.as_str())))
}

/// One CSV data line. `None` is SQL NULL (empty field).
pub fn csv_line(cells: &[Option<String>]) -> String {
    csv_row(cells.iter().map(|c| c.as_deref()))
}

/// Whole CSV document (convenience for tests and small results).
pub fn csv_doc(headers: &[String], rows: &[Vec<Option<String>>]) -> String {
    let mut out = String::new();
    out.push_str(&csv_header_line(headers));
    out.push('\n');
    for row in rows {
        out.push_str(&csv_line(row));
        out.push('\n');
    }
    out
}

/// Excel sheet name from a tab name: at most 31 chars, none of
/// `[]:*?/\` (replaced with `-`), non-empty.
pub fn sheet_name(tab_name: &str) -> String {
    let mut s: String = tab_name
        .chars()
        .map(|c| match c {
            '[' | ']' | ':' | '*' | '?' | '/' | '\\' => '-',
            c => c,
        })
        .collect();
    while s.chars().count() > 31 {
        s.pop();
    }
    if s.trim().is_empty() {
        s = "results".to_string();
    }
    s
}

/// Incremental XLSX builder over constant-memory worksheets: rows stream
/// in, memory stays flat. The first sheet holds the data (bold header, every
/// cell a string — display values, no type inference in v1); a second sheet
/// named `query` holds the exported SQL, one line per row, as the audit
/// trail. Sheets are written strictly in order (query sheet fully first),
/// which constant-memory mode requires.
pub struct XlsxBuilder {
    workbook: rust_xlsxwriter::Workbook,
    next_row: u32,
}

impl XlsxBuilder {
    pub fn new(sheet: &str, headers: &[String], query_sql: &str) -> Result<Self, String> {
        let mut workbook = rust_xlsxwriter::Workbook::new();
        let header_fmt = rust_xlsxwriter::Format::new().set_bold();
        let worksheet = workbook.add_worksheet_with_constant_memory();
        worksheet
            .set_name(sheet)
            .map_err(|e| e.to_string())?;
        for (col, h) in headers.iter().enumerate() {
            worksheet
                .write_string_with_format(0, col as u16, h, &header_fmt)
                .map_err(|e| e.to_string())?;
        }
        let query_sheet = workbook.add_worksheet_with_constant_memory();
        query_sheet
            .set_name("query")
            .map_err(|e| e.to_string())?;
        for (row, line) in query_sql.lines().enumerate() {
            query_sheet
                .write_string(row as u32, 0, line)
                .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            workbook,
            next_row: 1,
        })
    }

    pub fn push_row(&mut self, cells: &[Option<String>]) -> Result<(), String> {
        let worksheet = self.workbook.worksheets_mut().first_mut().ok_or_else(|| {
            "xlsx export lost its worksheet".to_string()
        })?;
        for (col, cell) in cells.iter().enumerate() {
            worksheet
                .write_string(self.next_row, col as u16, cell.as_deref().unwrap_or(""))
                .map_err(|e| e.to_string())?;
        }
        self.next_row += 1;
        Ok(())
    }

    pub fn row_count(&self) -> u64 {
        (self.next_row as u64).saturating_sub(1)
    }

    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        self.workbook.save_to_buffer().map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send<T: Send>() {}

    #[test]
    fn xlsx_builder_is_send_for_background_tasks() {
        // The drain loop owns the builder on the background executor.
        assert_send::<XlsxBuilder>();
    }

    #[test]
    fn csv_doc_has_header_and_nulls() {
        let out = csv_doc(
            &["A".to_string(), "B,C".to_string()],
            &[
                vec![Some("1".to_string()), None],
                vec![Some("x\"y".to_string()), Some("z".to_string())],
            ],
        );
        assert_eq!(out, "A,\"B,C\"\n1,\n\"x\"\"y\",z\n");
    }

    #[test]
    fn sheet_name_sanitizes() {
        assert_eq!(sheet_name("Untitled 1"), "Untitled 1");
        assert_eq!(sheet_name("a/b:c*d?e[f]g\\h"), "a-b-c-d-e-f-g-h");
        assert_eq!(sheet_name(&"x".repeat(40)).len(), 31);
        assert_eq!(sheet_name("   "), "results");
        assert_eq!(sheet_name(""), "results");
    }

    #[test]
    fn xlsx_builds_valid_zip() {
        let mut b =
            XlsxBuilder::new("results", &["A".to_string(), "B".to_string()], "SELECT 1\nFROM dual")
                .unwrap();
        b.push_row(&[Some("1".to_string()), None]).unwrap();
        b.push_row(&[None, Some("hi".to_string())]).unwrap();
        assert_eq!(b.row_count(), 2);
        let bytes = b.finish().unwrap();
        // ZIP magic: PK\x03\x04.
        assert!(bytes.len() > 100, "non-trivial file");
        assert_eq!(&bytes[..4], &[0x50, 0x4B, 0x03, 0x04]);
    }

    #[test]
    fn xlsx_rejects_bad_sheet_name_but_builder_sanitizes() {
        // Our sheet_name() output must always be accepted.
        for name in ["query", &"x".repeat(31), "a-b"] {
            let s = sheet_name(name);
            assert!(
                XlsxBuilder::new(&s, &["A".to_string()], "SELECT 1").is_ok(),
                "{s}"
            );
        }
    }
}
