//! "Count rows": run `SELECT COUNT(*) FROM (<query>)` in the background on a
//! separate (throwaway) session and report the result in a popup.
//!
//! Part of the run pipeline (see `run.rs`). A separate session is used so the
//! grid's held cursor — and any rows still be fetched by scrolling — is never
//! disturbed.

use super::*;
use crate::session::throwaway_session;
use gpui_kit::component::WindowExt as _;
use gpui_kit_assets::IconName as KitIcon;

/// Insert thousands separators into an all-digit count string (Oracle returns
/// a bare integer). Non-digit input is returned unchanged.
fn format_count(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return trimmed.to_string();
    }
    let mut out = String::with_capacity(trimmed.len() + trimmed.len() / 3);
    for (i, ch) in trimmed.chars().enumerate() {
        if i > 0 && (trimmed.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

impl SqlHighlandView {
    /// Count the rows the tab's current query would return, on a throwaway
    /// session, and show the result in a popup. Only meaningful when the last
    /// statement produced a result set.
    pub(crate) fn start_count_rows(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let Some(ix) = self.tab_index(tab_id) else {
            return;
        };
        if self.tabs[ix].busy || self.tabs[ix].exporting {
            self.status = "Wait for the current operation to finish".into();
            cx.notify();
            return;
        }
        // The query that produced the grid, minus a trailing statement
        // terminator. `last_sql` still carries any `:bind` placeholders, which
        // the server will reject — that error is surfaced in the popup.
        let sql = self.tabs[ix]
            .last_sql
            .trim()
            .trim_end_matches(';')
            .trim()
            .to_string();
        if sql.is_empty()
            || statement_kind(&sql) != StatementKind::Query
            || is_describe_statement(&sql)
        {
            self.status = "Count rows needs a query result".into();
            cx.notify();
            return;
        }
        let conn_id = match self.tabs[ix].connection_id.clone() {
            Some(id) => id,
            None => {
                self.status = "Select a connection for this tab".into();
                cx.notify();
                return;
            }
        };
        let Some(mut cfg) = self.connections.iter().find(|c| c.id == conn_id).cloned() else {
            self.status = "Connection not found — pick another".into();
            cx.notify();
            return;
        };
        if let Some(pw) = self.effective_password(&cfg) {
            cfg.password = pw;
        }
        let count_sql = format!("SELECT COUNT(*) FROM ({sql})");
        self.status = "Counting rows…".into();
        cx.notify();

        let bg = cx.background_executor().clone();
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let outcome = bg
                .spawn(async move {
                    let session = throwaway_session(cfg.engine);
                    let mut guard = lock(&session);
                    let outcome = (|| -> Result<String, String> {
                        guard.connect(&cfg).map_err(|e| e.to_string())?;
                        let result = guard
                            .run_query(&count_sql, 1, &[])
                            .map_err(|e| e.to_string())?;
                        Ok(result
                            .rows
                            .first()
                            .and_then(|row| row.first())
                            .and_then(|cell| cell.clone())
                            .unwrap_or_default())
                    })();
                    // Always tear the throwaway session down (a no-op if the
                    // connect failed); closes the cursor before the socket.
                    guard.disconnect();
                    outcome
                })
                .await;
            view.update(cx, |this, cx| {
                this.status = "".into();
                this.note_dialog_open();
                let ok = outcome.is_ok();
                let message = match outcome {
                    Ok(value) => format!("{} rows", format_count(&value)),
                    Err(e) => e,
                };
                if let Some(handle) = cx.windows().into_iter().next() {
                    let _ = handle.update(cx, |_, window, cx| {
                        window.open_alert_dialog(cx, move |alert, _, _| {
                            let alert = alert
                                .title(if ok { "Row count" } else { "Count failed" })
                                .description(message.clone());
                            if ok {
                                alert
                            } else {
                                alert.icon(KitIcon::TriangleAlert)
                            }
                        });
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::format_count;

    #[test]
    fn formats_thousands() {
        assert_eq!(format_count("0"), "0");
        assert_eq!(format_count("999"), "999");
        assert_eq!(format_count("1000"), "1,000");
        assert_eq!(format_count("1234567"), "1,234,567");
        assert_eq!(format_count(" 42 "), "42");
    }

    #[test]
    fn non_numeric_passthrough() {
        assert_eq!(format_count(""), "");
        assert_eq!(format_count("NULL"), "NULL");
    }
}
