use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use crate::gotime::Time;
use crate::models::{Agent, StateEntry};
use crate::sqlite::{
    AGENT_COLUMNS, STATE_COLUMNS, Store, int, parse_time, query_all, query_one, scan_agent,
    scan_state, text,
};

/// TREE_SCHEMA records the agent tree's history for agent export. Spawn
/// opens a run and cleanup closes it, so a departed agent stays readable;
/// spawn, cleanup, and every status report append an agent event, which
/// followers tail by id.
pub(crate) const TREE_SCHEMA: &str = r#"
	CREATE TABLE IF NOT EXISTS runs (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		name TEXT NOT NULL,
		parent TEXT NOT NULL,
		backend TEXT NOT NULL,
		session TEXT NOT NULL,
		cwd TEXT NOT NULL,
		created_at TEXT NOT NULL,
		ended_at TEXT NOT NULL DEFAULT '',
		status TEXT NOT NULL DEFAULT '',
		transcript TEXT NOT NULL DEFAULT '',
		end_reason TEXT NOT NULL DEFAULT ''
	);

	CREATE INDEX IF NOT EXISTS idx_runs_name ON runs(name, id);

	CREATE TABLE IF NOT EXISTS agent_events (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		at TEXT NOT NULL,
		kind TEXT NOT NULL,
		agent TEXT NOT NULL,
		run INTEGER NOT NULL,
		status TEXT NOT NULL DEFAULT '',
		event TEXT NOT NULL DEFAULT '',
		detail TEXT NOT NULL DEFAULT '',
		tool TEXT NOT NULL DEFAULT '',
		tool_input TEXT NOT NULL DEFAULT '',
		session_id TEXT NOT NULL DEFAULT '',
		transcript TEXT NOT NULL DEFAULT '',
		end_reason TEXT NOT NULL DEFAULT ''
	);
"#;

/// AGENT_EVENTS_KEEP is how many agent events the table keeps; every
/// AGENT_EVENTS_PRUNE_EVERY-th insert deletes older ones. (Store fields, so
/// tests can change them.)
pub(crate) const AGENT_EVENTS_KEEP: i64 = 20000;
pub(crate) const AGENT_EVENTS_PRUNE_EVERY: i64 = 100;

// Agent event kinds.
/// spawn recorded the agent
pub const AGENT_SPAWNED: &str = "spawned";
/// a status report was recorded
pub const AGENT_STATUS: &str = "status";
/// cleanup (or a failed spawn) deleted the record
pub const AGENT_REMOVED: &str = "removed";

/// The state key whose value becomes an agent's removed event's detail: why
/// it was removed, set just before a cleanup that a person approved.
pub const STATE_REMOVED_WHY: &str = "_removed_why";

/// Run is one spawn of an agent name, from spawn until cleanup and after.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Run {
    pub id: i64,
    pub name: String,
    /// spawned_by; "" for a person
    pub parent: String,
    pub backend: String,
    pub session: String,
    pub cwd: String,
    pub created_at: Time,
    /// zero while the agent is live
    pub ended_at: Time,
    // Status, EndReason and Transcript are the agent's last reported
    // status, end reason and transcript path, copied when the run ends.
    pub status: String,
    pub end_reason: String,
    pub transcript: String,
}

/// AgentEvent is one change to the agent tree. Status events carry the
/// report; spawned and removed events name the run, which holds the rest.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AgentEvent {
    pub id: i64,
    pub at: Time,
    /// AGENT_SPAWNED, AGENT_STATUS, or AGENT_REMOVED
    pub kind: String,
    pub agent: String,
    /// the agent's run; 0 for an agent spawned before runs were recorded
    pub run: i64,

    pub status: String,
    pub event: String,
    pub detail: String,
    pub tool: String,
    /// only for a waiting status: inputs can be 64 KiB
    pub tool_input: String,
    /// only when the report set it
    pub session_id: String,
    /// Transcript is the CLI's transcript path, when the report named one.
    pub transcript: String,
    /// EndReason says how the session ended, with a status of ended.
    pub end_reason: String,
}

