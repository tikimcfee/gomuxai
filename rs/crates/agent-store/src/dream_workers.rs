//! A dream's workers and the ideas they propose (docs/DREAMING.md, "The
//! workers and the idea engine"). Every agent a dream runs is a worker: a
//! project dream has one `dreamer`; the global dream has a `generator` (the
//! synthesizer, proposing ideas) and a `critic` (judging them), and code
//! applies what the critic approved. An idea is one proposal of a caller of
//! the idea engine (`synthesis`: one proposed memory change or friction
//! triage), with its verdict and scores; a vote records a worker proposing
//! it (again, for callers that match ideas across runs).

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Row, params};

use crate::dreams::{DREAM_OPEN, Dream};
use crate::gotime::Time;
use crate::jobs::has_table;
use crate::sqlite::{Store, int, parse_time, query_all, query_one, text};
use crate::tree::time_nanos;

/// A project dream's single agent: it reads, decides and writes itself.
pub const ROLE_DREAMER: &str = "dreamer";
/// A generator run of the idea engine: it proposes ideas (`agent dream idea`).
pub const ROLE_GENERATOR: &str = "generator";
/// The critic: it judges each idea against its primary sources (`agent
/// dream judge`).
pub const ROLE_CRITIC: &str = "critic";

/// A worker at work.
pub const WORKER_RUNNING: &str = "running";
/// A worker that finished its part (`agent dream done`, or its dream
/// committed).
pub const WORKER_DONE: &str = "done";
/// A worker that went away, or whose dream was abandoned while it worked.
pub const WORKER_FAILED: &str = "failed";

/// The caller of the idea engine whose ideas are proposed memory changes
/// and friction triage: the global dream's synthesizer.
pub const CALLER_SYNTHESIS: &str = "synthesis";

/// An idea not judged yet.
pub const IDEA_PROPOSED: &str = "proposed";
/// An idea the critic approved.
pub const IDEA_APPROVED: &str = "approved";
/// An idea the critic rejected.
pub const IDEA_REJECTED: &str = "rejected";
/// An approved idea code applied.
pub const IDEA_APPLIED: &str = "applied";
/// An approved idea code couldn't apply (a version conflict, say).
pub const IDEA_SKIPPED: &str = "skipped";

pub(crate) const DREAM_WORKERS_SCHEMA: &str = r#"
	-- Every agent a dream runs. role: dreamer, generator, critic.
	CREATE TABLE IF NOT EXISTS dream_workers (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		dream_id INTEGER NOT NULL REFERENCES dreams(id) ON DELETE CASCADE,
		agent TEXT NOT NULL,
		role TEXT NOT NULL,
		model TEXT NOT NULL DEFAULT '',
		effort TEXT NOT NULL DEFAULT '',
		lens TEXT NOT NULL DEFAULT '',
		status TEXT NOT NULL,
		started_at TEXT NOT NULL,
		finished_at TEXT NOT NULL DEFAULT '',
		UNIQUE (dream_id, agent)
	);
	CREATE INDEX IF NOT EXISTS idx_dream_workers_agent ON dream_workers(agent);

	-- The idea engine's ideas. caller: synthesis (for now). idea: the
	-- proposal as JSON; citations: a JSON list. Scores are 1 to 5, NULL
	-- when not given; rank is for a later tournament (NULL until then).
	CREATE TABLE IF NOT EXISTS dream_ideas (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		dream_id INTEGER NOT NULL REFERENCES dreams(id) ON DELETE CASCADE,
		caller TEXT NOT NULL,
		worker TEXT NOT NULL,
		idea TEXT NOT NULL,
		citations TEXT NOT NULL DEFAULT '[]',
		status TEXT NOT NULL,
		created_at TEXT NOT NULL,
		judged_by TEXT NOT NULL DEFAULT '',
		judged_at TEXT NOT NULL DEFAULT '',
		why TEXT NOT NULL DEFAULT '',
		checked TEXT NOT NULL DEFAULT '[]',
		leverage INTEGER,
		evidence INTEGER,
		feasibility INTEGER,
		novelty INTEGER,
		rank REAL,
		outcome TEXT NOT NULL DEFAULT ''
	);
	CREATE INDEX IF NOT EXISTS idx_dream_ideas_dream ON dream_ideas(dream_id);

	-- A worker proposing an idea: the first vote is its generator's; a
	-- caller that matches ideas across runs adds one per match.
	CREATE TABLE IF NOT EXISTS dream_idea_votes (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		idea_id INTEGER NOT NULL REFERENCES dream_ideas(id) ON DELETE CASCADE,
		dream_id INTEGER NOT NULL,
		worker TEXT NOT NULL,
		model TEXT NOT NULL DEFAULT '',
		lens TEXT NOT NULL DEFAULT '',
		evidence TEXT NOT NULL DEFAULT '[]',
		created_at TEXT NOT NULL
	);
