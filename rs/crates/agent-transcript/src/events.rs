//! The event stream: what an agent session did, one event per thing that
//! happened, in transcript order. Every reader (`agent logs`, `agent events`,
//! the displays that consume them) sees these same events; none of them parses
//! a transcript itself. `docs/EVENTS.md` is the wire contract, and
//! `testdata/corpus` the transcripts with the events they must produce.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use chrono::{DateTime, FixedOffset};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::meta::MetaAt;
use crate::tools::{FileText, ToolSummary};
use crate::transcript::parse_time;

// Event kinds.
pub const KIND_PROMPT: &str = "prompt"; // a message into the agent
pub const KIND_THINK: &str = "think"; // a thinking block
pub const KIND_SAY: &str = "say"; // text the agent wrote
pub const KIND_TOOL: &str = "tool"; // a tool call; its result arrives as an update
pub const KIND_SYSTEM: &str = "system"; // a harness record about the session (turn timing, compaction, errors)
pub const KIND_REQUEST: &str = "request"; // what one model call was sent (only with Options::requests)

// Event ops: a changed event (a tool's result landed, a provisional reply
// was confirmed) is sent again, whole, with the same ID.
pub const OP_ADD: &str = "add";
pub const OP_UPDATE: &str = "update";

// Prompt origins: where a message into the agent came from.
pub const ORIGIN_HUMAN: &str = "human"; // typed by a person
pub const ORIGIN_QUEUED: &str = "queued"; // sent while the agent was busy (mid-turn)
pub const ORIGIN_DELIVERED: &str = "delivered"; // gomuxai's held-message delivery (Stop hook context)
pub const ORIGIN_TASK_NOTIFICATION: &str = "task-notification"; // a background task reporting back
pub const ORIGIN_PEER: &str = "peer"; // another agent
pub const ORIGIN_COMMAND: &str = "command"; // a slash command
pub const ORIGIN_SYSTEM: &str = "system"; // injected by the harness

// Tool result statuses.
pub const STATUS_PENDING: &str = "pending"; // no result yet
pub const STATUS_OK: &str = "ok"; // returned
pub const STATUS_ERROR: &str = "error"; // failed
pub const STATUS_INTERRUPTED: &str = "interrupted"; // the turn was interrupted before it returned
pub const STATUS_BACKGROUND: &str = "background"; // moved to the background; a later update completes it

pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

pub(crate) fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

pub(crate) fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

/// Raw JSON text (`json.RawMessage` in Go). Serializing compacts insignificant
/// whitespace outside strings while keeping object key order and number text
/// intact, matching Go's `encoding/json`.
#[derive(Clone)]
pub struct RawJson(Box<RawValue>);

impl RawJson {
    pub fn from_raw(raw: Box<RawValue>) -> Self {
        Self(raw)
    }

    pub fn from_json_string(s: String) -> serde_json::Result<Self> {
        RawValue::from_string(s).map(Self)
    }

    pub fn from_json_str(s: &str) -> serde_json::Result<Self> {
        Self::from_json_string(s.to_string())
    }

    pub fn as_str(&self) -> &str {
        self.0.get()
    }

    pub fn as_bytes(&self) -> &[u8] {
        self.0.get().as_bytes()
    }

    pub fn into_raw(self) -> Box<RawValue> {
        self.0
    }
}

impl Deref for RawJson {
    type Target = str;

    fn deref(&self) -> &str {
        self.0.get()
    }
}

impl AsRef<str> for RawJson {
    fn as_ref(&self) -> &str {
        self.0.get()
    }
}

impl AsRef<[u8]> for RawJson {
    fn as_ref(&self) -> &[u8] {
        self.0.get().as_bytes()
    }
}

impl PartialEq for RawJson {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}

impl Eq for RawJson {}

impl PartialEq<str> for RawJson {
    fn eq(&self, other: &str) -> bool {
        self.0.get() == other
    }
}

