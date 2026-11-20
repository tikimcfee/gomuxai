//! A realistic [`View`] for tests and screenshots: three hosts (one unreachable), an agent
//! tree with a parent on another host, ~300 sessions over the three harnesses in every
//! [`SessionState`], subagent sessions, stale and untracked ones, sessions whose tokens
//! aren't counted yet, friction, history and a data report. Deterministic for a given `now`.

use std::collections::BTreeMap;

use agent_manage::{
    AgentRow, Backup, DataReport, Footprint, FrictionRow, HistoryRow, HostView, SessionRow,
    SessionState, Tokens, Usage, View,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};

/// The fixture's clock: the view says it was read at this time.
pub fn default_now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-20T12:00:00Z")
        .unwrap()
        .to_utc()
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

const PROJECTS: &[(&str, &str)] = &[
    ("/home/user/dev/shop", "shop"),
    ("/home/user/dev/shop/api", "shop/api"),
    ("/home/user/dev/widget", "widget"),
    ("/home/user/dev/garden", "garden"),
    ("/home/user/notes", "notes"),
    ("/srv/build", "build"),
];

const TITLES: &[&str] = &[
    "Rewrite the bus client in Rust",
    "Fix held-message delivery after Esc",
    "Review internal/auth for injection bugs",
    "Port the sessions table to the new store",
    "Why does wait --next miss the turn?",
    "Add usage counting to transcripts",
    "Fix the list's scroll jank",
    "Draft the fleet report",
    "Sign the macOS build over ssh",
    "Investigate OOM in the cargo build",
    "Write the friction link tests",
    "Explain the trust prompt detection",
    "Tidy the docs for agent run",
    "Benchmark transcript parsing",
    "Phone shell: keyboard layout",
    "Rename host and re-trust peers",
    "",
];

const MODELS: &[&str] = &["claude-opus-5-5", "claude-sonnet-5-5", "claude-fable-5-2"];

fn usage(rng: &mut Rng, scale: i64) -> Usage {
    let calls = 3 + rng.below(400) as i64;
    let cache_read = calls * (8_000 + rng.below(60_000) as i64) * scale / 4;
    let input = calls * (50 + rng.below(900) as i64);
    let output = calls * (200 + rng.below(2_500) as i64);
    let cache_creation = calls * rng.below(4_000) as i64;
    Usage {
        tokens: Tokens {
            input,
            output,
            cache_read,
            cache_creation,
            calls,
        },
        context_tokens: 8_000 + rng.below(180_000) as i64,
        compactions: rng.below(4) as i64,
        by_model: BTreeMap::new(),
    }
}

struct SessionSpec<'a> {
    harness: &'a str,
    state: SessionState,
    age: Duration,
    agent: &'a str,
    id: String,
}

fn session(rng: &mut Rng, now: DateTime<Utc>, s: SessionSpec<'_>, counted: bool) -> SessionRow {
    let (cwd, project) = PROJECTS[rng.below(PROJECTS.len() as u64) as usize];
    let mtime = now - s.age;
    let (status, end_reason) = match s.state {
        SessionState::Running => (rng.pick(&["idle", "idle", "processing", "waiting"]), ""),
        SessionState::Dormant => ("ended", "gone"),
        SessionState::Ended => ("ended", rng.pick(&["quit", "finished", "closed"])),
        SessionState::Missing => ("ended", "gone"),
        SessionState::Unknown => ("unknown", ""),
    };
    let missing = s.state == SessionState::Missing;
    let path = if missing {
        String::new()
    } else {
        match s.harness {
            "claude" => format!(
                "/home/user/.claude/projects/{}/{}.jsonl",
                cwd.replace('/', "-"),
                s.id
            ),
            "agy" => format!(
                "/home/user/.gemini/antigravity/brain/{}/transcript.jsonl",
                s.id
            ),
            _ => format!("/home/user/.kimi/sessions/{}/wire.jsonl", s.id),
        }
    };
    let model = match s.harness {
        "claude" => rng.pick(MODELS).to_string(),
        "agy" => "gemini-3-pro".to_string(),
        _ => "kimi-k2".to_string(),
    };
    SessionRow {
        session_id: s.id,
        harness: s.harness.to_string(),
        path,
        size: if missing {
            0
        } else {
            20_000 + rng.below(40_000_000) as i64
        },
        mtime: if missing { String::new() } else { ts(mtime) },
        cwd: cwd.to_string(),
        project: project.to_string(),
        title: rng.pick(TITLES).to_string(),
        model,
        git_branch: rng.pick(&["main", "manager", "rust", ""]).to_string(),
        agent: s.agent.to_string(),
        parent_session: String::new(),
        status: status.to_string(),
        end_reason: end_reason.to_string(),
        state: s.state,
        stale: s.state != SessionState::Running && s.age >= Duration::days(14),
        untracked: false,
        usage: (counted && !missing).then(|| usage(rng, 1)),
        usage_with_subagents: None,
    }
}

