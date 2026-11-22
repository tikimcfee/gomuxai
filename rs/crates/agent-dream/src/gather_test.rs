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

fn cursor(now: DateTime<Utc>) -> Cursor {
    Cursor {
        now,
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
        ["session:claude:a", "friction:h-1", "handoff:/doc.md"],
        "a closed entry no dream saw isn't evidence"
    );
    assert_eq!(g.items[1].summary, "#1 bug +0: it broke");
    commit(&mut cur, &g, at(10));
    // The resolved one was reviewed before (as a dream would have it).
    cur.reviewed
        .insert(("friction".into(), "h-2".into()), "t0+0".into());

    let g = gather(&f, &cursor_after(&cur, 11)).unwrap();
    assert_eq!(
        keys(&g),
        ["friction:h-2"],
        "a closed entry that changed since comes back"
    );
    cur.reviewed
        .insert(("friction".into(), "h-2".into()), "t1+0".into());

    let g = gather(&f, &cursor_after(&cur, 11)).unwrap();
    assert!(g.quiet, "{g:?}");
    assert_eq!(g.reason, "nothing new since the last committed dream");

    // A vote, and a session written to again.
    f.friction[0].votes = 1;
    f.sessions[0].size = 200;
    let g = gather(&f, &cursor_after(&cur, 11)).unwrap();
    assert_eq!(keys(&g), ["session:claude:a", "friction:h-1"]);
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
    assert_eq!(g.since, Some(at(3)));
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
fn one_line_clips() {
    assert_eq!(one_line("a\n  b", 10), "a b");
    assert_eq!(one_line("abcdefghijkl", 8), "abcde...");
}