"#;

/// Dreams from before workers had their one agent in `dreams.agent`: each
/// becomes that dream's dreamer, done if its dream committed (or was quiet),
/// failed if it was abandoned, running while it is open.
pub(crate) const DREAM_WORKERS_MIGRATE: &str = r#"
	INSERT INTO dream_workers (dream_id, agent, role, status, started_at, finished_at)
		SELECT d.id, d.agent, 'dreamer',
			CASE d.status WHEN 'open' THEN 'running' WHEN 'abandoned' THEN 'failed' ELSE 'done' END,
			d.started_at, d.finished_at
		FROM dreams d
		WHERE d.agent != '' AND NOT EXISTS (SELECT 1 FROM dream_workers w WHERE w.dream_id = d.id);
"#;

/// One agent of a dream.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct DreamWorker {
    pub dream_id: i64,
    pub agent: String,
    /// [`ROLE_DREAMER`], [`ROLE_GENERATOR`] or [`ROLE_CRITIC`].
    pub role: String,
    pub model: String,
    pub effort: String,
    pub lens: String,
    /// [`WORKER_RUNNING`], [`WORKER_DONE`] or [`WORKER_FAILED`].
    pub status: String,
    pub started_at: Time,
    /// Zero while running.
    pub finished_at: Time,
}

impl DreamWorker {
    /// A worker of dream `dream_id` (0 until it is recorded) about to start.
    pub fn new(agent: &str, role: &str, model: &str, effort: &str) -> DreamWorker {
        DreamWorker {
            agent: agent.to_string(),
            role: role.to_string(),
            model: model.to_string(),
            effort: effort.to_string(),
            status: WORKER_RUNNING.to_string(),
            started_at: Time::now(),
            ..Default::default()
        }
    }
}

/// An idea as the store records it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct DreamIdea {
    pub id: i64,
    pub dream_id: i64,
    pub caller: String,
    /// The worker that proposed it.
    pub worker: String,
    /// The proposal, as JSON.
    pub idea: String,
    pub citations: Vec<String>,
    pub status: String,
    pub created_at: Time,
    pub judged_by: String,
    pub judged_at: Time,
    /// The critic's why.
    pub why: String,
    /// The sources the critic says it read.
    pub checked: Vec<String>,
    pub scores: Scores,
    /// A tournament's rank, once there is one.
    pub rank: Option<f64>,
    /// What applying it did.
    pub outcome: String,
}

/// The critic's absolute scores, 1 to 5 each, None when not given.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Scores {
    pub leverage: Option<i64>,
    pub evidence: Option<i64>,
    pub feasibility: Option<i64>,
    pub novelty: Option<i64>,
}

/// A worker's vote for an idea.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct IdeaVote {
    pub idea_id: i64,
    pub dream_id: i64,
    pub worker: String,
    pub model: String,
    pub lens: String,
    pub evidence: Vec<String>,
}

/// A verdict on an idea.
#[derive(Clone, Debug, Default)]
pub struct Verdict {
    /// [`IDEA_APPROVED`] or [`IDEA_REJECTED`].
    pub status: String,
    pub judged_by: String,
    pub why: String,
    pub checked: Vec<String>,
    pub scores: Scores,
}

