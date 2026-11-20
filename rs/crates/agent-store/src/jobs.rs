//! Jobs that `agent run` started, and the scripts it promoted. Both tables are
//! the Rust build's own: the Go binary never reads them, and opens a store
//! that has them as before (its migration creates only its own tables).

use anyhow::{Context, Result};
use rusqlite::{Connection, Row, params};
use serde::Serialize;

use crate::gotime::Time;
use crate::sqlite::{Store, int, null_text, parse_time, query_all, query_one, text};
use crate::tree::time_nanos;

/// JOBS_SCHEMA keeps one row per `agent run`: the script as it ran (path as
/// given and absolute, sha256, content, and the job's own copy that ran),
/// who started it where, the scope and limits it ran under, and how it
/// ended. ended_at is '' while the job runs, and stays '' for a job found
/// gone (its runner killed), whose end isn't known: found_gone_at says when
/// it was found so. exit_code is NULL until it ends, and stays NULL for a
/// job found gone. not_run is 1 for a job whose script never started.
pub(crate) const JOBS_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS jobs (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		script TEXT NOT NULL,
		path TEXT NOT NULL,
		sha256 TEXT NOT NULL,
		content TEXT NOT NULL,
		started_by TEXT NOT NULL,
		host TEXT NOT NULL,
		cwd TEXT NOT NULL,
		args TEXT NOT NULL DEFAULT '[]',
		unit TEXT NOT NULL DEFAULT '',
		memory_high TEXT NOT NULL DEFAULT '',
		memory_max TEXT NOT NULL DEFAULT '',
		memory_swap_max TEXT NOT NULL DEFAULT '',
		note TEXT NOT NULL DEFAULT '',
		started_at TEXT NOT NULL,
		ended_at TEXT NOT NULL DEFAULT '',
		exit_code INTEGER,
		pid INTEGER NOT NULL DEFAULT 0,
		pid_start TEXT NOT NULL DEFAULT '',
		ran TEXT NOT NULL DEFAULT '',
		found_gone_at TEXT NOT NULL DEFAULT '',
		not_run INTEGER NOT NULL DEFAULT 0
	);
"#;

/// JOBS_ADDED_COLUMNS are the jobs columns added after the table was first
/// shipped: migrate adds them to a store that lacks them (defaults keep the
/// old rows' meaning), and a read-only store without them reads as if they
/// held their defaults.
pub(crate) const JOBS_ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("jobs", "ran", "TEXT NOT NULL DEFAULT ''"),
    ("jobs", "found_gone_at", "TEXT NOT NULL DEFAULT ''"),
    ("jobs", "not_run", "INTEGER NOT NULL DEFAULT 0"),
];

/// SCRIPTS_SCHEMA records each promoted script (`<data dir>/scripts/<name>`)
/// with its sha256 at promotion, so a run can tell a script edited in place
/// from the one promoted.
pub(crate) const SCRIPTS_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS scripts (
		name TEXT PRIMARY KEY,
		sha256 TEXT NOT NULL,
		source TEXT NOT NULL,
		promoted_by TEXT NOT NULL,
		promoted_at TEXT NOT NULL
	);
"#;

/// Job is one `agent run`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Job {
    pub id: i64,
    /// The script as given: a path, or a promoted script's name.
    pub script: String,
    /// The absolute path that ran.
    pub path: String,
    pub sha256: String,
    /// The script's text as it ran.
    pub content: String,
    /// Who started it: an agent's name, or "user".
    pub by: String,
    pub host: String,
    pub cwd: String,
    pub args: Vec<String>,
    /// The file that ran: the job's own copy of the script
    /// (`<data dir>/jobs/<id>/script`). "" in records from before copies,
    /// whose job ran `path` itself.
    pub ran: String,
    /// The systemd scope it ran in ("agent-job-<store>-<id>.scope"); "" when
    /// it ran plainly.
    pub unit: String,
    /// The limits systemd reported for its scope, in systemd's size syntax;
    /// "" when none applied.
    pub memory_high: String,
    pub memory_max: String,
    pub memory_swap_max: String,
    /// Why it ran without limits, or other provenance worth keeping.
    pub note: String,
    pub started_at: Time,
    /// When it ended: None while it runs, and for a job found gone, whose
    /// end isn't known.
    pub ended_at: Option<Time>,
    /// When a job whose runner was killed was found no longer running.
    pub found_gone_at: Option<Time>,
    /// The script never started: refused before it ran, stopped before it
    /// started, or killed by its scope's limit first. note says which.
    pub not_run: bool,
    /// None while it runs, and for a job found gone without its code.
    pub exit_code: Option<i32>,
    pub pid: i64,
    /// The process's start time as the OS tells it (Linux: clock ticks since
    /// boot from /proc/<pid>/stat), so a reused pid isn't taken for the job.
    pub pid_start: String,
}

impl Job {
    /// running says whether the row records the job as still running. Whether
    /// it really is takes a look at the system (agent-fleet's run::list).
    pub fn running(&self) -> bool {
        self.ended_at.is_none() && self.found_gone_at.is_none()
    }
}

