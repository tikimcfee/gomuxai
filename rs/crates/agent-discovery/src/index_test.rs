use std::collections::HashMap;

use agent_base::gostd::filepath;
use chrono::{DateTime, TimeDelta, Utc};

use crate::discovery_test::{lines, temp_dir, touch, write};
use crate::index::{ProjectMatch, is_alnum};
use crate::{
    Agent, Dir, HARNESS_AGY, HARNESS_CLAUDE, Index, Live, ORIGIN_DEFAULT, Query, STATUS_ENDED,
    STATUS_IDLE, STATUS_UNKNOWN, Session, r#continue, new_index, scan,
};

fn claude_session(projects: &str, id: &str, cwd: &str, at: DateTime<Utc>, subs: usize) -> String {
    let folder: String = cwd
        .chars()
        .map(|r| if is_alnum(r) { r } else { '-' })
        .collect();
    let path = filepath::join(&[projects, &folder, &format!("{id}.jsonl")]);
    write(
        &path,
        &lines(&[
            &format!(
                r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"hi"}}}}"#
            ),
            &format!(
                r#"{{"type":"custom-title","customTitle":"Title of {id}","sessionId":"{id}"}}"#
            ),
        ]),
    );
    touch(&path, at);
    for i in 0..subs {
        let sub = filepath::join(&[
            projects,
            &folder,
            id,
            "subagents",
            &format!("agent-{id}-{i}.jsonl"),
        ]);
        write(
            &sub,
            &lines(&[&format!(
                r#"{{"type":"user","isSidechain":true,"agentId":"x","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"sub"}}}}"#
            )]),
        );
        touch(&sub, at - TimeDelta::minutes(1));
    }
    path
}

fn ids(sessions: &[Session]) -> Vec<String> {
    sessions.iter().map(|s| s.session_id.clone()).collect()
}

fn probed_ids(x: &Index) -> Vec<String> {
    let mut out: Vec<String> =
        x.j.sessions
            .iter()
            .filter(|s| s.probed)
            .map(|s| s.session_id.clone())
            .collect();
    out.sort();
    out
}

#[test]
fn test_index_select_limit() {
    let tmp = temp_dir();
    let projects = filepath::join(&[tmp.path().to_str().unwrap(), "projects"]);
    let now = Utc::now();
    for i in 0..5 {
        let subs = if i == 1 { 2 } else { 0 };
        claude_session(
            &projects,
            &format!("s{i}"),
            "/home/u/app",
            now - TimeDelta::hours(i as i64),
            subs,
        );
    }
    let mut x = new_index(
        &[Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_DEFAULT.to_string(),
        }],
        &[],
    );
    assert!(probed_ids(&x).is_empty());
    assert_eq!(x.sessions().len(), 7);

    let (picked, left) = x.select(&Query {
        limit: 2,
        ..Query::default()
    });
    assert_eq!(ids(&picked), vec!["s0", "s1", "agent-s1-0", "agent-s1-1"]);
    assert_eq!(left, 3);
    assert!(probed_ids(&x).is_empty());

    let resolved = x.resolve(
        &picked,
        &Live {
            claude: Some(HashMap::new()),
            ..Live::default()
        },
    );
    assert_eq!(probed_ids(&x), vec!["agent-s1-0", "agent-s1-1", "s0", "s1"]);
    for s in &resolved {
        assert_eq!(s.cwd, "/home/u/app");
        assert_eq!(s.status, STATUS_ENDED);
    }
    assert_eq!(resolved[0].title, "Title of s0");

    let (picked_all, left_all) = x.select(&Query::default());
    assert_eq!(picked_all.len(), 7);
    assert_eq!(left_all, 0);

    let (picked_since, left_since) = x.select(&Query {
        since: Some(now - TimeDelta::minutes(90)),
        limit: 1,
        ..Query::default()
    });
    assert_eq!(ids(&picked_since), vec!["s0"]);
    assert_eq!(left_since, 1);

    let (picked_agy, _) = x.select(&Query {
        harness: HARNESS_AGY.to_string(),
        ..Query::default()
    });
    assert!(picked_agy.is_empty());
}

