//! A host's roster as it reads now: its entries with their availability and
//! note counts (agent roster), and one entry with its notes (agent roster
//! show). host_view is what a host serves its peers, so its types are plain
//! serde.
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use agent_store::{RosterEntry, RosterNote, Store};
use serde::{Deserialize, Serialize};

use agent_manage::llm_usage::LlmUsage;
use chrono::{DateTime, Utc};

use crate::key::Key;
use crate::limits::{LimitView, entry_limit, fresh};

/// Tally counts notes of one verdict, and the distinct sessions they cite,
/// so a reader sees whether a claim rests on one run or on five.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tally {
    pub notes: i64,
    pub sessions: i64,
}

/// NoteCounts counts an entry's notes by verdict, leaving out superseded
/// ones.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NoteCounts {
    pub good: Tally,
    pub mixed: Tally,
    pub bad: Tally,
    /// The distinct sessions all of them cite.
    #[serde(default)]
    pub sessions: i64,
}

impl NoteCounts {
    pub fn total(&self) -> i64 {
        self.good.notes + self.mixed.notes + self.bad.notes
    }

    /// describe says the counts in words: "2 good, 1 bad from 3 sessions",
    /// "1 mixed from 1 session", or "no notes".
    pub fn describe(&self) -> String {
        let parts: Vec<String> = [
            ("good", &self.good),
            ("mixed", &self.mixed),
            ("bad", &self.bad),
        ]
        .iter()
        .filter(|(_, t)| t.notes > 0)
        .map(|(name, t)| format!("{} {name}", t.notes))
        .collect();
        if parts.is_empty() {
            return "no notes".into();
        }
        let s = if self.sessions == 1 {
            "session"
        } else {
            "sessions"
        };
        format!("{} from {} {s}", parts.join(", "), self.sessions)
    }

    /// count adds up notes, which are all of one entry and none superseded.
    fn count<'a>(notes: impl IntoIterator<Item = &'a RosterNote>) -> NoteCounts {
        let mut sessions: [BTreeSet<&str>; 3] = Default::default();
        let mut c = NoteCounts::default();
        for n in notes {
            let (t, s) = match n.verdict.as_str() {
                "good" => (&mut c.good, &mut sessions[0]),
                "mixed" => (&mut c.mixed, &mut sessions[1]),
                "bad" => (&mut c.bad, &mut sessions[2]),
                _ => continue,
            };
            t.notes += 1;
            s.insert(n.source.as_str());
        }
        c.good.sessions = sessions[0].len() as i64;
        c.mixed.sessions = sessions[1].len() as i64;
        c.bad.sessions = sessions[2].len() as i64;
        let all: BTreeSet<&str> = sessions.iter().flatten().copied().collect();
        c.sessions = all.len() as i64;
        c
    }
}

/// EntryView is an entry as read now: whether it is available, and its
/// notes counted, in all and by task kind.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryView {
    /// HARNESS/MODEL[@EFFORT].
    pub key: String,
    #[serde(flatten)]
    pub entry: RosterEntry,
    pub available: bool,
    /// Why it isn't available ("disabled", "agy not on PATH"); "" when it is.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub unavailable: String,
    pub notes: NoteCounts,
    /// The notes counted by the task kind they were about.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_task: BTreeMap<String, NoteCounts>,
    /// The fullest window of its plan, when its host's usage snapshot is
    /// fresh and covers its harness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<LimitView>,
    /// Whether it is near its plan's limit (pick ranks it down).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub near_limit: bool,
    /// Its limits in words: "week 22%, resets Fri 04:00", "near limit:
    /// ...", "at limit until ...", or "limits not known".
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limits: String,
}

/// HostRoster is one host's roster: its entries, and its notes on
/// harnesses and models it has no entry for. It is what a host's bus
/// answers the roster op with.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HostRoster {
    pub host: String,
    pub entries: Vec<EntryView>,
    /// HARNESS/MODEL[@EFFORT] of each set of notes with no entry, with their
    /// counts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub unlisted: BTreeMap<String, NoteCounts>,
    /// Why this host's roster couldn't be read (set by the asking side);
    /// "" when it was.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// What to run, and where, so it can be read next time.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub fix: String,
    /// When the usage snapshot its limits come from was captured; "" when
    /// there was none fresh enough to use.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub limits_as_of: String,
}

impl HostRoster {
    /// unread is a host whose roster couldn't be read, with why and the fix.
    pub fn unread(host: &str, error: impl Into<String>, fix: impl Into<String>) -> HostRoster {
        HostRoster {
            host: host.to_string(),
            error: error.into(),
            fix: fix.into(),
            ..Default::default()
        }
    }

    /// read says whether the roster was read.
    pub fn read(&self) -> bool {
        self.error.is_empty()
    }
}

/// RosterRequest is what the roster op asks: nothing yet (the whole roster).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RosterRequest {}

/// NoteView is a note as show lists it: with the note that superseded it,
/// if any.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NoteView {
    #[serde(flatten)]
    pub note: RosterNote,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub superseded_by: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// EntryShow is one entry with all its notes, oldest first. entry is None
/// when there are notes on a harness and model with no entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EntryShow {
    pub host: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<EntryView>,
    pub notes: Vec<NoteView>,
}

/// Checks are what a view is judged against: the PATH check, this host's
/// usage snapshot (None: none), and the time now.
pub struct Checks<'a> {
    pub on_path: &'a dyn Fn(&str) -> bool,
    pub usage: Option<&'a LlmUsage>,
    pub now: DateTime<Utc>,
}