/// PromotedScript is a promoted script's record.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PromotedScript {
    pub name: String,
    pub sha256: String,
    /// The absolute path it was promoted from.
    pub source: String,
    pub promoted_by: String,
    pub promoted_at: Time,
}

const JOB_COLUMNS: &str = "id, script, path, sha256, content, started_by, host, cwd, args, unit, memory_high, memory_max, memory_swap_max, note, started_at, ended_at, exit_code, pid, pid_start, ran, found_gone_at, not_run";

/// optional_time reads a time column that is '' when unset.
fn optional_time(s: &str) -> Option<Time> {
    if s.is_empty() {
        None
    } else {
        Some(parse_time(s))
    }
}

fn scan_job(row: &Row<'_>) -> rusqlite::Result<Job> {
    let args: String = text(row, 8)?;
    Ok(Job {
        id: int(row, 0)?,
        script: text(row, 1)?,
        path: text(row, 2)?,
        sha256: text(row, 3)?,
        content: text(row, 4)?,
        by: text(row, 5)?,
        host: text(row, 6)?,
        cwd: text(row, 7)?,
        args: serde_json::from_str(&args).unwrap_or_default(),
        unit: text(row, 9)?,
        memory_high: text(row, 10)?,
        memory_max: text(row, 11)?,
        memory_swap_max: text(row, 12)?,
        note: text(row, 13)?,
        started_at: parse_time(&text(row, 14)?),
        ended_at: optional_time(&text(row, 15)?),
        exit_code: match row.get_ref(16)? {
            rusqlite::types::ValueRef::Null => None,
            _ => Some(int(row, 16)? as i32),
        },
        pid: int(row, 17)?,
        pid_start: null_text(row, 18)?,
        ran: text(row, 19)?,
        found_gone_at: optional_time(&text(row, 20)?),
        not_run: int(row, 21)? != 0,
    })
}

/// job_select is the column list to read jobs with: JOB_COLUMNS, but for a
/// read-only store from before JOBS_ADDED_COLUMNS, their defaults in their
/// place.
fn job_select(s: &Store, conn: &Connection) -> Result<String> {
    if s.schema.is_none() {
        return Ok(JOB_COLUMNS.to_string());
    }
    let mut cols = Vec::new();
    for col in JOB_COLUMNS.split(", ") {
        let added = JOBS_ADDED_COLUMNS.iter().find(|(_, c, _)| *c == col);
        let has = |c: &str| -> Result<bool> {
            let n = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('jobs') WHERE name = ?",
                [c],
                |r| int(r, 0),
            )?;
            Ok(n > 0)
        };
        match added {
            Some((_, c, def)) if !has(c)? => {
                let default = if def.starts_with("INTEGER") {
                    "0"
                } else {
                    "''"
                };
                cols.push(format!("{default} AS {c}"));
            }
            _ => cols.push(col.to_string()),
        }
    }
    Ok(cols.join(", "))
}

fn scan_script(row: &Row<'_>) -> rusqlite::Result<PromotedScript> {
    Ok(PromotedScript {
        name: text(row, 0)?,
        sha256: text(row, 1)?,
        source: text(row, 2)?,
        promoted_by: text(row, 3)?,
        promoted_at: parse_time(&text(row, 4)?),
    })
}

fn time_or_empty(t: &Option<Time>) -> String {
    t.as_ref().map(time_nanos).unwrap_or_default()
}

/// has_table says whether a store opened read-only (which may predate a
/// table) has it; a migrated store has every table.
pub(crate) fn has_table(s: &Store, conn: &Connection, table: &str) -> Result<bool> {
    if s.schema.is_none() {
        return Ok(true);
    }
    let n = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?",
        [table],
        |r| int(r, 0),
    )?;
    Ok(n > 0)
}

impl Store {
    /// create_job records a job about to start and sets job.id.
    pub fn create_job(&self, job: &mut Job) -> Result<()> {
        let args = serde_json::to_string(&job.args).context("encoding job args")?;
        job.id = self.insert(
            "inserting job",
            &format!(
                "INSERT INTO jobs ({}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &JOB_COLUMNS["id, ".len()..]
            ),
            params![
                job.script,
                job.path,
                job.sha256,
                job.content,
                job.by,
                job.host,
                job.cwd,
                args,
                job.unit,
                job.memory_high,
                job.memory_max,
                job.memory_swap_max,
                job.note,
                time_nanos(&job.started_at),
                time_or_empty(&job.ended_at),
                job.exit_code,
                job.pid,
                job.pid_start,
                job.ran,
                time_or_empty(&job.found_gone_at),
                job.not_run
            ],
        )?;
        Ok(())
    }

