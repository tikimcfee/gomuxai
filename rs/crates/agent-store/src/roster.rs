//! The roster's tables (docs/ROSTER.md, "What it holds"): which harness and
//! model this host can run, and the notes on how they did. Plain rows only;
//! the rules (task kinds, the session check, availability, pick) are in the
//! agent-roster crate.
use anyhow::{Context, Result};
use rusqlite::{Row, params};
use serde::{Deserialize, Serialize};

use crate::commands::time_key;
use crate::gotime::Time;
use crate::models::is_zero_i64;
use crate::sqlite::{Store, int, parse_time, query_all, query_one, text};

/// ROSTER_SCHEMA holds the roster: one entry per harness, model and effort,
/// and the notes on them, which are never edited. An entry's tasks are
/// stored comma-separated. The Rust build's own tables; the Go never reads
/// them.
pub(crate) const ROSTER_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS roster_entries (
		harness TEXT NOT NULL,
		model TEXT NOT NULL,
		effort TEXT NOT NULL DEFAULT '',
		good_at TEXT NOT NULL DEFAULT '',
		avoid TEXT NOT NULL DEFAULT '',
		tasks TEXT NOT NULL DEFAULT '',
		speed TEXT NOT NULL DEFAULT '',
		cost TEXT NOT NULL DEFAULT '',
		limits TEXT NOT NULL DEFAULT '',
		enabled INTEGER NOT NULL DEFAULT 1,
		set_by TEXT NOT NULL DEFAULT '',
		updated_at TEXT NOT NULL,
		PRIMARY KEY (harness, model, effort)
	);

	CREATE TABLE IF NOT EXISTS roster_notes (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		harness TEXT NOT NULL,
		model TEXT NOT NULL,
		effort TEXT NOT NULL DEFAULT '',
		task TEXT NOT NULL,
		verdict TEXT NOT NULL,
		text TEXT NOT NULL,
		why TEXT NOT NULL DEFAULT '',
		source TEXT NOT NULL,
		written_by TEXT NOT NULL,
		supersedes INTEGER NOT NULL DEFAULT 0,
		created_at TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_roster_notes_entry
		ON roster_notes(harness, model, effort);

	CREATE TABLE IF NOT EXISTS roster_picks (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		name TEXT NOT NULL,
		version INTEGER NOT NULL,
		argv TEXT NOT NULL,
		harness TEXT NOT NULL,
		model TEXT NOT NULL,
		effort TEXT NOT NULL DEFAULT '',
		host TEXT NOT NULL,
		task TEXT NOT NULL,
		why TEXT NOT NULL DEFAULT '',
		saved_by TEXT NOT NULL,
		saved_at TEXT NOT NULL,
		UNIQUE (name, version)
	);
"#;

/// RosterPick is one version of a saved pick: a startup command kept by
/// name (agent roster pick --save), with the entry it came from.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RosterPick {
    #[serde(skip)]
    pub id: i64,
    pub name: String,
    /// 1, 2, ...: a new version only when the command changed.
    pub version: i64,
    /// The command's words ("agent", "spawn", "NAME", ...); stored as JSON.
    pub argv: Vec<String>,
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// The host whose entry it is.
    pub host: String,
    /// The task kind it was picked for.
    pub task: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    pub saved_by: String,
    pub saved_at: Time,
}

/// RosterEntry is one harness and model (at an effort, if set) this host can
/// run, with what a person or agent said about it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RosterEntry {
    pub harness: String,
    /// What spawn --model takes; "" for the harness's default.
    #[serde(default)]
    pub model: String,
    /// low ... max, for harnesses that take one; "" for none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub good_at: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub avoid: String,
    /// The task kinds it suits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tasks: Vec<String>,
    /// fast, medium or slow; "" if unsaid.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub speed: String,
    /// low, medium or high; "" if unsaid.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cost: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limits: String,
    pub enabled: bool,
    /// The agent or person that last set it ("user" for a person).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub set_by: String,
    pub updated_at: Time,
}

/// RosterNote is one run's evaluation of an entry. Notes are never edited; a
/// later note can supersede one.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RosterNote {
    pub id: i64,
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// The task kind it was tried on.
    pub task: String,
    /// good, mixed or bad.
    pub verdict: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
    /// The session it came from, as harness:session-id.
    pub source: String,
    /// The agent or person that wrote it (dream-<id> for a dream).
    pub by: String,
    /// The note this one replaces; 0 for none.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub supersedes: i64,
    pub created_at: Time,
}

