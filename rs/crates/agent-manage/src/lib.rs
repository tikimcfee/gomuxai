//! The manager's view of the fleet: what `agent manage --json` prints, what a
//! host's bus answers op `manage` with, and what the manager GUI draws.
//!
//! It is one document, [`View`], with a [`HostView`] per host: its agents,
//! every harness session with a [`SessionState`] and its token [`Usage`], its
//! friction box, its recent command history, and what its data takes on disk.
//! Everything in it is read from the `agent` store and the harnesses'
//! transcripts when asked; nothing here is stored anywhere.
//!
//! Times are RFC 3339 strings, "" when unknown. A field a host on an older
//! `agent` doesn't send reads as its default.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use agent_transcript::usage::{Tokens, Usage};

/// What a host is asked for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Only the newest N sessions (each with its subagents); 0 means all.
    #[serde(default)]
    pub sessions: i64,
    /// Only the newest N commands of the history; 0 means none.
    #[serde(default)]
    pub history: i64,
    /// Include the sessions a person stopped tracking.
    #[serde(default)]
    pub untracked: bool,
    /// A session untouched for this many days is flagged stale.
    #[serde(default)]
    pub stale_days: i64,
    /// How long the host may spend counting tokens in transcripts it hasn't
    /// counted yet, in milliseconds; 0 means as long as it takes. Sessions it
    /// didn't get to have no usage, and `HostView::usage_pending` counts them.
    #[serde(default)]
    pub usage_budget_ms: i64,
}

impl Default for Request {
    fn default() -> Self {
        Request {
            sessions: 0,
            history: 200,
            untracked: false,
            stale_days: DEFAULT_STALE_DAYS,
            usage_budget_ms: 0,
        }
    }
}

pub const DEFAULT_STALE_DAYS: i64 = 14;

/// The fleet as the manager sees it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct View {
    #[serde(default)]
    pub at: String,
    #[serde(default)]
    pub hosts: Vec<HostView>,
}

/// One host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HostView {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub build: String,
    /// False for a host that didn't answer; `error` says why.
    #[serde(default)]
    pub reachable: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    /// Every recorded agent, flat; `parent` makes the tree.
    #[serde(default)]
    pub agents: Vec<AgentRow>,
    /// Sessions, newest first, a subagent's after the session that started it.
    #[serde(default)]
    pub sessions: Vec<SessionRow>,
    /// Sessions left out by `Request::sessions`.
    #[serde(default)]
    pub sessions_omitted: i64,
    /// Sessions whose tokens weren't counted within the budget.
    #[serde(default)]
    pub usage_pending: i64,
    #[serde(default)]
    pub friction: Vec<FrictionRow>,
    #[serde(default)]
    pub history: Vec<HistoryRow>,
    #[serde(default)]
    pub data: DataReport,
}

/// A recorded agent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentRow {
    #[serde(default)]
    pub name: String,
    /// The agent that spawned it (`name` or `name@host`), "" for a person.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent: String,
    #[serde(default)]
    pub backend: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_reason: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    /// When it reached this status.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub since: String,
    /// Messages held for it.
    #[serde(default)]
    pub held: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    /// What needs a look, as `agent fleet` flags it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<String>,
    /// Whether `agent resume` would continue it.
    #[serde(default)]
    pub resumable: bool,
    #[serde(default)]
    pub adopted: bool,
}

/// Where a session stands, for deciding what to do with it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// A CLI holds it open now.
    Running,
    /// Not on right now, and nobody ended it: its CLI went away (a reboot, a
    /// closed terminal). `agent resume` or the CLI's own resume continues it.
    Dormant,
    /// Ended on purpose: quit, finished, closed, replaced, or its parent
    /// ended.
    Ended,
    /// An agent's record names it, but its transcript can't be found.
    Missing,
    /// Its status can't be told.
    #[default]
    Unknown,
}

impl SessionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SessionState::Running => "running",
            SessionState::Dormant => "dormant",
            SessionState::Ended => "ended",
            SessionState::Missing => "missing",
            SessionState::Unknown => "unknown",
        }
    }

    pub fn parse(s: &str) -> Option<SessionState> {
        Some(match s {
            "running" => SessionState::Running,
            "dormant" => SessionState::Dormant,
            "ended" => SessionState::Ended,
            "missing" => SessionState::Missing,
            "unknown" => SessionState::Unknown,
            _ => return None,
        })
    }

    pub const ALL: [SessionState; 5] = [
        SessionState::Running,
        SessionState::Dormant,
        SessionState::Ended,
        SessionState::Missing,
        SessionState::Unknown,
    ];
}

