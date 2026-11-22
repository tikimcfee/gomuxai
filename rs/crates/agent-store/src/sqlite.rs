use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use rusqlite::types::{Type, ValueRef};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Params, Row, Transaction, params};

use crate::commands::COMMAND_LOG_SCHEMA;
use crate::dreams::DREAMS_SCHEMA;
use crate::friction::FRICTION_SCHEMA;
use crate::friction_links::FRICTION_LINKS_SCHEMA;
use crate::gotime::Time;
use crate::host_state::HOST_STATE_SCHEMA;
use crate::jobs::{JOBS_ADDED_COLUMNS, JOBS_SCHEMA, SCRIPTS_SCHEMA};
use crate::manage::MANAGE_SCHEMA;
use crate::models::*;
use crate::roster::ROSTER_SCHEMA;
use crate::session_reports::SESSION_REPORTS_SCHEMA;
use crate::tree::{
    AgentEvent, Schema, TREE_SCHEMA, create_agent_tx, delete_agent_tx, insert_agent_event,
    replace_agent_tx,
};

/// Store implements the Go `Storage` interface (and `SQLiteStorage`'s extra
/// methods) on SQLite. The Go keeps a connection pool; here one connection
/// behind a mutex serves a Store, and other processes (hooks, the CLI, the
/// bus) share the file through SQLite's own locking, as they do with Go.
pub struct Store {
    pub(crate) conn: Mutex<Connection>,
    /// schema is what a store opened read-only has; None for a migrated
    /// store, which has everything.
    pub(crate) schema: Option<Schema>,
    /// How many agent events and command records the tables keep, and how
    /// often an insert prunes them (Go package variables, per store here so
    /// tests can change them without touching each other).
    pub(crate) agent_events_keep: i64,
    pub(crate) agent_events_prune_every: i64,
    pub(crate) command_log_keep: i64,
    pub(crate) command_log_prune_every: i64,
}

/// The state change claim_pending_messages applies with the messages it
/// claimed (Go `func(claimed []*Message) StateUpdate`).
pub type ClaimUpdate<'a> = &'a mut dyn FnMut(&[Message]) -> StateUpdate;

/// new_sqlite_storage creates a new SQLite storage instance (Go
/// `NewSQLiteStorage`); `Store::open` is the same.
pub fn new_sqlite_storage(db_path: impl AsRef<Path>) -> Result<Store> {
    Store::open(db_path)
}

impl Store {
    fn with_conn(conn: Connection, schema: Option<Schema>) -> Store {
        Store {
            conn: Mutex::new(conn),
            schema,
            agent_events_keep: crate::tree::AGENT_EVENTS_KEEP,
            agent_events_prune_every: crate::tree::AGENT_EVENTS_PRUNE_EVERY,
            command_log_keep: crate::commands::COMMAND_LOG_KEEP,
            command_log_prune_every: crate::commands::COMMAND_LOG_PRUNE_EVERY,
        }
    }

    /// Opens (creating if need be) and migrates the store at db_path.
    pub fn open(db_path: impl AsRef<Path>) -> Result<Store> {
        // The Go opens lazily, so its first failure (a missing directory, say)
        // comes from the journal_mode statement; the words follow that.
        const WAL: &str = "executing PRAGMA journal_mode = WAL";
        let conn = Connection::open(db_path.as_ref()).context(WAL)?;
        // Per-connection pragmas, which the Go puts in the DSN so every pooled
        // connection gets them. busy_timeout: backend hooks write status from
        // several short-lived processes at once. foreign_keys: cleanup relies
        // on the agents → state cascade.
        conn.busy_timeout(Duration::from_millis(5000))
            .context(WAL)?;
        conn.pragma_update(None, "foreign_keys", 1).context(WAL)?;
        conn.pragma_update(None, "synchronous", "NORMAL")
            .context(WAL)?;

        // journal_mode is stored in the database file, so once is enough.
        conn.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
            .context(WAL)?;

        let s = Store::with_conn(conn, None);
        s.migrate().context("running migrations")?;
        Ok(s)
    }