fn session_id(rng: &mut Rng, harness: &str) -> String {
    let a = rng.next();
    let b = rng.next();
    match harness {
        "claude" => format!(
            "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
            a as u32,
            (a >> 16) as u16,
            b as u16,
            (b >> 8) as u16,
            a.wrapping_mul(b) & 0xffff_ffff_ffff
        ),
        "agy" => format!("{:016x}", a ^ (b << 20)),
        _ => format!("kimi-{:012x}", a.wrapping_mul(b) & 0xffff_ffff_ffff),
    }
}

/// Sessions for one host, newest first, each subagent's after the session that started it.
/// `agents` names the sessions the host's agents own: (agent, session id, harness, state).
fn sessions(
    rng: &mut Rng,
    now: DateTime<Utc>,
    count: usize,
    agents: &[(&str, &str, &str, SessionState)],
    pending: usize,
) -> Vec<SessionRow> {
    let mut tops: Vec<SessionRow> = Vec::new();
    for (agent, id, harness, state) in agents {
        let age = match state {
            SessionState::Running => Duration::seconds(rng.below(900) as i64),
            _ => Duration::minutes(30 + rng.below(60 * 48) as i64),
        };
        let spec = SessionSpec {
            harness,
            state: *state,
            age,
            agent,
            id: id.to_string(),
        };
        tops.push(session(rng, now, spec, true));
    }
    while tops.len() < count {
        let harness = match rng.below(10) {
            0..6 => "claude",
            6..8 => "agy",
            _ => "kimi",
        };
        let state = match rng.below(20) {
            0..2 => SessionState::Running,
            2..9 => SessionState::Dormant,
            9..17 => SessionState::Ended,
            17 => SessionState::Missing,
            _ => SessionState::Unknown,
        };
        let age = match state {
            SessionState::Running => Duration::seconds(rng.below(3_600) as i64),
            _ => Duration::minutes(rng.below(60 * 24 * 60) as i64),
        };
        let id = session_id(rng, harness);
        let spec = SessionSpec {
            harness,
            state,
            age,
            agent: "",
            id,
        };
        let mut row = session(rng, now, spec, true);
        if state == SessionState::Missing {
            row.agent = format!("old-helper-{}", rng.below(90));
        }
        if !matches!(state, SessionState::Running) && rng.below(9) == 0 {
            row.untracked = true;
        }
        tops.push(row);
    }
    // The newest have no usage yet: the budget ran out before them.
    tops.sort_by(|a, b| b.mtime.cmp(&a.mtime));
    let mut uncounted = pending;
    for row in tops.iter_mut() {
        if uncounted == 0 {
            break;
        }
        if row.state != SessionState::Missing && row.agent.is_empty() && row.harness != "agy" {
            row.usage = None;
            uncounted -= 1;
        }
    }
    let mut out = Vec::new();
    for mut top in tops {
        let subs = if top.harness == "claude" && top.usage.is_some() && rng.below(6) == 0 {
            1 + rng.below(3) as usize
        } else {
            0
        };
        let mut children = Vec::new();
        for _ in 0..subs {
            let mut child = top.clone();
            child.session_id = format!("agent-{:08x}", rng.next() as u32);
            child.parent_session = top.session_id.clone();
            child.title = rng
                .pick(&[
                    "Explore: find the hook installer",
                    "general-purpose: search the store migrations",
                    "Plan: split the bus client",
                    "review: read the diff",
                ])
                .to_string();
            child.size /= 6;
            child.agent = String::new();
            child.usage = Some(usage(rng, 1));
            children.push(child);
        }
        if !children.is_empty() {
            let mut total = top.usage.as_ref().unwrap().tokens;
            for c in &children {
                total.add(&c.usage.as_ref().unwrap().tokens);
            }
            top.usage_with_subagents = Some(total);
        }
        out.push(top);
        out.extend(children);
    }
    out
}

fn agent(name: &str, parent: &str, backend: &str, status: &str) -> AgentRow {
    AgentRow {
        name: name.to_string(),
        parent: parent.to_string(),
        backend: backend.to_string(),
        model: match backend {
            "claude" => "claude-opus-5-5".to_string(),
            "agy" => "gemini-3-pro".to_string(),
            _ => String::new(),
        },
        status: status.to_string(),
        cwd: "/home/user/dev/shop".to_string(),
        ..AgentRow::default()
    }
}

