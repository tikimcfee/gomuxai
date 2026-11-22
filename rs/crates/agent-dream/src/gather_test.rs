// Each test fills a Fake field by field, as the case needs.
#![allow(clippy::field_reassign_with_default)]

use super::*;
use chrono::TimeZone;

fn at(day: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, 12, 0, 0).unwrap()
}

#[derive(Default)]
struct Fake {
    sessions: Vec<SessionIn>,
    friction: Vec<FrictionIn>,
    handoffs: Vec<HandoffIn>,
    /// By the session's cwd.
    memory: HashMap<String, MemoryIn>,
    /// By cwd.
    commits: HashMap<String, Vec<CommitIn>>,
}

impl Sources for Fake {
    fn sessions(&self) -> Result<Vec<SessionIn>> {
        Ok(self.sessions.clone())
    }
    fn friction(&self) -> Result<Vec<FrictionIn>> {
        Ok(self.friction.clone())
    }
    fn handoffs(&self) -> Result<Vec<HandoffIn>> {
        Ok(self.handoffs.clone())
    }
    fn memory_dir(&self, s: &SessionIn) -> Option<MemoryIn> {
        self.memory.get(&s.cwd).cloned()
    }
    fn commits(&self, cwd: &str, _since: DateTime<Utc>) -> Vec<CommitIn> {
        self.commits.get(cwd).cloned().unwrap_or_default()
    }
}

fn session(id: &str, day: u32, cwd: &str) -> SessionIn {
    SessionIn {
        harness: "claude".into(),
        session_id: id.into(),
        path: format!("/p/{id}.jsonl"),
        size: 100,
        mtime: Some(at(day)),
        cwd: cwd.into(),
        project: cwd.trim_start_matches('/').into(),
        ..Default::default()
    }
}

/// A cursor at now with everything in scope and a floor before every
/// fixture session (the scope and floor have tests of their own).
fn cursor(now: DateTime<Utc>) -> Cursor {
    Cursor {
        now,
        scope: vec!["/".into()],
        floor: Some(at(1) - TimeDelta::days(1)),
        ..Default::default()
    }
}

fn keys(g: &Gathered) -> Vec<String> {
    g.items
        .iter()
        .map(|i| format!("{}:{}", i.kind, i.key))
        .collect()
}

/// Commits the items as a dream would: the cursor now has them reviewed.
fn commit(cur: &mut Cursor, g: &Gathered, when: DateTime<Utc>) {
    for i in &g.items {
        cur.reviewed
            .insert((i.kind.clone(), i.key.clone()), i.fingerprint.clone());
    }
    cur.last_committed = Some(when);
}

#[test]
fn sessions_filtered_and_oldest_first() {
    let mut f = Fake::default();
    f.sessions = vec![
        session("new", 5, "/a"),
        session("old", 2, "/a"),
        SessionIn {
            running: true,
            ..session("live", 1, "/a")
        },
        SessionIn {
            untracked: true,
            ..session("hidden", 1, "/a")
        },
        SessionIn {
            agent: "dream-1".into(),
            ..session("dreamt", 1, "/a")
        },
        SessionIn {
            parent_session: "old".into(),
            ..session("sub", 1, "/a")
        },
        SessionIn {
            path: String::new(),
            ..session("nofile", 1, "/a")
        },
    ];
    let mut cur = cursor(at(10));
    cur.dream_agents.insert("dream-1".into());
    let g = gather(&f, &cur).unwrap();
    assert_eq!(keys(&g), ["session:claude:old", "session:claude:new"]);
    assert!(!g.quiet);
    assert_eq!(g.items[0].read, "agent transcript old");
    assert_eq!(g.counts(), [("session", 2)]);
    // The evidence reads each session's transcript, a subagent's under its
    // parent's key.
    let paths: Vec<(&str, &str)> = g
        .transcripts
        .iter()
        .map(|t| (t.session.as_str(), t.path.as_str()))
        .collect();
    assert_eq!(
        paths,
        [
            ("claude:old", "/p/old.jsonl"),
            ("claude:old", "/p/sub.jsonl"),
            ("claude:new", "/p/new.jsonl")
        ]
    );
}

