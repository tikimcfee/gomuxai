//! What the next dream reviews (docs/DREAMING.md, "Gather"). The rules are
//! here and deterministic; what they read (sessions, friction, handoff docs,
//! memory directories, git history) comes through [`Sources`], which the CLI
//! implements over the host and tests implement over fixed lists.
//!
//! The rules:
//!
//! - A session counts once it is no longer running, unless a person
//!   untracked it, a dream agent ran it, or it is a subagent's (it is read
//!   with the session that started it). At most [`MAX_SESSIONS`] are taken,
//!   oldest first; the rest wait for the next dream.
//! - An item is new unless a committed dream reviewed it at the fingerprint
//!   it has now: a session written to again, a friction entry voted on, a
//!   handoff doc edited comes back.
//! - Friction: open and accepted entries, and a closed one that changed
//!   since a dream reviewed it.
//! - Memory directories go with the sessions whose project they belong to,
//!   changed or not: they are where what the dream learns is written. Only a
//!   changed one counts as new.
//! - Commits in the repos the taken sessions ran in, since the last committed
//!   dream (or [`DUE_AFTER`] back, before any).
//! - Nothing new makes the dream quiet, unless the last committed dream is
//!   [`DUE_AFTER`] old (or there has been none): then it runs anyway over the
//!   memory directories of every project with a finished session.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use serde::Serialize;

/// The most sessions one dream takes.
pub const MAX_SESSIONS: usize = 200;

/// How old the last committed dream may get before a dream runs with nothing
/// new.
pub const DUE_AFTER: TimeDelta = TimeDelta::days(7);

pub const KIND_SESSION: &str = "session";
pub const KIND_FRICTION: &str = "friction";
pub const KIND_HANDOFF: &str = "handoff";
pub const KIND_MEMORY: &str = "memory";
pub const KIND_COMMIT: &str = "commit";

/// The kinds in the order a digest lists them.
pub const KINDS: &[&str] = &[
    KIND_SESSION,
    KIND_FRICTION,
    KIND_HANDOFF,
    KIND_MEMORY,
    KIND_COMMIT,
];

/// A session as discovery sees it.
#[derive(Clone, Debug, Default)]
pub struct SessionIn {
    pub harness: String,
    pub session_id: String,
    /// The transcript; "" when it isn't on disk.
    pub path: String,
    pub size: i64,
    pub mtime: Option<DateTime<Utc>>,
    pub cwd: String,
    pub project: String,
    pub title: String,
    /// The workgroup agent that owns it, "" for none.
    pub agent: String,
    /// Set on a subagent's session.
    pub parent_session: String,
    /// A CLI holds it open now (or might: its state can't be told).
    pub running: bool,
    pub untracked: bool,
}

impl SessionIn {
    pub fn key(&self) -> String {
        format!("{}:{}", self.harness, self.session_id)
    }
}

/// A friction entry on this host.
#[derive(Clone, Debug, Default)]
pub struct FrictionIn {
    pub id: i64,
    pub uid: String,
    pub status: String,
    pub kind: String,
    pub summary: String,
    /// As stored, for the fingerprint.
    pub updated_at: String,
    pub votes: i64,
}

impl FrictionIn {
    pub fn fingerprint(&self) -> String {
        format!("{}+{}", self.updated_at, self.votes)
    }
}

/// A handoff doc an agent's `handoff` state key names.
#[derive(Clone, Debug, Default)]
pub struct HandoffIn {
    pub agent: String,
    pub path: String,
    /// "" when the file can't be read: it is left out.
    pub fingerprint: String,
}

/// A memory directory.
#[derive(Clone, Debug, Default)]
pub struct MemoryIn {
    pub dir: String,
    pub files: usize,
    pub fingerprint: String,
}

/// A commit in a repo a session ran in.
#[derive(Clone, Debug, Default)]
pub struct CommitIn {
    /// The repo's top directory.
    pub repo: String,
    pub hash: String,
    pub subject: String,
}

/// Where gather reads from.
pub trait Sources {
    /// Every session this host can see.
    fn sessions(&self) -> Result<Vec<SessionIn>>;
    /// This host's friction entries, whatever their status.
    fn friction(&self) -> Result<Vec<FrictionIn>>;
    /// The handoff docs agents' state names.
    fn handoffs(&self) -> Result<Vec<HandoffIn>>;
    /// The memory directory of the project session belongs to, if it has one.
    fn memory_dir(&self, session: &SessionIn) -> Option<MemoryIn>;
    /// The commits since `since` in the repo `cwd` is in; none if it isn't
    /// in one, or is gone.
    fn commits(&self, cwd: &str, since: DateTime<Utc>) -> Vec<CommitIn>;
}

