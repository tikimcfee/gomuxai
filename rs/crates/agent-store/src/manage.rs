//! What `agent manage` keeps and does in the store: the sessions a person
//! stopped tracking, a cache of each transcript's token usage, backups, and
//! pruning of old records.
//!
//! None of it is a second record of anything: an untracked session is a
//! choice only a person can make, and the usage cache is rebuilt from the
//! transcripts whenever it is missing or behind.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::params;

use crate::gotime::Time;
use crate::sqlite::{Store, int, parse_time, query_all, text};
use crate::tree::time_nanos;

pub(crate) const MANAGE_SCHEMA: &str = r#"
	-- Sessions a person asked agent to stop tracking: agent manage leaves
	-- them out unless asked. The transcript itself is untouched.
	CREATE TABLE IF NOT EXISTS untracked_sessions (
		harness TEXT NOT NULL,
		session_id TEXT NOT NULL,
		path TEXT NOT NULL DEFAULT '',
		title TEXT NOT NULL DEFAULT '',
		by TEXT NOT NULL DEFAULT '',
		at TEXT NOT NULL,
		PRIMARY KEY (harness, session_id)
	);

	-- Each transcript's token usage as last counted, with where the count
	-- stopped (agent_transcript::usage::Scan, as JSON). A cache: deleting a
	-- row only costs reading that transcript again.
	CREATE TABLE IF NOT EXISTS session_usage (
		path TEXT PRIMARY KEY,
		size INTEGER NOT NULL,
		scan TEXT NOT NULL,
		at TEXT NOT NULL
	);
"#;

/// A session a person stopped tracking.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Untracked {
    pub harness: String,
    pub session_id: String,
    pub path: String,
    pub title: String,
    pub by: String,
    pub at: Time,
}

/// A transcript's cached usage scan.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageRow {
    pub path: String,
    /// The transcript's size when it was counted.
    pub size: i64,
    pub scan: String,
}

/// What [`Store::prune`] deletes, or would.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Rows per table.
    pub rows: BTreeMap<String, i64>,
}

impl Pruned {
    pub fn total(&self) -> i64 {
        self.rows.values().sum()
    }
}

/// The tables [`Store::prune`] covers, with what makes a row old: only
/// records of things that are over. A row whose time can't be read stays.
const PRUNE: &[(&str, &str)] = &[
    ("command_log", "julianday(started_at) < julianday(?1)"),
    ("agent_events", "julianday(at) < julianday(?1)"),
    // A departed agent's run. The run of an agent still recorded stays,
    // whatever its age.
    (
        "runs",
        "ended_at != '' AND julianday(ended_at) < julianday(?1) AND name NOT IN (SELECT name FROM agents)",
    ),
    (
        "session_reports",
        "status = 'ended' AND julianday(at) < julianday(?1)",
    ),
    (
        "messages",
        "status NOT IN ('pending', 'handover') AND julianday(created_at) < julianday(?1)",
    ),
    (
        "jobs",
        "(ended_at != '' AND julianday(ended_at) < julianday(?1)) OR (found_gone_at != '' AND julianday(found_gone_at) < julianday(?1))",
    ),
    ("session_usage", "julianday(at) < julianday(?1)"),
];

impl Store {
    /// Stops tracking a session (again: the newest call's details stand).
    pub fn untrack_session(&self, u: &Untracked) -> Result<()> {
        self.exec(
            "untracking a session",
            "
		INSERT INTO untracked_sessions (harness, session_id, path, title, by, at) VALUES (?, ?, ?, ?, ?, ?)
		ON CONFLICT (harness, session_id) DO UPDATE SET
			path = excluded.path, title = excluded.title, by = excluded.by, at = excluded.at",
            params![u.harness, u.session_id, u.path, u.title, u.by, time_nanos(&u.at)],
        )?;
        Ok(())
    }

    /// Tracks a session again. Reports whether it was untracked.
    pub fn track_session(&self, harness: &str, session_id: &str) -> Result<bool> {
        let n = self.exec(
            "tracking a session",
            "DELETE FROM untracked_sessions WHERE harness = ? AND session_id = ?",
            params![harness, session_id],
        )?;
        Ok(n > 0)
    }