/// Tree is a store's agents, read in one transaction.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// Cursor is the newest agent event's id (0 if none): the tree includes
    /// every event up to it, and none after.
    pub cursor: i64,
    /// Events reports whether the store records agent events: one written by
    /// an agent built before agent export has no events to follow.
    pub events: bool,
    /// live agents, oldest first
    pub agents: Vec<Agent>,
    /// live agents' runs, by name
    pub runs: HashMap<String, Run>,
    /// live agents' reserved status keys
    pub state: Vec<StateEntry>,
    /// departed runs, oldest first, if asked for
    pub ended: Vec<Run>,
}

/// TREE_STATE_KEYS are the state keys a tree reads: the reserved status keys
/// (see service.StateKey*).
const TREE_STATE_KEYS: &[&str] = &[
    "_status",
    "_event",
    "_detail",
    "_tool",
    "_tool_input",
    "_session_id",
    "_transcript",
    "_end_reason",
];

/// Schema says which parts of the current schema a store has. A store
/// opened read-only may predate them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Schema {
    spawned_by: bool,
    cwd: bool,
    adopted: bool,
    runs: bool,
    events: bool,
    end_reason: bool,
}

/// CURRENT_SCHEMA is a migrated store's.
const CURRENT_SCHEMA: Schema = Schema {
    spawned_by: true,
    cwd: true,
    adopted: true,
    runs: true,
    events: true,
    end_reason: true,
};

/// open_read_only opens another binary's store without writing to it: no
/// migrations, and SQLite's read-only mode. Only the tree readers work on it.
pub fn open_read_only(db_path: impl AsRef<Path>) -> Result<Store> {
    Store::open_read_only(db_path)
}

impl Store {
    /// open_read_only opens another binary's store without writing to it: no
    /// migrations, and SQLite's read-only mode. Only the tree readers work on
    /// it. (One connection, as the Go also uses here: data_version is per
    /// connection, so it must be compared on the same one.)
    pub fn open_read_only(db_path: impl AsRef<Path>) -> Result<Store> {
        let path = db_path.as_ref();
        let reading = || format!("reading {}", path.display());
        let conn = Store::open_read_only_conn(path).with_context(reading)?;
        let schema = read_schema(&conn).with_context(reading)?;
        Ok(Store::from_read_only(conn, schema))
    }

    fn schema_of(&self) -> Schema {
        self.schema.unwrap_or(CURRENT_SCHEMA)
    }

    /// run_columns and event_columns are the columns scan_run and
    /// scan_agent_event read; a store from before end reasons were kept reads
    /// them as ''.
    fn run_columns(&self) -> String {
        format!(
            "id, name, parent, backend, session, cwd, created_at, ended_at, status, transcript, {}",
            self.end_reason_column()
        )
    }

    fn event_columns(&self) -> String {
        format!(
            "id, at, kind, agent, run, status, event, detail, tool, tool_input, session_id, transcript, {}",
            self.end_reason_column()
        )
    }

