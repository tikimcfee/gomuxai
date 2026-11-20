//! Snapshot and follow stream of the agent tree across stores and hosts
//! (`internal/export/export.go`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{BufRead, BufReader};

use agent_base::gostd::filepath;
use agent_base::types::{END_CLOSED, Status, resumable};
pub use agent_bus::{AgentInfo, Context};
pub use agent_store::Time;
use agent_store::{
    AGENT_REMOVED, AGENT_SPAWNED, AGENT_STATUS, AgentEvent, Run, StateEntry,
    Store as SQLiteStorage, gotime, open_read_only,
};
use agent_termd::DEFAULT_NAMESPACE;
use agent_transcript::session_id_from_path;
use chrono::{DateTime, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An agent running in a `g3d-termd` terminal managed by gomuxai.
pub const KIND_TERMD: &str = "termd";

// Line types after the snapshot.
/// Spawn recorded an agent.
pub const LINE_SPAWNED: &str = "spawned";
/// An agent changed; `reason` says what.
pub const LINE_CHANGED: &str = "changed";
/// Cleanup removed an agent (`agent` is its departed form).
pub const LINE_REMOVED: &str = "removed";
/// A store's agents, replacing what was known of it.
pub const LINE_STORE: &str = "store";
/// Another host's agents, replacing what was known of it.
pub const LINE_HOST: &str = "host";
/// The sessions no store records, replacing the last list.
pub const LINE_UNRECORDED: &str = "unrecorded";

// Reasons for a changed line.
/// A status report (an agent event).
pub const REASON_STATUS: &str = "status";
/// Its terminal session appeared or went away.
pub const REASON_SESSION: &str = "session";
/// Its transcript grew or moved.
pub const REASON_TRANSCRIPT: &str = "transcript";

/// Bounds the events one [`Tree::poll`] reads per store.
const POLL_BATCH: i64 = 500;

fn is_false(b: &bool) -> bool {
    !*b
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Go's `Snapshot.Stores` is `[]Store` without `omitempty` and starts `nil`, so
/// an empty slice encodes as `null` while non-empty encodes as `[...]`.
fn serialize_stores<S: Serializer>(stores: &[Store], s: S) -> Result<S::Ok, S::Error> {
    if stores.is_empty() {
        s.serialize_none()
    } else {
        stores.serialize(s)
    }
}

/// One agent as `agent export` prints it. `status`, `event`, `detail`, `tool`,
/// and `tool_input` are the last recorded report (hooks or `agent report`), not
/// the resolved status `agent status` shows: no backend CLI runs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Agent {
    #[serde(default, deserialize_with = "null_default")]
    pub name: String,
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default, deserialize_with = "null_default")]
    pub kind: String,
    /// The spawn's id in its store; 0 if spawned before runs were recorded.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub run: i64,
    /// The agent that spawned it; absent for a person.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub parent: String,
    #[serde(default, deserialize_with = "null_default")]
    pub backend: String,
    /// Terminal id on `host`.
    #[serde(default, deserialize_with = "null_default")]
    pub session: String,
    /// `g3d-termd` namespace on this host (e.g. `"agents"`); absent for other hosts.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub namespace: String,
    /// The data dir that records it; absent for other hosts.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub store: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub cwd: String,
    #[serde(default)]
    pub created_at: Time,
    /// When cleanup removed it.
    #[serde(default, skip_serializing_if = "gotime::is_zero")]
    pub ended_at: Time,
    /// It has a record (not cleaned up).
    #[serde(default)]
    pub live: bool,
    /// Whether its terminal session exists; absent for another host that
    /// predates sending it (unknown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attached: Option<bool>,
    /// Whether `agent resume` would continue it, by its own host's rules
    /// (`agent_base::types::resumable`); absent for another host that predates
    /// sending it (unknown). A departed agent is never resumable: resume needs
    /// its record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumable: Option<bool>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub status: String,
    /// How its session ended, with a status of `ended` (see `agent statuses`).
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
    /// A pending permission prompt's full call.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub tool_input: String,
    #[serde(default, skip_serializing_if = "gotime::is_zero")]
    pub status_at: Time,
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
    pub transcript: String,
    /// How to read `transcript`: `claude` or `agy`.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub dialect: String,
    /// Transcript file's byte size, when it exists on this host.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub transcript_size: i64,
    /// Transcript file's modification time, when it exists on this host.
    #[serde(default, skip_serializing_if = "gotime::is_zero")]
    pub transcript_mtime: Time,

    /// Registered with `agent adopt`, which `resume` refuses.
    #[serde(skip)]
    #[doc(hidden)]
    pub adopted: bool,
}

