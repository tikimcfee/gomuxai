//! SQLite-based persistence for the agent tool: the records it stores.

use std::borrow::Cow;

use serde::{Deserialize, Deserializer, Serialize};

use crate::gotime::{self, Time};
use crate::tree::AgentEvent;

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

/// A JSON `null` (Go's nil slice) reads as an empty list.
fn null_as_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// Agent represents a managed agent in the database.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub session_name: String,
    pub backend: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_file: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub log_file: String,
    /// agent that spawned it; "" for a person
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub spawned_by: String,
    /// where it was spawned; "" if spawned before this was recorded
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// pre-existing session; cleanup leaves the session running
    #[serde(default, skip_serializing_if = "is_false")]
    pub adopted: bool,
    pub created_at: Time,
}

/// MessageStatus represents the delivery status of a message. A string, like
/// the Go type, so a status a newer binary writes still reads.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MessageStatus(pub Cow<'static, str>);

impl MessageStatus {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for MessageStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for MessageStatus {
    fn from(s: String) -> Self {
        MessageStatus(Cow::Owned(s))
    }
}

impl From<&'static str> for MessageStatus {
    fn from(s: &'static str) -> Self {
        MessageStatus(Cow::Borrowed(s))
    }
}

impl rusqlite::ToSql for MessageStatus {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        Ok(rusqlite::types::ToSqlOutput::from(self.as_str()))
    }
}

pub const MESSAGE_STATUS_PENDING: MessageStatus = MessageStatus(Cow::Borrowed("pending"));
pub const MESSAGE_STATUS_DELIVERED: MessageStatus = MessageStatus(Cow::Borrowed("delivered"));
pub const MESSAGE_STATUS_FAILED: MessageStatus = MessageStatus(Cow::Borrowed("failed"));
/// MESSAGE_STATUS_HANDOVER is a message typed into the agent as one short
/// line, whose whole text the hook of the prompt that line starts hands
/// over (see service/handover.go). Nothing else delivers it.
pub const MESSAGE_STATUS_HANDOVER: MessageStatus = MessageStatus(Cow::Borrowed("handover"));

/// Message represents a queued message between agents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub id: i64,
    pub sender_id: String,
    pub receiver_id: String,
    pub content: String,
    pub status: MessageStatus,
    pub created_at: Time,
}

/// Task represents a reusable task definition.
/// Tasks are standalone entities that can be referenced by multiple agents.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub name: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source_file: String,
    pub created_at: Time,
}

/// CommandRecord is one run of the agent command, for agent history.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CommandRecord {
    pub id: i64,
    pub started_at: Time,
    pub duration_ms: i64,
    pub host: String,
    /// AGENT_NAME; "" for a person
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// the agent's spawned_by, filled in by log_command
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    pub cwd: String,
    /// without the program name
    #[serde(deserialize_with = "null_as_empty")]
    pub args: Vec<String>,
    pub exit_code: i64,
    /// first line of the error
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub build: String,
}

/// CommandFilter selects command records. The default selects all.
#[derive(Clone, Debug, Default)]
pub struct CommandFilter {
    /// only Agent's commands
    pub by_agent: bool,
    /// with by_agent; "" for a person's
    pub agent: String,
    /// only commands run in this directory, if set
    pub cwd: String,
    /// only commands that exited non-zero
    pub failed: bool,
    pub since: Time,
    /// the newest Limit records, if set
    pub limit: i64,
}

// Friction statuses: open → accepted → resolved, wontfix, or duplicate.
pub const FRICTION_OPEN: &str = "open";
pub const FRICTION_ACCEPTED: &str = "accepted";
pub const FRICTION_RESOLVED: &str = "resolved";
pub const FRICTION_WONTFIX: &str = "wontfix";
pub const FRICTION_DUPLICATE: &str = "duplicate";