    fn end_reason_column(&self) -> &'static str {
        if self.schema_of().end_reason {
            "end_reason"
        } else {
            "'' AS end_reason"
        }
    }

    /// get_run returns a run by id, or None if there is none.
    pub fn get_run(&self, id: i64) -> Result<Option<Run>> {
        if !self.schema_of().runs {
            return Ok(None);
        }
        query_one(
            &self.conn(),
            &format!("SELECT {} FROM runs WHERE id = ?", self.run_columns()),
            [id],
            scan_run,
        )
        .context("querying run")
    }

    /// agent_events_after returns up to limit agent events with ids above
    /// after, oldest first, and the oldest id the store still keeps (0 if
    /// none), so a caller can tell whether events after `after` were pruned.
    pub fn agent_events_after(&self, after: i64, limit: i64) -> Result<(Vec<AgentEvent>, i64)> {
        if !self.schema_of().events {
            return Ok((Vec::new(), 0));
        }
        let conn = self.conn();
        let oldest: i64 = conn
            .query_row("SELECT COALESCE(MIN(id), 0) FROM agent_events", [], |r| {
                int(r, 0)
            })
            .context("querying agent events")?;
        let events = query_all(
            &conn,
            &format!(
                "SELECT {} FROM agent_events WHERE id > ? ORDER BY id LIMIT ?",
                self.event_columns()
            ),
            [after, limit],
            scan_agent_event,
        )
        .context("querying agent events")?;
        Ok((events, oldest))
    }

    /// data_version is SQLite's data_version: it changes when another
    /// connection commits, so a caller can skip re-reading a quiet store.
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn()
            .query_row("PRAGMA data_version", [], |r| int(r, 0))?)
    }

    /// read_tree reads the live agents with their runs and status keys, and
    /// with ended, the departed runs, all in one read transaction so they
    /// agree with each other and with cursor. Departed agents of a store that
    /// predates runs come from its command log's spawns, newest per name,
    /// with no id.
    pub fn read_tree(&self, ended: bool) -> Result<Tree> {
        let sc = self.schema_of();
        let mut conn = self.conn();
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .context("reading agent tree")?;

        let mut t = Tree {
            events: sc.events,
            ..Tree::default()
        };
        if sc.events {
            t.cursor = tx
                .query_row("SELECT COALESCE(MAX(id), 0) FROM agent_events", [], |r| {
                    int(r, 0)
                })
                .context("reading agent events")?;
        }

        let cols = format!(
            "name, session_name, backend, task_file, log_file, created_at, {}, {}, {}",
            if sc.spawned_by { "spawned_by" } else { "NULL" },
            if sc.cwd { "cwd" } else { "NULL" },
            if sc.adopted { "adopted" } else { "0" },
        );
        t.agents = query_all(
            &tx,
            &format!("SELECT {cols} FROM agents ORDER BY created_at, name"),
            [],
            scan_agent,
        )
        .context("reading agents")?;
        let live: HashSet<String> = t.agents.iter().map(|a| a.name.clone()).collect();

        let keys = format!("'{}'", TREE_STATE_KEYS.join("','"));
        t.state = query_all(
            &tx,
            &format!(
                "SELECT {STATE_COLUMNS} FROM state WHERE key IN ({keys}) ORDER BY agent_name, key"
            ),
            [],
            scan_state,
        )
        .context("reading agent state")?;

        let mut has_run: HashSet<String> = HashSet::new();
        if sc.runs {
            let runs = query_all(
                &tx,
                &format!("SELECT {} FROM runs ORDER BY id", self.run_columns()),
                [],
                scan_run,
            )
            .context("reading runs")?;
            for r in runs {
                has_run.insert(r.name.clone());
                if r.ended_at.is_zero() && live.contains(&r.name) {
                    t.runs.insert(r.name.clone(), r);
                } else if !r.ended_at.is_zero() && ended {
                    t.ended.push(r);
                }
            }
        }

        // Stores and agents from before runs: the command log's spawns give
        // cwd for live agents and the whole record for departed ones.
        let mut need_spawns = ended;
        for a in &t.agents {
            if a.cwd.is_empty() && !t.runs.contains_key(&a.name) {
                need_spawns = true;
            }
        }
        if !need_spawns {
            return Ok(t);
        }
        let spawns = logged_spawns(&tx)?;
        for a in &mut t.agents {
            if let Some(sp) = spawns.get(&a.name)
                && a.cwd.is_empty()
                && !t.runs.contains_key(&a.name)
            {
                a.cwd = sp.cwd.clone();
            }
        }
        if ended {
            let mut legacy: Vec<Run> = spawns
                .into_iter()
                .filter(|(name, _)| !live.contains(name) && !has_run.contains(name))
                .map(|(_, sp)| sp)
                .collect();
            sort_runs(&mut legacy);
            legacy.append(&mut t.ended);
            t.ended = legacy;
        }
        Ok(t)
    }
}