/// What earlier dreams left: the reviewed cursor.
#[derive(Clone, Debug, Default)]
pub struct Cursor {
    /// (kind, key) to the fingerprint a committed dream last reviewed it at.
    pub reviewed: HashMap<(String, String), String>,
    /// Agents that have dreamt: their sessions are never evidence.
    pub dream_agents: HashSet<String>,
    /// When the last committed dream started.
    pub last_committed: Option<DateTime<Utc>>,
    pub now: DateTime<Utc>,
}

impl Cursor {
    fn is_new(&self, kind: &str, key: &str, fingerprint: &str) -> bool {
        self.reviewed
            .get(&(kind.to_string(), key.to_string()))
            .is_none_or(|fp| fp != fingerprint)
    }

    fn was_reviewed(&self, kind: &str, key: &str) -> bool {
        self.reviewed
            .contains_key(&(kind.to_string(), key.to_string()))
    }

    /// Whether a dream is due with nothing new.
    pub fn due(&self) -> bool {
        self.last_committed
            .is_none_or(|t| self.now.signed_duration_since(t) >= DUE_AFTER)
    }
}

/// One thing for the dream to review.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Item {
    pub kind: String,
    pub key: String,
    pub fingerprint: String,
    /// One line saying what it is.
    pub summary: String,
    /// How to read it: a command or a path.
    pub read: String,
    /// Whether it is new since the last committed dream (a memory directory
    /// can come along unchanged).
    pub new: bool,
}

/// What the next dream would get.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Gathered {
    pub items: Vec<Item>,
    /// Sessions left for a later dream by the cap.
    pub sessions_left: usize,
    /// Commits are taken from here on.
    pub since: Option<DateTime<Utc>>,
    /// Nothing new, and not due: no agent is started.
    pub quiet: bool,
    /// Why it is quiet, or runs with nothing new.
    pub reason: String,
}

impl Gathered {
    /// How many items are new.
    pub fn new_count(&self) -> usize {
        self.items.iter().filter(|i| i.new).count()
    }

    /// Items per kind, in [`KINDS`] order, kinds with none left out.
    pub fn counts(&self) -> Vec<(&'static str, usize)> {
        KINDS
            .iter()
            .map(|k| (*k, self.items.iter().filter(|i| i.kind == *k).count()))
            .filter(|(_, n)| *n > 0)
            .collect()
    }
}

fn session_fingerprint(s: &SessionIn) -> String {
    let at = s
        .mtime
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true))
        .unwrap_or_default();
    format!("{}@{at}", s.size)
}

/// Sessions that can be evidence at all: finished, tracked, not a dream's,
/// not a subagent's, with a transcript.
fn finished_sessions<'a>(sessions: &'a [SessionIn], cur: &Cursor) -> Vec<&'a SessionIn> {
    sessions
        .iter()
        .filter(|s| {
            !s.running
                && !s.untracked
                && !s.path.is_empty()
                && s.parent_session.is_empty()
                && !cur.dream_agents.contains(&s.agent)
        })
        .collect()
}

fn session_item(s: &SessionIn) -> Item {
    let mut summary = format!("{} session in {}", s.harness, or_dash(&s.project));
    if !s.agent.is_empty() {
        summary += &format!(" (agent {})", s.agent);
    }
    if !s.title.is_empty() {
        summary += &format!(": {}", one_line(&s.title, 80));
    }
    Item {
        kind: KIND_SESSION.into(),
        key: s.key(),
        fingerprint: session_fingerprint(s),
        summary,
        read: format!("agent transcript {}", s.session_id),
        new: true,
    }
}