    pub(crate) fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// migrate creates the database schema.
    fn migrate(&self) -> Result<()> {
        let schema = String::from(
            r#"
	CREATE TABLE IF NOT EXISTS agents (
		name TEXT PRIMARY KEY,
		session_name TEXT NOT NULL,
		backend TEXT NOT NULL,
		task_file TEXT,
		log_file TEXT,
		created_at TEXT NOT NULL,
		spawned_by TEXT,
		cwd TEXT,
		adopted INTEGER NOT NULL DEFAULT 0
	);

	CREATE TABLE IF NOT EXISTS messages (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		sender_id TEXT NOT NULL,
		receiver_id TEXT NOT NULL,
		content TEXT NOT NULL,
		status TEXT NOT NULL DEFAULT 'pending',
		created_at TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_messages_receiver_status
		ON messages(receiver_id, status);

	-- Tasks as standalone repository (not tied to agents)
	CREATE TABLE IF NOT EXISTS tasks (
		name TEXT PRIMARY KEY,
		content TEXT NOT NULL,
		source_file TEXT,
		created_at TEXT NOT NULL
	);

	-- State as key-value store
	CREATE TABLE IF NOT EXISTS state (
		agent_name TEXT NOT NULL,
		key TEXT NOT NULL,
		value TEXT NOT NULL,
		updated_at TEXT NOT NULL,
		PRIMARY KEY (agent_name, key),
		FOREIGN KEY (agent_name) REFERENCES agents(name) ON DELETE CASCADE
	);
	"#,
        ) + COMMAND_LOG_SCHEMA
            + FRICTION_SCHEMA
            + FRICTION_LINKS_SCHEMA
            + TREE_SCHEMA
            + SESSION_REPORTS_SCHEMA
            // The Rust build's own tables, which the Go never reads: adding
            // them leaves a store the Go opens as before.
            + JOBS_SCHEMA
            + SCRIPTS_SCHEMA
            + HOST_STATE_SCHEMA
            + MANAGE_SCHEMA
            + DREAMS_SCHEMA
            + ROSTER_SCHEMA;

        let conn = self.conn();
        conn.execute_batch(&schema)?;
        for (table, column, def) in [
            ("agents", "spawned_by", "TEXT"),
            ("agents", "cwd", "TEXT"),
            ("agents", "adopted", "INTEGER NOT NULL DEFAULT 0"),
            ("runs", "end_reason", "TEXT NOT NULL DEFAULT ''"),
            ("messages", "urgency", "TEXT NOT NULL DEFAULT 'soon'"),
            ("messages", "topic", "TEXT NOT NULL DEFAULT ''"),
            ("messages", "superseded_by", "INTEGER NOT NULL DEFAULT 0"),
            ("messages", "delivered_at", "TEXT NOT NULL DEFAULT ''"),
            ("messages", "delivery_via", "TEXT NOT NULL DEFAULT ''"),
            ("messages", "delivery_batch", "INTEGER NOT NULL DEFAULT 0"),
        ]
        .iter()
        .chain(JOBS_ADDED_COLUMNS)
        {
            add_column(&conn, table, column, def)?;
        }
        let added = add_column(
            &conn,
            "agent_events",
            "end_reason",
            "TEXT NOT NULL DEFAULT ''",
        )?;
        if !added {
            return Ok(());
        }
        // The status for a session no CLI runs any more is "ended" (beside an
        // end reason); stores written before end reasons called it "exited".
        // Nothing recorded how those ended, which is what "gone" means.
        conn.execute_batch(
            r#"
		INSERT OR REPLACE INTO state (agent_name, key, value, updated_at)
			SELECT agent_name, '_end_reason', 'gone', updated_at FROM state WHERE key = '_status' AND value = 'exited';
		UPDATE state SET value = 'ended' WHERE key = '_status' AND value = 'exited';
		UPDATE runs SET status = 'ended', end_reason = 'gone' WHERE status = 'exited';
		UPDATE agent_events SET status = 'ended', end_reason = 'gone' WHERE status = 'exited';"#,
        )?;
        Ok(())
    }

    /// exec runs a statement, wrapping any error with what it was doing.
    pub(crate) fn exec<P: Params>(&self, what: &str, query: &str, args: P) -> Result<usize> {
        self.conn()
            .execute(query, args)
            .with_context(|| what.to_string())
    }

    /// insert runs an INSERT and returns the new row's id.
    pub(crate) fn insert<P: Params>(&self, what: &str, query: &str, args: P) -> Result<i64> {
        let conn = self.conn();
        conn.execute(query, args)
            .with_context(|| what.to_string())?;
        Ok(conn.last_insert_rowid())
    }

    /// in_tx runs f in a transaction, committing if it succeeds.
    pub(crate) fn in_tx<T>(&self, f: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.conn();
        let tx = conn.transaction().context("beginning transaction")?;
        let v = f(&tx)?;
        tx.commit().context("committing")?;
        Ok(v)
    }

    /// create_agent creates a new agent record, opens its run, and records
    /// its spawned event, in one transaction.
    pub fn create_agent(&self, agent: &Agent) -> Result<()> {
        self.in_tx(|tx| create_agent_tx(self, tx, agent))
    }

    /// get_agent retrieves an agent by name.
    pub fn get_agent(&self, name: &str) -> Result<Option<Agent>> {
        query_one(
            &self.conn(),
            &format!("SELECT {AGENT_COLUMNS} FROM agents WHERE name = ?"),
            [name],
            scan_agent,
        )
        .context("querying agent")
    }

    /// list_agents returns all agents.
    pub fn list_agents(&self) -> Result<Vec<Agent>> {
        query_all(
            &self.conn(),
            &format!("SELECT {AGENT_COLUMNS} FROM agents ORDER BY created_at DESC"),
            [],
            scan_agent,
        )
        .context("querying agents")
    }

    /// delete_agent removes an agent record, closes its run, and records its
    /// removed event, in one transaction.
    pub fn delete_agent(&self, name: &str) -> Result<()> {
        self.in_tx(|tx| delete_agent_tx(self, tx, name))
    }

    /// replace_agent closes the run of the agent named agent.name and opens a
    /// new one under agent, in one transaction: a resume. The agent's state
    /// and messages are untouched, so they carry over.
    pub fn replace_agent(&self, agent: &Agent) -> Result<()> {
        self.in_tx(|tx| replace_agent_tx(self, tx, agent))
    }

    /// replace_agent_with is replace_agent with state changes around it, all
    /// in one transaction (agent revive's take-over): before is applied
    /// first (how the closing run ended, which closing it records), then the
    /// run is replaced, then after (what the new run starts with). Either all
    /// of it is recorded or none.
    pub fn replace_agent_with(
        &self,
        agent: &Agent,
        before: &StateUpdate,
        after: &StateUpdate,
    ) -> Result<()> {
        let apply = |tx: &Connection, u: &StateUpdate| -> Result<()> {
            apply_state_update(tx, &u.entries, &u.agent_name, &u.delete)?;
            if let Some(ev) = &u.event {
                insert_agent_event(self, tx, ev)?;
            }
            Ok(())
        };
        self.in_tx(|tx| {
            apply(tx, before)?;
            replace_agent_tx(self, tx, agent)?;
            apply(tx, after)
        })
    }

    /// update_agent_backend updates an agent's backend and its open run's
    /// backend in place without closing the run (e.g. an adopted shell pane
    /// where a CLI was later started).
    pub fn update_agent_backend(&self, name: &str, backend: &str) -> Result<()> {
        self.in_tx(|tx| {
            tx.execute("UPDATE agents SET backend = ? WHERE name = ?", [backend, name])
                .context("updating agent backend")?;
            tx.execute(
                "UPDATE runs SET backend = ? WHERE id = (SELECT MAX(id) FROM runs WHERE name = ? AND ended_at = '')",
                [backend, name],
            )
            .context("updating run backend")?;
            Ok(())
        })
    }

    /// create_message creates a new message record and sets msg.id.
    pub fn create_message(&self, msg: &mut Message) -> Result<()> {
        let _ = self.create_message_replacing(msg, false, 0)?;
        Ok(())
    }

    /// Atomically inserts `msg` and optionally marks earlier `pending` messages
    /// for `msg.receiver_id` as `superseded` (`superseded_by = new_id`):
    /// - `supersede_id > 0`: supersedes that specific pending message; if it
    ///   was already delivered, returns `Some(target)` without mutating it.
    /// - `replace_sender` (or non-empty `msg.topic`): supersedes still-pending
    ///   messages from `msg.sender_id` (scoped to `msg.topic` when set); if
    ///   `replace_sender` found nothing pending and `already_delivered` is not
    ///   set, returns the sender's latest already-delivered message to this
    ///   receiver so the caller learns it escaped.
    pub fn create_message_replacing(
        &self,
        msg: &mut Message,
        replace_sender: bool,
        supersede_id: i64,
    ) -> Result<(Vec<i64>, Option<Message>)> {
        let urgency = if msg.urgency.is_empty() || msg.urgency == "normal" {
            URGENCY_SOON.to_string()
        } else {
            msg.urgency.clone()
        };
        let created_str = msg.created_at.format_rfc3339();
        let delivered_str = if msg.delivered_at.is_zero() {
            String::new()
        } else {
            msg.delivered_at.format_rfc3339_nano()
        };

        let (new_id, superseded_ids, already_delivered) = self.in_tx(|tx| {
            tx.execute(
                "INSERT INTO messages (sender_id, receiver_id, content, status, created_at, urgency, topic, superseded_by, delivered_at, delivery_via, delivery_batch) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                params![
                    msg.sender_id,
                    msg.receiver_id,
                    msg.content,
                    msg.status,
                    created_str,
                    urgency,
                    msg.topic,
                    msg.superseded_by,
                    delivered_str,
                    msg.delivery_via,
                    msg.delivery_batch,
                ],
            )
            .context("inserting message")?;
            let new_id = tx.last_insert_rowid();
            let mut superseded = Vec::new();
            let mut already_delivered: Option<Message> = None;

            if supersede_id > 0
                && supersede_id != new_id
                && let Some(target) = query_one(
                    tx,
                    &format!(
                        "SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ? AND receiver_id = ?"
                    ),
                    params![supersede_id, msg.receiver_id],
                    scan_message,
                )
                .context("looking up message to supersede")?
            {
                if target.status == MESSAGE_STATUS_PENDING {
                    let n = tx
                        .execute(
                            "UPDATE messages SET status = 'superseded', superseded_by = ? WHERE id = ? AND status = 'pending'",
                            params![new_id, supersede_id],
                        )
                        .context("superseding message")?;
                    if n == 1 {
                        superseded.push(supersede_id);
                    }
                } else if target.status == MESSAGE_STATUS_DELIVERED
                    || target.status == MESSAGE_STATUS_HANDOVER
                {
                    already_delivered = Some(target);
                }
            }

            if replace_sender || !msg.topic.is_empty() {
                let rows: Vec<i64> = if !msg.topic.is_empty() {
                    query_all(
                        tx,
                        "UPDATE messages SET status = 'superseded', superseded_by = ? WHERE receiver_id = ? AND sender_id = ? AND topic = ? AND status = 'pending' AND id != ? RETURNING id",
                        params![
                            new_id,
                            msg.receiver_id,
                            msg.sender_id,
                            msg.topic,
                            new_id
                        ],
                        |row| int(row, 0),
                    )
                    .context("superseding topic messages")?
                } else {
                    query_all(
                        tx,
                        "UPDATE messages SET status = 'superseded', superseded_by = ? WHERE receiver_id = ? AND sender_id = ? AND status = 'pending' AND id != ? RETURNING id",
                        params![new_id, msg.receiver_id, msg.sender_id, new_id],
                        |row| int(row, 0),
                    )
                    .context("superseding sender messages")?
                };
                for id in rows {
                    if !superseded.contains(&id) {
                        superseded.push(id);
                    }
                }

                if (replace_sender || !msg.topic.is_empty())
                    && superseded.is_empty()
                    && already_delivered.is_none()
                {
                    already_delivered = if !msg.topic.is_empty() {
                        query_one(
                            tx,
                            &format!(
                                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE receiver_id = ? AND sender_id = ? AND topic = ? AND status IN ('delivered', 'handover') AND id != ? ORDER BY id DESC LIMIT 1"
                            ),
                            params![msg.receiver_id, msg.sender_id, msg.topic, new_id],
                            scan_message,
                        )
                        .context("checking previous delivered topic message")?
                    } else {
                        query_one(
                            tx,
                            &format!(
                                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE receiver_id = ? AND sender_id = ? AND status IN ('delivered', 'handover') AND id != ? ORDER BY id DESC LIMIT 1"
                            ),
                            params![msg.receiver_id, msg.sender_id, new_id],
                            scan_message,
                        )
                        .context("checking previous delivered sender message")?
                    };
                }
            }

            superseded.sort_unstable();
            Ok((new_id, superseded, already_delivered))
        })?;
        msg.id = new_id;
        msg.urgency = urgency;
        Ok((superseded_ids, already_delivered))
    }