    /// update_job_launch records how a job was started: the copy it runs,
    /// its unit, the limits that applied, the note, and its process. A job
    /// being launched runs, so this also undoes an end a reader recorded
    /// while it had no live process yet (between a failed scope and the
    /// plain run, say).
    pub fn update_job_launch(&self, job: &Job) -> Result<()> {
        self.exec(
            "updating job",
            "UPDATE jobs SET ran = ?, unit = ?, memory_high = ?, memory_max = ?, memory_swap_max = ?, note = ?, pid = ?, pid_start = ?, ended_at = '', exit_code = NULL, found_gone_at = '', not_run = 0 WHERE id = ?",
            params![
                job.ran,
                job.unit,
                job.memory_high,
                job.memory_max,
                job.memory_swap_max,
                job.note,
                job.pid,
                job.pid_start,
                job.id
            ],
        )?;
        Ok(())
    }

    /// finish_job records a job's end. exit_code None means it ended with no
    /// code known.
    pub fn finish_job(&self, id: i64, ended_at: &Time, exit_code: Option<i32>) -> Result<()> {
        self.exec(
            "finishing job",
            "UPDATE jobs SET ended_at = ?, exit_code = ?, found_gone_at = '' WHERE id = ?",
            params![time_nanos(ended_at), exit_code, id],
        )?;
        Ok(())
    }

    /// record_job_end records how a job ended as its runner saw it: ended_at
    /// (now, if job has none), exit code, note, and whether its script ran.
    /// It wins over a reader having found the job gone meanwhile.
    pub fn record_job_end(&self, job: &Job) -> Result<()> {
        let ended = job.ended_at.unwrap_or_else(Time::now);
        self.exec(
            "finishing job",
            "UPDATE jobs SET ended_at = ?, exit_code = ?, note = ?, not_run = ?, found_gone_at = '' WHERE id = ?",
            params![time_nanos(&ended), job.exit_code, job.note, job.not_run, job.id],
        )?;
        Ok(())
    }

    /// found_job_gone records that a job recorded as running was found not
    /// to be (its runner killed): when it was found, and note. Its end stays
    /// unknown. A job whose end was recorded meanwhile is left as it is; the
    /// result says whether this call recorded it.
    pub fn found_job_gone(&self, id: i64, found_at: &Time, note: &str) -> Result<bool> {
        let n = self.exec(
            "settling job",
            "UPDATE jobs SET found_gone_at = ?, note = ? WHERE id = ? AND ended_at = '' AND found_gone_at = ''",
            params![time_nanos(found_at), note, id],
        )?;
        Ok(n > 0)
    }

    /// get_job returns a job by id.
    pub fn get_job(&self, id: i64) -> Result<Option<Job>> {
        let conn = self.conn();
        if !has_table(self, &conn, "jobs")? {
            return Ok(None);
        }
        let cols = job_select(self, &conn)?;
        query_one(
            &conn,
            &format!("SELECT {cols} FROM jobs WHERE id = ?"),
            [id],
            scan_job,
        )
        .context("querying job")
    }

    /// list_jobs returns the jobs recorded as running, or with all every job,
    /// oldest first. A store from before jobs has none.
    pub fn list_jobs(&self, all: bool) -> Result<Vec<Job>> {
        let conn = self.conn();
        if !has_table(self, &conn, "jobs")? {
            return Ok(Vec::new());
        }
        let cols = job_select(self, &conn)?;
        let filter = if all {
            ""
        } else {
            " WHERE ended_at = '' AND found_gone_at = ''"
        };
        let filter = if !all && cols.contains("'' AS found_gone_at") {
            " WHERE ended_at = ''"
        } else {
            filter
        };
        query_all(
            &conn,
            &format!("SELECT {cols} FROM jobs{filter} ORDER BY id"),
            [],
            scan_job,
        )
        .context("querying jobs")
    }

    /// set_script records (or replaces) a promoted script's record.
    pub fn set_script(&self, s: &PromotedScript) -> Result<()> {
        self.exec(
            "recording promoted script",
            "INSERT OR REPLACE INTO scripts (name, sha256, source, promoted_by, promoted_at) VALUES (?, ?, ?, ?, ?)",
            params![s.name, s.sha256, s.source, s.promoted_by, time_nanos(&s.promoted_at)],
        )?;
        Ok(())
    }

    /// get_script returns a promoted script's record.
    pub fn get_script(&self, name: &str) -> Result<Option<PromotedScript>> {
        let conn = self.conn();
        if !has_table(self, &conn, "scripts")? {
            return Ok(None);
        }
        query_one(
            &conn,
            "SELECT name, sha256, source, promoted_by, promoted_at FROM scripts WHERE name = ?",
            [name],
            scan_script,
        )
        .context("querying promoted script")
    }

    /// list_scripts returns every promoted script's record, by name.
    pub fn list_scripts(&self) -> Result<Vec<PromotedScript>> {
        let conn = self.conn();
        if !has_table(self, &conn, "scripts")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            "SELECT name, sha256, source, promoted_by, promoted_at FROM scripts ORDER BY name",
            [],
            scan_script,
        )
        .context("querying promoted scripts")
    }
}