impl PartialEq<&str> for RawJson {
    fn eq(&self, other: &&str) -> bool {
        self.0.get() == *other
    }
}

impl fmt::Debug for RawJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawJson({})", self.0.get())
    }
}

impl fmt::Display for RawJson {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0.get())
    }
}

impl From<Box<RawValue>> for RawJson {
    fn from(raw: Box<RawValue>) -> Self {
        Self(raw)
    }
}

impl Serialize for RawJson {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match compact_json(self.0.get()) {
            Cow::Borrowed(_) => self.0.serialize(serializer),
            Cow::Owned(compacted) => {
                let raw = RawValue::from_string(compacted).map_err(serde::ser::Error::custom)?;
                raw.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for RawJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(Self)
    }
}

/// Deserializes a present JSON field (including `null`) as `Some(RawJson)`.
/// Combined with `#[serde(default)]`, an absent field is `None` while an
/// explicit `null` is `Some("null")`, matching Go's `json.RawMessage`.
pub(crate) fn deserialize_present_raw<'de, D>(deserializer: D) -> Result<Option<RawJson>, D::Error>
where
    D: Deserializer<'de>,
{
    RawJson::deserialize(deserializer).map(Some)
}

/// Strips ASCII whitespace outside JSON string literals, like Go's
/// `json.Compact`.
pub fn compact_json(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    let mut in_str = false;
    let mut escaped = false;
    let mut needs_compact = false;
    for &b in bytes {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            needs_compact = true;
            break;
        }
    }
    if !needs_compact {
        return Cow::Borrowed(s);
    }
    let mut out = Vec::with_capacity(bytes.len());
    in_str = false;
    escaped = false;
    for &b in bytes {
        if in_str {
            out.push(b);
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
            out.push(b);
        } else if !matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
            out.push(b);
        }
    }
    Cow::Owned(String::from_utf8(out).expect("valid utf-8 json"))
}

/// Event is one thing that happened in a session. Exactly the field of its
/// `kind` is set.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Stable across reads and updates: the `tool_use` id for a tool; otherwise
    /// the source record's id and the block's index (`"<uuid>#2"`).
    pub id: String,
    pub op: String,
    /// The event's 0-based position in its stream (a request event's, in the
    /// request channel, which numbers its own); `at` and `line` locate the
    /// source line (byte offset, line index), so a reader can page and resume.
    #[serde(default)]
    pub seq: usize,
    #[serde(default)]
    pub at: i64,
    #[serde(default)]
    pub line: usize,
    /// The source record's own timestamp, as it wrote it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ts: String,
    /// The prompt this event belongs to, so a reader may group; `None` before
    /// the first prompt a parse has seen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<TurnRef>,
    /// The subagent id on a subagent's own events.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// Marks an event of a branch the conversation left: a prompt the person
    /// edited and sent again, or rewound past, and everything that answered it.
    /// It arrives as an update; the model no longer sees the branch.
    #[serde(default, skip_serializing_if = "is_false")]
    pub abandoned: bool,
    pub kind: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub think: Option<ThinkEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub say: Option<SayEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<RequestEvent>,
}

impl Event {
    /// Returns the event's timestamp, or `None` if it has none or cannot be parsed.
    pub fn time(&self) -> Option<DateTime<FixedOffset>> {
        parse_time(&self.ts)
    }
}

/// TurnRef names a turn: the harness's own prompt id and position where the
/// transcript records them, derived otherwise (`derived` says which).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnRef {
    pub id: String,
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub derived: bool,
}

/// PromptEvent is a message into the agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptEvent {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub origin: String,
    /// A slash command's `"/name"`; `text` is then `"/name args"`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

/// Image is an image a prompt carried, its base64 data whole.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    pub media_type: String,
    pub data: String,
}

/// ThinkEvent is a thinking block; a redacted one (signature only) is still an
/// event, with no text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThinkEvent {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub redacted: bool,
}