    /// get_message returns message `id`, or None if no such row exists.
    pub fn get_message(&self, id: i64) -> Result<Option<Message>> {
        query_one(
            &self.conn(),
            &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?"),
            [id],
            scan_message,
        )
        .with_context(|| format!("reading message {id}"))
    }

    /// list_messages returns up to `limit` newest messages for `receiver_id`,
    /// newest first.
    pub fn list_messages(&self, receiver_id: &str, limit: i64) -> Result<Vec<Message>> {
        query_all(
            &self.conn(),
            &format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE receiver_id = ? ORDER BY id DESC LIMIT ?"
            ),
            params![receiver_id, limit],
            scan_message,
        )
        .context("listing messages")
    }

    /// has_interrupt_pending returns the newest pending interrupt-urgency
    /// message for `receiver_id`, if any.
    pub fn has_interrupt_pending(&self, receiver_id: &str) -> Result<Option<Message>> {
        query_one(
            &self.conn(),
            &format!(
                "SELECT {MESSAGE_COLUMNS} FROM messages WHERE receiver_id = ? AND status = 'pending' AND urgency = 'interrupt' ORDER BY id DESC LIMIT 1"
            ),
            [receiver_id],
            scan_message,
        )
        .context("checking pending interrupt messages")
    }

    /// get_pending_messages retrieves pending messages for a receiver, oldest
    /// first.
    pub fn get_pending_messages(&self, receiver_id: &str, limit: i64) -> Result<Vec<Message>> {
        // created_at has whole seconds; id keeps messages queued within one
        // second in the order they were queued.
        query_all(
            &self.conn(),
            &format!(
                "
		SELECT {MESSAGE_COLUMNS}
		FROM messages
		WHERE receiver_id = ? AND status = 'pending'
		ORDER BY created_at ASC, id ASC
		LIMIT ?
	"
            ),
            params![receiver_id, limit],
            scan_message,
        )
        .context("querying messages")
    }

    /// update_message_status updates the status of a message.
    pub fn update_message_status(&self, id: i64, status: MessageStatus) -> Result<()> {
        self.exec(
            "updating message status",
            "UPDATE messages SET status = ? WHERE id = ?",
            params![status, id],
        )?;
        Ok(())
    }

    /// claim_message marks a pending message delivered, reporting whether
    /// this call did: of two callers racing to deliver it, only one gets true.
    pub fn claim_message(&self, id: i64) -> Result<bool> {
        let now_str = Time::now().format_rfc3339_nano();
        let n = self
            .conn()
            .execute(
                "UPDATE messages SET status = 'delivered', delivered_at = ? WHERE id = ? AND status = 'pending'",
                params![now_str, id],
            )
            .context("claiming message")?;
        Ok(n == 1)
    }

    /// Claims a single pending message `id` (plus any unreported `superseded`
    /// rows whose `superseded_by` transitively resolves to `id`) without
    /// claiming unrelated pending rows — used when `send --now` runs while a
    /// human has draft text in the input box so held rows keep waiting.
    pub fn claim_single_message_batch(&self, id: i64, delivery_via: &str) -> Result<Vec<Message>> {
        self.in_tx(|tx| {
            let now_str = Time::now().format_rfc3339_nano();
            let Some(msg) = query_one(
                tx,
                &format!(
                    "UPDATE messages SET status = 'delivered', delivered_at = ?, delivery_via = ?, delivery_batch = ? WHERE id = ? AND status = 'pending' RETURNING {MESSAGE_COLUMNS}"
                ),
                params![now_str, delivery_via, id, id],
                scan_message,
            )
            .with_context(|| format!("claiming message {id}"))?
            else {
                return Ok(Vec::new());
            };
            let receiver_id = msg.receiver_id.clone();
            let mut batch = vec![msg];
            let sup_candidates = query_all(
                tx,
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE receiver_id = ? AND status = 'superseded' AND delivery_batch = 0"
                ),
                [receiver_id.as_str()],
                scan_message,
            )
            .context("reading superseded candidates")?;
            let included = transitive_superseded_ids(&[id], &sup_candidates);
            for mut s in sup_candidates {
                if included.contains(&s.id) {
                    tx.execute(
                        "UPDATE messages SET delivered_at = ?, delivery_via = ?, delivery_batch = ? WHERE id = ? AND status = 'superseded'",
                        params![now_str, delivery_via, id, s.id],
                    )
                    .context("stamping superseded batch")?;
                    s.delivered_at = parse_time(&now_str);
                    s.delivery_via = delivery_via.to_string();
                    s.delivery_batch = id;
                    batch.push(s);
                }
            }
            batch.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
            Ok(batch)
        })
    }

    /// claim_handover marks receiver_id's handover message id delivered and
    /// returns it, or None if no such message awaits handover: of two
    /// callers, only one gets it.
    pub fn claim_handover(&self, receiver_id: &str, id: i64) -> Result<Option<Message>> {
        let now_str = Time::now().format_rfc3339_nano();
        query_one(
            &self.conn(),
            &format!(
                "
		UPDATE messages SET status = 'delivered', delivered_at = ?, delivery_via = 'handover', delivery_batch = ?
		WHERE id = ? AND receiver_id = ? AND status = 'handover'
		RETURNING {MESSAGE_COLUMNS}"
            ),
            params![now_str, id, id, receiver_id],
            scan_message,
        )
        .with_context(|| format!("claiming message {id}"))
    }

    /// fail_handover marks handover message id failed, reporting whether this
    /// call did: false if it was handed over first.
    pub fn fail_handover(&self, id: i64) -> Result<bool> {
        let n = self
            .conn()
            .execute(
                "UPDATE messages SET status = 'failed' WHERE id = ? AND status = 'handover'",
                [id],
            )
            .with_context(|| format!("failing message {id}"))?;
        Ok(n == 1)
    }

    /// get_message_status returns message id's status.
    pub fn get_message_status(&self, id: i64) -> Result<MessageStatus> {
        let status = self
            .conn()
            .query_row("SELECT status FROM messages WHERE id = ?", [id], |row| {
                text(row, 0)
            })
            .map_err(go_sql_error)
            .with_context(|| format!("reading message {id}"))?;
        Ok(MessageStatus::from(status))
    }

    /// claim_pending_messages marks every pending message for receiver_id
    /// delivered and returns them, oldest first. If update is given, the state
    /// change it returns for them is applied in the same transaction.
    pub fn claim_pending_messages(
        &self,
        receiver_id: &str,
        update: Option<ClaimUpdate<'_>>,
    ) -> Result<Vec<Message>> {
        let batch = self.claim_pending_batch_matching(receiver_id, ClaimFilter::All, "", update)?;
        Ok(batch
            .into_iter()
            .filter(|m| m.status == MESSAGE_STATUS_DELIVERED)
            .collect())
    }

    /// Convenience wrapper claiming all pending + unreported superseded rows.
    pub fn claim_pending_batch(
        &self,
        receiver_id: &str,
        delivery_via: &str,
        update: Option<ClaimUpdate<'_>>,
    ) -> Result<Vec<Message>> {
        self.claim_pending_batch_matching(receiver_id, ClaimFilter::All, delivery_via, update)
    }

    /// Atomically claims pending messages matching `filter` for `receiver_id`
    /// (stamping `delivered_at`, `delivery_via`, and `delivery_batch`), plus
    /// unreported `superseded` rows (`delivery_batch = 0`) whose replacement is
    /// in this claimed batch (or all unreported `superseded` rows when
    /// `filter == ClaimFilter::All`).
    pub fn claim_pending_batch_matching(
        &self,
        receiver_id: &str,
        filter: ClaimFilter,
        delivery_via: &str,
        update: Option<ClaimUpdate<'_>>,
    ) -> Result<Vec<Message>> {
        let mut conn = self.conn();
        let tx = conn.transaction().context("beginning transaction")?;
        let now_str = Time::now().format_rfc3339_nano();
        let filter_clause = match filter {
            ClaimFilter::All => "",
            ClaimFilter::NonLater => " AND urgency != 'later'",
        };

        let mut claimed = query_all(
            &tx,
            &format!(
                "
		UPDATE messages
		SET status = 'delivered', delivered_at = ?, delivery_via = ?
		WHERE receiver_id = ? AND status = 'pending'{filter_clause}
		RETURNING {MESSAGE_COLUMNS}"
            ),
            params![now_str, delivery_via, receiver_id],
            scan_message,
        )
        .context("claiming messages")?;

        if !claimed.is_empty() {
            let batch_id = claimed.iter().map(|m| m.id).max().unwrap_or(0);
            let active_ids: Vec<i64> = claimed.iter().map(|m| m.id).collect();
            for m in &mut claimed {
                m.delivery_batch = batch_id;
                tx.execute(
                    "UPDATE messages SET delivery_batch = ? WHERE id = ?",
                    params![batch_id, m.id],
                )
                .context("stamping delivery batch")?;
            }

            let sup_candidates = query_all(
                &tx,
                &format!(
                    "SELECT {MESSAGE_COLUMNS} FROM messages WHERE receiver_id = ? AND status = 'superseded' AND delivery_batch = 0"
                ),
                [receiver_id],
                scan_message,
            )
            .context("reading superseded messages for batch")?;
            let included = if filter == ClaimFilter::All {
                sup_candidates.iter().map(|s| s.id).collect()
            } else {
                transitive_superseded_ids(&active_ids, &sup_candidates)
            };
            for mut s in sup_candidates {
                if included.contains(&s.id) {
                    tx.execute(
                        "UPDATE messages SET delivered_at = ?, delivery_via = ?, delivery_batch = ? WHERE id = ? AND status = 'superseded'",
                        params![now_str, delivery_via, batch_id, s.id],
                    )
                    .context("claiming superseded message for batch")?;
                    s.delivered_at = parse_time(&now_str);
                    s.delivery_via = delivery_via.to_string();
                    s.delivery_batch = batch_id;
                    claimed.push(s);
                }
            }
            // RETURNING has no order; keep exact chronological queue order (oldest first).
            claimed.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        }

        if let Some(update) = update {
            let u = update(&claimed);
            apply_state_update(&tx, &u.entries, &u.agent_name, &u.delete)?;
            if let Some(ev) = &u.event {
                insert_agent_event(self, &tx, ev)?;
            }
        }
        tx.commit().context("committing claim")?;
        Ok(claimed)
    }

    /// Puts previously claimed active messages back to `pending` (clearing
    /// `delivered_at`, `delivery_via`, `delivery_batch`), and clears
    /// `delivered_at` / `delivery_via` / `delivery_batch` on any `superseded`
    /// rows from that batch so they ride along with the retried delivery.
    pub fn requeue_batch(&self, msgs: &[Message]) -> Result<()> {
        self.in_tx(|tx| {
            for m in msgs {
                if m.status == MESSAGE_STATUS_SUPERSEDED {
                    tx.execute(
                        "UPDATE messages SET delivered_at = '', delivery_via = '', delivery_batch = 0 WHERE id = ? AND status = 'superseded'",
                        [m.id],
                    )
                    .context("requeueing superseded message")?;
                } else {
                    tx.execute(
                        "UPDATE messages SET status = 'pending', delivered_at = '', delivery_via = '', delivery_batch = 0 WHERE id = ?",
                        [m.id],
                    )
                    .context("requeueing message")?;
                }
            }
            Ok(())
        })
    }

    /// Aborts a newly inserted send (`failed_id`) whose immediate typing failed:
    /// marks `failed_id` `failed` (so it never lingers as a ghost `pending`
    /// message), restores any rows in `superseded_ids` back to `pending`
    /// (`superseded_by = 0`), and requeues any *earlier* held rows in `claimed`.
    pub fn abort_failed_send(
        &self,
        failed_id: i64,
        superseded_ids: &[i64],
        claimed: &[Message],
    ) -> Result<()> {
        self.in_tx(|tx| {
            for m in claimed {
                if m.id == failed_id || superseded_ids.contains(&m.id) {
                    continue;
                }
                if m.status == MESSAGE_STATUS_SUPERSEDED {
                    tx.execute(
                        "UPDATE messages SET delivered_at = '', delivery_via = '', delivery_batch = 0 WHERE id = ? AND status = 'superseded'",
                        [m.id],
                    )
                    .context("requeueing superseded message")?;
                } else {
                    tx.execute(
                        "UPDATE messages SET status = 'pending', delivered_at = '', delivery_via = '', delivery_batch = 0 WHERE id = ?",
                        [m.id],
                    )
                    .context("requeueing message")?;
                }
            }
            for &sup_id in superseded_ids {
                tx.execute(
                    "UPDATE messages SET status = 'pending', superseded_by = 0, delivered_at = '', delivery_via = '', delivery_batch = 0 WHERE id = ? AND superseded_by = ?",
                    params![sup_id, failed_id],
                )
                .context("restoring superseded message after failed send")?;
            }
            tx.execute(
                "UPDATE messages SET status = 'failed', delivered_at = '', delivery_via = '', delivery_batch = 0 WHERE id = ?",
                [failed_id],
            )
            .context("marking aborted send failed")?;
            Ok(())
        })
    }

    /// delete_messages_by_agent removes all messages involving an agent.
    pub fn delete_messages_by_agent(&self, agent_name: &str) -> Result<()> {
        self.exec(
            "deleting messages",
            "DELETE FROM messages WHERE sender_id = ? OR receiver_id = ?",
            [agent_name, agent_name],
        )?;
        Ok(())
    }

    /// close closes the database connection. Dropping the Store does the same,
    /// without the error.
    pub fn close(self) -> Result<()> {
        let conn = self.conn.into_inner().unwrap_or_else(|e| e.into_inner());
        conn.close().map_err(|(_, e)| anyhow!(e))
    }

    /// set_task creates or updates a task in the repository.
    pub fn set_task(&self, task: &Task) -> Result<()> {
        self.exec(
            "setting task",
            &format!(
                "
		INSERT INTO tasks ({TASK_COLUMNS})
		VALUES (?, ?, ?, ?)
		ON CONFLICT(name) DO UPDATE SET
			content = excluded.content,
			source_file = excluded.source_file,
			created_at = excluded.created_at
	"
            ),
            params![
                task.name,
                task.content,
                task.source_file,
                task.created_at.format_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// get_task retrieves a task by name.
    pub fn get_task(&self, name: &str) -> Result<Option<Task>> {
        query_one(
            &self.conn(),
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE name = ?"),
            [name],
            scan_task,
        )
        .context("querying task")
    }

    /// list_tasks retrieves all tasks.
    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        query_all(
            &self.conn(),
            &format!("SELECT {TASK_COLUMNS} FROM tasks ORDER BY name ASC"),
            [],
            scan_task,
        )
        .context("querying tasks")
    }

    /// delete_task removes a task by name.
    pub fn delete_task(&self, name: &str) -> Result<()> {
        self.exec("deleting task", "DELETE FROM tasks WHERE name = ?", [name])?;
        Ok(())
    }

    /// set_state creates or updates a state entry for an agent.
    pub fn set_state(&self, entry: &StateEntry) -> Result<()> {
        self.exec("setting state", SET_STATE_QUERY, state_args(entry))?;
        Ok(())
    }

    /// set_states creates or updates several state entries atomically, so
    /// readers never see a mix of two concurrent writes.
    pub fn set_states(&self, entries: &[StateEntry]) -> Result<()> {
        self.update_states(entries, "", &[])
    }

    /// update_states sets entries and deletes agent_name's keys in del, in one
    /// transaction.
    pub fn update_states(
        &self,
        entries: &[StateEntry],
        agent_name: &str,
        del: &[String],
    ) -> Result<()> {
        self.record_report(entries, agent_name, del, None)
    }

    /// record_report is update_states that also appends ev (if given) to the
    /// agent events, in the same transaction.
    pub fn record_report(
        &self,
        entries: &[StateEntry],
        agent_name: &str,
        del: &[String],
        ev: Option<&AgentEvent>,
    ) -> Result<()> {
        self.in_tx(|tx| {
            apply_state_update(tx, entries, agent_name, del)?;
            match ev {
                None => Ok(()),
                Some(ev) => insert_agent_event(self, tx, ev).map(|_| ()),
            }
        })
    }

    /// get_state retrieves a single state entry.
    pub fn get_state(&self, agent_name: &str, key: &str) -> Result<Option<StateEntry>> {
        query_one(
            &self.conn(),
            &format!("SELECT {STATE_COLUMNS} FROM state WHERE agent_name = ? AND key = ?"),
            [agent_name, key],
            scan_state,
        )
        .context("querying state")
    }

    /// list_state retrieves all state entries for an agent.
    pub fn list_state(&self, agent_name: &str) -> Result<Vec<StateEntry>> {
        query_all(
            &self.conn(),
            &format!("SELECT {STATE_COLUMNS} FROM state WHERE agent_name = ? ORDER BY key ASC"),
            [agent_name],
            scan_state,
        )
        .context("querying state")
    }

    /// delete_state removes all state entries for an agent.
    pub fn delete_state(&self, agent_name: &str) -> Result<()> {
        self.exec(
            "deleting state",
            "DELETE FROM state WHERE agent_name = ?",
            [agent_name],
        )?;
        Ok(())
    }

    /// delete_state_key removes one state entry for an agent.
    pub fn delete_state_key(&self, agent_name: &str, key: &str) -> Result<()> {
        self.exec(
            "deleting state",
            "DELETE FROM state WHERE agent_name = ? AND key = ?",
            [agent_name, key],
        )?;
        Ok(())
    }

    /// Opens a store read-only without checking it is one (the Go
    /// `OpenReadOnly` is in tree.rs); used there.
    pub(crate) fn open_read_only_conn(db_path: &Path) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_millis(5000))?;
        Ok(conn)
    }

    pub(crate) fn from_read_only(conn: Connection, schema: Schema) -> Store {
        Store::with_conn(conn, Some(schema))
    }
}