/// Friction is an entry in the friction box: something about the agent tool
/// that got in someone's way.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Friction {
    pub id: i64,
    /// unique across hosts, for merging boxes
    pub uid: String,
    pub host: String,
    /// agent name, or "user"
    pub filed_by: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    pub build: String,
    pub created_at: Time,
    pub updated_at: Time,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AttachedCommand>,
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accepted_by: String,
    #[serde(default, skip_serializing_if = "gotime::is_zero")]
    pub accepted_at: Time,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub accept_note: String,
    /// also for wontfix and duplicate
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolved_by: String,
    #[serde(default, skip_serializing_if = "gotime::is_zero")]
    pub resolved_at: Time,
    /// the note: how it was fixed, or why not
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub duplicate_of: i64,

    /// Votes counts +1s, read only: the entry's own, its duplicates', and
    /// one for each duplicate's filing.
    pub votes: i64,
}

/// AttachedCommand is a failed agent command, copied from the command log
/// into a friction entry or vote (the log is pruned).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AttachedCommand {
    #[serde(default, deserialize_with = "null_as_empty")]
    pub args: Vec<String>,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    #[serde(default)]
    pub at: Time,
}

/// FrictionVote is a +1 on a friction entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrictionVote {
    pub id: i64,
    pub friction_id: i64,
    pub uid: String,
    pub host: String,
    /// agent name, or "user"
    pub voter: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    pub build: String,
    pub created_at: Time,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<AttachedCommand>,
}

/// FrictionLink says that a friction entry and one on another host are the
/// same problem. Each host stores its own half; the peer fields are what
/// this host last saw of the other end.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct FrictionLink {
    #[serde(skip)]
    pub id: i64,
    pub friction_id: i64,
    #[serde(rename = "host")]
    pub peer_host: String,
    #[serde(rename = "id")]
    pub peer_id: i64,
    /// PeerUID is the other entry's uid, so a reused id isn't taken for it;
    /// empty until the other host has answered.
    #[serde(rename = "uid", default, skip_serializing_if = "String::is_empty")]
    pub peer_uid: String,
    pub linked_by: String,
    pub linked_at: Time,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
    /// ConfirmedAt is when the other host was seen to hold its half; zero
    /// for a link recorded while it was unreachable.
    #[serde(default, skip_serializing_if = "gotime::is_zero")]
    pub confirmed_at: Time,
    #[serde(rename = "status", default, skip_serializing_if = "String::is_empty")]
    pub peer_status: String,
    #[serde(rename = "kind", default, skip_serializing_if = "String::is_empty")]
    pub peer_kind: String,
    #[serde(rename = "summary", default, skip_serializing_if = "String::is_empty")]
    pub peer_summary: String,
    #[serde(rename = "votes")]
    pub peer_votes: i64,
    #[serde(
        rename = "resolved_by",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub peer_resolved_by: String,
    #[serde(
        rename = "resolution",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub peer_resolution: String,
    #[serde(
        rename = "checked_at",
        default,
        skip_serializing_if = "gotime::is_zero"
    )]
    pub peer_checked_at: Time,
}

/// FrictionFilter selects friction entries. The default selects all.
#[derive(Clone, Debug, Default)]
pub struct FrictionFilter {
    /// any of these, if set
    pub statuses: Vec<String>,
    pub kind: String,
    pub filed_by: String,
    pub duplicate_of: i64,
}

/// StateUpdate is a change to an agent's state: entries to set, and
/// agent_name's keys in delete to remove.
#[derive(Clone, Debug, Default)]
pub struct StateUpdate {
    pub entries: Vec<StateEntry>,
    pub agent_name: String,
    pub delete: Vec<String>,
    /// Event, if set, is appended to the agent events in the same
    /// transaction.
    pub event: Option<AgentEvent>,
}

/// StateEntry represents a key-value state entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StateEntry {
    pub agent_name: String,
    pub key: String,
    pub value: String,
    pub updated_at: Time,
}