/// SayEvent is text the agent wrote. `final` marks a turn's closing reply;
/// `provisional`, one taken from the Stop hook before the transcript holds it
/// (the transcript's own copy then arrives as an update of the same ID).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SayEvent {
    #[serde(default)]
    pub text: String,
    #[serde(rename = "final", default, skip_serializing_if = "is_false")]
    pub r#final: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub provisional: bool,
}

/// ToolEvent is a tool call and, once it lands, its result. The embedded
/// `ToolSummary` (`action`, `target`, `detail`, `meta`, `file`) comes from the
/// one tool table, [`crate::summarize_tool`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolEvent {
    pub name: String,
    pub input: Option<RawJson>,
    pub result: Option<ToolResult>,
    pub tool_summary: ToolSummary,
    /// The sessions a subagent-spawning call started, in the order its result
    /// names them.
    pub sub_sessions: Vec<String>,
}

impl Deref for ToolEvent {
    type Target = ToolSummary;

    fn deref(&self) -> &Self::Target {
        &self.tool_summary
    }
}

impl DerefMut for ToolEvent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.tool_summary
    }
}

impl Serialize for ToolEvent {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut count = 2; // name, action
        if self.input.is_some() {
            count += 1;
        }
        if self.result.is_some() {
            count += 1;
        }
        if !self.tool_summary.target.is_empty() {
            count += 1;
        }
        if !self.tool_summary.detail.is_empty() {
            count += 1;
        }
        if self.tool_summary.meta.is_some() {
            count += 1;
        }
        if self.tool_summary.file.is_some() {
            count += 1;
        }
        if !self.sub_sessions.is_empty() {
            count += 1;
        }

        let mut st = serializer.serialize_struct("ToolEvent", count)?;
        st.serialize_field("name", &self.name)?;
        if let Some(ref input) = self.input {
            st.serialize_field("input", input)?;
        }
        if let Some(ref result) = self.result {
            st.serialize_field("result", result)?;
        }
        st.serialize_field("action", &self.tool_summary.action)?;
        if !self.tool_summary.target.is_empty() {
            st.serialize_field("target", &self.tool_summary.target)?;
        }
        if !self.tool_summary.detail.is_empty() {
            st.serialize_field("detail", &self.tool_summary.detail)?;
        }
        if let Some(ref meta) = self.tool_summary.meta {
            st.serialize_field("meta", meta)?;
        }
        if let Some(ref file) = self.tool_summary.file {
            st.serialize_field("file", file)?;
        }
        if !self.sub_sessions.is_empty() {
            st.serialize_field("sub_sessions", &self.sub_sessions)?;
        }
        st.end()
    }
}

impl<'de> Deserialize<'de> for ToolEvent {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ToolEventVisitor;

        impl<'de> Visitor<'de> for ToolEventVisitor {
            type Value = ToolEvent;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a ToolEvent object")
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut name: Option<String> = None;
                let mut input: Option<RawJson> = None;
                let mut result: Option<ToolResult> = None;
                let mut action: Option<String> = None;
                let mut target: Option<String> = None;
                let mut detail: Option<String> = None;
                let mut meta: Option<RawJson> = None;
                let mut file: Option<FileText> = None;
                let mut sub_sessions: Option<Vec<String>> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "name" => name = Some(map.next_value()?),
                        "input" => input = Some(map.next_value()?),
                        "result" => result = map.next_value()?,
                        "action" => action = Some(map.next_value()?),
                        "target" => target = Some(map.next_value()?),
                        "detail" => detail = Some(map.next_value()?),
                        "meta" => meta = Some(map.next_value()?),
                        "file" => file = map.next_value()?,
                        "sub_sessions" => sub_sessions = Some(map.next_value()?),
                        _ => {
                            let _: de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                Ok(ToolEvent {
                    name: name.unwrap_or_default(),
                    input,
                    result,
                    tool_summary: ToolSummary {
                        action: action.unwrap_or_default(),
                        target: target.unwrap_or_default(),
                        detail: detail.unwrap_or_default(),
                        meta,
                        file,
                    },
                    sub_sessions: sub_sessions.unwrap_or_default(),
                })
            }
        }