fn read_schema(conn: &Connection) -> Result<Schema> {
    let has = |query: &str, args: &[&str]| -> rusqlite::Result<bool> {
        let n: i64 = conn.query_row(query, rusqlite::params_from_iter(args), |r| int(r, 0))?;
        Ok(n > 0)
    };
    let column = "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?";
    let table = "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?";
    if !has(table, &["agents"])? {
        return Err(anyhow!("no agents table: not an agent store"));
    }
    Ok(Schema {
        spawned_by: has(column, &["agents", "spawned_by"])?,
        cwd: has(column, &["agents", "cwd"])?,
        adopted: has(column, &["agents", "adopted"])?,
        runs: has(table, &["runs"])?,
        events: has(table, &["agent_events"])?,
        end_reason: has(column, &["agent_events", "end_reason"])?,
    })
}

fn bool_to_int(b: bool) -> i64 {
    if b { 1 } else { 0 }
}

/// create_agent_tx records a new agent, opens its run, and appends its
/// spawned event.
pub(crate) fn create_agent_tx(s: &Store, tx: &Connection, agent: &Agent) -> Result<()> {
    let created = agent.created_at.format_rfc3339();
    tx.execute(
        &format!("INSERT INTO agents ({AGENT_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"),
        params![
            agent.name,
            agent.session_name,
            agent.backend,
            agent.task_file,
            agent.log_file,
            created,
            agent.spawned_by,
            agent.cwd,
            bool_to_int(agent.adopted)
        ],
    )
    .context("inserting agent")?;
    open_run_tx(s, tx, agent)
}

/// replace_agent_tx closes the run of the agent named agent.name, rewrites
/// its record from agent, and opens a new run: a resume. The record is
/// updated, not deleted, since deleting it would cascade to the agent's state.
pub(crate) fn replace_agent_tx(s: &Store, tx: &Connection, agent: &Agent) -> Result<()> {
    let found = close_run_tx(s, tx, &agent.name)?;
    if !found {
        return Err(anyhow!("no agent {} to replace", agent.name));
    }
    tx.execute(
        "UPDATE agents SET session_name = ?, backend = ?, task_file = ?, log_file = ?, created_at = ?, spawned_by = ?, cwd = ?, adopted = ? WHERE name = ?",
        params![
            agent.session_name,
            agent.backend,
            agent.task_file,
            agent.log_file,
            agent.created_at.format_rfc3339(),
            agent.spawned_by,
            agent.cwd,
            bool_to_int(agent.adopted),
            agent.name
        ],
    )
    .context("updating agent")?;
    open_run_tx(s, tx, agent)
}

/// open_run_tx opens a run for agent and appends its spawned event.
fn open_run_tx(s: &Store, tx: &Connection, agent: &Agent) -> Result<()> {
    tx.execute(
        "INSERT INTO runs (name, parent, backend, session, cwd, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        params![
            agent.name,
            agent.spawned_by,
            agent.backend,
            agent.session_name,
            agent.cwd,
            time_nanos(&agent.created_at)
        ],
    )
    .context("opening run")?;
    let run = tx.last_insert_rowid();
    insert_agent_event(
        s,
        tx,
        &AgentEvent {
            at: agent.created_at,
            kind: AGENT_SPAWNED.to_string(),
            agent: agent.name.clone(),
            run,
            ..AgentEvent::default()
        },
    )?;
    Ok(())
}

/// delete_agent_tx deletes an agent's record, closes its run (close_run_tx),
/// and appends its removed event.
pub(crate) fn delete_agent_tx(s: &Store, tx: &Connection, name: &str) -> Result<()> {
    if !close_run_tx(s, tx, name)? {
        return Ok(()); // no such agent
    }
    tx.execute("DELETE FROM agents WHERE name = ?", [name])
        .context("deleting agent")?;
    Ok(())
}