#[test]
fn cap_leaves_the_newest() {
    let mut f = Fake::default();
    for i in 0..(MAX_SESSIONS + 3) {
        let mut s = session(&format!("s{i:03}"), 1, "/a");
        s.mtime = Some(at(1) + TimeDelta::minutes(i as i64));
        f.sessions.push(s);
    }
    let g = gather(&f, &cursor(at(10))).unwrap();
    assert_eq!(g.items.len(), MAX_SESSIONS);
    assert_eq!(g.sessions_left, 3);
    assert_eq!(g.items[0].key, "claude:s000");
    assert_eq!(
        g.items.last().unwrap().key,
        format!("claude:s{:03}", MAX_SESSIONS - 1)
    );
}

#[test]
fn reviewed_items_stay_away_until_they_change() {
    let mut f = Fake::default();
    f.sessions = vec![session("a", 2, "/a")];
    f.friction = vec![
        FrictionIn {
            id: 1,
            uid: "h-1".into(),
            status: "open".into(),
            kind: "bug".into(),
            summary: "it broke".into(),
            updated_at: "t1".into(),
            votes: 0,
        },
        FrictionIn {
            id: 2,
            uid: "h-2".into(),
            status: "resolved".into(),
            updated_at: "t1".into(),
            ..Default::default()
        },
    ];
    f.handoffs = vec![
        HandoffIn {
            agent: "lead".into(),
            path: "/doc.md".into(),
            fingerprint: "10@t".into(),
        },
        HandoffIn {
            agent: "gone".into(),
            path: "/missing.md".into(),
            fingerprint: String::new(),
        },
    ];
    let mut cur = cursor(at(10));
    let g = gather(&f, &cur).unwrap();
    assert_eq!(
        keys(&g),
        ["session:claude:a", "handoff:/doc.md"],
        "friction is the global dream's"
    );
    commit(&mut cur, &g, at(10));

    let g = gather(&f, &cursor_after(&cur, 11)).unwrap();
    assert!(g.quiet, "{g:?}");
    assert_eq!(g.reason, "nothing new since the last committed dream");

    // A session written to again.
    f.sessions[0].size = 200;
    let g = gather(&f, &cursor_after(&cur, 11)).unwrap();
    assert_eq!(keys(&g), ["session:claude:a"]);
}

#[test]
fn the_global_dream_triages_friction() {
    let mut f = Fake::default();
    f.friction = vec![
        FrictionIn {
            id: 1,
            uid: "h-1".into(),
            status: "open".into(),
            kind: "bug".into(),
            summary: "it broke".into(),
            updated_at: "t1".into(),
            votes: 0,
        },
        FrictionIn {
            id: 2,
            uid: "h-2".into(),
            status: "resolved".into(),
            updated_at: "t1".into(),
            ..Default::default()
        },
    ];
    let mut cur = cursor(at(10));
    let g = gather_global(&f, &cur).unwrap();
    assert_eq!(
        keys(&g),
        ["friction:h-1"],
        "a closed entry no dream saw isn't evidence"
    );
    assert_eq!(g.items[0].summary, "#1 bug +0: it broke");
    commit(&mut cur, &g, at(10));
    // The resolved one was reviewed before (as a dream would have it).
    cur.reviewed
        .insert(("friction".into(), "h-2".into()), "t0+0".into());

    let g = gather_global(&f, &cursor_after(&cur, 11)).unwrap();
    assert_eq!(
        keys(&g),
        ["friction:h-2"],
        "a closed entry that changed since comes back"
    );
    cur.reviewed
        .insert(("friction".into(), "h-2".into()), "t1+0".into());

    let g = gather_global(&f, &cursor_after(&cur, 11)).unwrap();
    assert!(g.quiet, "{g:?}");

    // A vote.
    f.friction[0].votes = 1;
    let g = gather_global(&f, &cursor_after(&cur, 11)).unwrap();
    assert_eq!(keys(&g), ["friction:h-1"]);
}

/// A Fake that also has project dreams, project memory and the machine's.
#[derive(Default)]
struct GlobalFake {
    base: Fake,
    dreams: Vec<DreamIn>,
    /// By scope dir.
    project_memory: HashMap<String, MemoryIn>,
    machine: Option<MemoryIn>,
}