        deserializer.deserialize_map(ToolEventVisitor)
    }
}

/// ToolResult is what a tool call returned, or that it hasn't yet.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub output: String,
    /// The harness's structured response (Claude Code's `toolUseResult` object),
    /// whole.
    #[serde(
        default,
        deserialize_with = "deserialize_present_raw",
        skip_serializing_if = "Option::is_none"
    )]
    pub structured: Option<RawJson>,
}

/// SystemEvent is a harness record about the session itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemEvent {
    #[serde(default)]
    pub subtype: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present_raw",
        skip_serializing_if = "Option::is_none"
    )]
    pub data: Option<RawJson>,
}

/// RequestEvent is what one model call was sent: Claude Code's `api-request`
/// line, with the messages its tail's hashes name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestEvent {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(
        default,
        deserialize_with = "deserialize_present_raw",
        skip_serializing_if = "Option::is_none"
    )]
    pub params: Option<RawJson>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub base: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tail: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub blobs: BTreeMap<String, RawJson>,
}

/// Session is what a transcript records about its session (`meta.rs` has the
/// rules). `read_file` adds the file's path and size; who owns the session and
/// its status come from elsewhere (`agent sessions`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub size: i64,
    /// The working directory the session recorded first; `project` is its last
    /// element, never the storage folder's name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// The latest title the harness recorded (Claude Code: a custom title wins
    /// over an AI one).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_branch: String,
    /// Set on a subagent's session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<ParentSession>,
    /// The session this one continued in.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub continued_in: String,

    /// Set once `title` holds a custom title, which a later AI title doesn't
    /// replace.
    #[serde(skip)]
    #[doc(hidden)]
    pub custom_title: bool,
    /// Places each harvested field's value in the transcript.
    #[serde(skip)]
    #[doc(hidden)]
    pub at: MetaAt,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.harness == other.harness
            && self.path == other.path
            && self.size == other.size
            && self.cwd == other.cwd
            && self.project == other.project
            && self.title == other.title
            && self.agent_name == other.agent_name
            && self.model == other.model
            && self.version == other.version
            && self.git_branch == other.git_branch
            && self.parent_session == other.parent_session
            && self.continued_in == other.continued_in
    }
}

impl Eq for Session {}

/// ParentSession is the session that started a subagent, and the tool call
/// that did, where the harness records it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParentSession {
    #[serde(default)]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub tool_id: String,
}

/// Pos is a place in a transcript: the byte offset where a line starts, that
/// line's index, and the `seq` the next event gets; `request_seq`, the `seq`
/// the next request event gets (the request channel numbers its own).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pos {
    pub at: i64,
    pub line: usize,
    pub seq: usize,
    pub request_seq: usize,
}

/// Lookup of the index of a turn by prompt id (`Options::turns`).
pub type TurnsFn = Arc<dyn Fn(&str) -> Option<usize> + Send + Sync>;

/// Options choose what a parse reads and emits.
#[derive(Clone, Default)]
pub struct Options {
    /// Adds the request channel: Claude Code's `api-request` lines as request
    /// events.
    pub requests: bool,
    /// Looks up the index of a turn the transcript names but doesn't number,
    /// by prompt id: a Claude subagent's records hold its parent's prompt id,
    /// and only the parent's transcript its index (`claude_subagent_turns`).
    pub turns: Option<TurnsFn>,
    /// What files beside the transcript say about its session, where its
    /// records don't (Kimi Code's `state.json`, agy's subagent index, which of
    /// agy's logs it is): the parse starts from it. On a subagent's
    /// (`parent_session` set), its `session_id` is the `agent` of every event.
    pub session: Session,
    /// The turn events carry until a prompt opens one, for a transcript whose
    /// records name no turn at all: an agy subagent's carry the parent's turn
    /// whose call started it.
    pub turn: Option<TurnRef>,
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("requests", &self.requests)
            .field("turns", &self.turns.as_ref().map(|_| "<fn>"))
            .field("session", &self.session)
            .field("turn", &self.turn)
            .finish()
    }
}

