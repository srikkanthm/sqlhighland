//! `@` / `@@` / `START` directives and include expansion.
//!
//! Part of the `sql` module (see `sql.rs`).

/// An `@` / `@@` / `START` script directive parsed from a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtDirective {
    /// True for `@@` (resolve against the including file's directory).
    pub double_at: bool,
    /// Raw path token as typed (quotes stripped).
    pub path: String,
}

/// Parse one line as a script directive. Matches SQL*Plus/SQL Developer:
/// `@path`, `@@path`, or `START path`, with the path optionally
/// single/double-quoted (for spaces). A trailing `;` on a bare token is
/// stripped. Returns `None` for ordinary SQL lines.
pub fn parse_at_directive(line: &str) -> Option<AtDirective> {
    let t = line.trim_start();
    // `@@` must precede `@`.
    let (double_at, rest) = if let Some(r) = t.strip_prefix("@@") {
        (true, r)
    } else if let Some(r) = t.strip_prefix('@') {
        (false, r)
    } else {
        // `START path` alias (case-insensitive, whitespace-separated).
        // `START` alone is not a directive.
        let first: String = t.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
        if !first.eq_ignore_ascii_case("start") {
            return None;
        }
        let rest = &t[first.len()..];
        // Require whitespace after START (`STARTUP` is not a directive).
        if !rest.starts_with(|c: char| c.is_whitespace()) || rest.trim().is_empty() {
            return None;
        }
        (false, rest)
    };
    let rest = rest.trim_start();
    if rest.is_empty() {
        return None;
    }
    let path = if let Some(q) = rest.strip_prefix('"') {
        q.split('"').next().unwrap_or("").to_string()
    } else if let Some(q) = rest.strip_prefix('\'') {
        q.split('\'').next().unwrap_or("").to_string()
    } else {
        // Bare token: up to whitespace or `;`. Extra trailing args
        // (`@seed.sql arg1`) are ignored for v1.
        rest.split(|c: char| c.is_whitespace() || c == ';')
            .next()
            .unwrap_or("")
            .to_string()
    };
    if path.is_empty() {
        return None;
    }
    Some(AtDirective { double_at, path })
}

/// The editor line containing byte `offset` (clamped). Used to spot an
/// `@`-directive under the caret without involving statement splitting
/// (a bare `@file` line has no terminator, so the splitter would merge
/// it with whatever follows).
pub fn line_at(text: &str, offset: usize) -> String {
    let mut offset = offset.min(text.len());
    // Clamp to a char boundary (cursor offsets are byte offsets).
    while offset > 0 && !text.is_char_boundary(offset) {
        offset -= 1;
    }
    // Past-the-end belongs to the last line.
    if offset == text.len() && offset > 0 {
        offset -= 1;
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
    }
    // A caret sitting exactly on a newline belongs to the line it ends.
    if offset > 0 && text.as_bytes().get(offset) == Some(&b'\n') {
        offset -= 1;
        while offset > 0 && !text.is_char_boundary(offset) {
            offset -= 1;
        }
    }
    let start = text[..offset].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[offset..]
        .find('\n')
        .map(|i| offset + i)
        .unwrap_or(text.len());
    text.get(start..end).unwrap_or("").to_string()
}

/// Expanded script: full text with every directive spliced in, plus the
/// files involved (entry first) for summaries and cycle reports.
#[derive(Debug, Clone)]
pub struct ExpandedScript {
    pub text: String,
    pub files: Vec<std::path::PathBuf>,
}

/// Max `@` nesting depth (deploy → schema → …). Past this, report an
/// error rather than recursing forever.
pub const MAX_SCRIPT_DEPTH: usize = 10;

/// Expand `@` / `@@` / `START` directive lines in `entry_text`, reading
/// files relative to `base_dir` (the tab file's dir when file-backed,
/// else the process working directory). `@@` resolves against the
/// *including* file's directory, so nested trees work from anywhere;
/// `@`/`START` resolve against `base_dir` at the top level and against
/// the includer's directory below it (SQL*Plus parity for nesting).
/// Absolute paths pass through verbatim; `~` expands to `$HOME`; a
/// missing extension tries `+.sql` (SQL Developer parity); quoted paths
/// carry spaces. Only full-line directives expand (leading whitespace
/// allowed) — an `@` mid-statement or inside `--` / `/* */` comments is
/// left alone. Errors name the offending file.
pub fn expand_at_directives(
    entry_text: &str,
    base_dir: &std::path::Path,
) -> Result<ExpandedScript, String> {
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    let mut files: Vec<std::path::PathBuf> = Vec::new();
    let text = expand_text(entry_text, base_dir, &mut stack, &mut files, 0)?;
    Ok(ExpandedScript { text, files })
}