const WORKER_COLUMNS: &str =
    "dream_id, agent, role, model, effort, lens, status, started_at, finished_at";

fn opt_time(s: String) -> Time {
    if s.is_empty() {
        Time::zero()
    } else {
        parse_time(&s)
    }
}

fn scan_worker(row: &Row<'_>) -> rusqlite::Result<DreamWorker> {
    Ok(DreamWorker {
        dream_id: int(row, 0)?,
        agent: text(row, 1)?,
        role: text(row, 2)?,
        model: text(row, 3)?,
        effort: text(row, 4)?,
        lens: text(row, 5)?,
        status: text(row, 6)?,
        started_at: parse_time(&text(row, 7)?),
        finished_at: opt_time(text(row, 8)?),
    })
}

const IDEA_COLUMNS: &str = "id, dream_id, caller, worker, idea, citations, status, created_at,
	judged_by, judged_at, why, checked, leverage, evidence, feasibility, novelty, rank, outcome";

fn json_list(s: &str) -> Vec<String> {
    serde_json::from_str(s).unwrap_or_default()
}

fn scan_idea(row: &Row<'_>) -> rusqlite::Result<DreamIdea> {
    Ok(DreamIdea {
        id: int(row, 0)?,
        dream_id: int(row, 1)?,
        caller: text(row, 2)?,
        worker: text(row, 3)?,
        idea: text(row, 4)?,
        citations: json_list(&text(row, 5)?),
        status: text(row, 6)?,
        created_at: parse_time(&text(row, 7)?),
        judged_by: text(row, 8)?,
        judged_at: opt_time(text(row, 9)?),
        why: text(row, 10)?,
        checked: json_list(&text(row, 11)?),
        scores: Scores {
            leverage: row.get(12)?,
            evidence: row.get(13)?,
            feasibility: row.get(14)?,
            novelty: row.get(15)?,
        },
        rank: row.get(16)?,
        outcome: text(row, 17)?,
    })
}

/// Records worker w of dream `dream_id` in tx.
pub(crate) fn insert_worker(
    tx: &rusqlite::Transaction<'_>,
    dream_id: i64,
    w: &DreamWorker,
) -> Result<()> {
    tx.execute(
        &format!("INSERT INTO dream_workers ({WORKER_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"),
        params![
            dream_id,
            w.agent,
            w.role,
            w.model,
            w.effort,
            w.lens,
            w.status,
            time_nanos(&w.started_at),
            if w.finished_at.is_zero() {
                String::new()
            } else {
                time_nanos(&w.finished_at)
            },
        ],
    )
    .with_context(|| format!("recording {} as a worker of dream #{dream_id}", w.agent))?;
    Ok(())
}

/// Closes dream `dream_id`'s running workers in tx: done when it commits,
/// failed when it is abandoned.
pub(crate) fn close_workers(
    tx: &rusqlite::Transaction<'_>,
    dream_id: i64,
    committed: bool,
) -> Result<()> {
    let status = if committed {
        WORKER_DONE
    } else {
        WORKER_FAILED
    };
    tx.execute(
        "UPDATE dream_workers SET status = ?, finished_at = ? WHERE dream_id = ? AND status = ?",
        params![status, time_nanos(&Time::now()), dream_id, WORKER_RUNNING],
    )
    .context("closing the dream's workers")?;
    Ok(())
}

fn check_score(name: &str, v: Option<i64>) -> Result<()> {
    if let Some(v) = v
        && !(1..=5).contains(&v)
    {
        bail!("--{name} is {v}: scores go from 1 to 5");
    }
    Ok(())
}

impl Store {
    /// Adds worker w to open dream `dream_id`.
    pub fn add_dream_worker(&self, dream_id: i64, w: &DreamWorker) -> Result<()> {
        self.in_tx(|tx| {
            crate::dreams::require_open(tx, dream_id)?;
            insert_worker(tx, dream_id, w)
        })
    }

