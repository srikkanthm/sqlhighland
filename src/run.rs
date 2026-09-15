//! Query/script execution: run entry gates, sequential script runner,
//! background executors, export drain UI, cancellation.
//!
//! Extracted from `app.rs` (refactor Phase 2); behavior unchanged.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use gpui::{Context, Window};

use crate::app::{
    describe_fetch, to_shared, ExportFormat, FetchState, Output, ResultData, RunKind, ScriptResume,
    SqlHighlandView,
};
use crate::bind_dialog::PendingBind;
use crate::config::Preferences;
use crate::conn_picker::{PendingPick, PickAfter};
use crate::db::{is_describe_statement, BindParam, FetchPage, SharedSession, FETCH_CHUNK};
use crate::export::{csv_header_line_with, csv_line_with, sheet_name, XlsxBuilder};
use crate::model::ColumnInfo;
use crate::session::lock;
use crate::sql::{
    apply_substitutions, exec_summary, expand_at_directives, expand_script_file, find_bind_vars,
    find_substitution_vars, is_dml, parse_at_directive, split_statements, statement_kind, txn_end,
    StatementKind, SubVar,
};

mod count;
mod export;
mod query;
mod script;

// Preserve the `crate::run::file_stem` path used by `app.rs`.
pub(crate) use export::file_stem;