/// Works out what the next dream reviews.
pub fn gather(src: &dyn Sources, cur: &Cursor) -> Result<Gathered> {
    let sessions = src.sessions()?;
    let finished = finished_sessions(&sessions, cur);

    // New sessions, oldest first, capped.
    let mut fresh: Vec<&SessionIn> = finished
        .iter()
        .copied()
        .filter(|s| cur.is_new(KIND_SESSION, &s.key(), &session_fingerprint(s)))
        .collect();
    fresh.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.key().cmp(&b.key())));
    let sessions_left = fresh.len().saturating_sub(MAX_SESSIONS);
    fresh.truncate(MAX_SESSIONS);

    let mut items: Vec<Item> = fresh.iter().map(|s| session_item(s)).collect();

    for f in src.friction()? {
        let fp = f.fingerprint();
        let open = f.status == "open" || f.status == "accepted";
        let changed = cur.is_new(KIND_FRICTION, &f.uid, &fp);
        if !changed || !(open || cur.was_reviewed(KIND_FRICTION, &f.uid)) {
            continue;
        }
        let status = if f.status == "open" {
            String::new()
        } else {
            format!(" {}", f.status)
        };
        items.push(Item {
            kind: KIND_FRICTION.into(),
            key: f.uid.clone(),
            fingerprint: fp,
            summary: format!(
                "#{}{status} {} +{}: {}",
                f.id,
                f.kind,
                f.votes,
                one_line(&f.summary, 100)
            ),
            read: format!("agent friction show {}", f.id),
            new: true,
        });
    }

    let mut seen_docs = BTreeSet::new();
    for h in src.handoffs()? {
        if h.fingerprint.is_empty()
            || !seen_docs.insert(h.path.clone())
            || !cur.is_new(KIND_HANDOFF, &h.path, &h.fingerprint)
        {
            continue;
        }
        items.push(Item {
            kind: KIND_HANDOFF.into(),
            key: h.path.clone(),
            fingerprint: h.fingerprint,
            summary: format!("handoff doc of agent {}", h.agent),
            read: h.path,
            new: true,
        });
    }

    let since = cur.last_committed.unwrap_or(cur.now - DUE_AFTER);
    let mut seen_cwds = BTreeSet::new();
    let mut commits: BTreeMap<String, Item> = BTreeMap::new();
    for s in &fresh {
        if s.cwd.is_empty() || !seen_cwds.insert(s.cwd.clone()) {
            continue;
        }
        for c in src.commits(&s.cwd, since) {
            let key = format!("{}@{}", c.repo, c.hash);
            if commits.contains_key(&key) || !cur.is_new(KIND_COMMIT, &key, &c.hash) {
                continue;
            }
            let short: String = c.hash.chars().take(12).collect();
            let name = Path::new(&c.repo)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.repo.clone());
            commits.insert(
                key.clone(),
                Item {
                    kind: KIND_COMMIT.into(),
                    key,
                    fingerprint: c.hash.clone(),
                    summary: format!("{name} {short} {}", one_line(&c.subject, 100)),
                    read: format!("git -C {} show {short}", c.repo),
                    new: true,
                },
            );
        }
    }

    let new_count = items.len() + commits.len();
    let due = new_count == 0 && cur.due();
    // Memory goes with the sessions taken; a due dream with nothing new
    // takes every finished project's.
    let mut memory: BTreeMap<String, Item> = BTreeMap::new();
    let pool: Vec<&SessionIn> = if due { finished.clone() } else { fresh.clone() };
    for s in pool {
        let Some(m) = src.memory_dir(s) else {
            continue;
        };
        if memory.contains_key(&m.dir) {
            continue;
        }
        let new = cur.is_new(KIND_MEMORY, &m.dir, &m.fingerprint);
        memory.insert(
            m.dir.clone(),
            Item {
                kind: KIND_MEMORY.into(),
                key: m.dir.clone(),
                fingerprint: m.fingerprint,
                summary: format!(
                    "memory of {} ({} file{})",
                    or_dash(&s.project),
                    m.files,
                    if m.files == 1 { "" } else { "s" }
                ),
                read: m.dir,
                new,
            },
        );
    }
    items.extend(memory.into_values());
    items.extend(commits.into_values());

    let mut g = Gathered {
        items,
        sessions_left,
        since: Some(since),
        ..Default::default()
    };
    if g.new_count() == 0 {
        if due && !g.items.is_empty() {
            g.reason = match cur.last_committed {
                None => "nothing new, but no dream has committed yet".into(),
                Some(t) => format!(
                    "nothing new, but the last committed dream is {} days old",
                    cur.now.signed_duration_since(t).num_days()
                ),
            };
        } else {
            g.quiet = true;
            g.reason = if due {
                "nothing new, and nothing to consolidate".into()
            } else {
                "nothing new since the last committed dream".into()
            };
        }
    }
    Ok(g)
}

/// A memory directory's fingerprint, and how many `.md` files it has:
/// `<files>:<bytes>@<newest mtime in ns>` over its top-level `.md` files.
/// None when it isn't a directory. Gather and a dream's own memory writes
/// both use it, so a dream's write is recorded at the fingerprint the next
/// gather sees.
pub fn memory_fingerprint(dir: &Path) -> Option<(usize, String)> {
    let entries = std::fs::read_dir(dir).ok()?;
    let (mut files, mut bytes, mut newest) = (0usize, 0u64, 0i128);
    for e in entries.flatten() {
        if e.path().extension().is_none_or(|x| x != "md") {
            continue;
        }
        let Ok(md) = e.metadata() else { continue };
        if !md.is_file() {
            continue;
        }
        files += 1;
        bytes += md.len();
        let at = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i128)
            .unwrap_or_default();
        newest = newest.max(at);
    }
    Some((files, format!("{files}:{bytes}@{newest}")))
}

fn or_dash(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}

/// s on one line, clipped to max characters with "...".
pub fn one_line(s: &str, max: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let mut out: String = flat.chars().take(max.saturating_sub(3)).collect();
    out += "...";
    out
}

#[cfg(test)]
#[path = "gather_test.rs"]
mod gather_test;