/// add_column adds a column that databases created before it lack. The
/// result is true when this call added it.
fn add_column(conn: &Connection, table: &str, column: &str, def: &str) -> Result<bool> {
    let has = || -> rusqlite::Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pragma_table_info(?) WHERE name = ?",
            [table, column],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    };
    if has()? {
        return Ok(false);
    }
    if let Err(e) = conn.execute(
        &format!("ALTER TABLE {table} ADD COLUMN {column} {def}"),
        [],
    ) {
        // Another process may have added it first.
        if has().unwrap_or(false) {
            return Ok(false);
        }
        return Err(e.into());
    }
    Ok(true)
}

/// go_sql_error words a missing row as Go's database/sql does.
pub(crate) fn go_sql_error(e: rusqlite::Error) -> anyhow::Error {
    match e {
        rusqlite::Error::QueryReturnedNoRows => anyhow!("sql: no rows in result set"),
        e => e.into(),
    }
}

/// query_one runs a single-row query, returning None (and no error) when no
/// row matches.
pub(crate) fn query_one<T, P: Params>(
    conn: &Connection,
    query: &str,
    args: P,
    scan: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Option<T>> {
    conn.query_row(query, args, scan).optional()
}

/// query_all runs a query and scans every row.
pub(crate) fn query_all<T, P: Params>(
    conn: &Connection,
    query: &str,
    args: P,
    scan: fn(&Row<'_>) -> rusqlite::Result<T>,
) -> rusqlite::Result<Vec<T>> {
    let mut stmt = conn.prepare(query)?;
    let rows = stmt.query_map(args, scan)?;
    rows.collect()
}

/// text reads a column as Go's Scan into a string does: any value but NULL,
/// numbers in decimal.
pub(crate) fn text(row: &Row<'_>, i: usize) -> rusqlite::Result<String> {
    match row.get_ref(i)? {
        ValueRef::Text(b) | ValueRef::Blob(b) => Ok(String::from_utf8_lossy(b).into_owned()),
        ValueRef::Integer(n) => Ok(n.to_string()),
        ValueRef::Real(f) => Ok(f.to_string()),
        ValueRef::Null => Err(null_error(row, i)),
    }
}

/// null_text reads a column as Go's sql.NullString: NULL is "".
pub(crate) fn null_text(row: &Row<'_>, i: usize) -> rusqlite::Result<String> {
    match row.get_ref(i)? {
        ValueRef::Null => Ok(String::new()),
        _ => text(row, i),
    }
}

/// int reads a column as Go's Scan into an int64 does: an integer, or text
/// that is one.
pub(crate) fn int(row: &Row<'_>, i: usize) -> rusqlite::Result<i64> {
    match row.get_ref(i)? {
        ValueRef::Integer(n) => Ok(n),
        ValueRef::Real(f) if f.fract() == 0.0 => Ok(f as i64),
        ValueRef::Text(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| rusqlite::Error::InvalidColumnType(i, column_name(row, i), Type::Text)),
        ValueRef::Real(_) => Err(rusqlite::Error::InvalidColumnType(
            i,
            column_name(row, i),
            Type::Real,
        )),
        ValueRef::Blob(_) => Err(rusqlite::Error::InvalidColumnType(
            i,
            column_name(row, i),
            Type::Blob,
        )),
        ValueRef::Null => Err(null_error(row, i)),
    }
}

/// null_int reads a column as Go's sql.NullInt64: NULL is 0.
pub(crate) fn null_int(row: &Row<'_>, i: usize) -> rusqlite::Result<i64> {
    match row.get_ref(i)? {
        ValueRef::Null => Ok(0),
        _ => int(row, i),
    }
}

fn column_name(row: &Row<'_>, i: usize) -> String {
    row.as_ref()
        .column_name(i)
        .map(str::to_string)
        .unwrap_or_default()
}

fn null_error(row: &Row<'_>, i: usize) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(i, column_name(row, i), Type::Null)
}

/// parse_time reads a stored timestamp. RFC3339 parsing also accepts the
/// fractional seconds that state timestamps carry.
pub(crate) fn parse_time(s: &str) -> Time {
    Time::parse_or_zero(s)
}

pub(crate) const AGENT_COLUMNS: &str =
    "name, session_name, backend, task_file, log_file, created_at, spawned_by, cwd, adopted";

pub(crate) fn scan_agent(row: &Row<'_>) -> rusqlite::Result<Agent> {
    Ok(Agent {
        name: text(row, 0)?,
        session_name: text(row, 1)?,
        backend: text(row, 2)?,
        task_file: null_text(row, 3)?,
        log_file: null_text(row, 4)?,
        created_at: parse_time(&text(row, 5)?),
        spawned_by: null_text(row, 6)?,
        cwd: null_text(row, 7)?,
        adopted: null_int(row, 8)? != 0,
    })
}

fn transitive_superseded_ids(
    active_ids: &[i64],
    superseded: &[Message],
) -> std::collections::HashSet<i64> {
    let mut reachable: std::collections::HashSet<i64> = active_ids.iter().copied().collect();
    let mut included = std::collections::HashSet::new();
    loop {
        let mut grew = false;
        for s in superseded {
            if !included.contains(&s.id) && reachable.contains(&s.superseded_by) {
                included.insert(s.id);
                reachable.insert(s.id);
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }
    included
}

const MESSAGE_COLUMNS: &str = "id, sender_id, receiver_id, content, status, created_at, urgency, topic, superseded_by, delivered_at, delivery_via, delivery_batch";

fn scan_message(row: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: int(row, 0)?,
        sender_id: text(row, 1)?,
        receiver_id: text(row, 2)?,
        content: text(row, 3)?,
        status: MessageStatus::from(text(row, 4)?),
        created_at: parse_time(&text(row, 5)?),
        urgency: null_text(row, 6)?,
        topic: null_text(row, 7)?,
        superseded_by: null_int(row, 8)?,
        delivered_at: parse_time(&null_text(row, 9)?),
        delivery_via: null_text(row, 10)?,
        delivery_batch: null_int(row, 11)?,
    })
}

const TASK_COLUMNS: &str = "name, content, source_file, created_at";

fn scan_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        name: text(row, 0)?,
        content: text(row, 1)?,
        source_file: null_text(row, 2)?,
        created_at: parse_time(&text(row, 3)?),
    })
}