const ENTRY_COLUMNS: &str = "harness, model, effort, good_at, avoid, tasks, speed, cost, limits, enabled, set_by, updated_at";

fn scan_entry(row: &Row<'_>) -> rusqlite::Result<RosterEntry> {
    Ok(RosterEntry {
        harness: text(row, 0)?,
        model: text(row, 1)?,
        effort: text(row, 2)?,
        good_at: text(row, 3)?,
        avoid: text(row, 4)?,
        tasks: split_tasks(&text(row, 5)?),
        speed: text(row, 6)?,
        cost: text(row, 7)?,
        limits: text(row, 8)?,
        enabled: int(row, 9)? != 0,
        set_by: text(row, 10)?,
        updated_at: parse_time(&text(row, 11)?),
    })
}

fn split_tasks(s: &str) -> Vec<String> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(String::from)
        .collect()
}

const NOTE_COLUMNS: &str = "id, harness, model, effort, task, verdict, text, why, source, written_by, supersedes, created_at";

fn scan_note(row: &Row<'_>) -> rusqlite::Result<RosterNote> {
    Ok(RosterNote {
        id: int(row, 0)?,
        harness: text(row, 1)?,
        model: text(row, 2)?,
        effort: text(row, 3)?,
        task: text(row, 4)?,
        verdict: text(row, 5)?,
        text: text(row, 6)?,
        why: text(row, 7)?,
        source: text(row, 8)?,
        by: text(row, 9)?,
        supersedes: int(row, 10)?,
        created_at: parse_time(&text(row, 11)?),
    })
}

impl Store {
    /// put_roster_entry adds e, or replaces the entry with its harness, model
    /// and effort.
    pub fn put_roster_entry(&self, e: &RosterEntry) -> Result<()> {
        self.exec(
            "saving roster entry",
            &format!(
                "INSERT OR REPLACE INTO roster_entries ({ENTRY_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
            ),
            params![
                e.harness,
                e.model,
                e.effort,
                e.good_at,
                e.avoid,
                e.tasks.join(","),
                e.speed,
                e.cost,
                e.limits,
                e.enabled as i64,
                e.set_by,
                time_key(&e.updated_at)
            ],
        )?;
        Ok(())
    }

    /// get_roster_entry returns the entry for harness, model and effort, or
    /// None if there is none.
    pub fn get_roster_entry(
        &self,
        harness: &str,
        model: &str,
        effort: &str,
    ) -> Result<Option<RosterEntry>> {
        query_one(
            &self.conn(),
            &format!(
                "SELECT {ENTRY_COLUMNS} FROM roster_entries WHERE harness = ? AND model = ? AND effort = ?"
            ),
            [harness, model, effort],
            scan_entry,
        )
        .context("querying roster")
    }

    /// list_roster_entries returns every entry, by harness, model and effort.
    pub fn list_roster_entries(&self) -> Result<Vec<RosterEntry>> {
        query_all(
            &self.conn(),
            &format!("SELECT {ENTRY_COLUMNS} FROM roster_entries ORDER BY harness, model, effort"),
            [],
            scan_entry,
        )
        .context("querying roster")
    }

    /// remove_roster_entry deletes an entry (its notes stay), and says
    /// whether there was one.
    pub fn remove_roster_entry(&self, harness: &str, model: &str, effort: &str) -> Result<bool> {
        let n = self.exec(
            "removing roster entry",
            "DELETE FROM roster_entries WHERE harness = ? AND model = ? AND effort = ?",
            [harness, model, effort],
        )?;
        Ok(n > 0)
    }

    /// add_roster_note records a note and sets n.id.
    pub fn add_roster_note(&self, n: &mut RosterNote) -> Result<()> {
        let id = self.insert(
            "adding roster note",
            "
		INSERT INTO roster_notes (harness, model, effort, task, verdict, text, why, source,
			written_by, supersedes, created_at)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
	",
            params![
                n.harness,
                n.model,
                n.effort,
                n.task,
                n.verdict,
                n.text,
                n.why,
                n.source,
                n.by,
                n.supersedes,
                time_key(&n.created_at)
            ],
        )?;
        n.id = id;
        Ok(())
    }

