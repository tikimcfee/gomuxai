//! Subjects, op names, and wire types shared between hosts (`internal/natsbus/contract.go`).

use std::collections::BTreeMap;
use std::fmt;
use std::sync::LazyLock;

use agent_base::gostd::strconv::quote;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::value::RawValue;

use crate::gotime::Time;

/// Prefixes every v1 subject, so bus permissions can be scoped to it.
pub const SUBJECT_ROOT: &str = "gomux";

// RPC operations a bus serves. Each host's config allowlists which ones
// remote callers may use; OP_KEYS, OP_SPAWN and OP_RESUME are off for remote
// callers by default.
pub const OP_LIST: &str = "list";
pub const OP_STATUS: &str = "status";
pub const OP_CAPTURE: &str = "capture";
pub const OP_SEND: &str = "send";
pub const OP_RESPOND: &str = "respond";
pub const OP_KEYS: &str = "keys";
pub const OP_SPAWN: &str = "spawn";
pub const OP_LOGS: &str = "logs";
pub const OP_RESUME: &str = "resume";
pub const OP_FRICTION: &str = "friction";
pub const OP_FLEET: &str = "fleet";
pub const OP_TEMPLATE: &str = "template";

/// Lists every RPC op a bus serves to local clients.
pub const ALL_OPS: &[&str] = &[
    OP_LIST,
    OP_STATUS,
    OP_CAPTURE,
    OP_SEND,
    OP_RESPOND,
    OP_KEYS,
    OP_SPAWN,
    OP_FRICTION,
    OP_LOGS,
    OP_RESUME,
    OP_FLEET,
    OP_TEMPLATE,
];

/// Ops a bus serves to other hosts unless configured otherwise.
pub const DEFAULT_REMOTE_OPS: &[&str] = &[
    OP_LIST,
    OP_STATUS,
    OP_CAPTURE,
    OP_SEND,
    OP_RESPOND,
    OP_FRICTION,
    OP_LOGS,
    OP_FLEET,
    OP_TEMPLATE,
];

static NAME_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]*$").unwrap());

static BINARY_PATH_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[A-Za-z0-9._+,@%=/-]*$").unwrap());

pub(crate) fn is_valid_name(s: &str) -> bool {
    NAME_PATTERN.is_match(s)
}

/// Reports whether `host` can be used as a host name on the bus.
pub fn validate_host(host: &str) -> anyhow::Result<()> {
    if !is_valid_name(host) {
        anyhow::bail!(
            "invalid host name {}: use letters, digits, '-' and '_'",
            quote(host)
        );
    }
    Ok(())
}

/// Reports whether `name` can be used as an agent name on the bus.
pub fn validate_agent(name: &str) -> anyhow::Result<()> {
    if !is_valid_name(name) {
        anyhow::bail!("invalid agent name {}", quote(name));
    }
    Ok(())
}

/// The request subject for `op` on `host`.
pub fn subject_rpc(host: &str, op: &str) -> String {
    format!("{SUBJECT_ROOT}.{host}.rpc.{op}")
}

/// A subject only local clients of `host` can reach: peer links never carry
/// `gomux.<host>.local.>`.
pub fn subject_local(host: &str, name: &str) -> String {
    format!("{SUBJECT_ROOT}.{host}.local.{name}")
}

/// Where `host` publishes `agent`'s status events.
pub fn subject_status(host: &str, agent: &str) -> String {
    format!("{SUBJECT_ROOT}.{host}.agent.{agent}.status")
}

/// Matches every status event on `host` (`"*"` for all hosts).
pub fn subject_all_status(host: &str) -> String {
    format!("{SUBJECT_ROOT}.{host}.agent.*.status")
}

/// Where `host` publishes focus events (`"*"` for all hosts).
pub fn subject_focus(host: &str) -> String {
    format!("{SUBJECT_ROOT}.{host}.focus")
}

/// Matches focus events across every host (`gomux.*.focus`).
pub fn subject_all_focus() -> String {
    subject_focus("*")
}

/// Where `host`'s clients receive replies.
pub fn inbox_prefix(host: &str) -> String {
    format!("_INBOX.{host}")
}

/// Addresses an agent, optionally on another host (`""` = local host).
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct AgentRef {
    pub name: String,
    pub host: String,
}

impl AgentRef {
    pub fn new(name: impl Into<String>, host: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            host: host.into(),
        }
    }

    pub fn local(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            host: String::new(),
        }
    }
}