pub(crate) const STATE_COLUMNS: &str = "agent_name, key, value, updated_at";

pub(crate) fn scan_state(row: &Row<'_>) -> rusqlite::Result<StateEntry> {
    Ok(StateEntry {
        agent_name: text(row, 0)?,
        key: text(row, 1)?,
        value: text(row, 2)?,
        updated_at: parse_time(&text(row, 3)?),
    })
}

/// apply_state_update sets entries and deletes agent_name's keys in del, in tx.
pub(crate) fn apply_state_update(
    tx: &Connection,
    entries: &[StateEntry],
    agent_name: &str,
    del: &[String],
) -> Result<()> {
    for entry in entries {
        tx.execute(SET_STATE_QUERY, state_args(entry))
            .with_context(|| format!("setting state {}", entry.key))?;
    }
    for key in del {
        tx.execute(
            "DELETE FROM state WHERE agent_name = ? AND key = ?",
            [agent_name, key.as_str()],
        )
        .with_context(|| format!("deleting state {key}"))?;
    }
    Ok(())
}

const SET_STATE_QUERY: &str = "
	INSERT INTO state (agent_name, key, value, updated_at)
	VALUES (?, ?, ?, ?)
	ON CONFLICT(agent_name, key) DO UPDATE SET
		value = excluded.value,
		updated_at = excluded.updated_at
";

fn state_args(entry: &StateEntry) -> [String; 4] {
    // Nanoseconds: `agent wait --next` tells reports apart by timestamp, and
    // a whole agent turn can fit inside one second.
    [
        entry.agent_name.clone(),
        entry.key.clone(),
        entry.value.clone(),
        entry.updated_at.format_rfc3339_nano(),
    ]
}