    /// Dream `dream_id`'s workers, in the order they started.
    pub fn dream_workers(&self, dream_id: i64) -> Result<Vec<DreamWorker>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_workers")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            &format!("SELECT {WORKER_COLUMNS} FROM dream_workers WHERE dream_id = ? ORDER BY id"),
            [dream_id],
            scan_worker,
        )
        .context("reading a dream's workers")
    }

    /// The worker `agent` of dream `dream_id`, or None.
    pub fn dream_worker(&self, dream_id: i64, agent: &str) -> Result<Option<DreamWorker>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_workers")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!("SELECT {WORKER_COLUMNS} FROM dream_workers WHERE dream_id = ? AND agent = ?"),
            params![dream_id, agent],
            scan_worker,
        )
        .context("reading a dream worker")
    }

    /// The newest open dream `agent` is a worker of, with its worker, or None.
    pub fn open_dream_worker(&self, agent: &str) -> Result<Option<(Dream, DreamWorker)>> {
        if agent.is_empty() {
            return Ok(None);
        }
        let id: Option<i64> = {
            let conn = self.conn();
            if !has_table(self, &conn, "dream_workers")? {
                return Ok(None);
            }
            conn.query_row(
                "SELECT w.dream_id FROM dream_workers w JOIN dreams d ON d.id = w.dream_id
				WHERE w.agent = ? AND d.status = ? ORDER BY d.id DESC LIMIT 1",
                params![agent, DREAM_OPEN],
                |r| r.get(0),
            )
            .optional()
            .context("reading an agent's open dream")?
        };
        let Some(id) = id else {
            return Ok(None);
        };
        match (self.get_dream(id)?, self.dream_worker(id, agent)?) {
            (Some(d), Some(w)) => Ok(Some((d, w))),
            _ => Ok(None),
        }
    }

    /// Sets worker `agent` of dream `dream_id` to status (done or failed),
    /// stamping when it finished.
    pub fn set_dream_worker_status(&self, dream_id: i64, agent: &str, status: &str) -> Result<()> {
        let finished = if status == WORKER_RUNNING {
            String::new()
        } else {
            time_nanos(&Time::now())
        };
        let n = self.exec(
            "setting a dream worker's status",
            "UPDATE dream_workers SET status = ?, finished_at = ? WHERE dream_id = ? AND agent = ?",
            params![status, finished, dream_id, agent],
        )?;
        if n == 0 {
            bail!("{agent} is no worker of dream #{dream_id}");
        }
        Ok(())
    }

    /// Records an idea of open dream `dream_id` by `worker`, with its first
    /// vote (the worker's own), and returns its id.
    pub fn add_dream_idea(
        &self,
        dream_id: i64,
        caller: &str,
        worker: &DreamWorker,
        idea: &str,
        citations: &[String],
    ) -> Result<i64> {
        self.in_tx(|tx| {
            crate::dreams::require_open(tx, dream_id)?;
            let now = time_nanos(&Time::now());
            let cites = serde_json::to_string(citations)?;
            tx.execute(
                "INSERT INTO dream_ideas (dream_id, caller, worker, idea, citations, status, created_at)
				VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![dream_id, caller, worker.agent, idea, cites, IDEA_PROPOSED, now],
            )
            .context("recording an idea")?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO dream_idea_votes (idea_id, dream_id, worker, model, lens, evidence, created_at)
				VALUES (?, ?, ?, ?, ?, ?, ?)",
                params![id, dream_id, worker.agent, worker.model, worker.lens, cites, now],
            )
            .context("recording an idea's vote")?;
            Ok(id)
        })
    }

    /// Adds a vote for idea `idea_id`: a later worker proposed it again.
    pub fn add_idea_vote(&self, v: &IdeaVote) -> Result<()> {
        self.exec(
            "recording an idea's vote",
            "INSERT INTO dream_idea_votes (idea_id, dream_id, worker, model, lens, evidence, created_at)
			VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                v.idea_id,
                v.dream_id,
                v.worker,
                v.model,
                v.lens,
                serde_json::to_string(&v.evidence)?,
                time_nanos(&Time::now())
            ],
        )?;
        Ok(())
    }

    /// Idea `idea_id`'s votes, oldest first.
    pub fn idea_votes(&self, idea_id: i64) -> Result<Vec<IdeaVote>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_idea_votes")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            "SELECT idea_id, dream_id, worker, model, lens, evidence FROM dream_idea_votes WHERE idea_id = ? ORDER BY id",
            [idea_id],
            |r| {
                Ok(IdeaVote {
                    idea_id: int(r, 0)?,
                    dream_id: int(r, 1)?,
                    worker: text(r, 2)?,
                    model: text(r, 3)?,
                    lens: text(r, 4)?,
                    evidence: json_list(&text(r, 5)?),
                })
            },
        )
        .context("reading an idea's votes")
    }

    /// Idea `id`, or None.
    pub fn get_dream_idea(&self, id: i64) -> Result<Option<DreamIdea>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_ideas")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!("SELECT {IDEA_COLUMNS} FROM dream_ideas WHERE id = ?"),
            [id],
            scan_idea,
        )
        .context("reading an idea")
    }

    /// Dream `dream_id`'s ideas, by id.
    pub fn dream_ideas(&self, dream_id: i64) -> Result<Vec<DreamIdea>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_ideas")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            &format!("SELECT {IDEA_COLUMNS} FROM dream_ideas WHERE dream_id = ? ORDER BY id"),
            [dream_id],
            scan_idea,
        )
        .context("reading a dream's ideas")
    }

    /// Every idea of `caller` from any dream, by id: what a new idea is
    /// matched against.
    pub fn ideas_of_caller(&self, caller: &str) -> Result<Vec<DreamIdea>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_ideas")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            &format!("SELECT {IDEA_COLUMNS} FROM dream_ideas WHERE caller = ? ORDER BY id"),
            [caller],
            scan_idea,
        )
        .context("reading ideas")
    }

    /// Records the verdict on idea `id`, whose dream must be open. A critic
    /// may judge an idea again until its dream applies it.
    pub fn judge_dream_idea(&self, id: i64, v: &Verdict) -> Result<()> {
        if v.status != crate::dream_workers::IDEA_APPROVED
            && v.status != crate::dream_workers::IDEA_REJECTED
        {
            bail!("a verdict is approve or reject, not {}", v.status);
        }
        check_score("leverage", v.scores.leverage)?;
        check_score("evidence", v.scores.evidence)?;
        check_score("feasibility", v.scores.feasibility)?;
        check_score("novelty", v.scores.novelty)?;
        self.in_tx(|tx| {
            let (dream_id, status): (i64, String) = tx
                .query_row(
                    "SELECT dream_id, status FROM dream_ideas WHERE id = ?",
                    [id],
                    |r| Ok((r.get(0)?, text(r, 1)?)),
                )
                .optional()
                .context("reading the idea")?
                .with_context(|| format!("there is no idea #{id}"))?;
            crate::dreams::require_open(tx, dream_id)?;
            if status != IDEA_PROPOSED && status != IDEA_APPROVED && status != IDEA_REJECTED {
                bail!("idea #{id} is {status}: it can't be judged again");
            }
            tx.execute(
                "UPDATE dream_ideas SET status = ?, judged_by = ?, judged_at = ?, why = ?, checked = ?,
				leverage = ?, evidence = ?, feasibility = ?, novelty = ? WHERE id = ?",
                params![
                    v.status,
                    v.judged_by,
                    time_nanos(&Time::now()),
                    v.why,
                    serde_json::to_string(&v.checked)?,
                    v.scores.leverage,
                    v.scores.evidence,
                    v.scores.feasibility,
                    v.scores.novelty,
                    id
                ],
            )
            .context("recording the verdict")?;
            Ok(())
        })
    }

    /// Records what applying idea `id` did: status applied or skipped, and
    /// the outcome in a sentence.
    pub fn set_idea_outcome(&self, id: i64, status: &str, outcome: &str) -> Result<()> {
        self.exec(
            "recording what applying an idea did",
            "UPDATE dream_ideas SET status = ?, outcome = ? WHERE id = ?",
            params![status, outcome, id],
        )?;
        Ok(())
    }
}