impl<'a> Checks<'a> {
    /// This host as it is now: its PATH, and usage.
    pub fn now(usage: Option<&'a LlmUsage>) -> Checks<'a> {
        Checks {
            on_path: &on_path,
            usage,
            now: Utc::now(),
        }
    }
}

/// on_path says whether cmd is an executable file in a directory of this
/// process's PATH.
pub fn on_path(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    if cmd.contains('/') {
        return is_executable(Path::new(cmd));
    }
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .filter(|d| !d.as_os_str().is_empty())
        .any(|d| is_executable(&d.join(cmd)))
}

fn is_executable(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The command a harness's CLI runs as: its own name for the built-in
/// backends (claude, agy, aider). A custom backend isn't remembered after
/// spawn, so its entry's harness name is taken for its command too.
fn harness_command(harness: &str) -> &str {
    harness
}

/// availability says whether e can be spawned now, and why not: disabled,
/// its CLI not on PATH, or its plan at its limit.
fn availability(e: &RosterEntry, limit: Option<&LimitView>, c: &Checks) -> (bool, String) {
    if !e.enabled {
        return (false, "disabled".into());
    }
    let cmd = harness_command(&e.harness);
    if !(c.on_path)(cmd) {
        return (false, format!("{cmd} not on PATH"));
    }
    if let Some(l) = limit
        && l.at()
    {
        return (false, l.describe(c.now));
    }
    (true, String::new())
}

/// The ids of notes another note supersedes.
fn superseded(notes: &[RosterNote]) -> HashSet<i64> {
    notes
        .iter()
        .filter(|n| n.supersedes != 0)
        .map(|n| n.supersedes)
        .collect()
}

fn entry_view(e: RosterEntry, notes: &[&RosterNote], c: &Checks) -> EntryView {
    let limit = entry_limit(&e.harness, &e.model, c.usage, c.now);
    let (available, unavailable) = availability(&e, limit.as_ref(), c);
    let near_limit = limit.as_ref().is_some_and(LimitView::near);
    let limits = match &limit {
        Some(l) => l.describe(c.now),
        None => "limits not known".to_string(),
    };
    let mut by_task: BTreeMap<String, Vec<&RosterNote>> = BTreeMap::new();
    for n in notes {
        by_task.entry(n.task.clone()).or_default().push(n);
    }
    EntryView {
        key: Key::of_entry(&e).to_string(),
        entry: e,
        available,
        unavailable,
        notes: NoteCounts::count(notes.iter().copied()),
        by_task: by_task
            .into_iter()
            .map(|(t, ns)| (t, NoteCounts::count(ns)))
            .collect(),
        limit,
        near_limit,
        limits,
    }
}

/// host_view reads this host's roster, checking each harness's CLI against
/// this process's PATH and its plan against usage, this host's llm-exp
/// snapshot (agent_manage::llm_usage::read_snapshot; None for none).
pub fn host_view(
    store: &Store,
    host: &str,
    usage: Option<&LlmUsage>,
) -> anyhow::Result<HostRoster> {
    host_view_with(store, host, &Checks::now(usage))
}

/// host_view_with is host_view with the checks given.
pub fn host_view_with(store: &Store, host: &str, c: &Checks) -> anyhow::Result<HostRoster> {
    let entries = store.list_roster_entries()?;
    let notes = store.list_roster_notes()?;
    let gone = superseded(&notes);
    let mut by_key: BTreeMap<Key, Vec<&RosterNote>> = BTreeMap::new();
    for n in notes.iter().filter(|n| !gone.contains(&n.id)) {
        by_key.entry(Key::of_note(n)).or_default().push(n);
    }
    let mut views = Vec::with_capacity(entries.len());
    for e in entries {
        let ns = by_key.remove(&Key::of_entry(&e)).unwrap_or_default();
        views.push(entry_view(e, &ns, c));
    }
    Ok(HostRoster {
        host: host.to_string(),
        entries: views,
        unlisted: by_key
            .into_iter()
            .map(|(k, ns)| (k.to_string(), NoteCounts::count(ns)))
            .collect(),
        limits_as_of: fresh(c.usage, c.now)
            .map(|u| u.captured_at.clone())
            .unwrap_or_default(),
        ..Default::default()
    })
}

/// show reads one entry and all its notes, superseded ones included. It
/// fails when there is neither an entry nor a note for key.
pub fn show(
    store: &Store,
    host: &str,
    key: &Key,
    usage: Option<&LlmUsage>,
) -> anyhow::Result<EntryShow> {
    show_with(store, host, key, &Checks::now(usage))
}

/// show_with is show with the checks given.
pub fn show_with(store: &Store, host: &str, key: &Key, c: &Checks) -> anyhow::Result<EntryShow> {
    let entry = store.get_roster_entry(&key.harness, &key.model, &key.effort)?;
    let notes = store.list_roster_notes_for(&key.harness, &key.model, &key.effort)?;
    if entry.is_none() && notes.is_empty() {
        anyhow::bail!(
            "no roster entry or note for {key} on this host (agent roster lists them; agent roster set {key} adds it)"
        );
    }
    let mut superseded_by: BTreeMap<i64, i64> = BTreeMap::new();
    for n in &notes {
        if n.supersedes != 0 {
            superseded_by.entry(n.supersedes).or_insert(n.id);
        }
    }
    let current: Vec<&RosterNote> = notes
        .iter()
        .filter(|n| !superseded_by.contains_key(&n.id))
        .collect();
    let entry = entry.map(|e| entry_view(e, &current, c));
    Ok(EntryShow {
        host: host.to_string(),
        key: key.to_string(),
        entry,
        notes: notes
            .iter()
            .map(|n| NoteView {
                note: n.clone(),
                superseded_by: superseded_by.get(&n.id).copied().unwrap_or(0),
            })
            .collect(),
    })
}
