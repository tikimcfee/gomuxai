//! Dreams (docs/DREAMING.md): one row per dream, and the items each was given
//! to review. An item counts as reviewed only once its dream commits, and
//! only at the fingerprint it was reviewed at: a session written to again, or
//! a friction entry voted on, comes back.

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, Row, params};

use crate::dream_workers::{DreamWorker, ROLE_DREAMER, close_workers, insert_worker};
use crate::gotime::Time;
use crate::jobs::has_table;
use crate::sqlite::{Store, int, parse_time, query_all, query_one, text};
use crate::tree::time_nanos;

/// A dream an agent is working on.
pub const DREAM_OPEN: &str = "open";
/// A dream whose agent finished: its items count as reviewed.
pub const DREAM_COMMITTED: &str = "committed";
/// A dream given up, by its agent or because its agent went away: its items
/// wait for the next dream.
pub const DREAM_ABANDONED: &str = "abandoned";
/// A dream that found nothing new and started no agent.
pub const DREAM_QUIET: &str = "quiet";

/// A dream over one project of the dream scope (or, for a row from before
/// projects, its `project` empty, over the whole scope).
pub const DREAM_KIND_PROJECT: &str = "project";
/// The machine-wide dream: committed project dreams, every scoped project's
/// memory, the machine memory, and the host's friction.
pub const DREAM_KIND_GLOBAL: &str = "global";

/// The dreams columns added after the table was first shipped: migrate adds
/// them to a store that lacks them. A row from before them is a project
/// dream over the whole scope (`project` empty).
pub(crate) const DREAMS_ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("dreams", "project", "TEXT NOT NULL DEFAULT ''"),
    ("dreams", "kind", "TEXT NOT NULL DEFAULT 'project'"),
];

pub(crate) const DREAMS_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS dreams (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		host TEXT NOT NULL,
		agent TEXT NOT NULL DEFAULT '',
		started_at TEXT NOT NULL,
		finished_at TEXT NOT NULL DEFAULT '',
		status TEXT NOT NULL,
		diary TEXT NOT NULL DEFAULT ''
	);

	-- What a dream was given to review. kind: session, friction, handoff,
	-- memory, commit, dream (a project dream a global one reviews). key names the thing (a session's harness:id, a friction
	-- entry's uid, a path, repo@commit); fingerprint says which version of it.
	CREATE TABLE IF NOT EXISTS dream_items (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		dream_id INTEGER NOT NULL REFERENCES dreams(id) ON DELETE CASCADE,
		kind TEXT NOT NULL,
		key TEXT NOT NULL,
		fingerprint TEXT NOT NULL,
		summary TEXT NOT NULL DEFAULT '',
		read TEXT NOT NULL DEFAULT '',
		UNIQUE (dream_id, kind, key)
	);

	CREATE INDEX IF NOT EXISTS idx_dream_items_kind_key ON dream_items(kind, key);
"#;

/// A dream as the store records it.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize)]
pub struct Dream {
    pub id: i64,
    pub host: String,
    /// The agent that dreams it ("" for a quiet dream).
    pub agent: String,
    pub started_at: Time,
    /// Zero while open.
    pub finished_at: Time,
    pub status: String,
    /// The dream's diary: what its agent noted, a line or more per note.
    pub diary: String,
    /// How many items it was given.
    pub items: i64,
    /// [`DREAM_KIND_PROJECT`] or [`DREAM_KIND_GLOBAL`].
    pub kind: String,
    /// The scope directory a project dream covers; "" for a global dream,
    /// and for a project dream from before projects (the whole scope).
    pub project: String,
}

/// What a new dream is over: its kind, and for a project dream, its project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DreamOf {
    pub kind: String,
    pub project: String,
}

impl DreamOf {
    /// A project dream over dir.
    pub fn project(dir: &str) -> DreamOf {
        DreamOf {
            kind: DREAM_KIND_PROJECT.to_string(),
            project: dir.to_string(),
        }
    }

    /// The global dream.
    pub fn global() -> DreamOf {
        DreamOf {
            kind: DREAM_KIND_GLOBAL.to_string(),
            project: String::new(),
        }
    }
}

/// One thing a dream was given to review.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DreamItem {
    pub kind: String,
    pub key: String,
    pub fingerprint: String,
    /// One line saying what it is.
    #[serde(default)]
    pub summary: String,
    /// How to read it: a command or a path.
    #[serde(default)]
    pub read: String,
}

const DREAM_COLUMNS: &str = "id, host, agent, started_at, finished_at, status, diary,
	(SELECT COUNT(*) FROM dream_items i WHERE i.dream_id = dreams.id), kind, project";

