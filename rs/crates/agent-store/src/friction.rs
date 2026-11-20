use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Row, params};

use crate::commands::time_key;
use crate::gojson;
use crate::gotime::Time;
use crate::models::{AttachedCommand, Friction, FrictionFilter, FrictionVote};
use crate::sqlite::{Store, int, parse_time, query_all, query_one, text};

/// FRICTION_SCHEMA holds the friction box (agent friction): entries, and the
/// +1 votes on them. An attached command is stored as JSON.
pub(crate) const FRICTION_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS friction (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		uid TEXT NOT NULL UNIQUE,
		host TEXT NOT NULL,
		filed_by TEXT NOT NULL,
		parent TEXT NOT NULL,
		cwd TEXT NOT NULL,
		build TEXT NOT NULL,
		created_at TEXT NOT NULL,
		updated_at TEXT NOT NULL,
		kind TEXT NOT NULL,
		summary TEXT NOT NULL,
		detail TEXT NOT NULL,
		command TEXT NOT NULL,
		status TEXT NOT NULL,
		accepted_by TEXT NOT NULL DEFAULT '',
		accepted_at TEXT NOT NULL DEFAULT '',
		accept_note TEXT NOT NULL DEFAULT '',
		resolved_by TEXT NOT NULL DEFAULT '',
		resolved_at TEXT NOT NULL DEFAULT '',
		resolution TEXT NOT NULL DEFAULT '',
		duplicate_of INTEGER
	);

	CREATE TABLE IF NOT EXISTS friction_votes (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		friction_id INTEGER NOT NULL REFERENCES friction(id) ON DELETE CASCADE,
		uid TEXT NOT NULL UNIQUE,
		host TEXT NOT NULL,
		voter TEXT NOT NULL,
		parent TEXT NOT NULL,
		build TEXT NOT NULL,
		created_at TEXT NOT NULL,
		note TEXT NOT NULL,
		command TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_friction_votes_friction
		ON friction_votes(friction_id);
"#;

/// FRICTION_COLUMNS ends with the vote count: the entry's own votes, its
/// duplicates' votes, and one for each duplicate's filing.
const FRICTION_COLUMNS: &str =
    "id, uid, host, filed_by, parent, cwd, build, created_at, updated_at, kind,
	summary, detail, command, status, accepted_by, accepted_at, accept_note,
	resolved_by, resolved_at, resolution, COALESCE(duplicate_of, 0),
	(SELECT COUNT(*) FROM friction_votes v WHERE v.friction_id = friction.id
		OR v.friction_id IN (SELECT d.id FROM friction d WHERE d.duplicate_of = friction.id))
	+ (SELECT COUNT(*) FROM friction d WHERE d.duplicate_of = friction.id)";

fn scan_friction(row: &Row<'_>) -> rusqlite::Result<Friction> {
    Ok(Friction {
        id: int(row, 0)?,
        uid: text(row, 1)?,
        host: text(row, 2)?,
        filed_by: text(row, 3)?,
        parent: text(row, 4)?,
        cwd: text(row, 5)?,
        build: text(row, 6)?,
        created_at: parse_time(&text(row, 7)?),
        updated_at: parse_time(&text(row, 8)?),
        kind: text(row, 9)?,
        summary: text(row, 10)?,
        detail: text(row, 11)?,
        command: decode_command(&text(row, 12)?),
        status: text(row, 13)?,
        accepted_by: text(row, 14)?,
        accepted_at: parse_time(&text(row, 15)?),
        accept_note: text(row, 16)?,
        resolved_by: text(row, 17)?,
        resolved_at: parse_time(&text(row, 18)?),
        resolution: text(row, 19)?,
        duplicate_of: int(row, 20)?,
        votes: int(row, 21)?,
    })
}

/// encode_command stores an attached command as JSON, "" for none.
fn encode_command(c: &Option<AttachedCommand>) -> String {
    match c {
        None => String::new(),
        Some(c) => gojson::marshal(c).unwrap_or_default(),
    }
}

fn decode_command(s: &str) -> Option<AttachedCommand> {
    if s.is_empty() {
        return None;
    }
    // Go's Unmarshal of "null" leaves the command zero, not nil.
    serde_json::from_str::<Option<AttachedCommand>>(s)
        .ok()
        .map(Option::unwrap_or_default)
}

/// optional_time formats t for storage, "" for the zero time.
pub(crate) fn optional_time(t: &Time) -> String {
    if t.is_zero() {
        return String::new();
    }
    time_key(t)
}

impl Store {
    /// add_friction files a friction entry and sets f.id.
    pub fn add_friction(&self, f: &mut Friction) -> Result<()> {
        let id = self.insert(
            "filing friction",
            "
		INSERT INTO friction (uid, host, filed_by, parent, cwd, build, created_at, updated_at,
			kind, summary, detail, command, status)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
	",
            params![
                f.uid,
                f.host,
                f.filed_by,
                f.parent,
                f.cwd,
                f.build,
                time_key(&f.created_at),
                time_key(&f.updated_at),
                f.kind,
                f.summary,
                f.detail,
                encode_command(&f.command),
                f.status
            ],
        )?;
        f.id = id;
        Ok(())
    }

    /// get_friction returns friction entry id, or None if there is none.
    pub fn get_friction(&self, id: i64) -> Result<Option<Friction>> {
        query_one(
            &self.conn(),
            &format!("SELECT {FRICTION_COLUMNS} FROM friction WHERE id = ?"),
            [id],
            scan_friction,
        )
        .context("querying friction")
    }

    /// list_friction returns the friction entries f selects, most votes
    /// first, then newest.
    pub fn list_friction(&self, f: &FrictionFilter) -> Result<Vec<Friction>> {
        let mut clauses: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        if !f.statuses.is_empty() {
            clauses.push(format!(
                "status IN (?{})",
                ", ?".repeat(f.statuses.len() - 1)
            ));
            for st in &f.statuses {
                args.push(Value::Text(st.clone()));
            }
        }
        if !f.kind.is_empty() {
            clauses.push("kind = ?".into());
            args.push(Value::Text(f.kind.clone()));
        }
        if !f.filed_by.is_empty() {
            clauses.push("filed_by = ?".into());
            args.push(Value::Text(f.filed_by.clone()));
        }
        if f.duplicate_of != 0 {
            clauses.push("duplicate_of = ?".into());
            args.push(Value::Integer(f.duplicate_of));
        }
        let mut query = format!("SELECT {FRICTION_COLUMNS} AS votes FROM friction");
        if !clauses.is_empty() {
            query += " WHERE ";
            query += &clauses.join(" AND ");
        }
        query += " ORDER BY votes DESC, id DESC";
        query_all(
            &self.conn(),
            &query,
            rusqlite::params_from_iter(args),
            scan_friction,
        )
        .context("querying friction")
    }

    /// last_friction_id returns the newest friction entry's id, or 0 if there
    /// are none.
    pub fn last_friction_id(&self) -> Result<i64> {
        self.conn()
            .query_row("SELECT COALESCE(MAX(id), 0) FROM friction", [], |r| {
                int(r, 0)
            })
            .context("querying friction")
    }

    /// update_friction saves f's status and the accept and resolve fields.
    /// Marking it a duplicate also moves entries that were duplicates of f
    /// onto its original, so duplicates stay one level deep, and its links to
    /// other hosts' entries.
    pub fn update_friction(&self, f: &Friction) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction().context("beginning transaction")?;

        let dup = if f.duplicate_of != 0 {
            Some(f.duplicate_of)
        } else {
            None
        };
        tx.execute(
            "
		UPDATE friction SET status = ?, updated_at = ?, accepted_by = ?, accepted_at = ?, accept_note = ?,
			resolved_by = ?, resolved_at = ?, resolution = ?, duplicate_of = ?
		WHERE id = ?
	",
            params![
                f.status,
                time_key(&f.updated_at),
                f.accepted_by,
                optional_time(&f.accepted_at),
                f.accept_note,
                f.resolved_by,
                optional_time(&f.resolved_at),
                f.resolution,
                dup,
                f.id
            ],
        )
        .context("updating friction")?;
        if let Some(dup) = dup {
            tx.execute(
                "UPDATE friction SET duplicate_of = ? WHERE duplicate_of = ?",
                [dup, f.id],
            )
            .context("moving duplicates")?;
            // Its links to other hosts' entries go to the original too; one
            // the original already has is dropped.
            tx.execute(
                "UPDATE OR IGNORE friction_links SET friction_id = ? WHERE friction_id = ?",
                [dup, f.id],
            )
            .context("moving friction links")?;
            tx.execute("DELETE FROM friction_links WHERE friction_id = ?", [f.id])
                .context("moving friction links")?;
        }
        tx.commit().context("committing friction")?;
        Ok(())
    }

    /// add_friction_vote records a +1 and sets v.id.
    pub fn add_friction_vote(&self, v: &mut FrictionVote) -> Result<()> {
        let id = self.insert(
            "voting on friction",
            "
		INSERT INTO friction_votes (friction_id, uid, host, voter, parent, build, created_at, note, command)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
	",
            params![
                v.friction_id,
                v.uid,
                v.host,
                v.voter,
                v.parent,
                v.build,
                time_key(&v.created_at),
                v.note,
                encode_command(&v.command)
            ],
        )?;
        v.id = id;
        Ok(())
    }

    /// list_friction_votes returns the votes on a friction entry, oldest
    /// first.
    pub fn list_friction_votes(&self, friction_id: i64) -> Result<Vec<FrictionVote>> {
        query_all(
            &self.conn(),
            &format!("SELECT {FRICTION_VOTE_COLUMNS} FROM friction_votes WHERE friction_id = ? ORDER BY id ASC"),
            [friction_id],
            scan_friction_vote,
        )
        .context("querying friction votes")
    }
}

const FRICTION_VOTE_COLUMNS: &str =
    "id, friction_id, uid, host, voter, parent, build, created_at, note, command";

fn scan_friction_vote(row: &Row<'_>) -> rusqlite::Result<FrictionVote> {
    Ok(FrictionVote {
        id: int(row, 0)?,
        friction_id: int(row, 1)?,
        uid: text(row, 2)?,
        host: text(row, 3)?,
        voter: text(row, 4)?,
        parent: text(row, 5)?,
        build: text(row, 6)?,
        created_at: parse_time(&text(row, 7)?),
        note: text(row, 8)?,
        command: decode_command(&text(row, 9)?),
    })
}