#[test]
fn test_index_select_project() {
    let tmp = temp_dir();
    let projects = filepath::join(&[tmp.path().to_str().unwrap(), "projects"]);
    let now = Utc::now();
    claude_session(&projects, "shop", "/home/u/src/Shop", now, 1);
    claude_session(
        &projects,
        "shop-wt",
        "/home/u/src/Shop/.claude/worktrees/fix",
        now - TimeDelta::minutes(1),
        0,
    );
    claude_session(
        &projects,
        "notes",
        "/home/u/notes",
        now - TimeDelta::minutes(2),
        0,
    );
    claude_session(
        &projects,
        "shopping",
        "/srv/shopping",
        now - TimeDelta::minutes(3),
        0,
    );
    let odd = filepath::join(&[&projects, "odd folder", "odd.jsonl"]);
    write(
        &odd,
        &lines(&[
            r#"{"type":"user","sessionId":"odd","cwd":"/home/u/src/Shop","message":{"role":"user","content":"hi"}}"#,
        ]),
    );
    touch(&odd, now - TimeDelta::minutes(4));
    let dirs = [Dir {
        harness: HARNESS_CLAUDE.to_string(),
        path: projects,
        origin: ORIGIN_DEFAULT.to_string(),
    }];

    let cases: &[(&str, &[&str], &[&str])] = &[
        (
            "shop",
            &["shop", "agent-shop-0", "shopping", "odd"],
            &["odd", "shop", "shop-wt", "shopping"],
        ),
        (
            "SHOP",
            &["shop", "agent-shop-0", "shopping", "odd"],
            &["odd", "shop", "shop-wt", "shopping"],
        ),
        (
            "/home/u/src/Shop",
            &["shop", "agent-shop-0", "shop-wt", "odd"],
            &["odd", "shop", "shop-wt"],
        ),
        (
            "/home/u/src/Shop/",
            &["shop", "agent-shop-0", "shop-wt", "odd"],
            &["odd", "shop", "shop-wt"],
        ),
        ("/home/u/src/Sho", &[], &["odd", "shop", "shop-wt"]),
        ("notes", &["notes"], &["notes", "odd"]),
        ("nothing", &[], &["odd"]),
    ];
    for &(project, want, read) in cases {
        let mut x = new_index(&dirs, &[]);
        let (picked, _) = x.select(&Query {
            project: project.to_string(),
            ..Query::default()
        });
        assert_eq!(ids(&picked), want, "--project {project}");
        assert_eq!(probed_ids(&x), read, "--project {project} read");
    }

    let mut x = new_index(&dirs, &[]);
    let (picked, left) = x.select(&Query {
        project: "shop".to_string(),
        limit: 1,
        ..Query::default()
    });
    assert_eq!(ids(&picked), vec!["shop", "agent-shop-0"]);
    assert_eq!(left, 2);
}

#[test]
fn test_index_select_workgroup() {
    let tmp = temp_dir();
    let projects = filepath::join(&[tmp.path().to_str().unwrap(), "projects"]);
    let now = Utc::now();
    let owned = claude_session(&projects, "owned", "/w", now, 1);
    claude_session(&projects, "mine", "/w", now - TimeDelta::minutes(1), 0);
    let agents = vec![
        Agent {
            name: "worker".to_string(),
            store: "/s".to_string(),
            host: "h".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now - TimeDelta::hours(1)),
            status: STATUS_IDLE.to_string(),
            session_id: "owned".to_string(),
            transcript: owned.clone(),
            history: vec![owned],
            ..Agent::default()
        },
        Agent {
            name: "aid".to_string(),
            store: "/s".to_string(),
            host: "h".to_string(),
            backend: "aider".to_string(),
            live: true,
            created_at: Some(now - TimeDelta::hours(48)),
            ..Agent::default()
        },
    ];
    let mut x = new_index(
        &[Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_DEFAULT.to_string(),
        }],
        &agents,
    );

    let (picked, _) = x.select(&Query {
        workgroup: true,
        ..Query::default()
    });
    assert_eq!(ids(&picked), vec!["", "owned", "agent-owned-0"]);

    let (local_picked, _) = x.select(&Query {
        local: true,
        ..Query::default()
    });
    assert_eq!(ids(&local_picked), vec!["mine"]);

    let (future_picked, _) = x.select(&Query {
        since: Some(now + TimeDelta::hours(1)),
        ..Query::default()
    });
    assert_eq!(ids(&future_picked), vec![""]);
    assert!(probed_ids(&x).is_empty());

    let resolved = x.resolve(
        &future_picked,
        &Live {
            claude: Some(HashMap::new()),
            ..Live::default()
        },
    );
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].workgroup.as_ref().unwrap().name, "aid");
    assert_eq!(resolved[0].harness, "aider");
    assert!(x.harnesses().iter().any(|h| h == "aider"));
}

