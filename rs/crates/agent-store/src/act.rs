//! The act layer's runs (docs/DREAMING-ACT.md, "The general actor"): one
//! agent given one cited item (a friction entry, or a finding a dream
//! wrote) and a scope, the ceiling of what it may change. Its verbs check
//! the scope; this table records each run, its log and how it ended.

use anyhow::{Context, Result, bail};
use rusqlite::{Row, params};

use crate::dream_workers::{json_list, opt_time};
use crate::gotime::Time;
use crate::jobs::has_table;
use crate::sqlite::{Store, int, parse_time, query_all, query_one, text};
use crate::tree::time_nanos;

/// Reads, and asks a person one question; changes nothing.
pub const SCOPE_ASK: &str = "ask";
/// Also changes what can be undone without loss: edits in its own
/// worktree, checks, a crawl of the index.
pub const SCOPE_FIX: &str = "fix";
/// Also commits in its own worktree, and does the local upkeep that git
/// keeps safe: brings a branch up to date with the default branch, removes
/// merged worktrees and branches, cleans up ended helpers whose work is
/// committed.
pub const SCOPE_COMMIT: &str = "commit";
/// Also lands its commits on the repo's `act/landed` branch.
pub const SCOPE_BRANCH: &str = "branch";

/// The scopes, lowest first.
pub const SCOPES: &[&str] = &[SCOPE_ASK, SCOPE_FIX, SCOPE_COMMIT, SCOPE_BRANCH];

/// The rank of a scope (ask 0 .. branch 3), or None for none of them.
pub fn scope_rank(scope: &str) -> Option<usize> {
    SCOPES.iter().position(|s| *s == scope)
}

/// At work.
pub const ACT_OPEN: &str = "open";
/// It finished, saying what it did.
pub const ACT_DONE: &str = "done";
/// It stopped with a question for a person.
pub const ACT_ASKED: &str = "asked";
/// It gave up, saying why.
pub const ACT_FAILED: &str = "failed";
/// A person stopped it, or its agent went away.
pub const ACT_ABANDONED: &str = "abandoned";

pub(crate) const ACT_SCHEMA: &str = r#"
	-- The act layer's runs. item: friction:<uid> or finding:<dream id>.
	-- sources: a JSON list of citations. scope: ask, fix, commit, branch.
	-- log: what the run did, one line each.
	CREATE TABLE IF NOT EXISTS act_runs (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		item TEXT NOT NULL,
		summary TEXT NOT NULL,
		sources TEXT NOT NULL DEFAULT '[]',
		scope TEXT NOT NULL,
		repo TEXT NOT NULL DEFAULT '',
		worktree TEXT NOT NULL DEFAULT '',
		branch TEXT NOT NULL DEFAULT '',
		base TEXT NOT NULL DEFAULT '',
		check_cmd TEXT NOT NULL DEFAULT '',
		agent TEXT NOT NULL,
		model TEXT NOT NULL DEFAULT '',
		status TEXT NOT NULL,
		report TEXT NOT NULL DEFAULT '',
		log TEXT NOT NULL DEFAULT '',
		started_by TEXT NOT NULL DEFAULT '',
		created_at TEXT NOT NULL,
		finished_at TEXT NOT NULL DEFAULT ''
	);
	CREATE INDEX IF NOT EXISTS idx_act_runs_agent ON act_runs(agent);
"#;

/// One run.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ActRun {
    pub id: i64,
    pub item: String,
    pub summary: String,
    pub sources: Vec<String>,
    pub scope: String,
    pub repo: String,
    pub worktree: String,
    pub branch: String,
    pub base: String,
    pub check_cmd: String,
    pub agent: String,
    pub model: String,
    pub status: String,
    pub report: String,
    pub log: String,
    pub started_by: String,
    pub created_at: Time,
    pub finished_at: Time,
}

impl ActRun {
    /// Whether its scope reaches `scope`.
    pub fn allows(&self, scope: &str) -> bool {
        matches!((scope_rank(&self.scope), scope_rank(scope)), (Some(a), Some(b)) if a >= b)
    }
}

const COLUMNS: &str = "id, item, summary, sources, scope, repo, worktree, branch, base, check_cmd, agent, model, status, report, log, started_by, created_at, finished_at";

