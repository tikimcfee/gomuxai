use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use agent_base::gostd::filepath;
use agent_base::types::{END_CLOSED, END_FINISHED, END_GONE, END_PARENT, END_REPLACED, EndReason};
use agent_transcript::session_id_from_path;
use chrono::{DateTime, TimeDelta, Utc};

use crate::dirs::HARNESS_CLAUDE;
use crate::sessions::{Liveness, Session, SubState, Workgroup, sort_sessions, stub_at};

// Statuses a session can have.
pub const STATUS_IDLE: &str = "idle";
pub const STATUS_PROCESSING: &str = "processing";
pub const STATUS_WAITING: &str = "waiting";
pub const STATUS_ERROR: &str = "error";
pub const STATUS_ENDED: &str = "ended";
pub const STATUS_UNKNOWN: &str = "unknown";

/// What a backend CLI's own record of its sessions (Claude Code's session
/// registry) says about one session.
#[derive(Debug, Default)]
pub struct Native {
    pub queried: bool, // the CLI answered
    pub listed: bool,  // it lists the session
    pub status: String,
    /// Why the agent's pane couldn't be read, when the session isn't listed and
    /// the pane was asked what it runs instead.
    pub pane_err: Option<anyhow::Error>,
    /// Why the CLI couldn't be asked (`queried` false).
    pub err: Option<anyhow::Error>,
}

// How reconcile_native decides.
pub const NATIVE_WINS: i32 = 0; // the CLI's status
pub const NATIVE_GONE: i32 = 1; // the CLI doesn't list a session the report says is alive
pub const REPORT_STANDS: i32 = 2; // the report stands
pub const REPORT_DENIED: i32 = 3; // the report, over the CLI's waiting just after a deny

/// Bounds how long after a deny native status may still say `waiting` before it
/// is believed again. Observed: under a second.
pub const DENIED_LAG: Duration = Duration::from_secs(5);

/// The one rule for a session that has both a hook report and a place in its
/// CLI's own session list, used by `agent status` and `agent sessions` alike.
pub fn reconcile_native<F: FnOnce() -> bool>(n: &Native, report: &str, denied: F) -> i32 {
    if n.listed && n.status == STATUS_UNKNOWN {
        REPORT_STANDS
    } else if n.listed && n.status == STATUS_WAITING && denied() {
        REPORT_DENIED
    } else if n.listed {
        NATIVE_WINS
    } else if n.queried && !report.is_empty() && report != STATUS_ENDED {
        NATIVE_GONE
    } else {
        REPORT_STANDS
    }
}

/// How recently a session with no other sign of life must have written its
/// transcript to count as possibly running (`unknown`) rather than `ended`
/// (`gone`).
pub const RECENT_WINDOW: Duration = Duration::from_secs(10 * 60);

/// One run of a gomuxai workgroup agent, live or departed, as the join needs
/// it.
#[derive(Debug, Clone, Default)]
pub struct Agent {
    pub name: String,
    pub parent: String,
    pub store: String,
    pub host: String,
    pub backend: String,
    pub live: bool, // it has a record (not cleaned up)
    pub created_at: Option<DateTime<Utc>>,
    pub ended_at: Option<DateTime<Utc>>,
    pub status: String,
    pub end_reason: String,
    pub event: String,
    pub status_at: Option<DateTime<Utc>>,
    pub session_id: String,
    pub transcript: String,
    pub history: Vec<String>,
}

/// What the harnesses say is running on this host now.
#[derive(Debug, Clone, Default)]
pub struct Live {
    /// Maps the id of every Claude Code session running on this host to its
    /// status, from Claude Code's session registry; `None` if that couldn't be
    /// asked.
    pub claude: Option<HashMap<String, String>>,
    /// Maps a session id to its last report from the global hook (`agent hook`),
    /// which hears every Claude Code session on the host.
    pub reports: HashMap<String, Report>,
    pub now: Option<DateTime<Utc>>,
}

/// A session's last report from the global hook.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub status: String,
    pub end_reason: String,
    pub at: Option<DateTime<Utc>>,
}