#[test]
fn test_index_lookup() {
    let tmp = temp_dir();
    let root = tmp.path().to_str().unwrap();
    let projects = filepath::join(&[root, "projects"]);
    let now = Utc::now();
    let parent = claude_session(&projects, "p", "/w", now, 1);
    claude_session(&projects, "other", "/w", now - TimeDelta::minutes(1), 0);
    let sub = filepath::join(&[
        parent.strip_suffix(".jsonl").unwrap(),
        "subagents",
        "agent-p-0.jsonl",
    ]);
    let mut x = new_index(
        &[Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_DEFAULT.to_string(),
        }],
        &[],
    );

    let s = x.lookup(&sub).expect("lookup sub");
    assert_eq!(s.session_id, "agent-p-0");
    assert_eq!(s.parent_session.as_ref().unwrap().session_id, "p");

    let mut claude_live = HashMap::new();
    claude_live.insert("p".to_string(), STATUS_IDLE.to_string());
    let resolved = x.resolve(
        &[s],
        &Live {
            claude: Some(claude_live),
            ..Live::default()
        },
    );
    assert_eq!(ids(&resolved), vec!["p", "agent-p-0"]);
    assert_eq!(probed_ids(&x), vec!["agent-p-0", "p"]);
    for r in &resolved {
        if r.session_id == "agent-p-0" {
            assert_eq!(r.status, STATUS_UNKNOWN);
        }
    }

    let elsewhere = filepath::join(&[root, "elsewhere", "e1.jsonl"]);
    write(
        &elsewhere,
        &lines(&[
            r#"{"type":"user","sessionId":"e1","cwd":"/e","message":{"role":"user","content":"hi"}}"#,
        ]),
    );
    let s = x.lookup(&elsewhere).expect("lookup outside");
    assert_eq!(s.session_id, "e1");
    assert!(
        x.lookup(&filepath::join(&[root, "nothing.jsonl"]))
            .is_none()
    );
    assert!(ids(&x.sessions()).iter().any(|id| id == "e1"));
}

#[test]
fn test_index_continue() {
    let tmp = temp_dir();
    let projects = filepath::join(&[tmp.path().to_str().unwrap(), "projects"]);
    let now = Utc::now();
    claude_session(&projects, "a-new", "/home/u/a", now, 0);
    claude_session(
        &projects,
        "a-old",
        "/home/u/a",
        now - TimeDelta::hours(1),
        0,
    );
    claude_session(&projects, "b", "/home/u/b", now + TimeDelta::minutes(1), 0);
    claude_session(
        &projects,
        "a-sub",
        "/home/u/a/sub",
        now + TimeDelta::minutes(2),
        0,
    );
    let dirs = [Dir {
        harness: HARNESS_CLAUDE.to_string(),
        path: projects,
        origin: ORIGIN_DEFAULT.to_string(),
    }];
    let mut x = new_index(&dirs, &[]);
    let s = x.r#continue(HARNESS_CLAUDE, "/home/u/a").expect("continue");
    let want = r#continue(&scan(&dirs), HARNESS_CLAUDE, "/home/u/a").expect("scan continue");
    assert_eq!(s.session_id, "a-new");
    assert_eq!(want.session_id, "a-new");
    assert_eq!(probed_ids(&x), vec!["a-new", "a-sub"]);
    assert!(
        new_index(&dirs, &[])
            .r#continue(HARNESS_CLAUDE, "/home/u/c")
            .is_none()
    );
}

#[test]
fn test_project_match_may_match() {
    let long_folder = "-a".repeat(100);
    let cases = [
        ("gomuxai", "-Users-user-localdev-gomuxai", true),
        ("GoMux", "-Users-user-localdev-gomuxai", true),
        ("gomuxai", "-Users-user-notes", false),
        ("my_app", "-home-u-my-app", true),
        ("my_app", "-home-u-myapp", true),
        (
            "/Users/user/localdev",
            "-Users-user-localdev-gomuxai--claude-worktrees-x",
            true,
        ),
        (
            "/Users/user/elsewhere",
            "-Users-user-localdev-gomuxai",
            false,
        ),
        ("x", "", true),
        ("x", "odd folder", true),
        ("x", long_folder.as_str(), true),
        ("-", "-home-u", true),
        ("é", "-home-u", true),
    ];
    for (q, folder, want) in cases {
        let got = ProjectMatch::new(q).may_match(folder);
        assert_eq!(got, want, "may_match({q:?}, {folder:?})");
    }
}