/// A data dir `export` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default, deserialize_with = "null_default")]
    pub dir: String,
    /// The running binary's own store.
    #[serde(default, skip_serializing_if = "is_false")]
    pub this: bool,
    /// The newest agent event the output includes: follow lines for this store
    /// carry larger ids.
    #[serde(default)]
    pub cursor: i64,
    /// `false` for a store written by an agent built before export: its changes
    /// arrive as whole store lines instead of events.
    #[serde(default)]
    pub events: bool,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub error: String,
}

/// An agent terminal session that no store `export` read records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    #[serde(default, deserialize_with = "null_default")]
    pub session: String,
    /// The data dir of the binary that started it, if found (it may be
    /// unreadable, or not record the session).
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub store: String,
}

/// Another host whose agents the output includes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(default)]
    pub agents: usize,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub error: String,
}

/// The whole tree.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// `"snapshot"`
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(default, deserialize_with = "null_default")]
    pub host: String,
    #[serde(
        default,
        deserialize_with = "null_default",
        serialize_with = "serialize_stores"
    )]
    pub stores: Vec<Store>,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub hosts: Vec<Host>,
    #[serde(default, deserialize_with = "null_default")]
    pub agents: Vec<Agent>,
    #[serde(default, deserialize_with = "null_default")]
    pub unrecorded: Vec<Session>,
}

/// A `spawned`, `changed`, or `removed` line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentLine {
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    pub r#type: String,
    /// The agent event's id in `store`; absent for session and transcript changes.
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub id: i64,
    /// Absent for other hosts.
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub store: String,
    #[serde(default)]
    pub at: Time,
    #[serde(
        default,
        deserialize_with = "null_default",
        skip_serializing_if = "String::is_empty"
    )]
    pub reason: String,
    #[serde(default)]
    pub agent: Agent,
}

/// Replaces what was known of a store: one found while following, or one
/// without events that changed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreLine {
    /// `"store"`
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(default)]
    pub store: Store,
    #[serde(default, deserialize_with = "null_default")]
    pub agents: Vec<Agent>,
}

/// Replaces what was known of another host's agents.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostLine {
    /// `"host"`
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(default)]
    pub host: Host,
    #[serde(default, deserialize_with = "null_default")]
    pub agents: Vec<Agent>,
}

/// Replaces the list of sessions no store records.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnrecordedLine {
    /// `"unrecorded"`
    #[serde(rename = "type", default, deserialize_with = "null_default")]
    pub r#type: String,
    #[serde(default, deserialize_with = "null_default")]
    pub sessions: Vec<Session>,
}

/// A follow line emitted after the [`Snapshot`].
///
/// Serializes identically to the underlying line struct (`AgentLine`,
/// `StoreLine`, `HostLine`, or `UnrecordedLine`), matching Go's `[]any`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum Line {
    Agent(Box<AgentLine>),
    Store(StoreLine),
    Host(HostLine),
    Unrecorded(UnrecordedLine),
}

