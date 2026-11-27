//! Changes a dream proposes and only a person may make (docs/DREAMING.md,
//! "Scope"): for now, to the skip list. A dream records one with `agent
//! dream propose`; it waits, whatever becomes of its dream, until a person
//! accepts it (and code makes the change) or rejects it, with `agent dream
//! proposals`.

use anyhow::{Context, Result, bail};
use rusqlite::{Row, params};

use crate::dream_workers::{json_list, opt_time};
use crate::gotime::Time;
use crate::jobs::has_table;
use crate::sqlite::{Store, int, parse_time, query_all, query_one, text};
use crate::tree::time_nanos;

/// A change to the skip list: `change` is `add NAME` or `remove NAME`.
pub const PROPOSAL_SKIP: &str = "skip";

/// Waiting for a person.
pub const PROPOSAL_PENDING: &str = "pending";
/// A person accepted it, and the change was made.
pub const PROPOSAL_ACCEPTED: &str = "accepted";
/// A person rejected it.
pub const PROPOSAL_REJECTED: &str = "rejected";

pub(crate) const DREAM_PROPOSALS_SCHEMA: &str = r#"
	-- What a dream proposes that only a person may do. kind: skip.
	-- citations: a JSON list. status: pending, accepted, rejected.
	CREATE TABLE IF NOT EXISTS dream_proposals (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		dream_id INTEGER NOT NULL,
		worker TEXT NOT NULL,
		kind TEXT NOT NULL,
		change TEXT NOT NULL,
		why TEXT NOT NULL,
		citations TEXT NOT NULL DEFAULT '[]',
		status TEXT NOT NULL,
		created_at TEXT NOT NULL,
		decided_by TEXT NOT NULL DEFAULT '',
		decided_at TEXT NOT NULL DEFAULT '',
		note TEXT NOT NULL DEFAULT ''
	);
	CREATE INDEX IF NOT EXISTS idx_dream_proposals_status ON dream_proposals(status);
"#;

/// One proposal.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct Proposal {
    pub id: i64,
    pub dream_id: i64,
    pub worker: String,
    pub kind: String,
    pub change: String,
    pub why: String,
    pub citations: Vec<String>,
    pub status: String,
    pub created_at: Time,
    pub decided_by: String,
    pub decided_at: Time,
    pub note: String,
}

const COLUMNS: &str = "id, dream_id, worker, kind, change, why, citations, status, created_at, decided_by, decided_at, note";

fn scan(row: &Row<'_>) -> rusqlite::Result<Proposal> {
    Ok(Proposal {
        id: int(row, 0)?,
        dream_id: int(row, 1)?,
        worker: text(row, 2)?,
        kind: text(row, 3)?,
        change: text(row, 4)?,
        why: text(row, 5)?,
        citations: json_list(&text(row, 6)?),
        status: text(row, 7)?,
        created_at: parse_time(&text(row, 8)?),
        decided_by: text(row, 9)?,
        decided_at: opt_time(text(row, 10)?),
        note: text(row, 11)?,
    })
}

impl Store {
    /// Records a proposal of open dream `dream_id`'s worker; its id. The
    /// same change pending already gets no second row: its id comes back.
    pub fn add_proposal(
        &self,
        dream_id: i64,
        worker: &str,
        kind: &str,
        change: &str,
        why: &str,
        citations: &[String],
    ) -> Result<(i64, bool)> {
        self.in_tx(|tx| {
            crate::dreams::require_open(tx, dream_id)?;
            let pending: Option<i64> = tx
                .query_row(
                    "SELECT id FROM dream_proposals WHERE kind = ? AND change = ? AND status = ?",
                    params![kind, change, PROPOSAL_PENDING],
                    |r| r.get(0),
                )
                .ok();
            if let Some(id) = pending {
                return Ok((id, false));
            }
            tx.execute(
                "INSERT INTO dream_proposals (dream_id, worker, kind, change, why, citations, status, created_at)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    dream_id,
                    worker,
                    kind,
                    change,
                    why,
                    serde_json::to_string(citations)?,
                    PROPOSAL_PENDING,
                    time_nanos(&Time::now())
                ],
            )
            .context("recording a proposal")?;
            Ok((tx.last_insert_rowid(), true))
        })
    }

    /// The proposals, newest first; only pending ones unless `all`.
    pub fn proposals(&self, all: bool) -> Result<Vec<Proposal>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_proposals")? {
            return Ok(Vec::new());
        }
        let q = if all {
            format!("SELECT {COLUMNS} FROM dream_proposals ORDER BY id DESC")
        } else {
            format!(
                "SELECT {COLUMNS} FROM dream_proposals WHERE status = 'pending' ORDER BY id DESC"
            )
        };
        query_all(&conn, &q, [], scan).context("reading proposals")
    }

    pub fn get_proposal(&self, id: i64) -> Result<Option<Proposal>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_proposals")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!("SELECT {COLUMNS} FROM dream_proposals WHERE id = ?"),
            [id],
            scan,
        )
        .context("reading a proposal")
    }

    /// Records a person's decision on pending proposal `id`.
    pub fn decide_proposal(&self, id: i64, status: &str, by: &str, note: &str) -> Result<()> {
        if status != PROPOSAL_ACCEPTED && status != PROPOSAL_REJECTED {
            bail!("a proposal is accepted or rejected, not {status:?}");
        }
        let n = self.exec(
            "deciding a proposal",
            "UPDATE dream_proposals SET status = ?, decided_by = ?, decided_at = ?, note = ?
			WHERE id = ? AND status = 'pending'",
            params![status, by, time_nanos(&Time::now()), note, id],
        )?;
        if n == 0 {
            bail!("proposal #{id} isn't pending (agent dream proposals --all shows it)");
        }
        Ok(())
    }
}