fn scan_dream(row: &Row<'_>) -> rusqlite::Result<Dream> {
    let finished = text(row, 4)?;
    Ok(Dream {
        id: int(row, 0)?,
        host: text(row, 1)?,
        agent: text(row, 2)?,
        started_at: parse_time(&text(row, 3)?),
        finished_at: if finished.is_empty() {
            Time::zero()
        } else {
            parse_time(&finished)
        },
        status: text(row, 5)?,
        diary: text(row, 6)?,
        items: int(row, 7)?,
        kind: text(row, 8)?,
        project: text(row, 9)?,
    })
}

fn scan_item(row: &Row<'_>) -> rusqlite::Result<DreamItem> {
    Ok(DreamItem {
        kind: text(row, 0)?,
        key: text(row, 1)?,
        fingerprint: text(row, 2)?,
        summary: text(row, 3)?,
        read: text(row, 4)?,
    })
}

const UPSERT_ITEM: &str = "
	INSERT INTO dream_items (dream_id, kind, key, fingerprint, summary, read) VALUES (?, ?, ?, ?, ?, ?)
	ON CONFLICT (dream_id, kind, key) DO UPDATE SET
		fingerprint = excluded.fingerprint,
		summary = CASE WHEN excluded.summary = '' THEN summary ELSE excluded.summary END,
		read = CASE WHEN excluded.read = '' THEN read ELSE excluded.read END";

/// The host state key holding the dream scope: a JSON list of absolute
/// directories (`agent dream scope`).
pub const HOST_KEY_DREAM_SCOPE: &str = "dream.scope";

impl Store {
    /// The dream scope: the project directories dreaming covers, or none.
    pub fn dream_scope(&self) -> Result<Vec<String>> {
        match self.get_host_state(HOST_KEY_DREAM_SCOPE)? {
            None => Ok(Vec::new()),
            Some(v) if v.is_empty() => Ok(Vec::new()),
            Some(v) => serde_json::from_str(&v).with_context(|| {
                format!("reading the dream scope (host state {HOST_KEY_DREAM_SCOPE})")
            }),
        }
    }

    /// Records a new open dream on host with its items, its agent named by
    /// agent_of from the dream's id, and returns it: a project dream with no
    /// project, as dreams were before projects (tests use it).
    pub fn open_dream(
        &self,
        host: &str,
        items: &[DreamItem],
        agent_of: impl FnOnce(i64) -> String,
    ) -> Result<Dream> {
        self.open_dream_of_kind(host, &DreamOf::project(""), items, agent_of)
    }

    /// Records a new open dream over `of` on host with its items, its agent
    /// (its one worker, the dreamer) named by agent_of from the dream's id,
    /// and returns it.
    pub fn open_dream_of_kind(
        &self,
        host: &str,
        of: &DreamOf,
        items: &[DreamItem],
        agent_of: impl FnOnce(i64) -> String,
    ) -> Result<Dream> {
        self.open_dream_with(host, of, items, |id| {
            DreamWorker::new(&agent_of(id), ROLE_DREAMER, "", "")
        })
    }

    /// Records a new open dream over `of` on host with its items and its
    /// first worker, made by first from the dream's id (the dream's agent is
    /// that worker), and returns it.
    pub fn open_dream_with(
        &self,
        host: &str,
        of: &DreamOf,
        items: &[DreamItem],
        first: impl FnOnce(i64) -> DreamWorker,
    ) -> Result<Dream> {
        let now = Time::now();
        let id = self.in_tx(|tx| {
            tx.execute(
                "INSERT INTO dreams (host, started_at, status, kind, project) VALUES (?, ?, ?, ?, ?)",
                params![host, time_nanos(&now), DREAM_OPEN, of.kind, of.project],
            )
            .context("recording a dream")?;
            let id = tx.last_insert_rowid();
            let w = first(id);
            tx.execute(
                "UPDATE dreams SET agent = ? WHERE id = ?",
                params![w.agent, id],
            )
            .context("naming the dream's agent")?;
            insert_worker(tx, id, &w)?;
            for it in items {
                tx.execute(
                    UPSERT_ITEM,
                    params![id, it.kind, it.key, it.fingerprint, it.summary, it.read],
                )
                .context("recording a dream's items")?;
            }
            Ok(id)
        })?;
        self.get_dream(id)?
            .context("reading back the dream just recorded")
    }

    /// Records a quiet dream over `of` (nothing new: no agent, no items),
    /// with why in its diary, and returns it.
    pub fn record_quiet_dream(&self, host: &str, of: &DreamOf, why: &str) -> Result<Dream> {
        let now = time_nanos(&Time::now());
        let id = self.insert(
            "recording a quiet dream",
            "INSERT INTO dreams (host, started_at, finished_at, status, diary, kind, project) VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![host, now, now, DREAM_QUIET, why, of.kind, of.project],
        )?;
        self.get_dream(id)?
            .context("reading back the dream just recorded")
    }