/// Expand starting from a raw directive path (as typed on an `@` line):
/// resolve against `base_dir` (absolute verbatim, `~` → `$HOME`), try
/// `+.sql` when extensionless (SQL Developer parity), read, then expand
/// nested directives against the entry file's own directory. The entry
/// file leads `files`, so summaries can name it.
pub fn expand_script_file(raw: &str, base_dir: &std::path::Path) -> Result<ExpandedScript, String> {
    let resolved = resolve_script_path(raw, base_dir);
    let candidate = with_sql_extension_fallback(&resolved);
    let content = std::fs::read_to_string(&candidate)
        .map_err(|_| format!("Cannot open script file: {}", candidate.display()))?;
    let canon = candidate
        .canonicalize()
        .unwrap_or_else(|_| candidate.clone());
    let mut stack = vec![canon];
    let mut files = vec![candidate.clone()];
    let child_dir: std::path::PathBuf = candidate
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| base_dir.to_path_buf());
    let text = expand_text(&content, &child_dir, &mut stack, &mut files, 0)?;
    Ok(ExpandedScript { text, files })
}

fn expand_text(
    text: &str,
    base_dir: &std::path::Path,
    stack: &mut Vec<std::path::PathBuf>,
    files: &mut Vec<std::path::PathBuf>,
    depth: usize,
) -> Result<String, String> {
    if depth > MAX_SCRIPT_DEPTH {
        return Err(format!(
            "Script nesting too deep (>{MAX_SCRIPT_DEPTH} levels) — cyclic includes?"
        ));
    }
    let mut out = String::with_capacity(text.len() + 64);
    let mut in_block_comment = false;
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        // Track `/* */` across lines so directives inside block comments
        // are never expanded. (Line comments and strings are handled by
        // only matching directives at line start — see below.)
        let mut scan = body;
        if in_block_comment {
            if let Some(end) = scan.find("*/") {
                scan = &scan[end + 2..];
                in_block_comment = false;
            } else {
                out.push_str(line);
                continue;
            }
        }
        // A `/*` before any directive token opens a comment; a directive
        // before any `/*` is real. (`--` to end-of-line likewise kills it.)
        let code = strip_line_comment(scan);
        let (directive_part, opens_block) = match code.find("/*") {
            Some(i) => (&code[..i], true),
            None => (code, false),
        };
        if opens_block {
            in_block_comment = true;
        }
        if let Some(d) = parse_at_directive(directive_part) {
            // Resolve: absolute verbatim; `@@` against the includer's dir
            // (== base_dir below the top level); `@` against base_dir.
            // (Both coincide below the top level — SQL*Plus parity.)
            let resolved = resolve_script_path(&d.path, base_dir);
            let candidate = with_sql_extension_fallback(&resolved);
            let content = std::fs::read_to_string(&candidate)
                .map_err(|_| format!("Cannot open script file: {}", candidate.display()))?;
            // Cycle check on the canonical path when available.
            let canon = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            if stack.contains(&canon) {
                return Err(format!(
                    "Cyclic script include: {} includes itself",
                    canon.display()
                ));
            }
            files.push(candidate.clone());
            stack.push(canon);
            let child_dir: std::path::PathBuf = candidate
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| base_dir.to_path_buf());
            let expanded = expand_text(&content, &child_dir, stack, files, depth + 1)?;
            stack.pop();
            out.push_str(&expanded);
            if !expanded.ends_with('\n') {
                out.push('\n');
            }
        } else {
            out.push_str(line);
            let _ = newline;
        }
    }
    Ok(out)
}

/// Strip a `--` line comment (outside quotes is overkill here: the caller
/// only feeds the pre-`/*` fragment, and a `--` inside a quoted `@` path
/// is vanishingly rare; a trailing comment after a directive is common).
fn strip_line_comment(s: &str) -> &str {
    match s.find("--") {
        Some(i) => &s[..i],
        None => s,
    }
}

/// Resolve a raw directive path: `~` → `$HOME`, absolute verbatim,
/// relative against `base_dir`.
fn resolve_script_path(raw: &str, base_dir: &std::path::Path) -> std::path::PathBuf {
    let expanded = if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        match crate::fsutil::home_dir() {
            Some(home) => home.join(rest),
            None => std::path::PathBuf::from(raw),
        }
    } else if raw == "~" {
        match crate::fsutil::home_dir() {
            Some(home) => home,
            None => std::path::PathBuf::from(raw),
        }
    } else {
        std::path::PathBuf::from(raw)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        base_dir.join(expanded)
    }
}

/// SQL Developer parity: `@clean_tables` finds `clean_tables.sql`.
/// Tries the path as-is first, then with `.sql` appended.
fn with_sql_extension_fallback(p: &std::path::Path) -> std::path::PathBuf {
    if p.exists() {
        return p.to_path_buf();
    }
    if p.extension().is_none() {
        let with_ext = p.with_extension("sql");
        if with_ext.exists() {
            return with_ext;
        }
    }
    p.to_path_buf()
}