/// Joins `sessions` to the workgroup `agents` whose transcripts they are, adds
/// those agents' transcripts that `sessions` lack (outside the scanned dirs)
/// and live agents with no transcript yet, and sets every session's status.
/// Returns the sessions newest first.
pub fn resolve(sessions: Vec<Session>, agents: Vec<Agent>, live: Live) -> Vec<Session> {
    let mut j = join(sessions, agents);
    let mut out = std::mem::take(&mut j.sessions);
    j.status(&mut out, &live);
    sort_sessions(&mut out);
    out
}

#[derive(Debug, Clone, Default)]
pub(crate) struct Joined {
    pub(crate) sessions: Vec<Session>,
    pub(crate) agents: Vec<Agent>,
    /// Maps a transcript's canonical path (`path_key`) to its run index in
    /// `agents`.
    pub(crate) owner: HashMap<String, usize>,
}

pub(crate) fn join(mut sessions: Vec<Session>, mut agents: Vec<Agent>) -> Joined {
    let mut by_path: HashMap<String, usize> = HashMap::new();
    for (i, s) in sessions.iter().enumerate() {
        by_path.insert(path_key(s), i);
    }
    for a in &agents {
        let mut paths = a.history.clone();
        paths.push(a.transcript.clone());
        for p in paths {
            let key = canonical(&p);
            if p.is_empty() || by_path.contains_key(&key) {
                continue;
            }
            if let Some(s) = stub_at(&p) {
                by_path.insert(key, sessions.len());
                sessions.push(s);
            }
        }
    }
    let mut owner = owners(&agents, |key| {
        by_path.get(key).and_then(|&i| sessions[i].mtime)
    });
    // A live agent whose current session no hook has named a transcript for
    // owns the session the scan found under that id.
    for (i, a) in agents.iter_mut().enumerate() {
        if !a.live || !a.transcript.is_empty() || a.session_id.is_empty() {
            continue;
        }
        for s in &sessions {
            if s.session_id == a.session_id && s.harness == a.backend && s.parent_session.is_none()
            {
                a.transcript = s.path.clone();
                owner.insert(path_key(s), i);
                break;
            }
        }
    }
    for a in &agents {
        if !a.live {
            continue;
        }
        if !a.transcript.is_empty() && by_path.contains_key(&canonical(&a.transcript)) {
            continue;
        }
        let mut s = Session {
            session_id: a.session_id.clone(),
            harness: a.backend.clone(),
            path: a.transcript.clone(),
            ..Session::default()
        };
        if !a.transcript.is_empty() {
            let from_path = session_id_from_path(&a.transcript);
            s.session_id = first_non_empty(&[&a.session_id, &from_path]);
        } else {
            s.workgroup = Some(workgroup_of(a));
            s.status = reported(a);
            s.status_at = a.status_at;
            if s.status == STATUS_ENDED {
                s.end_reason = first_non_empty(&[&a.end_reason, END_GONE.as_str()]);
            }
        }
        sessions.push(s);
    }
    Joined {
        sessions,
        agents,
        owner,
    }
}

