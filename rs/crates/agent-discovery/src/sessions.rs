use std::collections::HashSet;
use std::fs::Metadata;

use agent_base::gostd::filepath;
use agent_transcript::{agy_conversation_dir, claude_subagent_session, session_id_from_path};
use chrono::{DateTime, Datelike, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::agy::{new_agy_index, probe_agy, scan_agy};
use crate::claude::{is_claude_subagent, list_claude, probe_claude, probe_claude_subagent};
use crate::dirs::{Dir, HARNESS_AGY, HARNESS_CLAUDE, HARNESS_KIMI, harness_of, is_dir, os_path};
use crate::kimi::{list_kimi, probe_kimi};

/// Links a subagent's session to the one that started it.
pub type ParentSession = agent_transcript::ParentSession;

/// One agent session, as `docs/EVENTS.md` describes it: a transcript and what
/// it says about itself, the workgroup agent that owns it (`None` if none), and
/// its status.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Session {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub harness: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub size: i64,
    #[serde(
        default,
        serialize_with = "serialize_opt_time",
        deserialize_with = "deserialize_opt_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub mtime: Option<DateTime<Utc>>,
    /// The working directory the session recorded; `project` is its last
    /// component, never the storage folder's name.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    /// The latest title the harness recorded.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// The name the harness recorded for the session.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent_name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_branch: String,
    #[serde(default)]
    pub workgroup: Option<Workgroup>,
    #[serde(default)]
    pub status: String,
    /// Says how the session ended, with a status of `ended`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_reason: String,
    #[serde(
        default,
        serialize_with = "serialize_opt_time",
        deserialize_with = "deserialize_opt_time",
        skip_serializing_if = "Option::is_none"
    )]
    pub status_at: Option<DateTime<Utc>>,
    /// Set on a subagent's session: the session that started it, and the tool
    /// call that did, where the harness records it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<ParentSession>,
    /// The session this one continued in (Claude Code's `continued-in` record).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub continued_in: String,

    #[serde(skip)]
    #[doc(hidden)]
    pub live: Liveness,
    #[serde(skip)]
    #[doc(hidden)]
    pub state: SubState,
    #[serde(skip)]
    #[doc(hidden)]
    pub probed: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub has_info: bool,
    #[serde(skip)]
    #[doc(hidden)]
    pub key: String,
    #[serde(skip)]
    #[doc(hidden)]
    pub folder: String,
}

impl PartialEq for Session {
    fn eq(&self, other: &Self) -> bool {
        self.session_id == other.session_id
            && self.harness == other.harness
            && self.path == other.path
            && self.size == other.size
            && self.mtime == other.mtime
            && self.cwd == other.cwd
            && self.project == other.project
            && self.title == other.title
            && self.agent_name == other.agent_name
            && self.model == other.model
            && self.version == other.version
            && self.git_branch == other.git_branch
            && self.workgroup == other.workgroup
            && self.status == other.status
            && self.end_reason == other.end_reason
            && self.status_at == other.status_at
            && self.parent_session == other.parent_session
            && self.continued_in == other.continued_in
    }
}

impl Eq for Session {}

/// Formats `Some(dt)` as Go's `time.Time.MarshalJSON` in UTC (`RFC3339Nano`:
/// fractional nanoseconds with trailing zeros omitted, ending in `Z`).
fn serialize_opt_time<S: Serializer>(
    dt: &Option<DateTime<Utc>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match dt {
        None => serializer.serialize_none(),
        Some(t) => serializer.serialize_str(&format_rfc3339_nano_utc(t)),
    }
}

pub(crate) fn format_rfc3339_nano_utc(t: &DateTime<Utc>) -> String {
    let nanos = t.nanosecond() % 1_000_000_000;
    let base = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        t.year(),
        t.month(),
        t.day(),
        t.hour(),
        t.minute(),
        t.second()
    );
    if nanos == 0 {
        format!("{base}Z")
    } else {
        let mut frac = format!("{nanos:09}");
        while frac.ends_with('0') {
            frac.pop();
        }
        format!("{base}.{frac}Z")
    }
}

fn deserialize_opt_time<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error> {
    let opt: Option<String> = Option::deserialize(deserializer)?;
    let Some(s) = opt else {
        return Ok(None);
    };
    if s.is_empty() || s == "0001-01-01T00:00:00Z" {
        return Ok(None);
    }
    let dt = DateTime::parse_from_rfc3339(&s).map_err(serde::de::Error::custom)?;
    Ok(Some(dt.with_timezone(&Utc)))
}

/// The gomuxai agent that owns a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Workgroup {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String, // the agent that spawned it; absent for a person
    pub store: String, // the data dir that records it
    pub host: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[doc(hidden)]