/// What [`classify`] decides from.
#[derive(Debug, Clone, Copy, Default)]
pub struct Facts<'a> {
    /// The session's resolved status, as `agent sessions` prints it.
    pub status: &'a str,
    pub end_reason: &'a str,
    /// Whether its transcript is on disk.
    pub transcript: bool,
    /// Whether the harness holds it open while its status reads `unknown`
    /// (agy says only that much).
    pub held_open: bool,
}

/// Decides a session's [`SessionState`]. The rules, in order:
///
/// 1. `idle`, `processing`, `waiting`, `error`: `running` (even before its
///    CLI has written a transcript).
/// 2. No transcript on disk: `missing`.
/// 3. `ended (gone)`: `dormant`. Any other `ended`: `ended`.
/// 4. `unknown`: `running` if the harness holds it open, else `unknown`.
pub fn classify(f: &Facts<'_>) -> SessionState {
    if matches!(f.status, "idle" | "processing" | "waiting" | "error") {
        return SessionState::Running;
    }
    if !f.transcript {
        return SessionState::Missing;
    }
    match f.status {
        "ended" if f.end_reason == "gone" || f.end_reason.is_empty() => SessionState::Dormant,
        "ended" => SessionState::Ended,
        _ if f.held_open => SessionState::Running,
        _ => SessionState::Unknown,
    }
}

/// Whether a session last written at `mtime` is stale at `now`: not running,
/// and untouched for `stale_days` days or more (0 days: never).
pub fn is_stale(
    state: SessionState,
    mtime: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    stale_days: i64,
) -> bool {
    if stale_days <= 0 || state == SessionState::Running {
        return false;
    }
    mtime.is_some_and(|m| now.signed_duration_since(m).num_days() >= stale_days)
}

/// One harness session: a transcript, or an agent's session whose transcript
/// is missing.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionRow {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub harness: String,
    /// The transcript; "" when there is none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    #[serde(default)]
    pub size: i64,
    /// The transcript's last write.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub mtime: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cwd: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub project: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub git_branch: String,
    /// The workgroup agent that owns it, "" for a session a person started.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    /// The session that started this subagent's.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub parent_session: String,
    #[serde(default)]
    pub status: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub end_reason: String,
    #[serde(default)]
    pub state: SessionState,
    #[serde(default)]
    pub stale: bool,
    /// A person stopped tracking it (listed only when asked for).
    #[serde(default)]
    pub untracked: bool,
    /// Its own model calls; `None` if they weren't counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Its own and its subagents' tokens together, on a session that started
    /// subagents whose tokens were counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_with_subagents: Option<Tokens>,
}

impl SessionRow {
    /// `harness` and `session_id` together name a session.
    pub fn key(&self) -> String {
        format!("{}:{}", self.harness, self.session_id)
    }
}

/// A friction entry.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FrictionRow {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub summary: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub detail: String,
    #[serde(default)]
    pub filed_by: String,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub votes: i64,
    /// The entries on other hosts it is linked to, as `id@host`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub resolution: String,
}

/// One run of the `agent` command.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HistoryRow {
    #[serde(default)]
    pub at: String,
    /// The agent that ran it, "" for a person.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub agent: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub exit_code: i64,
    #[serde(default)]
    pub duration_ms: i64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
}

/// What a host's agent data takes on disk.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DataReport {
    /// The store's directory (`.gomuxai`).
    #[serde(default)]
    pub dir: String,
    /// The database with its write-ahead log.
    #[serde(default)]
    pub db_bytes: i64,
    /// Terminal logs and transcript lists (`logs/`).
    #[serde(default)]
    pub logs_bytes: i64,
    /// Of those, the logs of agents no longer recorded.
    #[serde(default)]
    pub departed_logs_bytes: i64,
    #[serde(default)]
    pub departed_logs: i64,
    /// Rows per table.
    #[serde(default)]
    pub tables: BTreeMap<String, i64>,
    /// Transcripts per harness.
    #[serde(default)]
    pub transcripts: BTreeMap<String, Footprint>,
    #[serde(default)]
    pub backups: Vec<Backup>,
    #[serde(default)]
    pub untracked: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Footprint {
    #[serde(default)]
    pub count: i64,
    #[serde(default)]
    pub bytes: i64,
}