/// Parses `"name"` or `"name@host"`.
pub fn parse_agent_ref(s: &str) -> anyhow::Result<AgentRef> {
    let (name, host) = match s.split_once('@') {
        Some((n, h)) => {
            validate_host(h)?;
            (n, h)
        }
        None => (s, ""),
    };
    validate_agent(name)?;
    Ok(AgentRef {
        name: name.to_string(),
        host: host.to_string(),
    })
}

impl fmt::Display for AgentRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.host.is_empty() {
            f.write_str(&self.name)
        } else {
            write!(f, "{}@{}", self.name, self.host)
        }
    }
}

/// Reports whether `path`, from another host's `list` reply, is safe to run
/// there: an absolute path of plain characters.
pub fn validate_agent_binary(path: &str) -> anyhow::Result<()> {
    if !BINARY_PATH_PATTERN.is_match(path) {
        anyhow::bail!(
            "its agent binary's path {} isn't an absolute path of letters, digits and ._+,@%=/-",
            quote(path)
        );
    }
    Ok(())
}

pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

pub(crate) fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

pub(crate) fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

fn raw_value_present<'de, D>(d: D) -> Result<Option<Box<RawValue>>, D::Error>
where
    D: Deserializer<'de>,
{
    let raw = Box::<RawValue>::deserialize(d)?;
    Ok(Some(raw))
}

/// Wraps every RPC reply. `error` is set when the op failed; `data` holds the
/// op's response type otherwise (absent for ops with no data).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RPCResponse {
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub error: String,
    #[serde(
        default,
        deserialize_with = "raw_value_present",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<Box<RawValue>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListRequest {}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub agents: Vec<AgentInfo>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub agent_binary: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInfo {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub spawned_by: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub adopted: bool,
    #[serde(default)]
    pub created_at: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub end_reason: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session_id: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumable: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
}

/// Mirrors `service.StatusInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub end_reason: String,
    #[serde(default, deserialize_with = "null_default")]
    pub source: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub event: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub tool: String,
    #[serde(default)]
    pub updated_at: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub tool_input: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub reported_status: String,
    #[serde(default)]
    pub reported_at: Time,
    #[serde(default)]
    pub sent_at: Time,
    #[serde(default)]
    pub turn_at: Time,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub held: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default)]
    pub lines: i64,
    #[serde(default)]
    pub raw: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub output: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default, deserialize_with = "null_default")]
    pub message: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub now: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub held: bool,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub held_count: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub held_until: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub typed: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub input_blocked: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub unchecked: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RespondRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default, deserialize_with = "null_default")]
    pub response: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub force: bool,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub expect_tool: String,
}

pub const RESPONSE_APPROVE: &str = "approve";
pub const RESPONSE_APPROVE_ALWAYS: &str = "approve_always";
pub const RESPONSE_DENY: &str = "deny";
pub const RESPONSE_DELIVER: &str = "deliver";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliverResponse {
    #[serde(default)]
    pub typed: i64,
    #[serde(default)]
    pub held: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeysRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub text: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub key: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub cwd: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub task_file: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub task_content: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub spawned_by: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpawnResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub session: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub spawned_by: String,
    #[serde(default, deserialize_with = "null_default")]
    pub cwd: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub task_file: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub model: String,
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub end_reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default)]
    pub lines: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub full: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub json: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub terminal: bool,
}

pub const MAX_LOGS_REPLY: usize = 512 << 10;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub output: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub hint: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub truncated: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub resumed_by: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub session: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    #[serde(default, deserialize_with = "null_default")]
    pub session_id: String,
    #[serde(default, deserialize_with = "null_default")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub held: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub end_reason: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
}

pub const FRICTION_GET: &str = "get";
pub const FRICTION_LINK: &str = "link";
pub const FRICTION_UNLINK: &str = "unlink";
pub const FRICTION_RESOLVE: &str = "resolve";

pub const FRICTION_ACTIONS: &[&str] = &[
    FRICTION_GET,
    FRICTION_LINK,
    FRICTION_UNLINK,
    FRICTION_RESOLVE,
];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub action: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub ids: Vec<i64>,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub id: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub uid: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<FrictionEntry>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub by: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub note: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionRef {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default)]
    pub id: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub uid: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionEntry {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default)]
    pub id: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub uid: String,
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub kind: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub summary: String,
    #[serde(default)]
    pub votes: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub resolved_by: String,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub resolved_at: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub resolution: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested: Option<FrictionRef>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub links: Vec<FrictionRef>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrictionResponse {
    #[serde(default, deserialize_with = "null_default")]
    pub entries: Vec<FrictionEntry>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub missing: Vec<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub resolved: bool,
}

