//! Substitution variables (`&name`) and binds (`:name`).
//!
//! Part of the `sql` module (see `sql.rs`).

/// One SQL*Plus substitution variable reference (`&name` / `&&name`).
/// `double` is true when first seen (or ever seen) with `&&`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubVar {
    pub name: String,
    pub double: bool,
}

fn is_var_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b == b'#'
}

/// Find substitution variables (`&name`, `&&name`, `&1`) in first-occurrence
/// order, deduped. Scans string literals too (SQL*Plus substitutes inside
/// `'...'`), but skips `--` / `/* */` comments. `\&` is an escape (literal
/// `&`, no prompt). A single trailing `.` after the name is a separator
/// (`SET CONCAT`) and does not join the name.
pub fn find_substitution_vars(sql: &str) -> Vec<SubVar> {
    let bytes = sql.as_bytes();
    let mut out: Vec<SubVar> = Vec::new();
    let mut i = 0usize;
    // Scanner states; strings are still scanned for `&`, comments are not.
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    // Helper records one `&` reference starting at `i`; returns new offset.
    macro_rules! record_sub {
        ($i:expr) => {{
            let mut j = $i + 1;
            let mut double = false;
            if bytes.get(j) == Some(&b'&') {
                double = true;
                j += 1;
            }
            let start = j;
            while j < bytes.len() && is_var_char(bytes[j]) {
                j += 1;
            }
            if start == j {
                $i += 1;
            } else {
                let name = sql[start..j].to_string();
                if bytes.get(j) == Some(&b'.') {
                    j += 1;
                }
                match out.iter_mut().find(|v| v.name == name) {
                    Some(existing) => {
                        existing.double |= double;
                    }
                    None => out.push(SubVar { name, double }),
                }
                $i = j;
            }
        }};
    }
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    in_single = false;
                    i += 1;
                }
            } else if b == b'\\' && bytes.get(i + 1) == Some(&b'&') {
                i += 2;
            } else if b == b'&' {
                // SQL*Plus substitutes inside string literals too.
                record_sub!(i);
            } else {
                i += 1;
            }
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block_comment = true;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'&') => {
                i += 2; // escaped ampersand: literal, no prompt
            }
            b'&' => {
                record_sub!(i);
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Find native bind variables (`:name`, `:1`) in first-occurrence order,
/// deduped. Skips strings, quoted identifiers, and comments. Skips `:=`
/// (PL/SQL assignment), `::`, and the trigger pseudo-binds `:NEW`, `:OLD`,
/// `:PARENT` (case-insensitive).
pub fn find_bind_vars(sql: &str) -> Vec<String> {
    let bytes = sql.as_bytes();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                } else {
                    in_single = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }
        if in_double {
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                in_block_comment = true;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b':') => {
                i += 2; // escaped colon: literal
            }
            b':' => {
                let next = bytes.get(i + 1).copied().unwrap_or(0);
                if next == b'=' || next == b':' {
                    i += 2;
                    continue;
                }
                let start = i + 1;
                let mut j = start;
                if next.is_ascii_digit() {
                    while j < bytes.len() && bytes[j].is_ascii_digit() {
                        j += 1;
                    }
                } else {
                    while j < bytes.len() && is_var_char(bytes[j]) {
                        j += 1;
                    }
                }
                if start == j {
                    i += 1;
                    continue;
                }
                let name = sql[start..j].to_string();
                if name.eq_ignore_ascii_case("new")
                    || name.eq_ignore_ascii_case("old")
                    || name.eq_ignore_ascii_case("parent")
                {
                    i = j;
                    continue;
                }
                if !out.iter().any(|n| n == &name) {
                    out.push(name);
                }
                i = j;
            }
            _ => {
                i += 1;
            }
        }
    }
    out
}

/// Apply substitution values textually (non-recursive: values are never
/// rescanned). Missing names are left in place. `\&` unescapes to `&`.
/// Comments are passed through untouched; strings are substituted.
pub fn apply_substitutions(
    sql: &str,
    values: &std::collections::HashMap<String, String>,
) -> String {
    let bytes = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 32);
    let mut i = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_line_comment {
            out.push(b as char);
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }
        if in_block_comment {
            if b == b'*' && bytes.get(i + 1) == Some(&b'/') {
                out.push_str("*/");
                in_block_comment = false;
                i += 2;
            } else {
                out.push(b as char);
                i += 1;
            }
            continue;
        }
        if in_single {
            if b == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    out.push_str("''");
                    i += 2;
                } else {
                    out.push('\'');
                    in_single = false;
                    i += 1;
                }
            } else if b == b'\\' && bytes.get(i + 1) == Some(&b'&') {
                // Escaped inside strings too: value or literal?
                // `\&name` never prompts, so it never has a value; emit `&`.
                out.push('&');
                i += 2;
            } else if b == b'&' {
                let (name, j, dot) = read_sub_name(sql, bytes, i);
                match name {
                    Some(name) => match values.get(&name) {
                        Some(v) => {
                            out.push_str(v);
                            i = j + usize::from(dot);
                        }
                        None => {
                            out.push_str(if sql[i + 1..].starts_with('&') {
                                "&&"
                            } else {
                                "&"
                            });
                            out.push_str(&name);
                            if dot {
                                out.push('.');
                            }
                            i = j + usize::from(dot);
                        }
                    },
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            } else {
                out.push(b as char);
                i += 1;
            }
            continue;
        }
        if in_double {
            out.push(b as char);
            if b == b'"' {
                in_double = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'\'' => {
                out.push('\'');
                in_single = true;
                i += 1;
            }
            b'"' => {
                out.push('"');
                in_double = true;
                i += 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => {
                out.push_str("--");
                in_line_comment = true;
                i += 2;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                out.push_str("/*");
                in_block_comment = true;
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b'&') => {
                out.push('&');
                i += 2;
            }
            b'\\' if bytes.get(i + 1) == Some(&b':') => {
                out.push(':');
                i += 2;
            }
            b'&' => {
                let (name, j, dot) = read_sub_name(sql, bytes, i);
                match name {
                    Some(name) => match values.get(&name) {
                        Some(v) => {
                            out.push_str(v);
                            i = j + usize::from(dot);
                        }
                        None => {
                            out.push_str(if sql[i + 1..].starts_with('&') {
                                "&&"
                            } else {
                                "&"
                            });
                            out.push_str(&name);
                            if dot {
                                out.push('.');
                            }
                            i = j + usize::from(dot);
                        }
                    },
                    None => {
                        out.push('&');
                        i += 1;
                    }
                }
            }
            _ => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    out
}

/// Read `&` / `&&` name at byte offset `i` (which is `&`). Returns
/// (name, end-of-name offset, had-trailing-dot).
fn read_sub_name(sql: &str, bytes: &[u8], i: usize) -> (Option<String>, usize, bool) {
    let mut j = i + 1;
    if bytes.get(j) == Some(&b'&') {
        j += 1;
    }
    let start = j;
    while j < bytes.len() && is_var_char(bytes[j]) {
        j += 1;
    }
    if start == j {
        return (None, i + 1, false);
    }
    let mut dot = false;
    if bytes.get(j) == Some(&b'.') {
        dot = true;
    }
    (Some(sql[start..j].to_string()), j, dot)
}