impl Joined {
    pub(crate) fn status(&self, sessions: &mut [Session], live: &Live) {
        let now = live.now.unwrap_or_else(Utc::now);
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut done = vec![false; sessions.len()];
        for (i, s) in sessions.iter_mut().enumerate() {
            s.fill();
            if s.parent_session.is_some() {
                continue;
            }
            done[i] = true;
            if s.path.is_empty() && s.workgroup.is_some() {
                continue;
            }
            if let Some(&ai) = self.owner.get(&path_key(s)) {
                let a = &self.agents[ai];
                s.workgroup = Some(workgroup_of(a));
                owned_status(s, a, live, now);
            } else {
                local_status(s, live, now);
            }
            by_id.insert(format!("{}\0{}", s.harness, s.session_id), i);
        }

        let mut progress = true;
        let mut wait_for_parent = true;
        loop {
            if !progress {
                if !wait_for_parent {
                    break;
                }
                wait_for_parent = false;
            }
            progress = false;
            for i in 0..sessions.len() {
                if done[i] {
                    continue;
                }
                let parent_sess_id = sessions[i]
                    .parent_session
                    .as_ref()
                    .map(|p| p.session_id.clone())
                    .unwrap_or_default();
                let key = format!("{}\0{}", sessions[i].harness, parent_sess_id);
                let parent_idx = by_id.get(&key).copied();
                if parent_idx.is_none() && wait_for_parent && has_session(sessions, &done, &key) {
                    continue;
                }
                let parent_info = parent_idx
                    .map(|pi| (sessions[pi].workgroup.clone(), sessions[pi].status.clone()));
                let s = &mut sessions[i];
                if let Some(&ai) = self.owner.get(&path_key(s)) {
                    s.workgroup = Some(workgroup_of(&self.agents[ai]));
                } else if let Some((ref wg, _)) = parent_info {
                    s.workgroup = wg.clone();
                }
                subagent_status(
                    s,
                    parent_info.as_ref().map(|(_, st)| st.as_str()),
                    live,
                    now,
                );
                by_id.insert(format!("{}\0{}", s.harness, s.session_id), i);
                done[i] = true;
                progress = true;
                wait_for_parent = true;
            }
        }
    }
}

fn has_session(sessions: &[Session], done: &[bool], key: &str) -> bool {
    sessions
        .iter()
        .enumerate()
        .any(|(i, s)| !done[i] && format!("{}\0{}", s.harness, s.session_id) == key)
}

pub(crate) fn path_key(s: &Session) -> String {
    if !s.key.is_empty() {
        return s.key.clone();
    }
    canonical(&s.path)
}

pub(crate) fn canonical(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    filepath::eval_symlinks(path).unwrap_or_else(|_| filepath::clean(path))
}

fn owners<F: Fn(&str) -> Option<DateTime<Utc>>>(
    agents: &[Agent],
    mtime: F,
) -> HashMap<String, usize> {
    let mut owner: HashMap<String, usize> = HashMap::new();
    let mut runs: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (i, a) in agents.iter().enumerate() {
        let key = format!("{}\0{}", canonical(&a.store), a.name);
        runs.entry(key).or_default().push(i);
    }
    for (_, mut rs) in runs {
        rs.sort_by_key(|&i| agents[i].created_at);
        for &ai in &rs {
            for raw_p in &agents[ai].history {
                let p = canonical(raw_p);
                if owner.contains_key(&p) {
                    continue;
                }
                let mut chosen = *rs.last().unwrap();
                if let Some(at) = mtime(&p) {
                    chosen = rs[0];
                    for &ri in &rs {
                        if agents[ri].created_at.is_none_or(|c| c <= at) {
                            chosen = ri;
                        }
                    }
                }
                owner.insert(p, chosen);
            }
        }
        for &ai in &rs {
            if !agents[ai].transcript.is_empty() {
                owner.insert(canonical(&agents[ai].transcript), ai);
            }
        }
    }
    owner
}

fn workgroup_of(a: &Agent) -> Workgroup {
    Workgroup {
        name: a.name.clone(),
        parent: a.parent.clone(),
        store: a.store.clone(),
        host: a.host.clone(),
    }
}

fn reported(a: &Agent) -> String {
    first_non_empty(&[&a.status, STATUS_UNKNOWN])
}