/// Published on `subject_status` whenever an agent's reported status is recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusEvent {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default, deserialize_with = "null_default")]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub end_reason: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub event: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub tool: String,
    #[serde(default)]
    pub at: Time,
}

/// Published on `subject_focus` whenever a host's focused agent changes.
/// `agent` `""` means nothing has focus (`agent focus --clear`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusEvent {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session: String,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub at: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub by: String,
}

/// Request payload for `OP_FLEET` (`gomux.<host>.rpc.fleet`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetRequest {
    #[serde(default, skip_serializing_if = "is_false")]
    pub report: bool,
}

/// Combined cross-host fleet document (`agent fleet --json`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetView {
    #[serde(default)]
    pub at: Time,
    #[serde(default, deserialize_with = "null_default")]
    pub hosts: Vec<FleetHost>,
}

/// One host's fleet snapshot (`OP_FLEET` reply and element of [`FleetView`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetHost {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub build: String,
    #[serde(default)]
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub partial: bool,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub error: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub agents: Vec<FleetAgent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focus: Option<FocusEvent>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub jobs: Vec<FleetJob>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub handoffs: Vec<FleetHandoff>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub friction: Option<FleetFriction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<FleetReport>,
}

/// One agent in a [`FleetHost`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetAgent {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub spawned_by: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub backend: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub model: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub status: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub end_reason: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub detail: String,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub since: Time,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub held: i64,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub oldest_held: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session_id: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session_name: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub cwd: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub flags: Vec<String>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub children: Vec<FleetAgent>,
}

/// A running `agent run` job in [`FleetHost`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetJob {
    #[serde(default)]
    pub id: i64,
    #[serde(default, deserialize_with = "null_default")]
    pub script: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub by: String,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub started_at: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub unit: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub memory_max: String,
}

/// A handoff document older than its repo's latest commit in [`FleetHost`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetHandoff {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(default, deserialize_with = "null_default")]
    pub path: String,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub doc_at: Time,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub commit_at: Time,
}

/// Open friction summary in [`FleetHost`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetFriction {
    #[serde(default)]
    pub open: i64,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub by_kind: BTreeMap<String, i64>,
}

/// Baseline metrics returned when `FleetRequest { report: true }` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetReport {
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub agents_by_status: BTreeMap<String, i64>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub agents_by_backend: BTreeMap<String, i64>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub sessions_7d_by_harness: BTreeMap<String, i64>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub messages_per_day_7d: BTreeMap<String, i64>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub friction_open_by_kind: BTreeMap<String, i64>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub friction_resolved_by_kind: BTreeMap<String, i64>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub long_sessions: Vec<FleetLongSession>,
}

/// A session running longer than 3 days in [`FleetReport`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetLongSession {
    #[serde(default, deserialize_with = "null_default")]
    pub agent: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub session_id: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub backend: String,
    #[serde(default, skip_serializing_if = "Time::is_zero")]
    pub started_at: Time,
    #[serde(default)]
    pub compactions: i64,
}

pub const TEMPLATE_LIST: &str = "list";
pub const TEMPLATE_START: &str = "start";
pub const TEMPLATE_RESUME: &str = "resume";

pub const TEMPLATE_ACTIONS: &[&str] = &[TEMPLATE_LIST, TEMPLATE_START, TEMPLATE_RESUME];

/// One template entry in `<data dir>/templates.json` and [`TemplateResponse`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub model: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub cwd: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub brief: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub problem: String,
}

/// Request payload for `OP_TEMPLATE` (`gomux.<host>.rpc.template`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateRequest {
    #[serde(default, deserialize_with = "null_default")]
    pub action: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub template: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub from: String,
}

/// Response payload for `OP_TEMPLATE`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateResponse {
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub templates: Vec<Template>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<AgentInfo>,
}

/// Sentinel error meaning no bus is running on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("no agent bus running on this host (start one with `agent bus start`)")]
pub struct ErrNoBus;

/// Go's `errors.Is(err, natsbus.ErrNoBus)`.
pub fn is_no_bus(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.downcast_ref::<ErrNoBus>().is_some())
}