fn history(rng: &mut Rng, now: DateTime<Utc>, agents: &[&str], n: usize) -> Vec<HistoryRow> {
    let cmds: &[&[&str]] = &[
        &["list"],
        &["status", "{a}", "-v"],
        &["send", "{a}", "run the tests and report back"],
        &[
            "wait",
            "{a}",
            "--for",
            "idle,waiting",
            "--next",
            "--timeout",
            "30m",
        ],
        &["state", "get", "{a}", "_result"],
        &["capture", "{a}", "--lines", "50"],
        &["fleet"],
        &["friction", "list"],
        &["spawn", "{a}-review", "--backend", "claude"],
        &["cleanup", "{a}-review"],
        &["run", "build.sh", "--release"],
        &["manage", "--json", "--untracked", "--usage-budget", "2s"],
    ];
    let mut out = Vec::new();
    let mut t = now - Duration::seconds(20);
    for _ in 0..n {
        let a = rng.pick(agents);
        let cmd = cmds[rng.below(cmds.len() as u64) as usize];
        let args: Vec<String> = cmd.iter().map(|s| s.replace("{a}", a)).collect();
        let failed = rng.below(9) == 0;
        let who = if rng.below(4) == 0 {
            ""
        } else {
            rng.pick(agents)
        };
        out.push(HistoryRow {
            at: ts(t),
            agent: who.to_string(),
            exit_code: if failed {
                if args[0] == "wait" { 2 } else { 1 }
            } else {
                0
            },
            duration_ms: match args[0].as_str() {
                "wait" => 5_000 + rng.below(900_000) as i64,
                "run" => 30_000 + rng.below(300_000) as i64,
                _ => 5 + rng.below(800) as i64,
            },
            error: if failed {
                match args[0].as_str() {
                    "wait" => "timed out after 30m".to_string(),
                    _ => format!("no agent named {a}-review on this host"),
                }
            } else {
                String::new()
            },
            args,
        });
        t -= Duration::seconds(30 + rng.below(1_800) as i64);
    }
    out
}

fn data(rng: &mut Rng, now: DateTime<Utc>, sessions: &[SessionRow], dir: &str) -> DataReport {
    let mut transcripts: BTreeMap<String, Footprint> = BTreeMap::new();
    for s in sessions.iter().filter(|s| !s.path.is_empty()) {
        let f = transcripts.entry(s.harness.clone()).or_default();
        f.count += 1;
        f.bytes += s.size;
    }
    let tables = [
        ("agents", 9),
        ("agent_events", 48_211),
        ("command_log", 96_534),
        ("friction", 41),
        ("jobs", 212),
        ("messages", 3_870),
        ("runs", 1_204),
        ("scripts", 6),
        ("state", 812),
        ("tasks", 4),
        ("untracked", 0),
    ];
    let untracked = sessions.iter().filter(|s| s.untracked).count() as i64;
    DataReport {
        dir: dir.to_string(),
        db_bytes: 61_000_000 + rng.below(30_000_000) as i64,
        logs_bytes: 900_000_000 + rng.below(400_000_000) as i64,
        departed_logs_bytes: 610_000_000,
        departed_logs: 187,
        tables: tables
            .iter()
            .map(|(k, v)| {
                let v = if *k == "untracked" { untracked } else { *v };
                (k.to_string(), v)
            })
            .collect(),
        transcripts,
        backups: vec![
            Backup {
                path: format!("{dir}/backups/gomuxai-2026-11-13T09-00-00Z.db"),
                bytes: 58_400_000,
                at: ts(now - Duration::days(7) - Duration::hours(3)),
            },
            Backup {
                path: format!("{dir}/backups/gomuxai-2026-11-19T22-14-05Z.db"),
                bytes: 60_900_000,
                at: ts(now - Duration::hours(13)),
            },
        ],
        untracked,
    }
}