fn owned_status(s: &mut Session, a: &Agent, live: &Live, now: DateTime<Utc>) {
    let current = canonical(&a.transcript) == canonical(&s.path);
    if !a.live && current {
        ended(s, &END_CLOSED, s.mtime);
        if a.ended_at.is_some() {
            s.status_at = a.ended_at;
        }
        if a.status == STATUS_ENDED && !a.end_reason.is_empty() {
            s.end_reason = a.end_reason.clone();
        }
        return;
    }
    if !current {
        ended(s, &END_REPLACED, s.mtime);
        return;
    }
    s.status = reported(a);
    s.status_at = a.status_at;
    if s.status == STATUS_ENDED {
        s.end_reason = first_non_empty(&[&a.end_reason, END_GONE.as_str()]);
    }
    if s.harness == HARNESS_CLAUDE {
        let (queried, listed, st) = match live.claude {
            Some(ref m) => match m.get(&s.session_id) {
                Some(st) => (true, true, st.clone()),
                None => (true, false, String::new()),
            },
            None => (false, false, String::new()),
        };
        let n = Native {
            queried,
            listed,
            status: st.clone(),
            pane_err: None,
            err: None,
        };
        let denied = || a.event == "denied" && within_duration(now, a.status_at, DENIED_LAG);
        match reconcile_native(&n, &a.status, denied) {
            NATIVE_WINS => {
                if st != s.status {
                    s.status = st;
                    s.status_at = s.mtime;
                    s.end_reason.clear();
                }
            }
            NATIVE_GONE => ended(s, &END_GONE, s.mtime),
            _ => {}
        }
    } else if s.live == Liveness::No && s.status != STATUS_ENDED {
        ended(s, &END_GONE, s.mtime);
    }
}

fn ended(s: &mut Session, reason: &EndReason, at: Option<DateTime<Utc>>) {
    s.status = STATUS_ENDED.to_string();
    s.end_reason = reason.as_str().to_string();
    s.status_at = at;
}

fn local_status(s: &mut Session, live: &Live, now: DateTime<Utc>) {
    s.status_at = s.mtime;
    if s.harness == HARNESS_CLAUDE {
        let Some(ref claude_map) = live.claude else {
            s.status = STATUS_UNKNOWN.to_string();
            return;
        };
        if let Some(st) = claude_map.get(&s.session_id) {
            s.status = st.clone();
        } else if let Some(r) = live.reports.get(&s.session_id)
            && r.status == STATUS_ENDED
            && !r.end_reason.is_empty()
        {
            ended(s, &EndReason::from(r.end_reason.as_str()), r.at);
        } else {
            ended(s, &END_GONE, s.mtime);
        }
    } else if s.live == Liveness::Yes {
        s.status = STATUS_UNKNOWN.to_string();
    } else if s.live == Liveness::No {
        ended(s, &END_GONE, s.mtime);
    } else {
        recent(s, now);
    }
}

fn subagent_status(s: &mut Session, parent_status: Option<&str>, live: &Live, now: DateTime<Utc>) {
    s.status_at = s.mtime;
    let r = live.reports.get(&s.session_id);
    if s.state == SubState::Finished
        || r.is_some_and(|r| r.status == STATUS_ENDED && r.end_reason == END_FINISHED.as_str())
    {
        ended(s, &END_FINISHED, s.mtime);
    } else if s.live == Liveness::No {
        ended(s, &END_GONE, s.mtime);
    } else if parent_status == Some(STATUS_ENDED) {
        ended(s, &END_PARENT, s.mtime);
    } else if let Some(r) = r
        && r.status == STATUS_PROCESSING
    {
        s.status = STATUS_PROCESSING.to_string();
        s.status_at = r.at;
    } else if s.live == Liveness::Yes || parent_status.is_some() {
        s.status = STATUS_UNKNOWN.to_string();
    } else {
        recent(s, now);
    }
}

fn recent(s: &mut Session, now: DateTime<Utc>) {
    if within_duration(now, s.mtime, RECENT_WINDOW) {
        s.status = STATUS_UNKNOWN.to_string();
    } else {
        ended(s, &END_GONE, s.mtime);
    }
}

fn within_duration(now: DateTime<Utc>, then: Option<DateTime<Utc>>, window: Duration) -> bool {
    let Some(then) = then else {
        return false;
    };
    let delta = TimeDelta::from_std(window).unwrap_or(TimeDelta::MAX);
    now.signed_duration_since(then) < delta
}

fn first_non_empty(values: &[&str]) -> String {
    for &v in values {
        if !v.is_empty() {
            return v.to_string();
        }
    }
    String::new()
}