/// A Reader parses one harness's transcript format.
pub trait Reader: Send + Sync {
    /// Names the format, e.g. `"claude"`.
    fn harness(&self) -> &'static str;
    /// Returns a parser for one transcript, from its start.
    fn new_parser(&self, opts: Options) -> Parser;
}

/// lineReader is a harness's half of a Parser: it reads one line at a time,
/// keeping what later lines need (the open turn, calls waiting for their
/// results).
pub(crate) trait LineReader: Send {
    /// Parses one non-blank line, which starts at byte offset `at` and is line
    /// index `n`, and returns its events (`seq` unset), and `false` if the line
    /// isn't a record of this format.
    fn read(&mut self, line: &[u8], at: i64, n: usize) -> (Vec<Event>, bool);
    /// Returns what the lines so far say about the session.
    fn session(&self) -> Session;
}

/// Parser turns a transcript's lines into events. Feed it the transcript's
/// bytes, in order, from the start, as they arrive. A parse always starts at
/// the start: what a line means depends on the lines before it (the turn it is
/// in, the call a result answers). To resume after a line, skip the lines
/// before ([`resume_after`]).
pub struct Parser {
    harness: &'static str,
    lines_reader: Box<dyn LineReader>,
    pos: Pos,
    /// Counts the non-blank lines read; `skipped`, those that weren't records
    /// of the format.
    pub lines: usize,
    pub skipped: usize,
    /// How many bytes from `pos.at` the last `feed` found no newline in: the
    /// next `feed`, whose data starts at `pos.at` again, looks after them, so
    /// following a long line that arrives in small appends costs linear time,
    /// not quadratic.
    scanned: usize,
}

pub(crate) fn new_parser(harness: &'static str, lines: Box<dyn LineReader>) -> Parser {
    Parser {
        harness,
        lines_reader: lines,
        pos: Pos::default(),
        lines: 0,
        skipped: 0,
        scanned: 0,
    }
}

/// Go's `bytes.TrimSpace`: `data` without the Unicode whitespace at either
/// end (`unicode.IsSpace`, the same set as `char::is_whitespace`). An
/// invalid UTF-8 sequence isn't whitespace, and trimming stops there, however
/// valid the rest of the line is.
pub fn trim_space(data: &[u8]) -> &[u8] {
    let mut start = 0;
    while let Some((c, n)) = first_char(&data[start..])
        && c.is_whitespace()
    {
        start += n;
    }
    let mut end = data.len();
    while let Some((c, n)) = last_char(&data[start..end])
        && c.is_whitespace()
    {
        end -= n;
    }
    &data[start..end]
}

/// The character `b` starts with and its length, or `None` if `b` is empty
/// or starts with an invalid sequence.
fn first_char(b: &[u8]) -> Option<(char, usize)> {
    let &lead = b.first()?;
    let n = match lead {
        0x00..=0x7f => return Some((lead as char, 1)),
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => return None,
    };
    let c = std::str::from_utf8(b.get(..n)?).ok()?.chars().next()?;
    Some((c, n))
}

/// The character `b` ends with and its length, or `None` if `b` is empty or
/// ends with an invalid sequence.
fn last_char(b: &[u8]) -> Option<(char, usize)> {
    for n in 1..=b.len().min(4) {
        if let Ok(s) = std::str::from_utf8(&b[b.len() - n..]) {
            return s.chars().next().map(|c| (c, n));
        }
    }
    None
}

pub(crate) fn trim_bytes_space(data: &[u8]) -> &[u8] {
    trim_space(data)
}