fn friction(now: DateTime<Utc>) -> Vec<FrictionRow> {
    let f = |id: i64, kind: &str, status: &str, votes: i64, summary: &str, days: i64| FrictionRow {
        id,
        kind: kind.to_string(),
        status: status.to_string(),
        summary: summary.to_string(),
        detail: String::new(),
        filed_by: "lead".to_string(),
        created_at: ts(now - Duration::days(days)),
        votes,
        links: Vec::new(),
        resolution: String::new(),
    };
    let mut out = vec![
        FrictionRow {
            detail: "Ran `agent wait helper --next` right after a send that was held. It returned the previous turn at once, so the coordinator read a stale _result.".to_string(),
            links: vec!["4@phone".to_string()],
            ..f(34, "bug", "open", 5, "wait --next returns the previous turn when the send was held", 2)
        },
        f(36, "missing", "open", 3, "logs can't show just one turn", 5),
        FrictionRow {
            filed_by: "phone-shell@phone".to_string(),
            ..f(37, "docs", "open", 1, "The briefing doesn't say how to reach the hub from a leaf", 1)
        },
        f(38, "help", "open", 0, "agent help run doesn't mention --memory-high", 0),
        FrictionRow {
            status: "accepted".to_string(),
            ..f(29, "error", "accepted", 2, "send fails with 'input box never showed the message' over a dialog", 12)
        },
        FrictionRow {
            resolution: "status reads the trust prompt (abc1234)".to_string(),
            ..f(21, "bug", "resolved", 4, "Trust prompt reads as idle", 30)
        },
        FrictionRow {
            resolution: "fleet --report added".to_string(),
            ..f(17, "missing", "resolved", 2, "No way to see sessions older than 3 days with compactions", 41)
        },
    ];
    for r in &mut out {
        if r.status == "resolved" {
            r.detail = "Fixed and verified on both hosts.".to_string();
        }
    }
    out
}