    /// get_roster_note returns note id, or None if there is none.
    pub fn get_roster_note(&self, id: i64) -> Result<Option<RosterNote>> {
        query_one(
            &self.conn(),
            &format!("SELECT {NOTE_COLUMNS} FROM roster_notes WHERE id = ?"),
            [id],
            scan_note,
        )
        .context("querying roster notes")
    }

    /// list_roster_notes returns every note, oldest first.
    pub fn list_roster_notes(&self) -> Result<Vec<RosterNote>> {
        query_all(
            &self.conn(),
            &format!("SELECT {NOTE_COLUMNS} FROM roster_notes ORDER BY id ASC"),
            [],
            scan_note,
        )
        .context("querying roster notes")
    }

    /// list_roster_notes_for returns the notes on one harness, model and
    /// effort, oldest first.
    pub fn list_roster_notes_for(
        &self,
        harness: &str,
        model: &str,
        effort: &str,
    ) -> Result<Vec<RosterNote>> {
        query_all(
            &self.conn(),
            &format!(
                "SELECT {NOTE_COLUMNS} FROM roster_notes WHERE harness = ? AND model = ? AND effort = ? ORDER BY id ASC"
            ),
            [harness, model, effort],
            scan_note,
        )
        .context("querying roster notes")
    }
}

const PICK_COLUMNS: &str =
    "id, name, version, argv, harness, model, effort, host, task, why, saved_by, saved_at";

fn scan_pick(row: &Row<'_>) -> rusqlite::Result<RosterPick> {
    Ok(RosterPick {
        id: int(row, 0)?,
        name: text(row, 1)?,
        version: int(row, 2)?,
        argv: serde_json::from_str(&text(row, 3)?).unwrap_or_default(),
        harness: text(row, 4)?,
        model: text(row, 5)?,
        effort: text(row, 6)?,
        host: text(row, 7)?,
        task: text(row, 8)?,
        why: text(row, 9)?,
        saved_by: text(row, 10)?,
        saved_at: parse_time(&text(row, 11)?),
    })
}

impl Store {
    /// add_roster_pick records a saved pick's version and sets p.id. The
    /// name and version must be new.
    pub fn add_roster_pick(&self, p: &mut RosterPick) -> Result<()> {
        let argv = serde_json::to_string(&p.argv).context("encoding the command")?;
        let id = self.insert(
            "saving pick",
            &format!(
                "INSERT INTO roster_picks ({}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &PICK_COLUMNS["id, ".len()..]
            ),
            params![
                p.name,
                p.version,
                argv,
                p.harness,
                p.model,
                p.effort,
                p.host,
                p.task,
                p.why,
                p.saved_by,
                time_key(&p.saved_at)
            ],
        )?;
        p.id = id;
        Ok(())
    }

    /// get_roster_pick returns version of the saved pick name, or its
    /// newest version for 0; None if there is none.
    pub fn get_roster_pick(&self, name: &str, version: i64) -> Result<Option<RosterPick>> {
        let conn = self.conn();
        if version == 0 {
            return query_one(
                &conn,
                &format!(
                    "SELECT {PICK_COLUMNS} FROM roster_picks WHERE name = ? ORDER BY version DESC LIMIT 1"
                ),
                [name],
                scan_pick,
            )
            .context("querying saved picks");
        }
        query_one(
            &conn,
            &format!("SELECT {PICK_COLUMNS} FROM roster_picks WHERE name = ? AND version = ?"),
            params![name, version],
            scan_pick,
        )
        .context("querying saved picks")
    }

    /// list_roster_picks returns every version of the saved pick name,
    /// newest first; for "", the newest version of each, by name.
    pub fn list_roster_picks(&self, name: &str) -> Result<Vec<RosterPick>> {
        let conn = self.conn();
        if name.is_empty() {
            return query_all(
                &conn,
                &format!(
                    "SELECT {PICK_COLUMNS} FROM roster_picks p
                     WHERE version = (SELECT MAX(version) FROM roster_picks q WHERE q.name = p.name)
                     ORDER BY name"
                ),
                [],
                scan_pick,
            )
            .context("querying saved picks");
        }
        query_all(
            &conn,
            &format!(
                "SELECT {PICK_COLUMNS} FROM roster_picks WHERE name = ? ORDER BY version DESC"
            ),
            [name],
            scan_pick,
        )
        .context("querying saved picks")
    }
}