impl Sources for GlobalFake {
    fn sessions(&self) -> Result<Vec<SessionIn>> {
        self.base.sessions()
    }
    fn friction(&self) -> Result<Vec<FrictionIn>> {
        self.base.friction()
    }
    fn handoffs(&self) -> Result<Vec<HandoffIn>> {
        self.base.handoffs()
    }
    fn memory_dir(&self, s: &SessionIn) -> Option<MemoryIn> {
        self.base.memory_dir(s)
    }
    fn commits(&self, cwd: &str, since: DateTime<Utc>) -> Vec<CommitIn> {
        self.base.commits(cwd, since)
    }
    fn project_dreams(&self, since: DateTime<Utc>) -> Result<Vec<DreamIn>> {
        Ok(self
            .dreams
            .iter()
            .filter(|d| d.finished_at.is_some_and(|t| t >= since))
            .cloned()
            .collect())
    }
    fn project_memory(&self, dir: &str) -> Option<MemoryIn> {
        self.project_memory.get(dir).cloned()
    }
    fn machine_memory(&self) -> Option<MemoryIn> {
        self.machine.clone()
    }
}

fn mem(dir: &str, fp: &str) -> MemoryIn {
    MemoryIn {
        dir: dir.into(),
        files: 1,
        fingerprint: fp.into(),
    }
}

#[test]
fn the_global_dream_takes_project_dreams_memory_and_pooled_evidence() {
    let mut f = GlobalFake::default();
    f.base.sessions = vec![
        session("a", 4, "/srv/app"),
        session("b", 5, "/srv/tools/sub"),
        session("out", 5, "/home/someone"),
    ];
    f.dreams = vec![
        DreamIn {
            id: 3,
            project: "/srv/app".into(),
            finished_at: Some(at(5)),
            items: 4,
            diary: "merged two notes".into(),
        },
        DreamIn {
            id: 4,
            project: "/srv/tools".into(),
            finished_at: Some(at(6)),
            items: 1,
            diary: String::new(),
        },
    ];
    f.project_memory
        .insert("/srv/app".into(), mem("/m/app", "1"));
    f.machine = Some(mem("/data/memory", "0"));
    let mut cur = cursor(at(7));
    cur.scope = vec!["/srv/app".into(), "/srv/tools".into()];
    let g = gather_global(&f, &cur).unwrap();
    assert!(!g.quiet);
    assert_eq!(
        keys(&g),
        ["dream:3", "dream:4", "memory:/m/app", "memory:/data/memory"]
    );
    assert_eq!(
        g.items[0].summary,
        "dream #3 over /srv/app (4 items): merged two notes"
    );
    assert_eq!(g.items[0].read, "agent dream show 3");
    assert_eq!(g.items[3].summary, "the machine memory (1 file)");
    // Evidence comes from both projects' sessions, each with its project.
    let t: Vec<(&str, &str)> = g
        .transcripts
        .iter()
        .map(|t| (t.session.as_str(), t.project.as_str()))
        .collect();
    assert_eq!(t, [("claude:a", "/srv/app"), ("claude:b", "/srv/tools")]);
    commit(&mut cur, &g, at(7));

    // Nothing new: quiet, though a project's memory changed (a project
    // dream wrote it; it is news only alongside a new project dream).
    f.project_memory
        .insert("/srv/app".into(), mem("/m/app", "2"));
    let g = gather_global(&f, &cursor_after(&cur, 8)).unwrap();
    assert!(g.quiet, "{g:?}");
    assert_eq!(
        g.reason,
        "no project dream has committed and no friction changed since the last global dream"
    );

    // A new project dream: the changed memory is new, the machine's isn't.
    f.dreams.push(DreamIn {
        id: 6,
        project: "/srv/app".into(),
        finished_at: Some(at(8)),
        ..Default::default()
    });
    let g = gather_global(&f, &cursor_after(&cur, 9)).unwrap();
    assert_eq!(
        keys(&g),
        ["dream:6", "memory:/m/app", "memory:/data/memory"]
    );
    let news: Vec<bool> = g.items.iter().map(|i| i.new).collect();
    assert_eq!(news, [true, true, false]);
    assert_eq!(g.since, Some(at(7)));
    assert!(
        g.transcripts.is_empty(),
        "no session written since the last global dream"
    );

    // Seven days on, it runs with nothing new.
    f.dreams.pop();
    let g = gather_global(&f, &cursor_after(&cur, 14)).unwrap();
    assert!(!g.quiet);
    assert_eq!(
        g.reason,
        "no new project dream or friction, but the last committed global dream is 7 days old"
    );

    // An empty scope gathers nothing.
    let mut none = cursor(at(7));
    none.scope.clear();
    assert!(gather_global(&f, &none).unwrap().no_scope);
}