    /// Every untracked session, newest first.
    pub fn untracked_sessions(&self) -> Result<Vec<Untracked>> {
        let conn = self.conn();
        query_all(
            &conn,
            "SELECT harness, session_id, path, title, by, at FROM untracked_sessions ORDER BY at DESC",
            [],
            |row| {
                Ok(Untracked {
                    harness: text(row, 0)?,
                    session_id: text(row, 1)?,
                    path: text(row, 2)?,
                    title: text(row, 3)?,
                    by: text(row, 4)?,
                    at: parse_time(&text(row, 5)?),
                })
            },
        )
        .context("reading untracked sessions")
    }

    /// Every cached usage scan, by transcript path.
    pub fn session_usage(&self) -> Result<HashMap<String, UsageRow>> {
        let conn = self.conn();
        let rows = query_all(
            &conn,
            "SELECT path, size, scan FROM session_usage",
            [],
            |row| {
                Ok(UsageRow {
                    path: text(row, 0)?,
                    size: int(row, 1)?,
                    scan: text(row, 2)?,
                })
            },
        )
        .context("reading cached session usage")?;
        Ok(rows.into_iter().map(|r| (r.path.clone(), r)).collect())
    }

    /// Stores usage scans, replacing the ones for the same paths.
    pub fn set_session_usage(&self, rows: &[UsageRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let at = time_nanos(&Time::now());
        self.in_tx(|tx| {
            let mut st = tx.prepare(
                "INSERT INTO session_usage (path, size, scan, at) VALUES (?, ?, ?, ?)
			ON CONFLICT (path) DO UPDATE SET size = excluded.size, scan = excluded.scan, at = excluded.at",
            )?;
            for r in rows {
                st.execute(params![r.path, r.size, r.scan, at])?;
            }
            Ok(())
        })
        .context("storing session usage")
    }

    /// Forgets the usage cached for transcripts that are gone.
    pub fn forget_session_usage(&self, paths: &[String]) -> Result<()> {
        self.in_tx(|tx| {
            let mut st = tx.prepare("DELETE FROM session_usage WHERE path = ?")?;
            for p in paths {
                st.execute([p])?;
            }
            Ok(())
        })
        .context("forgetting session usage")
    }

    /// How many rows each table holds.
    pub fn table_counts(&self) -> Result<BTreeMap<String, i64>> {
        let conn = self.conn();
        let tables = query_all(
            &conn,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
            [],
            |row| text(row, 0),
        )?;
        let mut out = BTreeMap::new();
        for t in tables {
            // A table name from sqlite_master, quoted as an identifier.
            let n = conn.query_row(
                &format!("SELECT COUNT(*) FROM \"{}\"", t.replace('"', "\"\"")),
                [],
                |r| int(r, 0),
            )?;
            out.insert(t, n);
        }
        Ok(out)
    }

    /// Writes a consistent copy of the whole database to `dest`, which must
    /// not exist (SQLite's VACUUM INTO: safe while other processes write).
    pub fn backup_to(&self, dest: impl AsRef<Path>) -> Result<()> {
        let dest = dest.as_ref();
        if dest.exists() {
            bail!("{} already exists", dest.display());
        }
        let Some(path) = dest.to_str() else {
            bail!("the backup path {} isn't valid UTF-8", dest.display());
        };
        self.exec("writing the backup", "VACUUM INTO ?", [path])
            .with_context(|| format!("backing up to {}", dest.display()))?;
        Ok(())
    }

    /// Deletes the records of finished things older than `before`: command
    /// history, agent events, departed agents' runs, ended sessions' reports,
    /// delivered messages, finished jobs, and stale usage caches. With
    /// `dry_run` it only counts them. Agents, their state, held messages,
    /// friction, tasks, scripts and untracked sessions are never pruned.
    pub fn prune(&self, before: &Time, dry_run: bool) -> Result<Pruned> {
        let cutoff = time_nanos(before);
        let mut out = Pruned::default();
        self.in_tx(|tx| {
            for (table, cond) in PRUNE {
                let n = if dry_run {
                    tx.query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE {cond}"),
                        [&cutoff],
                        |r| int(r, 0),
                    )?
                } else {
                    tx.execute(&format!("DELETE FROM {table} WHERE {cond}"), [&cutoff])? as i64
                };
                if n > 0 {
                    out.rows.insert(table.to_string(), n);
                }
            }
            Ok(())
        })
        .context("pruning old records")?;
        Ok(out)
    }