pub enum Liveness {
    #[default]
    Unknown,
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[doc(hidden)]
pub enum SubState {
    #[default]
    Unrecorded,
    Finished,
    Running,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FileStat {
    pub(crate) size: i64,
    pub(crate) mtime: Option<DateTime<Utc>>,
}

impl FileStat {
    pub(crate) fn from_metadata(info: &Metadata) -> Self {
        let mtime = info.modified().ok().map(DateTime::<Utc>::from);
        Self {
            size: info.len() as i64,
            mtime,
        }
    }
}

/// Lists the sessions in `dirs`, newest first, with what their files say about
/// them. `workgroup` and `status` are left for [`crate::resolve`]. A directory
/// that doesn't exist adds nothing; a file that can't be read is left out.
pub fn scan(dirs: &[Dir]) -> Vec<Session> {
    let mut out = list(dirs);
    for s in &mut out {
        s.fill();
    }
    out
}

/// Lists the sessions in `dirs`, newest first, as stubs: from directory
/// listings and stat alone, reading no transcript. agy conversations are read
/// in full, since what they say about themselves is in small index files and
/// never in the transcript.
pub(crate) fn list(dirs: &[Dir]) -> Vec<Session> {
    let mut out = Vec::new();
    for d in dirs {
        if !is_dir(&d.path) {
            continue;
        }
        match d.harness.as_str() {
            HARNESS_CLAUDE => out.extend(list_claude(&d.path)),
            HARNESS_AGY => out.extend(scan_agy(&d.path)),
            HARNESS_KIMI => out.extend(list_kimi(&d.path)),
            _ => {}
        }
    }
    dedupe(out)
}

pub(crate) fn stub(harness: &str, path: &str, stat: FileStat) -> Session {
    Session {
        session_id: session_id_from_path(path),
        harness: harness.to_string(),
        path: path.to_string(),
        size: stat.size,
        mtime: stat.mtime,
        has_info: true,
        ..Session::default()
    }
}

pub(crate) fn stub_at(path: &str) -> Option<Session> {
    let info = std::fs::metadata(os_path(path)).ok()?;
    if info.is_dir() {
        return None;
    }
    match harness_of(path) {
        HARNESS_KIMI | HARNESS_AGY => return probe(path),
        _ => {}
    }
    let mut s = stub(HARNESS_CLAUDE, path, FileStat::from_metadata(&info));
    let parent = claude_subagent_session(path);
    if !parent.is_empty() {
        s.parent_session = Some(ParentSession {
            session_id: filepath::base(&parent),
            tool_id: String::new(),
        });
    }
    Some(s)
}

impl Session {
    /// Reads a stub's transcript (and the files beside it) for the rest of what
    /// it says about itself, as [`scan`] would have.
    pub(crate) fn fill(&mut self) {
        if self.probed || !self.has_info {
            return;
        }
        let stat = FileStat {
            size: self.size,
            mtime: self.mtime,
        };
        let mut full = if self.harness == HARNESS_KIMI {
            let mut f = probe_kimi(&self.path, stat);
            if f.cwd.is_empty() {
                f.cwd = self.cwd.clone();
                f.project = self.project.clone();
            }
            f
        } else if self.harness == HARNESS_AGY {
            probe(&self.path).unwrap_or_default()
        } else if is_claude_subagent(&self.path) {
            probe_claude_subagent(&self.path, stat)
        } else {
            probe_claude(&self.path, stat)
        };
        full.key = self.key.clone();
        full.folder = self.folder.clone();
        *self = full;
    }
}

/// Reads the one transcript at `path`, as [`scan`] would have, for a workgroup
/// agent's transcript outside the scanned directories. Returns `None` if there
/// is no such file.
pub fn probe(path: &str) -> Option<Session> {
    let info = std::fs::metadata(os_path(path)).ok()?;
    if info.is_dir() {
        return None;
    }
    let stat = FileStat::from_metadata(&info);
    match harness_of(path) {
        HARNESS_KIMI => Some(probe_kimi(path, stat)),
        HARNESS_AGY => {
            let conv = agy_conversation_dir(path);
            if conv.is_empty() {
                return None;
            }
            let home = filepath::dir(&filepath::dir(&conv));
            let idx = new_agy_index(&home);
            Some(probe_agy(&idx, &filepath::base(&conv), path, stat))
        }
        _ => {
            if is_claude_subagent(path) {
                Some(probe_claude_subagent(path, stat))
            } else {
                Some(probe_claude(path, stat))
            }
        }
    }
}

fn dedupe(sessions: Vec<Session>) -> Vec<Session> {
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(sessions.len());
    for s in sessions {
        let key = filepath::clean(&s.path);
        if seen.insert(key) {
            out.push(s);
        }
    }
    sort_sessions(&mut out);
    out
}

pub(crate) fn sort_sessions(sessions: &mut [Session]) {
    sessions.sort_by(|a, b| {
        if a.mtime != b.mtime {
            b.mtime.cmp(&a.mtime)
        } else {
            a.session_id.cmp(&b.session_id)
        }
    });
}

pub(crate) fn from_meta(
    harness: &str,
    path: &str,
    stat: FileStat,
    m: agent_transcript::Session,
) -> Session {
    Session {
        session_id: session_id_from_path(path),
        harness: harness.to_string(),
        path: path.to_string(),
        size: stat.size,
        mtime: stat.mtime,
        cwd: m.cwd,
        project: m.project,
        title: m.title,
        agent_name: m.agent_name,
        model: m.model,
        version: m.version,
        git_branch: m.git_branch,
        parent_session: m.parent_session,
        continued_in: m.continued_in,
        probed: true,
        has_info: true,
        ..Session::default()
    }
}