fn cursor_after(cur: &Cursor, day: u32) -> Cursor {
    Cursor {
        now: at(day),
        ..cur.clone()
    }
}

#[test]
fn memory_and_commits_follow_the_sessions_taken() {
    let mut f = Fake::default();
    f.sessions = vec![
        session("a", 2, "/repo/sub"),
        session("b", 3, "/repo"),
        session("c", 3, "/other"),
    ];
    f.memory.insert(
        "/repo/sub".into(),
        MemoryIn {
            dir: "/m/repo".into(),
            files: 2,
            fingerprint: "2:300@t".into(),
        },
    );
    f.memory.insert(
        "/repo".into(),
        MemoryIn {
            dir: "/m/repo".into(),
            files: 2,
            fingerprint: "2:300@t".into(),
        },
    );
    let c = |hash: &str| CommitIn {
        repo: "/repo".into(),
        hash: hash.into(),
        subject: format!("subject {hash}"),
    };
    f.commits
        .insert("/repo/sub".into(), vec![c("aaaa"), c("bbbb")]);
    f.commits.insert("/repo".into(), vec![c("aaaa"), c("bbbb")]);
    let mut cur = cursor(at(10));
    let g = gather(&f, &cur).unwrap();
    assert_eq!(
        keys(&g),
        [
            "session:claude:a",
            "session:claude:b",
            "session:claude:c",
            "memory:/m/repo",
            "commit:/repo@aaaa",
            "commit:/repo@bbbb"
        ],
        "one memory dir and one copy of each commit"
    );
    assert_eq!(g.items[3].summary, "memory of repo/sub (2 files)");
    assert_eq!(g.items[4].read, "git -C /repo show aaaa");
    assert_eq!(g.since, cur.floor, "before any committed dream, the floor");
    commit(&mut cur, &g, at(10));

    // A new session in the repo brings its memory dir along, unchanged (not
    // new), and only commits not reviewed.
    f.sessions.push(session("d", 11, "/repo"));
    f.commits.insert("/repo".into(), vec![c("bbbb"), c("cccc")]);
    let g = gather(&f, &cursor_after(&cur, 12)).unwrap();
    assert_eq!(
        keys(&g),
        ["session:claude:d", "memory:/m/repo", "commit:/repo@cccc"]
    );
    assert!(!g.items[1].new);
    assert_eq!(g.new_count(), 2);
    assert_eq!(g.since, Some(at(10)));
}

#[test]
fn nothing_new_is_quiet_until_due() {
    let mut f = Fake::default();
    f.sessions = vec![session("a", 2, "/repo")];
    f.memory.insert(
        "/repo".into(),
        MemoryIn {
            dir: "/m/repo".into(),
            files: 1,
            fingerprint: "1".into(),
        },
    );
    let mut cur = cursor(at(3));
    let g = gather(&f, &cur).unwrap();
    commit(&mut cur, &g, at(3));

    let g = gather(&f, &cursor_after(&cur, 9)).unwrap();
    assert!(g.quiet);
    assert!(g.items.is_empty());

    // Seven days on, it runs over the memory directories.
    let g = gather(&f, &cursor_after(&cur, 10)).unwrap();
    assert!(!g.quiet, "{g:?}");
    assert_eq!(keys(&g), ["memory:/m/repo"]);
    assert_eq!(
        g.reason,
        "nothing new, but the last committed dream is 7 days old"
    );

    // Due with nothing at all to look at stays quiet.
    let g = gather(&Fake::default(), &cursor(at(10))).unwrap();
    assert!(g.quiet);
    assert_eq!(g.reason, "nothing new, and nothing to consolidate");
}