    /// Returns the space deleted rows held to the filesystem (VACUUM).
    pub fn compact(&self) -> Result<()> {
        self.conn()
            .execute_batch("VACUUM")
            .context("compacting the database")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::CommandRecord;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path().join("t.db")).unwrap();
        (dir, s)
    }

    fn at(s: &str) -> Time {
        Time::parse_rfc3339(s).unwrap()
    }

    #[test]
    fn untrack_and_track() {
        let (_d, s) = store();
        let mut u = Untracked {
            harness: "claude".into(),
            session_id: "abc".into(),
            path: "/p/abc.jsonl".into(),
            title: "old".into(),
            by: "a person".into(),
            at: at("2026-11-01T10:00:00Z"),
        };
        s.untrack_session(&u).unwrap();
        u.title = "new".into();
        s.untrack_session(&u).unwrap();
        let all = s.untracked_sessions().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "new");
        assert!(!s.track_session("agy", "abc").unwrap());
        assert!(s.track_session("claude", "abc").unwrap());
        assert!(s.untracked_sessions().unwrap().is_empty());
    }

    #[test]
    fn usage_cache_round_trips() {
        let (_d, s) = store();
        let row = |size| UsageRow {
            path: "/p/a.jsonl".into(),
            size,
            scan: "{}".into(),
        };
        s.set_session_usage(&[row(1)]).unwrap();
        s.set_session_usage(&[row(2)]).unwrap();
        assert_eq!(s.session_usage().unwrap()["/p/a.jsonl"].size, 2);
        s.forget_session_usage(&["/p/a.jsonl".to_string()]).unwrap();
        assert!(s.session_usage().unwrap().is_empty());
    }

    #[test]
    fn prune_counts_then_deletes_only_old_finished_records() {
        let (_d, s) = store();
        for (when, arg) in [
            ("2026-10-01T00:00:00Z", "old"),
            ("2026-11-10T00:00:00+02:00", "new"),
        ] {
            let mut r = CommandRecord {
                started_at: at(when),
                args: vec![arg.to_string()],
                ..CommandRecord::default()
            };
            s.log_command(&mut r).unwrap();
        }
        {
            let conn = s.conn();
            conn.execute_batch(
                "INSERT INTO messages (sender_id, receiver_id, content, status, created_at) VALUES
					('a','b','old held','pending','2026-10-01T00:00:00Z'),
					('a','b','old sent','delivered','2026-10-01T00:00:00Z');
				 INSERT INTO agents (name, session_name, backend, created_at) VALUES ('kept','s','claude','2026-10-01T00:00:00Z');
				 INSERT INTO runs (name, parent, backend, session, cwd, created_at, ended_at) VALUES
					('gone','','claude','s','/','2026-09-01T00:00:00Z','2026-10-01T00:00:00Z'),
					('kept','','claude','s','/','2026-09-01T00:00:00Z','2026-10-01T00:00:00Z'),
					('live','','claude','s','/','2026-09-01T00:00:00Z','');",
            )
            .unwrap();
        }
        let cutoff = at("2026-11-01T00:00:00Z");
        let dry = s.prune(&cutoff, true).unwrap();
        let want: BTreeMap<String, i64> = [("command_log", 1), ("messages", 1), ("runs", 1)]
            .map(|(k, v)| (k.to_string(), v))
            .into();
        assert_eq!(dry.rows, want);
        assert_eq!(s.table_counts().unwrap()["command_log"], 2);

        let done = s.prune(&cutoff, false).unwrap();
        assert_eq!(done, dry);
        let counts = s.table_counts().unwrap();
        assert_eq!(
            (counts["command_log"], counts["messages"], counts["runs"]),
            (1, 1, 2)
        );
        assert_eq!(s.prune(&cutoff, false).unwrap().total(), 0);
        s.compact().unwrap();
    }

    #[test]
    fn backup_is_a_whole_copy_and_never_overwrites() {
        let (d, s) = store();
        s.untrack_session(&Untracked {
            harness: "kimi".into(),
            session_id: "x".into(),
            at: Time::now(),
            ..Untracked::default()
        })
        .unwrap();
        let dest = d.path().join("copy.db");
        s.backup_to(&dest).unwrap();
        assert!(s.backup_to(&dest).is_err());
        let copy = Store::open(&dest).unwrap();
        assert_eq!(copy.untracked_sessions().unwrap().len(), 1);
    }
}
