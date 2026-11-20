use std::collections::HashMap;

use anyhow::{Context, Result};
use rusqlite::params;

use crate::gotime::Time;
use crate::sqlite::{Store, int, parse_time, query_all, text};
use crate::tree::time_nanos;

/// SESSION_REPORTS_SCHEMA keeps the last hook report of every session the
/// global hook heard from, keyed by the harness's session id (a Claude
/// subagent's is agent-<agent id>, as agent sessions names it). Workgroup
/// agents' reports also go to their own state; these serve every session,
/// including the ones a person started, so agent sessions can say how one
/// ended.
pub(crate) const SESSION_REPORTS_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS session_reports (
		session_id TEXT PRIMARY KEY,
		harness TEXT NOT NULL,
		status TEXT NOT NULL,
		end_reason TEXT NOT NULL DEFAULT '',
		event TEXT NOT NULL DEFAULT '',
		detail TEXT NOT NULL DEFAULT '',
		transcript TEXT NOT NULL DEFAULT '',
		at TEXT NOT NULL
	);
"#;

/// SessionReport is a session's last hook report.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionReport {
    pub session_id: String,
    pub harness: String,
    pub status: String,
    pub end_reason: String,
    pub event: String,
    pub detail: String,
    /// "" leaves the stored one
    pub transcript: String,
    pub at: Time,
}

impl Store {
    /// record_session_report stores r as its session's last report, unless
    /// the stored one is newer (hooks run as separate processes, so two can
    /// finish out of order).
    pub fn record_session_report(&self, r: &SessionReport) -> Result<()> {
        self.exec(
            "recording a session report",
            "
		INSERT INTO session_reports (session_id, harness, status, end_reason, event, detail, transcript, at)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?)
		ON CONFLICT (session_id) DO UPDATE SET
			harness = excluded.harness, status = excluded.status, end_reason = excluded.end_reason,
			event = excluded.event, detail = excluded.detail, at = excluded.at,
			transcript = CASE WHEN excluded.transcript = '' THEN session_reports.transcript ELSE excluded.transcript END
		WHERE excluded.at >= session_reports.at",
            params![
                r.session_id,
                r.harness,
                r.status,
                r.end_reason,
                r.event,
                r.detail,
                r.transcript,
                time_nanos(&r.at)
            ],
        )?;
        Ok(())
    }

    /// session_reports returns every stored session report, by session id. A
    /// store from before session reports has none.
    pub fn session_reports(&self) -> Result<HashMap<String, SessionReport>> {
        let conn = self.conn();
        if self.schema.is_some() {
            let n = conn.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'session_reports'",
                [],
                |r| int(r, 0),
            )?;
            if n == 0 {
                return Ok(HashMap::new());
            }
        }
        let rows = query_all(
            &conn,
            "SELECT session_id, harness, status, end_reason, event, detail, transcript, at FROM session_reports",
            [],
            |row| {
                Ok(SessionReport {
                    session_id: text(row, 0)?,
                    harness: text(row, 1)?,
                    status: text(row, 2)?,
                    end_reason: text(row, 3)?,
                    event: text(row, 4)?,
                    detail: text(row, 5)?,
                    transcript: text(row, 6)?,
                    at: parse_time(&text(row, 7)?),
                })
            },
        )
        .context("reading session reports")?;
        Ok(rows
            .into_iter()
            .map(|r| (r.session_id.clone(), r))
            .collect())
    }
}