#[test]
fn scope_picks_the_sessions_and_what_goes_with_them() {
    let mut f = Fake::default();
    f.sessions = vec![
        session("in", 5, "/srv/app"),
        session("below", 5, "/srv/app/sub"),
        session("beside", 5, "/srv/app2"),
        session("personal", 5, "/home/someone/notes"),
        session("nowhere", 5, ""),
    ];
    for cwd in ["/srv/app", "/srv/app2", "/home/someone/notes"] {
        f.memory.insert(
            cwd.into(),
            MemoryIn {
                dir: format!("/m{cwd}"),
                files: 1,
                fingerprint: "1".into(),
            },
        );
        f.commits.insert(
            cwd.into(),
            vec![CommitIn {
                repo: cwd.into(),
                hash: "aaaa".into(),
                subject: "s".into(),
            }],
        );
    }
    f.handoffs = vec![
        HandoffIn {
            agent: "lead".into(),
            path: "/srv/app/docs/lead.md".into(),
            fingerprint: "1".into(),
        },
        HandoffIn {
            agent: "other".into(),
            path: "/home/someone/notes/h.md".into(),
            fingerprint: "1".into(),
        },
    ];
    f.friction = vec![FrictionIn {
        id: 1,
        uid: "h-1".into(),
        status: "open".into(),
        updated_at: "t".into(),
        ..Default::default()
    }];

    // An empty scope gathers nothing.
    let mut cur = cursor(at(10));
    cur.scope.clear();
    let g = gather(&f, &cur).unwrap();
    assert!(g.no_scope && g.quiet && g.items.is_empty(), "{g:?}");

    cur.scope = vec!["/srv/app".into()];
    let g = gather(&f, &cur).unwrap();
    assert_eq!(
        keys(&g),
        [
            "session:claude:below",
            "session:claude:in",
            "handoff:/srv/app/docs/lead.md",
            "memory:/m/srv/app",
            "commit:/srv/app@aaaa"
        ],
        "friction is the global dream's; the rest follows the scope"
    );
    assert!(!g.no_scope);
    assert!(g.transcripts.iter().all(|t| t.project == "/srv/app"));
}

#[test]
fn the_floor_keeps_old_sessions_out() {
    let mut f = Fake::default();
    f.sessions = vec![session("old", 1, "/a"), session("recent", 8, "/a")];
    // The first dream looks back 7 days from now.
    let mut cur = Cursor {
        now: at(10),
        scope: vec!["/a".into()],
        ..Default::default()
    };
    assert_eq!(cur.floor(), at(3));
    let g = gather(&f, &cur).unwrap();
    assert_eq!(keys(&g), ["session:claude:recent"]);
    assert_eq!(g.since, Some(at(3)));

    // Recorded, it stays: weeks later, old is still out, unless it is
    // written to again.
    cur.floor = Some(at(3));
    commit(&mut cur, &g, at(10));
    let mut later = cursor_after(&cur, 28);
    later.last_committed = Some(at(27));
    assert!(gather(&f, &later).unwrap().quiet);
    f.sessions[0].mtime = Some(at(27));
    f.sessions[0].size = 200;
    assert_eq!(keys(&gather(&f, &later).unwrap()), ["session:claude:old"]);
}

#[test]
fn in_scope_goes_by_path_components() {
    let dirs = vec!["/srv/app".to_string()];
    assert!(in_scope("/srv/app", &dirs));
    assert!(in_scope("/srv/app/x/y", &dirs));
    assert!(!in_scope("/srv/app2", &dirs));
    assert!(!in_scope("/srv", &dirs));
    assert!(!in_scope("", &dirs));
    assert!(!in_scope("srv/app", &dirs), "relative");
}

#[test]
fn one_line_clips() {
    assert_eq!(one_line("a\n  b", 10), "a b");
    assert_eq!(one_line("abcdefghijkl", 8), "abcde...");
}

#[test]
fn a_worktree_of_a_scoped_repo_is_in_scope() {
    let mut f = Fake::default();
    let mut wt = session("wt", 5, "/scratch/app-wt-feature");
    wt.root = "/srv/app".into();
    let mut other = session("other", 5, "/scratch/other-wt");
    other.root = "/srv/other".into();
    f.sessions = vec![wt, other, session("gone", 5, "/scratch/app-wt-old")];
    let mut cur = cursor(at(10));
    cur.scope = vec!["/srv/app".into()];
    let g = gather(&f, &cur).unwrap();
    let keys: Vec<&str> = g
        .items
        .iter()
        .filter(|i| i.kind == KIND_SESSION)
        .map(|i| i.key.as_str())
        .collect();
    assert_eq!(keys, vec!["claude:wt"]);
}