/// The fixture view.
pub fn view(now: DateTime<Utc>) -> View {
    let mut rng = Rng(0x5eed_1234_abcd);

    // hub: the lead's tree, a resumable ended helper, an agy agent with held messages.
    let lead_sid = session_id(&mut rng, "claude");
    let gui_sid = session_id(&mut rng, "claude");
    let review_sid = session_id(&mut rng, "claude");
    let docs_sid = session_id(&mut rng, "claude");
    let builder_sid = session_id(&mut rng, "agy");
    let old_sid = session_id(&mut rng, "claude");
    let mut m_agents = vec![
        AgentRow {
            since: ts(now - Duration::minutes(3)),
            session_id: lead_sid.clone(),
            ..agent("lead", "", "claude", "idle")
        },
        AgentRow {
            since: ts(now - Duration::seconds(40)),
            detail: "Bash: cargo build".to_string(),
            session_id: gui_sid.clone(),
            cwd: "/home/user/dev/shop/rs".to_string(),
            ..agent("lead-gui", "lead", "claude", "processing")
        },
        AgentRow {
            since: ts(now - Duration::minutes(14)),
            detail: "permission: Bash: rm -rf /var/tmp/agent-manager-target".to_string(),
            session_id: review_sid.clone(),
            flags: vec!["stuck: waiting 14m".to_string()],
            model: "claude-sonnet-5-5".to_string(),
            ..agent("lead-gui-review", "lead-gui", "claude", "waiting")
        },
        AgentRow {
            end_reason: "quit".to_string(),
            since: ts(now - Duration::hours(5)),
            session_id: docs_sid.clone(),
            resumable: true,
            ..agent("lead-docs", "lead", "claude", "ended")
        },
        AgentRow {
            end_reason: "gone".to_string(),
            since: ts(now - Duration::hours(26)),
            session_id: builder_sid.clone(),
            held: 3,
            resumable: true,
            flags: vec![
                "stuck: 3 held, oldest 22m".to_string(),
                "ended (gone), still recorded".to_string(),
            ],
            cwd: "/srv/build".to_string(),
            ..agent("builder", "", "agy", "ended")
        },
        AgentRow {
            end_reason: "gone".to_string(),
            since: ts(now - Duration::days(9)),
            session_id: old_sid.clone(),
            resumable: false,
            ..agent("old-helper", "lead-docs", "claude", "ended")
        },
        AgentRow {
            adopted: true,
            since: ts(now - Duration::hours(2)),
            ..agent("notes-pane", "", "claude", "idle")
        },
        AgentRow {
            since: ts(now - Duration::minutes(50)),
            ..agent("watcher", "", "shell", "unknown")
        },
    ];
    m_agents[6].cwd = "/home/user/notes".to_string();
    let m_sessions = sessions(
        &mut rng,
        now,
        150,
        &[
            ("lead", &lead_sid, "claude", SessionState::Running),
            ("lead-gui", &gui_sid, "claude", SessionState::Running),
            (
                "lead-gui-review",
                &review_sid,
                "claude",
                SessionState::Running,
            ),
            ("lead-docs", &docs_sid, "claude", SessionState::Ended),
            ("builder", &builder_sid, "agy", SessionState::Dormant),
            ("old-helper", &old_sid, "claude", SessionState::Missing),
        ],
        0,
    );
    let m_names = ["lead", "lead-gui", "lead-gui-review", "builder"];
    let hub_host = HostView {
        host: "hub".to_string(),
        build: "agent 0.2.0 (b50c70e)".to_string(),
        reachable: true,
        error: String::new(),
        agents: m_agents,
        data: data(&mut rng, now, &m_sessions, "/home/user/.local/bin/.gomuxai"),
        sessions: m_sessions,
        sessions_omitted: 0,
        usage_pending: 0,
        friction: friction(now),
        history: history(&mut rng, now, &m_names, 80),
    };

    let air = HostView {
        host: "laptop".to_string(),
        reachable: false,
        error: "no bus answering for host laptop (no reply in 3s)".to_string(),
        ..HostView::default()
    };

    // phone: a helper whose parent is on hub, and its own helper.
    let shell_sid = session_id(&mut rng, "claude");
    let ui_sid = session_id(&mut rng, "kimi");
    let t_agents = vec![
        AgentRow {
            since: ts(now - Duration::minutes(8)),
            session_id: shell_sid.clone(),
            cwd: "/home/user/dev/garden".to_string(),
            ..agent("phone-shell", "lead@hub", "claude", "idle")
        },
        AgentRow {
            end_reason: "gone".to_string(),
            since: ts(now - Duration::hours(3)),
            session_id: ui_sid.clone(),
            resumable: false,
            model: "kimi-k2".to_string(),
            cwd: "/home/user/dev/garden".to_string(),
            ..agent("phone-shell-ui", "phone-shell", "kimi", "ended")
        },
    ];
    let t_sessions = sessions(
        &mut rng,
        now,
        110,
        &[
            ("phone-shell", &shell_sid, "claude", SessionState::Running),
            ("phone-shell-ui", &ui_sid, "kimi", SessionState::Dormant),
        ],
        14,
    );
    let phone = HostView {
        host: "phone".to_string(),
        build: "agent 0.2.0 (b50c70e)".to_string(),
        reachable: true,
        error: String::new(),
        agents: t_agents,
        data: data(&mut rng, now, &t_sessions, "/home/user/.local/bin/.gomuxai"),
        sessions: t_sessions,
        sessions_omitted: 0,
        usage_pending: 14,
        friction: vec![FrictionRow {
            id: 4,
            kind: "bug".to_string(),
            status: "open".to_string(),
            summary: "wait --next returns the previous turn when the send was held".to_string(),
            filed_by: "phone-shell".to_string(),
            created_at: ts(now - Duration::days(3)),
            votes: 2,
            links: vec!["34@hub".to_string()],
            ..FrictionRow::default()
        }],
        history: history(&mut rng, now, &["phone-shell", "phone-shell-ui"], 40),
    };

    View {
        at: ts(now),
        host: "hub".to_string(),
        hosts: vec![hub_host, air, phone],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_what_the_ui_must_show() {
        let v = view(default_now());
        assert_eq!(v.hosts.len(), 3);
        assert!(!v.hosts[1].reachable);
        let all: Vec<&SessionRow> = v.hosts.iter().flat_map(|h| &h.sessions).collect();
        assert!(all.len() >= 280, "{} sessions", all.len());
        for s in SessionState::ALL {
            assert!(all.iter().any(|r| r.state == s), "{s:?}");
        }
        for h in ["claude", "agy", "kimi"] {
            assert!(all.iter().any(|r| r.harness == h), "{h}");
        }
        assert!(all.iter().any(|r| r.stale));
        assert!(all.iter().any(|r| r.untracked));
        assert!(all.iter().any(|r| !r.parent_session.is_empty()));
        assert!(all.iter().any(|r| r.usage_with_subagents.is_some()));
        assert_eq!(
            v.hosts[2]
                .sessions
                .iter()
                .filter(|s| s.usage.is_none() && s.state != SessionState::Missing)
                .count(),
            14
        );
        // Every agent with a session finds it on its host.
        for h in &v.hosts {
            for a in h.agents.iter().filter(|a| !a.session_id.is_empty()) {
                assert!(
                    h.sessions.iter().any(|s| s.session_id == a.session_id),
                    "{}",
                    a.name
                );
            }
        }
    }

    #[test]
    fn round_trips_as_json() {
        let v = view(default_now());
        let s = serde_json::to_string(&v).unwrap();
        let back: View = serde_json::from_str(&s).unwrap();
        assert_eq!(back, v);
    }
}