/// close_run_tx closes an agent's run with its last status and transcript,
/// and appends its removed event; the result is false if there is no such
/// agent. An agent spawned before runs were recorded gets a closed run from
/// its record.
///
/// Every statement that reads also writes: a transaction that starts with a
/// plain read can't take the write lock later while another process writes
/// (SQLITE_BUSY, which busy_timeout doesn't wait out).
fn close_run_tx(s: &Store, tx: &Connection, name: &str) -> Result<bool> {
    const LAST: &str = "COALESCE((SELECT value FROM state WHERE agent_name = ? AND key = ?), '')";
    let now = time_nanos(&Time::now());
    let args = params![
        now,
        name,
        "_status",
        name,
        "_transcript",
        name,
        "_end_reason",
        name,
        name
    ];
    let scan = |r: &Row<'_>| -> rusqlite::Result<(i64, String, String, String)> {
        Ok((int(r, 0)?, text(r, 1)?, text(r, 2)?, text(r, 3)?))
    };
    let mut found = tx
        .query_row(
            &format!(
                "UPDATE runs SET ended_at = ?, status = {LAST}, transcript = {LAST}, end_reason = {LAST}
		WHERE id = (SELECT MAX(id) FROM runs WHERE name = ? AND ended_at = '') AND EXISTS (SELECT 1 FROM agents WHERE name = ?)
		RETURNING id, status, transcript, end_reason"
            ),
            args,
            scan,
        )
        .optional()
        .context("closing run")?;
    if found.is_none() {
        found = tx
            .query_row(
                &format!(
                    "INSERT INTO runs (name, parent, backend, session, cwd, created_at, ended_at, status, transcript, end_reason)
			SELECT name, COALESCE(spawned_by, ''), backend, session_name, COALESCE(cwd, ''), created_at, ?, {LAST}, {LAST}, {LAST}
			FROM agents WHERE name = ? RETURNING id, status, transcript, end_reason"
                ),
                params![now, name, "_status", name, "_transcript", name, "_end_reason", name],
                scan,
            )
            .optional()
            .context("closing run")?;
    }
    let Some((run, status, transcript, end_reason)) = found else {
        return Ok(false);
    };
    // Why it was removed, when the remover said (cleanup
    // --approved-by-person). The transaction already holds the write lock.
    let detail: String = tx
        .query_row(
            "SELECT value FROM state WHERE agent_name = ? AND key = ?",
            params![name, STATE_REMOVED_WHY],
            |r| text(r, 0),
        )
        .optional()
        .context("reading why it was removed")?
        .unwrap_or_default();
    insert_agent_event(
        s,
        tx,
        &AgentEvent {
            at: parse_time(&now),
            kind: AGENT_REMOVED.to_string(),
            agent: name.to_string(),
            run,
            status,
            detail,
            transcript,
            end_reason,
            ..AgentEvent::default()
        },
    )?;
    Ok(true)
}

/// insert_agent_event appends ev and returns its id (the Go sets ev.ID). A
/// run of 0 means the agent's open run.
pub(crate) fn insert_agent_event(s: &Store, tx: &Connection, ev: &AgentEvent) -> Result<i64> {
    let run = if ev.run == 0 { None } else { Some(ev.run) };
    let id: i64 = tx
        .query_row(
            "
		INSERT INTO agent_events (at, kind, agent, run, status, event, detail, tool, tool_input, session_id, transcript, end_reason)
		VALUES (?, ?, ?, COALESCE(?, (SELECT MAX(id) FROM runs WHERE name = ? AND ended_at = ''), 0), ?, ?, ?, ?, ?, ?, ?, ?)
		RETURNING id",
            params![
                time_nanos(&ev.at),
                ev.kind,
                ev.agent,
                run,
                ev.agent,
                ev.status,
                ev.event,
                ev.detail,
                ev.tool,
                ev.tool_input,
                ev.session_id,
                ev.transcript,
                ev.end_reason
            ],
            |r| int(r, 0),
        )
        .context("recording agent event")?;
    if id % s.agent_events_prune_every == 0 {
        tx.execute(
            "DELETE FROM agent_events WHERE id <= ?",
            [id - s.agent_events_keep],
        )
        .context("pruning agent events")?;
    }
    Ok(id)
}