fn scan(row: &Row<'_>) -> rusqlite::Result<ActRun> {
    Ok(ActRun {
        id: int(row, 0)?,
        item: text(row, 1)?,
        summary: text(row, 2)?,
        sources: json_list(&text(row, 3)?),
        scope: text(row, 4)?,
        repo: text(row, 5)?,
        worktree: text(row, 6)?,
        branch: text(row, 7)?,
        base: text(row, 8)?,
        check_cmd: text(row, 9)?,
        agent: text(row, 10)?,
        model: text(row, 11)?,
        status: text(row, 12)?,
        report: text(row, 13)?,
        log: text(row, 14)?,
        started_by: text(row, 15)?,
        created_at: parse_time(&text(row, 16)?),
        finished_at: opt_time(text(row, 17)?),
    })
}

impl Store {
    /// Records a new open run; its id. `agent_of` names its agent from the
    /// id.
    pub fn open_act_run(&self, r: &ActRun, agent_of: impl FnOnce(i64) -> String) -> Result<ActRun> {
        if scope_rank(&r.scope).is_none() {
            bail!(
                "an act run's scope is one of {}, not {:?}",
                SCOPES.join(", "),
                r.scope
            );
        }
        let id = self.in_tx(|tx| {
            tx.execute(
                "INSERT INTO act_runs (item, summary, sources, scope, repo, worktree, branch, base, check_cmd, agent, model, status, started_by, created_at)
				VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, '', ?, ?, ?, ?)",
                params![
                    r.item,
                    r.summary,
                    serde_json::to_string(&r.sources)?,
                    r.scope,
                    r.repo,
                    r.worktree,
                    r.branch,
                    r.base,
                    r.check_cmd,
                    r.model,
                    ACT_OPEN,
                    r.started_by,
                    time_nanos(&Time::now())
                ],
            )
            .context("recording an act run")?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "UPDATE act_runs SET agent = ? WHERE id = ?",
                params![agent_of(id), id],
            )?;
            Ok(id)
        })?;
        self.get_act_run(id)?
            .with_context(|| format!("act run #{id} vanished"))
    }

    pub fn get_act_run(&self, id: i64) -> Result<Option<ActRun>> {
        let conn = self.conn();
        if !has_table(self, &conn, "act_runs")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!("SELECT {COLUMNS} FROM act_runs WHERE id = ?"),
            [id],
            scan,
        )
        .context("reading an act run")
    }

    /// The runs, newest first; only open ones unless `all`.
    pub fn act_runs(&self, all: bool) -> Result<Vec<ActRun>> {
        let conn = self.conn();
        if !has_table(self, &conn, "act_runs")? {
            return Ok(Vec::new());
        }
        let q = if all {
            format!("SELECT {COLUMNS} FROM act_runs ORDER BY id DESC")
        } else {
            format!("SELECT {COLUMNS} FROM act_runs WHERE status = 'open' ORDER BY id DESC")
        };
        query_all(&conn, &q, [], scan).context("reading act runs")
    }

    /// The open run `agent` is the agent of, if any.
    pub fn open_act_run_of(&self, agent: &str) -> Result<Option<ActRun>> {
        let conn = self.conn();
        if !has_table(self, &conn, "act_runs")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!("SELECT {COLUMNS} FROM act_runs WHERE agent = ? AND status = 'open' ORDER BY id DESC LIMIT 1"),
            [agent],
            scan,
        )
        .context("reading an agent's act run")
    }

    /// Records open run `id`'s worktree, its branch and the commit it
    /// started from.
    pub fn set_act_worktree(
        &self,
        id: i64,
        worktree: &str,
        branch: &str,
        base: &str,
    ) -> Result<()> {
        self.exec(
            "recording an act run's worktree",
            "UPDATE act_runs SET worktree = ?, branch = ?, base = ? WHERE id = ?",
            params![worktree, branch, base, id],
        )?;
        Ok(())
    }

    /// Adds a line to open run `id`'s log.
    pub fn log_act(&self, id: i64, line: &str) -> Result<()> {
        let n = self.exec(
            "logging an act run",
            "UPDATE act_runs SET log = log || ? WHERE id = ? AND status = 'open'",
            params![format!("{line}\n"), id],
        )?;
        if n == 0 {
            bail!("act run #{id} isn't open");
        }
        Ok(())
    }

    /// Ends open run `id` with `status` and what it said.
    pub fn end_act_run(&self, id: i64, status: &str, report: &str) -> Result<()> {
        if status == ACT_OPEN {
            bail!("ending an act run takes a final status, not open");
        }
        let n = self.exec(
            "ending an act run",
            "UPDATE act_runs SET status = ?, report = ?, finished_at = ? WHERE id = ? AND status = 'open'",
            params![status, report, time_nanos(&Time::now()), id],
        )?;
        if n == 0 {
            bail!("act run #{id} isn't open");
        }
        Ok(())
    }
}
