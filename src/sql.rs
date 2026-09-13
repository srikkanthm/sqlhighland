//! SQL text helpers: formatting and statement splitting.

/// Format a SQL string for display in the editor.
///
/// Uses `sqlformat` with uppercased keywords and 2-space indent.
pub fn format_sql(sql: &str) -> String {
    sqlformat::format(
        sql,
        &sqlformat::QueryParams::None,
        &sqlformat::FormatOptions {
            uppercase: Some(true),
            ..Default::default()
        },
    )
}

mod classify;
mod script;
mod split;
mod substitute;

// Re-exports preserve the public `crate::sql::…` API across the split.
pub use classify::*;
pub use script::*;
pub use split::*;
pub use substitute::*;

/// Skip whitespace, `--` line comments, and `/* */` block comments.
fn skip_ws_comments(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
        } else if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            break;
        }
    }
    i
}

/// Read `[A-Za-z_][A-Za-z0-9_$#]*` at `i`. Returns (keyword, next offset).
fn read_keyword(bytes: &[u8], i: usize) -> (String, usize) {
    let mut j = i;
    while j < bytes.len()
        && (bytes[j].is_ascii_alphanumeric()
            || bytes[j] == b'_'
            || bytes[j] == b'$'
            || bytes[j] == b'#')
    {
        j += 1;
    }
    (String::from_utf8_lossy(&bytes[i..j]).into_owned(), j)
}

/// If `i` starts a slash-terminator line (`/ `alone, optional surrounding
/// spaces/tabs), return the offset just past its newline (or end of input).
fn slash_line_end(bytes: &[u8], i: usize) -> Option<usize> {
    let mut p = i;
    while p < bytes.len() && (bytes[p] == b' ' || bytes[p] == b'\t') {
        p += 1;
    }
    if bytes.get(p) != Some(&b'/') {
        return None;
    }
    let mut q = p + 1;
    while q < bytes.len() && (bytes[q] == b' ' || bytes[q] == b'\t') {
        q += 1;
    }
    if q == bytes.len() {
        Some(q)
    } else if bytes[q] == b'\n' {
        Some(q + 1)
    } else {
        None
    }
}

/// Skip a `'...'`/`"..."` literal starting at the quote (handles `''`).
fn skip_quoted(bytes: &[u8], mut i: usize) -> usize {
    let quote = bytes[i];
    i += 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if bytes.get(i + 1) == Some(&quote) {
                i += 2;
                continue;
            }
            return i + 1;
        }
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests;
