use anyhow::{Context, Result};
use rusqlite::{Row, params};

use crate::gojson;
use crate::gotime::Time;
use crate::models::{CommandFilter, CommandRecord};
use crate::sqlite::{Store, int, parse_time, query_all, text};

/// COMMAND_LOG_SCHEMA records every agent invocation (agent history). Rows
/// are written once, when a command ends, and pruned to the newest
/// COMMAND_LOG_KEEP.
pub(crate) const COMMAND_LOG_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS command_log (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		started_at TEXT NOT NULL,
		duration_ms INTEGER NOT NULL,
		host TEXT NOT NULL,
		agent TEXT NOT NULL,
		parent TEXT NOT NULL,
		cwd TEXT NOT NULL,
		args TEXT NOT NULL,
		exit_code INTEGER NOT NULL,
		error TEXT NOT NULL,
		build TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_command_log_agent
		ON command_log(agent, id);
"#;

/// COMMAND_LOG_KEEP is how many command records the log keeps. Every
/// COMMAND_LOG_PRUNE_EVERY-th insert deletes the older ones, so most inserts
/// don't pay for it. (Store fields, so tests can change them.)
pub(crate) const COMMAND_LOG_KEEP: i64 = 20000;
pub(crate) const COMMAND_LOG_PRUNE_EVERY: i64 = 100;

/// time_key formats t so that stored times sort as text: UTC, with a fixed
/// number of fractional digits.
pub(crate) fn time_key(t: &Time) -> String {
    t.utc().format_millis()
}

const COMMAND_COLUMNS: &str =
    "id, started_at, duration_ms, host, agent, parent, cwd, args, exit_code, error, build";

fn scan_command(row: &Row<'_>) -> rusqlite::Result<CommandRecord> {
    let args = text(row, 7)?;
    Ok(CommandRecord {
        id: int(row, 0)?,
        started_at: parse_time(&text(row, 1)?),
        duration_ms: int(row, 2)?,
        host: text(row, 3)?,
        agent: text(row, 4)?,
        parent: text(row, 5)?,
        cwd: text(row, 6)?,
        args: match serde_json::from_str::<Option<Vec<String>>>(&args) {
            Ok(v) => v.unwrap_or_default(),
            Err(_) => vec![args],
        },
        exit_code: int(row, 8)?,
        error: text(row, 9)?,
        build: text(row, 10)?,
    })
}

impl Store {
    /// log_command records a command run, with its agent's recorded parent,
    /// and sets rec.id and rec.parent.
    pub fn log_command(&self, rec: &mut CommandRecord) -> Result<()> {
        // Go writes a nil list as "[]" too.
        let args = gojson::marshal(&rec.args).context("encoding command args")?;
        // The parent is looked up here, in the insert, rather than at read
        // time: by then the agent may have been cleaned up.
        let (id, parent) = self
            .conn()
            .query_row(
                "
		INSERT INTO command_log (started_at, duration_ms, host, agent, parent, cwd, args, exit_code, error, build)
		VALUES (?, ?, ?, ?, COALESCE((SELECT spawned_by FROM agents WHERE name = ? AND name != ''), ''), ?, ?, ?, ?, ?)
		RETURNING id, parent
	",
                params![
                    time_key(&rec.started_at),
                    rec.duration_ms,
                    rec.host,
                    rec.agent,
                    rec.agent,
                    rec.cwd,
                    args,
                    rec.exit_code,
                    rec.error,
                    rec.build
                ],
                |r| Ok((int(r, 0)?, text(r, 1)?)),
            )
            .context("logging command")?;
        rec.id = id;
        rec.parent = parent;
        if rec.id % self.command_log_prune_every == 0 {
            self.exec(
                "pruning command log",
                "DELETE FROM command_log WHERE id <= ?",
                [rec.id - self.command_log_keep],
            )?;
        }
        Ok(())
    }

    /// list_commands returns the command records f selects, newest first.
    pub fn list_commands(&self, f: &CommandFilter) -> Result<Vec<CommandRecord>> {
        let mut clauses: Vec<&str> = Vec::new();
        let mut args: Vec<String> = Vec::new();
        if f.by_agent {
            clauses.push("agent = ?");
            args.push(f.agent.clone());
        }
        if !f.cwd.is_empty() {
            clauses.push("cwd = ?");
            args.push(f.cwd.clone());
        }
        if f.failed {
            clauses.push("exit_code != 0");
        }
        if !f.since.is_zero() {
            clauses.push("started_at >= ?");
            args.push(time_key(&f.since));
        }
        let mut query = format!("SELECT {COMMAND_COLUMNS} FROM command_log");
        if !clauses.is_empty() {
            query += " WHERE ";
            query += &clauses.join(" AND ");
        }
        query += " ORDER BY id DESC";
        if f.limit > 0 {
            query += &format!(" LIMIT {}", f.limit);
        }
        query_all(
            &self.conn(),
            &query,
            rusqlite::params_from_iter(args),
            scan_command,
        )
        .context("querying command log")
    }
}