/// time_nanos formats t for the tree's tables: UTC with nanoseconds, since
/// a whole agent turn can fit in one second.
pub(crate) fn time_nanos(t: &Time) -> String {
    t.utc().format_rfc3339_nano()
}

fn scan_run(row: &Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        id: int(row, 0)?,
        name: text(row, 1)?,
        parent: text(row, 2)?,
        backend: text(row, 3)?,
        session: text(row, 4)?,
        cwd: text(row, 5)?,
        created_at: parse_time(&text(row, 6)?),
        ended_at: parse_time(&text(row, 7)?),
        status: text(row, 8)?,
        transcript: text(row, 9)?,
        end_reason: text(row, 10)?,
    })
}

fn scan_agent_event(row: &Row<'_>) -> rusqlite::Result<AgentEvent> {
    Ok(AgentEvent {
        id: int(row, 0)?,
        at: parse_time(&text(row, 1)?),
        kind: text(row, 2)?,
        agent: text(row, 3)?,
        run: int(row, 4)?,
        status: text(row, 5)?,
        event: text(row, 6)?,
        detail: text(row, 7)?,
        tool: text(row, 8)?,
        tool_input: text(row, 9)?,
        session_id: text(row, 10)?,
        transcript: text(row, 11)?,
        end_reason: text(row, 12)?,
    })
}

/// logged_spawns returns each name's newest successful spawn in the command
/// log, as a run with no id or end time.
fn logged_spawns(tx: &Connection) -> Result<BTreeMap<String, Run>> {
    let rows = query_all(
        tx,
        r#"SELECT started_at, agent, cwd, args FROM command_log
		WHERE args LIKE '["spawn",%' AND exit_code = 0 ORDER BY id"#,
        [],
        |r| Ok((text(r, 0)?, text(r, 1)?, text(r, 2)?, text(r, 3)?)),
    )
    .context("reading logged spawns")?;
    let mut out = BTreeMap::new();
    for (at, parent, cwd, args) in rows {
        let (name, backend) = spawn_args(&args);
        if name.is_empty() {
            continue;
        }
        out.insert(
            name.clone(),
            Run {
                session: format!("agent-{name}"),
                name,
                parent,
                backend,
                cwd,
                created_at: parse_time(&at),
                ..Run::default()
            },
        );
    }
    Ok(out)
}

/// spawn_args reads the agent name and --backend from a logged spawn's
/// arguments. The name is spawn's first argument that isn't a flag; only
/// --backend takes a value before it in practice.
pub(crate) fn spawn_args(raw: &str) -> (String, String) {
    let argv: Vec<String> = match serde_json::from_str::<Option<Vec<String>>>(raw) {
        Ok(v) => v.unwrap_or_default(),
        Err(_) => return (String::new(), String::new()),
    };
    if argv.len() < 2 || argv[0] != "spawn" {
        return (String::new(), String::new());
    }
    let (mut name, mut backend) = (String::new(), String::new());
    let mut i = 1;
    while i < argv.len() {
        let a = &argv[i];
        if (a == "--backend" || a == "--backend-config" || a == "--task") && i + 1 < argv.len() {
            if a == "--backend" {
                backend = argv[i + 1].clone();
            }
            i += 1;
        } else if let Some(b) = a.strip_prefix("--backend=") {
            backend = b.to_string();
        } else if a.starts_with('-') {
        } else if name.is_empty() {
            name = a.clone();
        }
        i += 1;
    }
    (name, backend)
}

fn sort_runs(runs: &mut [Run]) {
    runs.sort_by_key(|r| r.created_at);
}