impl Line {
    pub fn as_agent(&self) -> Option<&AgentLine> {
        match self {
            Line::Agent(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_store(&self) -> Option<&StoreLine> {
        match self {
            Line::Store(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_host(&self) -> Option<&HostLine> {
        match self {
            Line::Host(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_unrecorded(&self) -> Option<&UnrecordedLine> {
        match self {
            Line::Unrecorded(l) => Some(l),
            _ => None,
        }
    }
}

impl From<AgentLine> for Line {
    fn from(l: AgentLine) -> Self {
        Line::Agent(Box::new(l))
    }
}

impl From<StoreLine> for Line {
    fn from(l: StoreLine) -> Self {
        Line::Store(l)
    }
}

impl From<HostLine> for Line {
    fn from(l: HostLine) -> Self {
        Line::Host(l)
    }
}

impl From<UnrecordedLine> for Line {
    fn from(l: UnrecordedLine) -> Self {
        Line::Unrecorded(l)
    }
}

impl<'de> Deserialize<'de> for Line {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = Box::<serde_json::value::RawValue>::deserialize(deserializer)?;
        #[derive(Deserialize)]
        struct KindOnly {
            #[serde(rename = "type", default)]
            r#type: String,
        }
        let tag: KindOnly = serde_json::from_str(raw.get()).map_err(D::Error::custom)?;
        match tag.r#type.as_str() {
            LINE_SPAWNED | LINE_CHANGED | LINE_REMOVED => {
                serde_json::from_str::<AgentLine>(raw.get())
                    .map(Line::from)
                    .map_err(D::Error::custom)
            }
            LINE_STORE => serde_json::from_str::<StoreLine>(raw.get())
                .map(Line::Store)
                .map_err(D::Error::custom),
            LINE_HOST => serde_json::from_str::<HostLine>(raw.get())
                .map(Line::Host)
                .map_err(D::Error::custom),
            LINE_UNRECORDED => serde_json::from_str::<UnrecordedLine>(raw.get())
                .map(Line::Unrecorded)
                .map_err(D::Error::custom),
            other => Err(D::Error::custom(format!(
                "unknown export line type {other:?}"
            ))),
        }
    }
}

pub type SessionsFn = Box<dyn FnMut() -> anyhow::Result<Vec<String>>>;
pub type SessionStoreFn = Box<dyn FnMut(&str) -> String>;
pub type ListHostFn = Box<dyn FnMut(&Context, &str) -> anyhow::Result<Vec<AgentInfo>>>;
pub type NowFn = Box<dyn FnMut() -> Time>;

/// What a [`Tree`] needs from outside: `termd`, the bus, the clock.
#[derive(Default)]
pub struct Deps {
    /// This host's name.
    pub host: String,
    /// This host's `g3d-termd` namespace (default `"agents"`).
    pub namespace: String,
    /// Agent sessions' prefix, `"agent-"`.
    pub session_prefix: String,
    /// Lists the `termd` namespace's terminal ids.
    pub sessions: Option<SessionsFn>,
    /// Returns the data dir of the binary that started a session (from the
    /// `PATH` it was given), or `""`.
    pub session_store: Option<SessionStoreFn>,
    /// Lists another host's agents over the bus.
    pub list_host: Option<ListHostFn>,
    /// Clock; defaults to [`Time::now`].
    pub now: Option<NowFn>,
}

struct StoreState {
    dir: String,
    this: bool,
    db: Option<SQLiteStorage>,
    err: String,
    cursor: i64,
    events: bool,
    /// `data_version`, for stores without events.
    version: i64,
    live: BTreeMap<String, Agent>,
    ended: Vec<Agent>,
}

impl StoreState {
    fn info(&self) -> Store {
        Store {
            dir: self.dir.clone(),
            this: self.this,
            cursor: self.cursor,
            events: self.events,
            error: self.err.clone(),
        }
    }

    /// Returns this store's agents, live and departed, in tree order.
    fn agents(&self) -> Vec<Agent> {
        let mut out = Vec::with_capacity(self.live.len() + self.ended.len());
        out.extend(self.live.values().cloned());
        out.extend(self.ended.iter().cloned());
        sort_agents(&mut out);
        out
    }
}

/// The agent tree across stores and hosts, kept current by its `poll*` methods.
/// It isn't safe for concurrent use.
pub struct Tree {
    d: Deps,
    /// Include departed agents.
    all: bool,
    stores: Vec<StoreState>,
    hosts: Vec<String>,

    /// Live terminal sessions.
    sessions: BTreeSet<String>,
    /// `session -> session_store`'s answer, asked once.
    sess_store: HashMap<String, String>,
    unrecorded: Vec<Session>,
    /// Remote agents by host.
    remote: HashMap<String, Vec<Agent>>,
    /// Why a host's last list failed.
    host_errs: HashMap<String, String>,
}

/// Returns a tree over the stores in `dirs` (data dirs; the first is this
/// binary's own) and the hosts named. With `all`, departed agents are included.
pub fn new(d: Deps, dirs: &[String], hosts: &[String], all: bool) -> Tree {
    Tree::new(d, dirs, hosts, all)
}

impl Tree {
    /// Returns a tree over the stores in `dirs` (data dirs; the first is this
    /// binary's own) and the hosts named. With `all`, departed agents are
    /// included.
    pub fn new(mut d: Deps, dirs: &[String], hosts: &[String], all: bool) -> Self {
        if d.namespace.is_empty() {
            d.namespace = DEFAULT_NAMESPACE.to_string();
        }
        let mut t = Self {
            d,
            all,
            stores: Vec::new(),
            hosts: hosts.to_vec(),
            sessions: BTreeSet::new(),
            sess_store: HashMap::new(),
            unrecorded: Vec::new(),
            remote: HashMap::new(),
            host_errs: HashMap::new(),
        };
        for (i, dir) in dirs.iter().enumerate() {
            t.add_store(dir, i == 0);
        }
        t
    }

    /// Closes the stores. Dropping [`Tree`] also closes them.
    pub fn close(&mut self) {
        for s in &mut self.stores {
            if let Some(db) = s.db.take() {
                let _ = db.close();
            }
        }
    }

    fn now(&mut self) -> Time {
        match &mut self.d.now {
            Some(f) => f(),
            None => Time::now(),
        }
    }

    /// Adds a store unless it is known, returning `(index, is_new)`.
    fn add_store(&mut self, dir: &str, this: bool) -> (usize, bool) {
        let dir = filepath::clean(dir);
        if let Some(idx) = self.stores.iter().position(|s| s.dir == dir) {
            return (idx, false);
        }
        let mut s = StoreState {
            dir: dir.clone(),
            this,
            db: None,
            err: String::new(),
            cursor: 0,
            events: false,
            version: 0,
            live: BTreeMap::new(),
            ended: Vec::new(),
        };
        match open_read_only(filepath::join(&[&dir, "gomuxai.db"])) {
            Ok(db) => s.db = Some(db),
            Err(err) => s.err = format!("{err:#}"),
        }
        let idx = self.stores.len();
        self.stores.push(s);
        (idx, true)
    }

    /// Reads every store and host and returns the whole tree. Stores that
    /// sessions point at are found and read too.
    pub fn snapshot(&mut self, ctx: &Context) -> anyhow::Result<Snapshot> {
        self.refresh_sessions();
        let initial_len = self.stores.len();
        for idx in 0..initial_len {
            let is_this = self.stores[idx].this;
            if let Err(err) = self.read(idx)
                && is_this
            {
                return Err(err);
            }
        }
        self.find_stores();
        let hosts = self.hosts.clone();
        for h in &hosts {
            self.read_host(ctx, h);
        }
        Ok(self.build_snapshot())
    }

    pub(crate) fn build_snapshot(&self) -> Snapshot {
        let mut snap = Snapshot {
            r#type: "snapshot".to_string(),
            host: self.d.host.clone(),
            stores: Vec::with_capacity(self.stores.len()),
            hosts: Vec::with_capacity(self.hosts.len()),
            agents: Vec::new(),
            unrecorded: self.unrecorded.clone(),
        };
        for s in &self.stores {
            snap.stores.push(s.info());
            snap.agents.extend(s.agents());
        }
        for h in &self.hosts {
            snap.hosts.push(self.host_info(h));
            if let Some(remote) = self.remote.get(h) {
                snap.agents.extend(remote.iter().cloned());
            }
        }
        sort_agents(&mut snap.agents);
        snap
    }

    /// Fills the tree as [`Tree::snapshot`] does, but sets this store's cursor
    /// to `since`, so the next [`Tree::poll`] returns the events after it.
    /// Returns `false`, changing nothing, when those events are no longer all
    /// kept (or `since` is ahead of the store): the caller starts from a
    /// snapshot instead.
    pub fn resume(&mut self, ctx: &Context, since: i64) -> anyhow::Result<bool> {
        self.snapshot(ctx)?;
        let Some(own) = self.stores.first() else {
            return Ok(false);
        };
        let Some(db) = &own.db else {
            return Ok(false);
        };
        if !own.events || since > own.cursor {
            return Ok(false);
        }
        let (_, oldest) = db.agent_events_after(since, 1)?;
        if since < own.cursor && oldest > since.saturating_add(1) {
            return Ok(false);
        }
        self.stores[0].cursor = since;
        Ok(true)
    }

    /// Returns the lines for each store's new agent events, and a store line
    /// for each store without events whose contents changed.
    pub fn poll(&mut self, ctx: &Context) -> anyhow::Result<Vec<Line>> {
        let mut lines = Vec::new();
        self.poll_into(ctx, &mut lines)?;
        Ok(lines)
    }

    /// Like [`Tree::poll`], appending into `lines` as events are applied so a
    /// caller that logs and continues on a mid-poll store error (like
    /// `agent export --follow`) keeps the lines whose cursors already advanced.
    pub fn poll_into(&mut self, _ctx: &Context, lines: &mut Vec<Line>) -> anyhow::Result<()> {
        let store_len = self.stores.len();
        for idx in 0..store_len {
            if self.stores[idx].db.is_none() {
                continue;
            }
            if !self.stores[idx].events {
                let version = match self.stores[idx].db.as_ref().unwrap().data_version() {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if version == self.stores[idx].version {
                    continue;
                }
                lines.extend(self.reread(idx));
                continue;
            }
            loop {
                let cursor = self.stores[idx].cursor;
                let events = match self.stores[idx]
                    .db
                    .as_ref()
                    .unwrap()
                    .agent_events_after(cursor, POLL_BATCH)
                {
                    Ok((events, _)) => events,
                    Err(err) => {
                        let dir = self.stores[idx].dir.clone();
                        return Err(err.context(format!("reading agent events of {dir}")));
                    }
                };
                let count = events.len();
                for ev in &events {
                    if let Some(line) = self.apply(idx, ev) {
                        lines.push(line);
                    }
                    self.stores[idx].cursor = ev.id;
                }
                if (count as i64) < POLL_BATCH {
                    break;
                }
            }
        }
        Ok(())
    }

    /// Checks the terminal sessions and transcripts: a session that appeared or
    /// went away, a transcript that grew, a store found through a new session,
    /// and a change in the sessions no store records.
    pub fn poll_sessions(&mut self, _ctx: &Context) -> Vec<Line> {
        let mut lines = Vec::new();
        self.refresh_sessions();
        let now = self.now().utc();
        for s in &mut self.stores {
            let store_dir = s.dir.clone();
            for a in s.live.values_mut() {
                let attached = self.sessions.contains(&a.session);
                if a.attached != Some(attached) {
                    a.attached = Some(attached);
                    set_resumable(a);
                    lines.push(Line::from(AgentLine {
                        r#type: LINE_CHANGED.to_string(),
                        id: 0,
                        store: store_dir.clone(),
                        at: now,
                        reason: REASON_SESSION.to_string(),
                        agent: a.clone(),
                    }));
                }
                let (size, mtime) = (a.transcript_size, a.transcript_mtime);
                stat_transcript(a);
                if a.transcript_size != size || !a.transcript_mtime.equal(&mtime) {
                    lines.push(Line::from(AgentLine {
                        r#type: LINE_CHANGED.to_string(),
                        id: 0,
                        store: store_dir.clone(),
                        at: now,
                        reason: REASON_TRANSCRIPT.to_string(),
                        agent: a.clone(),
                    }));
                }
            }
        }
        let before = self.unrecorded.clone();
        for idx in self.find_stores() {
            let s = &self.stores[idx];
            lines.push(Line::Store(StoreLine {
                r#type: LINE_STORE.to_string(),
                store: s.info(),
                agents: s.agents(),
            }));
        }
        if before != self.unrecorded {
            lines.push(Line::Unrecorded(UnrecordedLine {
                r#type: LINE_UNRECORDED.to_string(),
                sessions: self.unrecorded.clone(),
            }));
        }
        lines
    }

    /// Lists the other hosts again and returns a host line for each whose
    /// agents changed. Each list makes that host resolve its agents' status, so
    /// call it rarely.
    pub fn poll_hosts(&mut self, ctx: &Context) -> Vec<Line> {
        let mut lines = Vec::new();
        let hosts = self.hosts.clone();
        for h in &hosts {
            let before = self.remote.get(h).cloned().unwrap_or_default();
            let had_err = self.host_err(h).to_string();
            self.read_host(ctx, h);
            let after = self.remote.get(h).map(Vec::as_slice).unwrap_or(&[]);
            if !same_agents(&before, after) || had_err != self.host_err(h) {
                lines.push(Line::Host(HostLine {
                    r#type: LINE_HOST.to_string(),
                    host: self.host_info(h),
                    agents: after.to_vec(),
                }));
            }
        }
        lines
    }

    /// Reads a store without events again. With the same agents, each one that
    /// changed gets a `changed` line; if agents came or went, a `store` line
    /// replaces them all, since without events nothing says whether they were
    /// spawned or cleaned up.
    fn reread(&mut self, idx: usize) -> Vec<Line> {
        let before = self.stores[idx].agents();
        if self.read(idx).is_err() {
            return Vec::new();
        }
        let after = self.stores[idx].agents();
        let same_keys = before.len() == after.len()
            && before
                .iter()
                .zip(&after)
                .all(|(x, y)| x.name == y.name && x.run == y.run && x.live == y.live);
        if !same_keys {
            return vec![Line::Store(StoreLine {
                r#type: LINE_STORE.to_string(),
                store: self.stores[idx].info(),
                agents: after,
            })];
        }
        let now = self.now().utc();
        let store_dir = self.stores[idx].dir.clone();
        let mut lines = Vec::new();
        for (b, a) in before.iter().zip(after) {
            if b != &a {
                lines.push(Line::from(AgentLine {
                    r#type: LINE_CHANGED.to_string(),
                    id: 0,
                    store: store_dir.clone(),
                    at: now,
                    reason: REASON_STATUS.to_string(),
                    agent: a,
                }));
            }
        }
        lines
    }

    /// Updates store `idx` for `ev` and returns its line.
    fn apply(&mut self, idx: usize, ev: &AgentEvent) -> Option<Line> {
        let store_dir = self.stores[idx].dir.clone();
        let mut line = AgentLine {
            r#type: String::new(),
            id: ev.id,
            store: store_dir.clone(),
            at: ev.at,
            reason: String::new(),
            agent: Agent::default(),
        };
        match ev.kind.as_str() {
            AGENT_SPAWNED => {
                let run = self.stores[idx]
                    .db
                    .as_ref()?
                    .get_run(ev.run)
                    .ok()
                    .flatten()?;
                let mut a = self.agent_from_run(&store_dir, &run);
                a.live = true;
                if let Ok(Some(rec)) = self.stores[idx].db.as_ref()?.get_agent(&a.name) {
                    a.adopted = rec.adopted;
                }
                // The session was created just before the record.
                self.refresh_sessions();
                let attached = self.sessions.contains(&a.session);
                a.attached = Some(attached);
                set_resumable(&mut a);
                self.stores[idx].live.insert(a.name.clone(), a.clone());
                line.r#type = LINE_SPAWNED.to_string();
                line.agent = a;
            }
            AGENT_STATUS => {
                let updated = if let Some(a) = self.stores[idx].live.get_mut(&ev.agent) {
                    apply_status_event(a, ev);
                    a.clone()
                } else {
                    // Removed since (a resumed follower replays events from before
                    // the tree it read): describe the run as it was.
                    let run = self.stores[idx]
                        .db
                        .as_ref()?
                        .get_run(ev.run)
                        .ok()
                        .flatten()?;
                    let mut r = self.agent_from_run(&store_dir, &run);
                    r.live = true;
                    r.ended_at = Time::zero();
                    apply_status_event(&mut r, ev);
                    r
                };
                line.r#type = LINE_CHANGED.to_string();
                line.reason = REASON_STATUS.to_string();
                line.agent = updated;
            }
            AGENT_REMOVED => {
                let run = self.stores[idx]
                    .db
                    .as_ref()?
                    .get_run(ev.run)
                    .ok()
                    .flatten()?;
                let mut a = self.agent_from_run(&store_dir, &run);
                if let Some(prev) = self.stores[idx].live.remove(&ev.agent) {
                    a.event = prev.event;
                    a.detail = prev.detail;
                    a.tool = prev.tool;
                    a.status_at = prev.status_at;
                    if !prev.session_id.is_empty() {
                        a.session_id = prev.session_id;
                    }
                }
                if self.all {
                    self.stores[idx].ended.push(a.clone());
                }
                line.r#type = LINE_REMOVED.to_string();
                line.agent = a;
            }
            _ => return None,
        }
        Some(Line::from(line))
    }

    /// Reads store `idx` whole.
    fn read(&mut self, idx: usize) -> anyhow::Result<()> {
        let dir = self.stores[idx].dir.clone();
        let Some(db) = &self.stores[idx].db else {
            anyhow::bail!("opening {}: {}", dir, self.stores[idx].err);
        };
        let version = db.data_version().ok();
        let tree = match db.read_tree(self.all) {
            Ok(t) => t,
            Err(err) => {
                self.stores[idx].err = format!("{err:#}");
                return Err(err.context(format!("reading {dir}")));
            }
        };
        if let Some(v) = version {
            self.stores[idx].version = v;
        }

        let mut state: HashMap<&str, HashMap<&str, &StateEntry>> = HashMap::new();
        for e in &tree.state {
            state
                .entry(e.agent_name.as_str())
                .or_default()
                .insert(e.key.as_str(), e);
        }

        let mut live = BTreeMap::new();
        for rec in &tree.agents {
            let mut a = Agent {
                name: rec.name.clone(),
                host: self.d.host.clone(),
                kind: KIND_TERMD.to_string(),
                parent: rec.spawned_by.clone(),
                backend: rec.backend.clone(),
                session: rec.session_name.clone(),
                namespace: self.d.namespace.clone(),
                store: dir.clone(),
                cwd: rec.cwd.clone(),
                created_at: rec.created_at.utc(),
                live: true,
                adopted: rec.adopted,
                ..Agent::default()
            };
            if let Some(run) = tree.runs.get(&rec.name) {
                a.run = run.id;
                if a.cwd.is_empty() {
                    a.cwd = run.cwd.clone();
                }
            }
            let attached = self.sessions.contains(&a.session);
            a.attached = Some(attached);
            let keys = state.get(rec.name.as_str());
            let value = |k: &str| -> String {
                keys.and_then(|m| m.get(k))
                    .map(|e| e.value.clone())
                    .unwrap_or_default()
            };
            if let Some(e) = keys.and_then(|m| m.get("_status")) {
                a.status = e.value.clone();
                a.status_at = e.updated_at.utc();
            }
            a.end_reason = value("_end_reason");
            a.event = value("_event");
            a.detail = value("_detail");
            a.tool = value("_tool");
            a.tool_input = value("_tool_input");
            if a.status != "waiting" {
                // As in events: only a pending prompt's.
                a.tool_input.clear();
            }
            a.session_id = value("_session_id");
            a.transcript = value("_transcript");
            a.dialect = dialect(&a.backend, &a.transcript).to_string();
            stat_transcript(&mut a);
            set_resumable(&mut a);
            live.insert(a.name.clone(), a);
        }

        let mut ended = Vec::with_capacity(tree.ended.len());
        for run in &tree.ended {
            ended.push(self.agent_from_run(&dir, run));
        }

        let s = &mut self.stores[idx];
        s.err.clear();
        s.cursor = tree.cursor;
        s.events = tree.events;
        s.live = live;
        s.ended = ended;
        Ok(())
    }

    /// Describes a run: departed if it ended, otherwise without the live
    /// agent's status.
    fn agent_from_run(&self, store_dir: &str, run: &Run) -> Agent {
        let mut a = Agent {
            name: run.name.clone(),
            host: self.d.host.clone(),
            kind: KIND_TERMD.to_string(),
            run: run.id,
            parent: run.parent.clone(),
            backend: run.backend.clone(),
            session: run.session.clone(),
            namespace: self.d.namespace.clone(),
            store: store_dir.to_string(),
            cwd: run.cwd.clone(),
            created_at: run.created_at.utc(),
            ended_at: run.ended_at.utc(),
            status: run.status.clone(),
            end_reason: run.end_reason.clone(),
            transcript: run.transcript.clone(),
            ..Agent::default()
        };
        if !run.ended_at.is_zero() {
            a.attached = Some(false);
            // Cleanup closed its pane, whatever its last report said, unless
            // that report already said how its session ended.
            if a.status != Status::Ended.as_str() || a.end_reason.is_empty() {
                a.status = Status::Ended.as_str().to_string();
                a.end_reason = END_CLOSED.as_str().to_string();
            }
        }
        if a.transcript.is_empty() && run.id == 0 {
            // Spawned before runs: the transcripts list beside its log has
            // the newest one.
            let list_path =
                filepath::join(&[store_dir, "logs", &format!("{}.transcripts", run.name)]);
            a.transcript = last_transcript(&list_path);
        }
        // Runs don't record the session id; the transcript's path names it.
        a.session_id = session_id_from_path(&a.transcript);
        a.dialect = dialect(&a.backend, &a.transcript).to_string();
        stat_transcript(&mut a);
        set_resumable(&mut a);
        a
    }

    /// Re-lists the terminal sessions. A failure (no server) is no sessions.
    fn refresh_sessions(&mut self) {
        self.sessions.clear();
        let Some(sessions_fn) = &mut self.d.sessions else {
            return;
        };
        if let Ok(names) = sessions_fn() {
            for n in names {
                self.sessions.insert(n);
            }
        }
    }

    /// Looks at each agent session no store records, adds and reads the store
    /// its binary uses, if that is new, and updates the unrecorded list.
    /// Returns the indices of stores it added and read without error.
    fn find_stores(&mut self) -> Vec<usize> {
        let mut added = Vec::new();
        let mut unrecorded = Vec::new();
        let sessions: Vec<String> = self.sessions.iter().cloned().collect();
        for sess in sessions {
            if !sess.starts_with(&self.d.session_prefix) || self.recorded(&sess) {
                continue;
            }
            let dir = if let Some(cached) = self.sess_store.get(&sess) {
                cached.clone()
            } else if let Some(session_store) = &mut self.d.session_store {
                let d = session_store(&sess);
                self.sess_store.insert(sess.clone(), d.clone());
                d
            } else {
                String::new()
            };
            if !dir.is_empty() {
                let (idx, is_new) = self.add_store(&dir, false);
                if is_new {
                    if self.read(idx).is_ok() {
                        added.push(idx);
                    }
                    if self.recorded(&sess) {
                        continue;
                    }
                }
            }
            unrecorded.push(Session {
                session: sess,
                store: dir,
            });
        }
        self.sess_store
            .retain(|sess, _| self.sessions.contains(sess));
        self.unrecorded = unrecorded;
        added
    }

    fn recorded(&self, session: &str) -> bool {
        self.stores
            .iter()
            .any(|s| s.live.values().any(|a| a.session == session))
    }

    /// Lists `host`'s agents over the bus.
    fn read_host(&mut self, ctx: &Context, host: &str) {
        let Some(list_host) = &mut self.d.list_host else {
            self.remote.insert(host.to_string(), Vec::new());
            return;
        };
        let infos = match list_host(ctx, host) {
            Ok(infos) => infos,
            Err(err) => {
                self.remote.insert(host.to_string(), Vec::new());
                self.host_errs.insert(host.to_string(), format!("{err:#}"));
                return;
            }
        };
        self.host_errs.insert(host.to_string(), String::new());
        let mut agents = Vec::with_capacity(infos.len());
        for info in infos {
            // Fields an older host doesn't send stay absent: unknown.
            agents.push(Agent {
                name: info.name,
                host: host.to_string(),
                kind: KIND_TERMD.to_string(),
                parent: info.spawned_by,
                backend: info.backend,
                session: info.session,
                cwd: info.cwd,
                created_at: Time(info.created_at.0).utc(),
                live: true,
                attached: info.attached,
                resumable: info.resumable,
                status: info.status,
                end_reason: info.end_reason,
                detail: info.detail,
                session_id: info.session_id,
                adopted: info.adopted,
                ..Agent::default()
            });
        }
        sort_agents(&mut agents);
        self.remote.insert(host.to_string(), agents);
    }

    fn host_err(&self, host: &str) -> &str {
        self.host_errs.get(host).map(String::as_str).unwrap_or("")
    }

    fn host_info(&self, host: &str) -> Host {
        Host {
            host: host.to_string(),
            agents: self.remote.get(host).map_or(0, Vec::len),
            error: self.host_err(host).to_string(),
        }
    }
}

fn apply_status_event(a: &mut Agent, ev: &AgentEvent) {
    a.status = ev.status.clone();
    a.end_reason = ev.end_reason.clone();
    a.event = ev.event.clone();
    a.detail = ev.detail.clone();
    a.tool = ev.tool.clone();
    a.tool_input = ev.tool_input.clone();
    a.status_at = ev.at;
    if !ev.session_id.is_empty() {
        if ev.session_id != a.session_id && ev.transcript.is_empty() {
            // Another session, whose transcript no hook has named yet: the old
            // one's isn't its.
            a.transcript.clear();
            a.dialect.clear();
            a.transcript_size = 0;
            a.transcript_mtime = Time::zero();
        }
        a.session_id = ev.session_id.clone();
    }
    if !ev.transcript.is_empty() {
        a.transcript = ev.transcript.clone();
        a.dialect = dialect(&a.backend, &a.transcript).to_string();
    }
    stat_transcript(a);
    set_resumable(a);
}

/// Works out whether `agent resume` would continue `a`, with resume's rules
/// (`agent_base::types::resumable`) applied to what `export` knows: the last
/// report, not the resolved status, except that a live agent whose terminal
/// session is gone has ended (`agent status` says `ended (gone)`). A departed
/// agent has no record left to resume.
fn set_resumable(a: &mut Agent) {
    let owned;
    let session_id = if a.session_id.is_empty() {
        owned = session_id_from_path(&a.transcript);
        owned.as_str()
    } else {
        a.session_id.as_str()
    };
    let gone = a.attached == Some(false);
    let ended = a.status == Status::Ended.as_str() || gone;
    let r = a.live && a.ended_at.is_zero() && resumable(&a.backend, a.adopted, session_id, ended);
    a.resumable = Some(r);
}

/// Orders agents by creation, then name, host, and run.
fn sort_agents(agents: &mut [Agent]) {
    agents.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.host.cmp(&b.host))
            .then_with(|| a.run.cmp(&b.run))
    });
}

fn same_agents(a: &[Agent], b: &[Agent]) -> bool {
    a == b
}

/// Fills in the transcript file's size and time, or clears them when there is
/// none.
fn stat_transcript(a: &mut Agent) {
    a.transcript_size = 0;
    a.transcript_mtime = Time::zero();
    if a.transcript.is_empty() {
        return;
    }
    if let Ok(info) = fs::metadata(&a.transcript)
        && let Ok(modified) = info.modified()
    {
        a.transcript_size = info.len() as i64;
        a.transcript_mtime = Time::from(DateTime::<Utc>::from(modified)).utc();
    }
}

/// Names the format a transcript is in.
fn dialect(backend: &str, transcript: &str) -> &'static str {
    if transcript.is_empty() {
        ""
    } else if backend == "claude" || transcript.contains("/.claude/projects/") {
        "claude"
    } else if backend == "agy" || transcript.contains("/antigravity-cli/brain/") {
        "agy"
    } else {
        ""
    }
}

/// Returns the newest path in a transcripts list (`"<backend>\t<path>"` lines),
/// or `""`.
fn last_transcript(list_path: &str) -> String {
    let Ok(file) = File::open(list_path) else {
        return String::new();
    };
    let mut last = String::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Some((_, p)) = line.split_once('\t')
            && !p.is_empty()
        {
            last = p.to_string();
        }
    }
    last
}