/// A database backup `agent manage backup` made.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Backup {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub bytes: i64,
    #[serde(default)]
    pub at: String,
}

/// A token count for a person: `812`, `45.2k`, `3.1M`, `2.4B`.
pub fn human_tokens(n: i64) -> String {
    human(n, 1000.0, &["", "k", "M", "B", "T"])
}

/// A size for a person: `812 B`, `45.2 KB`, `3.1 MB`.
pub fn human_bytes(n: i64) -> String {
    human(n, 1024.0, &[" B", " KB", " MB", " GB", " TB"])
}

fn human(n: i64, base: f64, units: &[&str]) -> String {
    let mut v = n as f64;
    let mut i = 0;
    while v.abs() >= base && i + 1 < units.len() {
        v /= base;
        i += 1;
    }
    if i == 0 {
        format!("{n}{}", units[0])
    } else {
        format!("{v:.1}{}", units[i])
    }
}

/// How long ago `then` was at `now`, for a person: `now`, `5m`, `3h`, `12d`.
pub fn human_age(then: &str, now: DateTime<Utc>) -> String {
    let Ok(t) = DateTime::parse_from_rfc3339(then) else {
        return String::new();
    };
    let secs = now.signed_duration_since(t).num_seconds().max(0);
    match secs {
        0..60 => "now".to_string(),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f<'a>(status: &'a str, end_reason: &'a str) -> Facts<'a> {
        Facts {
            status,
            end_reason,
            transcript: true,
            held_open: false,
        }
    }

    #[test]
    fn classify_rules() {
        use SessionState::*;
        for s in ["idle", "processing", "waiting", "error"] {
            assert_eq!(classify(&f(s, "")), Running, "{s}");
        }
        assert_eq!(classify(&f("ended", "gone")), Dormant);
        for r in ["quit", "finished", "closed", "replaced", "parent"] {
            assert_eq!(classify(&f("ended", r)), Ended, "{r}");
        }
        assert_eq!(classify(&f("unknown", "")), Unknown);
        assert_eq!(
            classify(&Facts {
                held_open: true,
                ..f("unknown", "")
            }),
            Running
        );
        assert_eq!(
            classify(&Facts {
                transcript: false,
                ..f("ended", "quit")
            }),
            Missing
        );
        assert_eq!(
            classify(&Facts {
                transcript: false,
                ..f("idle", "")
            }),
            Running
        );
    }

    #[test]
    fn stale_needs_age_and_not_running() {
        let now = DateTime::parse_from_rfc3339("2026-11-20T00:00:00Z")
            .unwrap()
            .to_utc();
        let old = Some(now - chrono::Duration::days(20));
        assert!(is_stale(SessionState::Dormant, old, now, 14));
        assert!(!is_stale(SessionState::Running, old, now, 14));
        assert!(!is_stale(SessionState::Dormant, old, now, 0));
        assert!(!is_stale(
            SessionState::Ended,
            Some(now - chrono::Duration::days(3)),
            now,
            14
        ));
        assert!(!is_stale(SessionState::Ended, None, now, 14));
    }

    #[test]
    fn states_round_trip_and_an_older_host_reads() {
        for s in SessionState::ALL {
            assert_eq!(SessionState::parse(s.as_str()), Some(s));
            assert_eq!(
                serde_json::to_string(&s).unwrap(),
                format!("\"{}\"", s.as_str())
            );
        }
        let h: HostView = serde_json::from_str(r#"{"host":"old","reachable":true}"#).unwrap();
        assert!(h.sessions.is_empty() && h.data.tables.is_empty());
        let r: Request = serde_json::from_str("{}").unwrap();
        assert_eq!(r.history, 0);
    }

    #[test]
    fn humans() {
        assert_eq!(human_tokens(812), "812");
        assert_eq!(human_tokens(45_200), "45.2k");
        assert_eq!(human_tokens(3_100_000), "3.1M");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        let now = DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z")
            .unwrap()
            .to_utc();
        assert_eq!(human_age("2026-11-20T11:55:00Z", now), "5m");
        assert_eq!(human_age("2026-11-08T12:00:00Z", now), "12d");
        assert_eq!(human_age("", now), "");
    }
}
