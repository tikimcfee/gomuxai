//! The Go `Storage` interface has one implementation, `SQLiteStorage`, so
//! here it is one struct, [`Store`](crate::Store), whose methods are the
//! interface's in snake_case with the same parameters and results, less the
//! `context.Context` (nothing here is cancelled). A Go `(*T, error)` that is
//! nil when there is no such row is `Result<Option<T>>`; a method that sets an
//! id on its argument takes it `&mut`.
//!
//! - Agents: `create_agent`, `get_agent`, `list_agents`, `delete_agent`,
//!   `replace_agent` (closes the named agent's run and opens a new one under
//!   agent, keeping its state and messages: a resume).
//! - Messages: `create_message`, `get_pending_messages`,
//!   `update_message_status`, `claim_message` (false if it wasn't pending:
//!   someone else delivered it), `claim_pending_messages` (marks
//!   receiver_id's pending messages delivered, returning them oldest first,
//!   and applies update's state change for them in the same transaction;
//!   update may be None), `delete_messages_by_agent`, `claim_handover`
//!   (None if there is no such message awaiting handover), `fail_handover`
//!   (false if it was already handed over), `get_message_status`.
//! - Tasks (a standalone repository): `set_task`, `get_task`, `list_tasks`,
//!   `delete_task`.
//! - State: `set_state`, `set_states` (atomic), `update_states` (sets entries
//!   and deletes agent_name's keys in del, atomically), `record_report`
//!   (update_states plus an agent event, atomically), `get_state`,
//!   `list_state`, `delete_state`, `delete_state_key`.
//! - Session reports, every session's last hook report (the global hook):
//!   `record_session_report`, `session_reports`.
//! - Command log: `log_command`, `list_commands` (newest first).
//! - Friction box: `add_friction`, `get_friction` (None if none),
//!   `list_friction` (most votes, then newest, first), `last_friction_id`
//!   (0 if none), `update_friction`, `add_friction_vote`,
//!   `list_friction_votes` (oldest first), `add_friction_link` (or update the
//!   existing one), `list_friction_links` (0: every link),
//!   `update_friction_link`, `delete_friction_link`.
//! - `rename_host` rewrites references to another host renamed from old to
//!   new: agents' spawned_by and friction links.
//! - Lifecycle: `close`.

/// The Go name of the store.
pub type SQLiteStorage = crate::Store;