impl Parser {
    /// Parses the whole lines at the start of `data`, which starts at
    /// `pos().at`, and returns their events, numbered from `pos().seq` (request
    /// events from `pos().request_seq`: the conversation is numbered the same
    /// with or without them). A last line with no newline yet is left for the
    /// next `feed`: that one's data starts again at `pos().at`.
    pub fn feed(&mut self, mut data: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        let mut from = std::mem::take(&mut self.scanned).min(data.len());
        loop {
            let Some(nl) = memchr::memchr(b'\n', &data[from..]).map(|i| from + i) else {
                self.scanned = data.len();
                return events;
            };
            from = 0;
            let line = trim_bytes_space(&data[..nl]);
            if !line.is_empty() {
                self.lines += 1;
                let (evs, ok) = if line[0] == b'{' {
                    self.lines_reader.read(line, self.pos.at, self.pos.line)
                } else {
                    (Vec::new(), false)
                };
                if !ok {
                    self.skipped += 1;
                }
                for mut e in evs {
                    if e.kind == KIND_REQUEST {
                        e.seq = self.pos.request_seq;
                        self.pos.request_seq += 1;
                    } else {
                        e.seq = self.pos.seq;
                        self.pos.seq += 1;
                    }
                    events.push(e);
                }
            }
            self.pos.at += (nl + 1) as i64;
            self.pos.line += 1;
            data = &data[nl + 1..];
        }
    }

    /// Reads `data` as `feed` does, keeping what later lines need, but returns
    /// nothing: the lines before the point a parse resumes at. Their events
    /// still count in `seq`, so the events after it are numbered as the whole
    /// stream numbers them.
    pub fn skip(&mut self, data: &[u8]) {
        let _ = self.feed(data);
    }

    /// Returns where the parse stopped: the start of the first line it hasn't
    /// read.
    pub fn pos(&self) -> Pos {
        self.pos
    }

    /// Returns what the lines read so far say about the session.
    pub fn session(&self) -> Session {
        let mut s = self.lines_reader.session();
        s.harness = self.harness.to_string();
        s
    }
}

/// Returns a parser of `r`'s for the transcript `data` holds from its start,
/// that has read, silently, every line up to and including the one that starts
/// at byte offset `at`: the `at` of the last event a reader holds. Feed it the
/// transcript from `pos().at` on, and it sends what comes after that line,
/// numbered and turned as the whole stream is. An `at` of `-1` resumes at the
/// start.
pub fn resume_after(r: &dyn Reader, opts: Options, data: &[u8], at: i64) -> anyhow::Result<Parser> {
    let mut p = r.new_parser(opts);
    if at < 0 {
        return Ok(p);
    }
    let len = data.len() as i64;
    if at >= len || (at > 0 && data[(at - 1) as usize] != b'\n') {
        anyhow::bail!(
            "offset {} is not the start of a line of the transcript ({} bytes)",
            at,
            data.len()
        );
    }
    let start = at as usize;
    let Some(nl) = memchr::memchr(b'\n', &data[start..]) else {
        anyhow::bail!("the line at offset {} has no end yet", at);
    };
    p.skip(&data[..start + nl + 1]);
    Ok(p)
}

/// Folds a stream into one event per ID, in its latest state (a tool with its
/// result) but where and when it was added: what a reader that doesn't follow
/// updates shows. An update whose event the stream doesn't hold (its add came
/// before a window) is left out.
pub fn latest(events: &[Event]) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::with_capacity(events.len());
    for e in events {
        if let Some(&i) = index.get(&e.id) {
            let added = &out[i];
            let mut next = e.clone();
            next.op = added.op.clone();
            next.seq = added.seq;
            next.at = added.at;
            next.line = added.line;
            next.ts = added.ts.clone();
            out[i] = next;
            continue;
        }
        if e.op == OP_UPDATE {
            continue;
        }
        index.insert(e.id.clone(), out.len());
        out.push(e.clone());
    }
    out
}