    /// Dream id, or None.
    pub fn get_dream(&self, id: i64) -> Result<Option<Dream>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!("SELECT {DREAM_COLUMNS} FROM dreams WHERE id = ?"),
            [id],
            scan_dream,
        )
        .context("reading a dream")
    }

    /// The newest dreams first, at most limit of them (0: all).
    pub fn list_dreams(&self, limit: i64) -> Result<Vec<Dream>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(Vec::new());
        }
        let limit = if limit <= 0 { -1 } else { limit };
        query_all(
            &conn,
            &format!("SELECT {DREAM_COLUMNS} FROM dreams ORDER BY id DESC LIMIT ?"),
            [limit],
            scan_dream,
        )
        .context("listing dreams")
    }

    /// Every dream still open, oldest first.
    pub fn open_dreams(&self) -> Result<Vec<Dream>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            &format!("SELECT {DREAM_COLUMNS} FROM dreams WHERE status = ? ORDER BY id"),
            [DREAM_OPEN],
            scan_dream,
        )
        .context("listing open dreams")
    }

    /// The open dream agent is a worker of, or None.
    pub fn open_dream_of(&self, agent: &str) -> Result<Option<Dream>> {
        Ok(self.open_dream_worker(agent)?.map(|(d, _)| d))
    }

    /// The newest committed dream, or None.
    pub fn last_committed_dream(&self) -> Result<Option<Dream>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(None);
        }
        query_one(
            &conn,
            &format!(
                "SELECT {DREAM_COLUMNS} FROM dreams WHERE status = ? ORDER BY id DESC LIMIT 1"
            ),
            [DREAM_COMMITTED],
            scan_dream,
        )
        .context("reading the last committed dream")
    }

    /// The newest committed dream that covered `of`: for a project dream,
    /// the newest committed project dream over that project or over the
    /// whole scope (from before projects); for the global dream, the newest
    /// committed global one. None if there is none.
    pub fn last_committed_dream_of(&self, of: &DreamOf) -> Result<Option<Dream>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(None);
        }
        let projects: Vec<&str> = if of.kind == DREAM_KIND_GLOBAL {
            vec![""]
        } else {
            vec![of.project.as_str(), ""]
        };
        query_one(
            &conn,
            &format!(
                "SELECT {DREAM_COLUMNS} FROM dreams WHERE status = ? AND kind = ? AND project IN (?, ?) ORDER BY id DESC LIMIT 1"
            ),
            params![DREAM_COMMITTED, of.kind, projects[0], projects[projects.len() - 1]],
            scan_dream,
        )
        .context("reading the last committed dream")
    }

    /// The committed project dreams that finished at or after `since`,
    /// oldest first: what the global dream reviews.
    pub fn committed_project_dreams_since(&self, since: &Time) -> Result<Vec<Dream>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(Vec::new());
        }
        // Compared as times: stored stamps drop trailing zeros, so their
        // text doesn't sort.
        let all = query_all(
            &conn,
            &format!(
                "SELECT {DREAM_COLUMNS} FROM dreams WHERE status = ? AND kind = ? ORDER BY id"
            ),
            params![DREAM_COMMITTED, DREAM_KIND_PROJECT],
            scan_dream,
        )
        .context("listing committed project dreams")?;
        Ok(all
            .into_iter()
            .filter(|d| d.finished_at.0 >= since.0)
            .collect())
    }

    /// The names of every agent that has dreamt (every worker of every
    /// dream) or acted (every act run's agent), whatever became of it: their
    /// sessions are the machine's own, no evidence for a dream.
    pub fn dream_agents(&self) -> Result<HashSet<String>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dreams")? {
            return Ok(HashSet::new());
        }
        let workers = if has_table(self, &conn, "dream_workers")? {
            " UNION SELECT agent FROM dream_workers WHERE agent != ''"
        } else {
            ""
        };
        // An act run's agent too: a failing check mid-fix isn't a trip.
        let actors = if has_table(self, &conn, "act_runs")? {
            " UNION SELECT agent FROM act_runs WHERE agent != ''"
        } else {
            ""
        };
        let names = query_all(
            &conn,
            &format!("SELECT DISTINCT agent FROM dreams WHERE agent != ''{workers}{actors}"),
            [],
            |r| text(r, 0),
        )
        .context("listing dream agents")?;
        Ok(names.into_iter().collect())
    }

    /// Dream id's items, by kind then key.
    pub fn dream_items(&self, id: i64) -> Result<Vec<DreamItem>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_items")? {
            return Ok(Vec::new());
        }
        query_all(
            &conn,
            "SELECT kind, key, fingerprint, summary, read FROM dream_items WHERE dream_id = ? ORDER BY kind, key",
            [id],
            scan_item,
        )
        .context("reading a dream's items")
    }

    /// Adds item to open dream id, or sets the fingerprint of the one it has
    /// under that kind and key: what the dream changed itself (a friction
    /// entry it triaged, a memory file it rewrote) is then reviewed as it
    /// left it, not brought back as new.
    pub fn set_dream_item(&self, id: i64, item: &DreamItem) -> Result<()> {
        self.in_tx(|tx| {
            require_open(tx, id)?;
            tx.execute(
                UPSERT_ITEM,
                params![
                    id,
                    item.kind,
                    item.key,
                    item.fingerprint,
                    item.summary,
                    item.read
                ],
            )
            .context("recording a dream item")?;
            Ok(())
        })
    }

    /// Appends text to dream id's diary, on a line of its own. The dream must
    /// be open.
    pub fn append_dream_diary(&self, id: i64, text: &str) -> Result<()> {
        self.in_tx(|tx| {
            require_open(tx, id)?;
            tx.execute(
                "UPDATE dreams SET diary = CASE WHEN diary = '' THEN ?1 ELSE diary || char(10) || ?1 END WHERE id = ?2",
                params![text, id],
            )
            .context("writing the dream's diary")?;
            Ok(())
        })
    }

    /// Commits open dream id: its items count as reviewed from now on.
    pub fn commit_dream(&self, id: i64) -> Result<()> {
        self.close_dream(id, DREAM_COMMITTED, "")
    }

    /// Abandons open dream id, adding why (if any) to its diary: its items
    /// wait for the next dream.
    pub fn abandon_dream(&self, id: i64, why: &str) -> Result<()> {
        self.close_dream(id, DREAM_ABANDONED, why)
    }

    fn close_dream(&self, id: i64, status: &str, why: &str) -> Result<()> {
        self.in_tx(|tx| {
            require_open(tx, id)?;
            tx.execute(
                "UPDATE dreams SET status = ?1, finished_at = ?2,
				diary = CASE WHEN ?3 = '' THEN diary WHEN diary = '' THEN ?3 ELSE diary || char(10) || ?3 END
				WHERE id = ?4",
                params![status, time_nanos(&Time::now()), why, id],
            )
            .context("closing the dream")?;
            close_workers(tx, id, status == DREAM_COMMITTED)?;
            Ok(())
        })
    }

    /// Whether a committed dream reviewed kind/key at fingerprint, as its
    /// latest review.
    pub fn is_reviewed(&self, kind: &str, key: &str, fingerprint: &str) -> Result<bool> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_items")? {
            return Ok(false);
        }
        let latest: Option<String> = conn
            .query_row(
                "SELECT i.fingerprint FROM dream_items i JOIN dreams d ON d.id = i.dream_id
				WHERE d.status = ? AND i.kind = ? AND i.key = ? ORDER BY d.id DESC LIMIT 1",
                params![DREAM_COMMITTED, kind, key],
                |r| text(r, 0),
            )
            .optional()
            .context("reading what dreams reviewed")?;
        Ok(latest.as_deref() == Some(fingerprint))
    }

    /// Every item a committed dream reviewed, by (kind, key), at the
    /// fingerprint its latest review saw.
    pub fn reviewed_fingerprints(&self) -> Result<HashMap<(String, String), String>> {
        self.reviewed_fingerprints_by("")
    }

    /// Every item a committed dream of kind `dream_kind` reviewed (any kind,
    /// for ""), by (kind, key), at the fingerprint its latest review saw.
    pub fn reviewed_fingerprints_by(
        &self,
        dream_kind: &str,
    ) -> Result<HashMap<(String, String), String>> {
        let conn = self.conn();
        if !has_table(self, &conn, "dream_items")? {
            return Ok(HashMap::new());
        }
        let rows = query_all(
            &conn,
            "SELECT i.kind, i.key, i.fingerprint FROM dream_items i JOIN dreams d ON d.id = i.dream_id
			WHERE d.status = ? AND (?2 = '' OR d.kind = ?2) ORDER BY d.id",
            params![DREAM_COMMITTED, dream_kind],
            |r| Ok((text(r, 0)?, text(r, 1)?, text(r, 2)?)),
        )
        .context("reading what dreams reviewed")?;
        // Oldest first, so the newest review stands.
        Ok(rows
            .into_iter()
            .map(|(kind, key, fp)| ((kind, key), fp))
            .collect())
    }
}

/// Fails unless dream id exists and is open.
pub(crate) fn require_open(tx: &rusqlite::Transaction<'_>, id: i64) -> Result<()> {
    let status: Option<String> = tx
        .query_row("SELECT status FROM dreams WHERE id = ?", [id], |r| {
            text(r, 0)
        })
        .optional()
        .context("reading the dream")?;
    match status.as_deref() {
        None => bail!("there is no dream #{id}"),
        Some(DREAM_OPEN) => Ok(()),
        Some(s) => bail!("dream #{id} is {s}, not open"),
    }
}
