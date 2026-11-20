use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use agent_base::gostd::filepath;
use agent_bus::{AgentInfo, Context, Time as BusTime};
use agent_store::{
    AGENT_STATUS, Agent as StoredAgent, AgentEvent, StateEntry, Store as SQLiteStorage,
    new_sqlite_storage,
};
use rusqlite::Connection;

use crate::*;

/// A temp dir under `/var/tmp` (RAM `/tmp` is off-limits on build hosts).
fn temp_dir() -> tempfile::TempDir {
    let base = if Path::new("/var/tmp").is_dir() {
        PathBuf::from("/var/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix("agent-export-test-")
        .tempdir_in(base)
        .unwrap()
}

fn open_store(dir: &str) -> SQLiteStorage {
    fs::create_dir_all(dir).unwrap();
    new_sqlite_storage(filepath::join(&[dir, "gomuxai.db"])).unwrap()
}

/// A store being written as `agent` writes it, and the sessions a fake `termd`
/// server has.
struct Fixture {
    _tmp: tempfile::TempDir,
    dir: String,
    w: SQLiteStorage,
    sessions: Rc<RefCell<Vec<String>>>,
    path_of: Rc<RefCell<HashMap<String, String>>>,
}

impl Fixture {
    fn new() -> Self {
        let tmp = temp_dir();
        let dir = filepath::join(&[tmp.path().to_str().unwrap(), ".gomuxai"]);
        let w = open_store(&dir);
        Self {
            _tmp: tmp,
            dir,
            w,
            sessions: Rc::new(RefCell::new(Vec::new())),
            path_of: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    fn deps(&self) -> Deps {
        let sessions = Rc::clone(&self.sessions);
        let path_of = Rc::clone(&self.path_of);
        Deps {
            host: "box".to_string(),
            namespace: String::new(),
            session_prefix: "agent-".to_string(),
            sessions: Some(Box::new(move || Ok(sessions.borrow().clone()))),
            session_store: Some(Box::new(move |s| {
                path_of.borrow().get(s).cloned().unwrap_or_default()
            })),
            list_host: None,
            now: None,
        }
    }

    fn tree(&self, all: bool, extra_dirs: &[String]) -> Tree {
        let mut dirs = vec![self.dir.clone()];
        dirs.extend_from_slice(extra_dirs);
        new(self.deps(), &dirs, &[], all)
    }

    fn spawn_in(&self, w: &SQLiteStorage, name: &str, parent: &str) {
        self.sessions.borrow_mut().push(format!("agent-{name}"));
        w.create_agent(&StoredAgent {
            name: name.to_string(),
            session_name: format!("agent-{name}"),
            backend: "claude".to_string(),
            spawned_by: parent.to_string(),
            cwd: "/w".to_string(),
            created_at: Time::now(),
            ..StoredAgent::default()
        })
        .unwrap();
    }

    fn spawn(&self, name: &str, parent: &str) {
        self.spawn_in(&self.w, name, parent);
    }

    fn report(&self, name: &str, status: &str, detail: &str, transcript: &str) {
        let now = Time::now();
        let mut entries = vec![
            StateEntry {
                agent_name: name.to_string(),
                key: "_status".to_string(),
                value: status.to_string(),
                updated_at: now,
            },
            StateEntry {
                agent_name: name.to_string(),
                key: "_detail".to_string(),
                value: detail.to_string(),
                updated_at: now,
            },
        ];
        if !transcript.is_empty() {
            entries.push(StateEntry {
                agent_name: name.to_string(),
                key: "_transcript".to_string(),
                value: transcript.to_string(),
                updated_at: now,
            });
        }
        let ev = AgentEvent {
            at: now,
            kind: AGENT_STATUS.to_string(),
            agent: name.to_string(),
            status: status.to_string(),
            detail: detail.to_string(),
            transcript: transcript.to_string(),
            ..AgentEvent::default()
        };
        self.w
            .record_report(&entries, name, &[], Some(&ev))
            .unwrap();
    }

    fn cleanup(&self, name: &str) {
        let target = format!("agent-{name}");
        self.sessions.borrow_mut().retain(|s| s != &target);
        self.w.delete_agent(name).unwrap();
    }
}

fn poll(tr: &mut Tree) -> Vec<Line> {
    tr.poll(&Context::background()).unwrap()
}

/// Renders lines as `"type:name[:reason][:status]"` for comparing.
fn summary(lines: &[Line]) -> Vec<String> {
    let mut out = Vec::new();
    for l in lines {
        match l {
            Line::Agent(l) => {
                let mut s = format!("{}:{}", l.r#type, l.agent.name);
                if !l.reason.is_empty() {
                    s.push(':');
                    s.push_str(&l.reason);
                }
                if !l.agent.status.is_empty() {
                    s.push(':');
                    s.push_str(&l.agent.status);
                }
                out.push(s);
            }
            Line::Store(l) => {
                out.push(format!(
                    "store:{}",
                    filepath::base(&filepath::dir(&l.store.dir))
                ));
            }
            Line::Host(l) => {
                out.push(format!("host:{}", l.host.host));
            }
            Line::Unrecorded(_) => {
                out.push("unrecorded".to_string());
            }
        }
    }
    out
}

#[test]
fn test_snapshot() {
    let f = Fixture::new();
    let tdir = temp_dir();
    let transcript = filepath::join(&[tdir.path().to_str().unwrap(), "s.jsonl"]);
    fs::write(&transcript, b"{}\n").unwrap();

    f.spawn("lead", "");
    f.spawn("lead-t1", "lead");
    f.report("lead-t1", "waiting", "permission: Bash: rm x", &transcript);
    f.spawn("gone", "lead");
    f.cleanup("gone");
    f.sessions
        .borrow_mut()
        .extend(["agent-stray".to_string(), "other-session".to_string()]);

    let ctx = Context::background();
    let snap = f.tree(false, &[]).snapshot(&ctx).unwrap();
    assert!(
        snap.stores.len() == 1
            && snap.stores[0].this
            && snap.stores[0].events
            && snap.stores[0].cursor == 5,
        "stores = {:?}, want this store with events up to 5",
        snap.stores
    );
    let names: Vec<&str> = snap.agents.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, vec!["lead", "lead-t1"], "gone was cleaned up");
    let a = &snap.agents[1];
    assert!(
        a.parent == "lead"
            && a.host == "box"
            && a.kind == KIND_TERMD
            && a.namespace == "agents"
            && a.live
            && a.attached == Some(true)
            && a.status == "waiting"
            && a.run != 0
            && a.store == f.dir
            && a.cwd == "/w",
        "lead-t1 = {a:?}"
    );
    let raw_agent = serde_json::to_string(a).unwrap();
    assert!(
        raw_agent.contains(r#""namespace":"agents""#),
        "marshaled local agent = {raw_agent}, want namespace:agents"
    );
    assert!(
        a.transcript == transcript
            && a.dialect == "claude"
            && a.transcript_size == 3
            && !a.transcript_mtime.is_zero(),
        "lead-t1 transcript = {:?} {:?} {} {:?}, want the file, claude, 3 bytes",
        a.transcript,
        a.dialect,
        a.transcript_size,
        a.transcript_mtime
    );
    assert!(
        snap.unrecorded.len() == 1 && snap.unrecorded[0].session == "agent-stray",
        "unrecorded = {:?}, want agent-stray only (other-session isn't an agent's)",
        snap.unrecorded
    );

    let snap = f.tree(true, &[]).snapshot(&ctx).unwrap();
    assert_eq!(snap.agents.len(), 3, "with all: want 3 agents");
    let g = &snap.agents[2];
    assert!(
        g.name == "gone"
            && !g.live
            && !g.ended_at.is_zero()
            && g.attached == Some(false)
            && g.parent == "lead",
        "departed = {g:?}, want gone, not live, ended, detached"
    );
}

// Runs don't record the session id, so a departed agent's comes from its
// transcript's path: in a snapshot, and in the removed line of a follower
// that never saw a _session_id for it.
#[test]
fn test_departed_session_id() {
    let f = Fixture::new();
    let claude = "/home/user/.claude/projects/-w/11111111-2222-3333-4444-555555555555.jsonl";
    let agy = "/home/user/.gemini/antigravity-cli/brain/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/.system_generated/logs/transcript_full.jsonl";
    f.spawn("cl", "");
    f.report("cl", "idle", "", claude);
    f.spawn("ag", "");
    f.report("ag", "idle", "", agy);
    f.spawn("none", "");

    let ctx = Context::background();
    let mut tr = f.tree(true, &[]);
    tr.snapshot(&ctx).unwrap();
    f.cleanup("cl");
    let lines = poll(&mut tr);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].as_agent().unwrap().agent.session_id,
        "11111111-2222-3333-4444-555555555555",
        "removed line = {lines:?}, want cl with its transcript's session id"
    );

    f.cleanup("ag");
    f.cleanup("none");
    let snap = f.tree(true, &[]).snapshot(&ctx).unwrap();
    let got: HashMap<String, String> = snap
        .agents
        .into_iter()
        .map(|a| (a.name, a.session_id))
        .collect();
    let want: HashMap<String, String> = HashMap::from([
        (
            "cl".to_string(),
            "11111111-2222-3333-4444-555555555555".to_string(),
        ),
        (
            "ag".to_string(),
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".to_string(),
        ),
        ("none".to_string(), String::new()),
    ]);
    assert_eq!(got, want);
}

#[test]
fn test_follow() {
    let f = Fixture::new();
    f.spawn("lead", "");
    let ctx = Context::background();
    let mut tr = f.tree(true, &[]);
    tr.snapshot(&ctx).unwrap();
    let got = poll(&mut tr);
    assert!(
        got.is_empty(),
        "a quiet store: {:?}, want no lines",
        summary(&got)
    );

    f.spawn("lead-t1", "lead");
    f.report("lead-t1", "processing", "Bash: ls", "");
    f.report("lead-t1", "waiting", "permission: Bash: rm x", "");
    f.report("lead-t1", "idle", "", "");
    let lines = poll(&mut tr);
    let want = vec![
        "spawned:lead-t1",
        "changed:lead-t1:status:processing",
        "changed:lead-t1:status:waiting",
        "changed:lead-t1:status:idle",
    ];
    assert_eq!(
        summary(&lines),
        want,
        "every report, even one already superseded"
    );
    let sp = lines[0].as_agent().unwrap();
    assert!(
        sp.id == 2
            && sp.store == f.dir
            && sp.agent.parent == "lead"
            && sp.agent.live
            && sp.agent.attached == Some(true),
        "spawned = {sp:?}"
    );
    let w = lines[2].as_agent().unwrap();
    assert!(
        w.agent.detail == "permission: Bash: rm x" && w.id == 4,
        "waiting line = {w:?}"
    );

    f.cleanup("lead-t1");
    let lines = poll(&mut tr);
    // Cleanup closed its pane: a departed agent is ended (closed).
    assert_eq!(
        summary(&lines),
        vec!["removed:lead-t1:ended"],
        "after cleanup"
    );
    let rm = &lines[0].as_agent().unwrap().agent;
    assert!(
        !rm.live
            && !rm.ended_at.is_zero()
            && rm.attached == Some(false)
            && rm.end_reason == "closed",
        "removed agent = {rm:?}, want departed"
    );
    assert_eq!(
        tr.build_snapshot().agents.len(),
        2,
        "tree with all after cleanup wants lead and the departed lead-t1"
    );

    // The session going away without a cleanup shows on the next session check.
    f.sessions.borrow_mut().clear();
    assert_eq!(
        summary(&tr.poll_sessions(&ctx)),
        vec!["changed:lead:session"],
        "session gone"
    );
    let quiet = tr.poll_sessions(&ctx);
    assert!(
        quiet.is_empty(),
        "unchanged sessions: {:?}, want no lines",
        summary(&quiet)
    );
    tr.close();
}

#[test]
fn test_follow_transcript_grows() {
    let f = Fixture::new();
    f.spawn("a", "");
    let tdir = temp_dir();
    let transcript = filepath::join(&[tdir.path().to_str().unwrap(), "a.jsonl"]);
    fs::write(&transcript, b"{}\n").unwrap();
    f.report("a", "processing", "", &transcript);

    let ctx = Context::background();
    let mut tr = f.tree(false, &[]);
    tr.snapshot(&ctx).unwrap();

    fs::write(&transcript, b"{}\n{}\n").unwrap();
    let lines = tr.poll_sessions(&ctx);
    assert_eq!(summary(&lines), vec!["changed:a:transcript:processing"]);
    assert_eq!(lines[0].as_agent().unwrap().agent.transcript_size, 6);
}

#[test]
fn test_resume() {
    let f = Fixture::new();
    f.spawn("a", "");
    f.report("a", "processing", "", "");
    f.report("a", "idle", "", "");

    let ctx = Context::background();
    let mut tr = f.tree(false, &[]);
    let ok = tr.resume(&ctx, 1).unwrap();
    assert!(ok, "Resume(1) = {ok}; want a resume");
    assert_eq!(
        summary(&poll(&mut tr)),
        vec!["changed:a:status:processing", "changed:a:status:idle"],
        "after resuming from 1: want the two reports"
    );
    let ok = f.tree(false, &[]).resume(&ctx, 99).unwrap();
    assert!(!ok, "Resume past the newest event resumed");
}

// A second store, found through an unrecorded session's PATH, is read and
// followed too.
#[test]
fn test_follow_other_stores() {
    let f = Fixture::new();
    f.spawn("coord", "");
    let ctx = Context::background();
    let mut tr = f.tree(false, &[]);
    tr.snapshot(&ctx).unwrap();

    let other_tmp = temp_dir();
    let other_dir = filepath::join(&[other_tmp.path().to_str().unwrap(), ".gomuxai"]);
    let other = open_store(&other_dir);
    f.spawn_in(&other, "coord-t1", "coord");
    f.path_of
        .borrow_mut()
        .insert("agent-coord-t1".to_string(), other_dir.clone());

    let lines = tr.poll_sessions(&ctx);
    assert_eq!(
        summary(&lines),
        vec![format!(
            "store:{}",
            filepath::base(&filepath::dir(&other_dir))
        )]
    );
    let sl = lines[0].as_store().unwrap();
    assert!(
        sl.agents.len() == 1 && sl.agents[0].parent == "coord" && sl.agents[0].store == other_dir,
        "store line = {sl:?}"
    );

    f.spawn_in(&other, "coord-t2", "coord");
    assert_eq!(summary(&poll(&mut tr)), vec!["spawned:coord-t2"]);

    // No store found: the session is listed as unrecorded, once.
    f.sessions.borrow_mut().push("agent-stray".to_string());
    assert_eq!(summary(&tr.poll_sessions(&ctx)), vec!["unrecorded"]);
    let quiet = tr.poll_sessions(&ctx);
    assert!(
        quiet.is_empty(),
        "same stray session: {:?}, want no lines",
        summary(&quiet)
    );
}

#[test]
fn test_follow_store_without_events() {
    let f = Fixture::new();
    let old_tmp = temp_dir();
    let old_dir = filepath::join(&[old_tmp.path().to_str().unwrap(), ".gomuxai"]);
    fs::create_dir_all(&old_dir).unwrap();
    let db = Connection::open(filepath::join(&[&old_dir, "gomuxai.db"])).unwrap();
    for stmt in [
        "CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL, spawned_by TEXT)",
        "CREATE TABLE state (agent_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (agent_name, key))",
        "CREATE TABLE command_log (id INTEGER PRIMARY KEY, started_at TEXT, agent TEXT, cwd TEXT, args TEXT, exit_code INTEGER)",
        "INSERT INTO agents VALUES ('old', 'agent-old', 'claude', '', '', '2026-10-24T14:00:00Z', '')",
    ] {
        db.execute_batch(stmt).unwrap();
    }

    let ctx = Context::background();
    let mut tr = f.tree(false, std::slice::from_ref(&old_dir));
    let snap = tr.snapshot(&ctx).unwrap();
    assert!(
        snap.stores.len() == 2 && !snap.stores[1].events && snap.agents.len() == 1,
        "snapshot = {snap:?}, want the old store without events and its agent"
    );
    let quiet = poll(&mut tr);
    assert!(
        quiet.is_empty(),
        "quiet old store: {:?}, want no lines",
        summary(&quiet)
    );

    db.execute(
        "INSERT INTO state VALUES ('old', '_status', 'processing', '2026-10-24T14:01:00Z')",
        [],
    )
    .unwrap();
    let lines = poll(&mut tr);
    assert_eq!(summary(&lines), vec!["changed:old:status:processing"]);
    assert_eq!(lines[0].as_agent().unwrap().id, 0);

    db.execute(
        "INSERT INTO agents VALUES ('new', 'agent-new', 'claude', '', '', '2026-10-24T14:02:00Z', '')",
        [],
    )
    .unwrap();
    let lines = poll(&mut tr);
    assert_eq!(lines.len(), 1);
    assert_eq!(
        lines[0].as_store().unwrap().agents.len(),
        2,
        "an agent appeared in the old store: want a store line with both"
    );
}

#[test]
fn test_hosts() {
    let f = Fixture::new();
    let listed: Rc<RefCell<Vec<AgentInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let list_err: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    let mut d = f.deps();
    let listed_c = Rc::clone(&listed);
    let err_c = Rc::clone(&list_err);
    d.list_host = Some(Box::new(move |_ctx, _host| {
        if let Some(msg) = err_c.borrow().as_ref() {
            anyhow::bail!("{msg}");
        }
        Ok(listed_c.borrow().clone())
    }));

    let ctx = Context::background();
    let mut tr = new(d, std::slice::from_ref(&f.dir), &["hub".to_string()], false);

    *listed.borrow_mut() = vec![AgentInfo {
        name: "far".to_string(),
        backend: "claude".to_string(),
        session: "agent-far".to_string(),
        spawned_by: "lead".to_string(),
        status: "idle".to_string(),
        created_at: BusTime::now(),
        ..AgentInfo::default()
    }];
    let snap = tr.snapshot(&ctx).unwrap();
    assert!(
        snap.hosts.len() == 1 && snap.hosts[0].agents == 1 && snap.agents.len() == 1,
        "snapshot = {snap:?}, want hub's one agent"
    );
    let a = &snap.agents[0];
    assert!(
        a.host == "hub"
            && a.store.is_empty()
            && a.attached.is_none()
            && a.status == "idle"
            && a.parent == "lead",
        "remote agent = {a:?}, want host hub, no store, attached unknown"
    );
    assert!(
        tr.poll_hosts(&ctx).is_empty(),
        "unchanged host: want no lines"
    );

    *list_err.borrow_mut() = Some("no bus".to_string());
    let lines = tr.poll_hosts(&ctx);
    assert_eq!(summary(&lines), vec!["host:hub"]);
    assert_eq!(lines[0].as_host().unwrap().host.error, "no bus");
}

// Resumable applies resume's rules to what export knows: a claude or agy
// agent, not adopted, with a session, ended (by its report, or because its
// terminal session is gone), and still recorded.
#[test]
fn test_resumable() {
    let f = Fixture::new();
    let transcript = "/home/user/.claude/projects/-w/11111111-2222-3333-4444-555555555555.jsonl";
    let create = |name: &str, backend: &str, adopted: bool| {
        f.sessions.borrow_mut().push(format!("agent-{name}"));
        f.w.create_agent(&StoredAgent {
            name: name.to_string(),
            session_name: format!("agent-{name}"),
            backend: backend.to_string(),
            adopted,
            cwd: "/w".to_string(),
            created_at: Time::now(),
            ..StoredAgent::default()
        })
        .unwrap();
    };
    create("quit", "claude", false);
    f.report("quit", "ended", "", transcript);
    create("gone", "claude", false);
    f.report("gone", "idle", "", transcript);
    f.sessions.borrow_mut().retain(|s| s != "agent-gone");
    create("busy", "claude", false);
    f.report("busy", "processing", "", transcript);
    create("nosession", "claude", false);
    f.report("nosession", "ended", "", "");
    create("aider", "aider", false);
    f.report("aider", "ended", "", transcript);
    create("adopted", "claude", true);
    f.report("adopted", "ended", "", transcript);
    create("departed", "claude", false);
    f.report("departed", "ended", "", transcript);
    f.cleanup("departed");

    let ctx = Context::background();
    let mut tr = f.tree(true, &[]);
    let snap = tr.snapshot(&ctx).unwrap();
    let mut got = HashMap::new();
    for a in &snap.agents {
        let r = a.resumable.unwrap_or_else(|| {
            panic!(
                "{}: resumable absent, want it set for a local agent",
                a.name
            )
        });
        got.insert(a.name.clone(), r);
    }
    let want: HashMap<String, bool> = HashMap::from([
        ("quit".to_string(), true),
        ("gone".to_string(), true),
        ("busy".to_string(), false),
        ("nosession".to_string(), false),
        ("aider".to_string(), false),
        ("adopted".to_string(), false),
        ("departed".to_string(), false),
    ]);
    assert_eq!(got, want);

    // Following: a report that ends it, and its terminal session going away,
    // each make it resumable in the changed line.
    f.report("busy", "ended", "", transcript);
    let lines = poll(&mut tr);
    assert!(
        lines.len() == 1 && lines[0].as_agent().unwrap().agent.resumable == Some(true),
        "busy ended: {lines:?}, want one changed line, resumable"
    );

    create("live", "agy", false);
    poll(&mut tr);
    f.report(
        "live",
        "idle",
        "",
        "/home/user/.gemini/antigravity-cli/brain/aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee/.system_generated/logs/transcript_full.jsonl",
    );
    let lines = poll(&mut tr);
    assert!(
        lines.len() == 1 && lines[0].as_agent().unwrap().agent.resumable == Some(false),
        "live idle: {lines:?}, want one changed line, not resumable"
    );

    f.sessions.borrow_mut().retain(|s| s != "agent-live");
    let poll_lines = tr.poll_sessions(&ctx);
    let session_line = poll_lines.iter().find_map(|l| {
        let al = l.as_agent()?;
        (al.agent.name == "live" && al.reason == REASON_SESSION).then_some(al)
    });
    assert!(
        matches!(session_line, Some(al) if al.agent.resumable == Some(true)),
        "live's session gone: {session_line:?}, want a session line, resumable"
    );
}

// Another host's agents carry what its list sends. A host that predates
// those fields sends none, and they stay absent (unknown), not false.
#[test]
fn test_hosts_fields() {
    let f = Fixture::new();
    let mut d = f.deps();
    let created = BusTime::date_utc(2026, 11, 1, 9, 0, 0, 0);
    d.list_host = Some(Box::new(move |_ctx, host| {
        if host == "old" {
            return Ok(vec![AgentInfo {
                name: "far".to_string(),
                backend: "claude".to_string(),
                session: "agent-far".to_string(),
                status: "ended".to_string(),
                created_at: created,
                ..AgentInfo::default()
            }]);
        }
        Ok(vec![AgentInfo {
            name: "far".to_string(),
            backend: "claude".to_string(),
            session: "agent-far".to_string(),
            status: "ended".to_string(),
            end_reason: "gone".to_string(),
            detail: "its tmux session is gone".to_string(),
            session_id: "5f0c".to_string(),
            cwd: "/srv/w".to_string(),
            attached: Some(false),
            resumable: Some(true),
            created_at: created,
            ..AgentInfo::default()
        }])
    }));

    let ctx = Context::background();
    let mut tr = new(
        d,
        std::slice::from_ref(&f.dir),
        &["new".to_string(), "old".to_string()],
        false,
    );
    let snap = tr.snapshot(&ctx).unwrap();
    assert_eq!(snap.agents.len(), 2, "want far on new and old");

    let by_host: HashMap<&str, &Agent> = snap.agents.iter().map(|a| (a.host.as_str(), a)).collect();
    let n = by_host["new"];
    assert!(
        n.end_reason == "gone"
            && !n.detail.is_empty()
            && n.session_id == "5f0c"
            && n.cwd == "/srv/w"
            && n.attached == Some(false)
            && n.resumable == Some(true),
        "new host's agent = {n:?}"
    );

    let raw = serde_json::to_string(by_host["old"]).unwrap();
    let fields: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&raw).unwrap();
    for k in [
        "attached",
        "resumable",
        "end_reason",
        "session_id",
        "cwd",
        "detail",
        "namespace",
    ] {
        assert!(
            !fields.contains_key(k),
            "old host's agent has {k} = {:?}, want it absent (unknown)",
            fields.get(k)
        );
    }

    assert!(
        tr.poll_hosts(&ctx).is_empty(),
        "unchanged hosts: want no lines"
    );
}

#[test]
fn test_edge_cases_and_json_roundtrip() {
    let f = Fixture::new();
    let ctx = Context::background();

    // Missing own store fails Snapshot with "opening <dir>: ..."; missing extra store stays in Snapshot.stores with error.
    let missing_own = filepath::join(&[f.dir.as_str(), "missing-own"]);
    let err = new(f.deps(), std::slice::from_ref(&missing_own), &[], false)
        .snapshot(&ctx)
        .unwrap_err();
    assert!(
        format!("{err:#}").starts_with(&format!("opening {missing_own}: ")),
        "unexpected error: {err:#}"
    );

    let missing_extra = filepath::join(&[f.dir.as_str(), "missing-extra"]);
    let snap = f
        .tree(false, std::slice::from_ref(&missing_extra))
        .snapshot(&ctx)
        .unwrap();
    assert_eq!(snap.stores.len(), 2);
    assert!(!snap.stores[1].error.is_empty());

    // Session switch without a transcript clears previous session's transcript fields;
    // tool_input is kept only while waiting in Snapshot.read.
    let tdir = temp_dir();
    let tr_path = filepath::join(&[tdir.path().to_str().unwrap(), "c.jsonl"]);
    fs::write(&tr_path, b"{}\n").unwrap();
    f.spawn("sw", "");
    let t0 = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
    f.w.record_report(
        &[
            StateEntry {
                agent_name: "sw".into(),
                key: "_status".into(),
                value: "idle".into(),
                updated_at: t0,
            },
            StateEntry {
                agent_name: "sw".into(),
                key: "_tool_input".into(),
                value: r#"{"command":"ls"}"#.into(),
                updated_at: t0,
            },
            StateEntry {
                agent_name: "sw".into(),
                key: "_session_id".into(),
                value: "s1".into(),
                updated_at: t0,
            },
            StateEntry {
                agent_name: "sw".into(),
                key: "_transcript".into(),
                value: tr_path.clone(),
                updated_at: t0,
            },
        ],
        "sw",
        &[],
        Some(&AgentEvent {
            at: t0,
            kind: AGENT_STATUS.into(),
            agent: "sw".into(),
            status: "idle".into(),
            session_id: "s1".into(),
            transcript: tr_path,
            ..AgentEvent::default()
        }),
    )
    .unwrap();

    let mut tr = f.tree(true, &[]);
    let snap = tr.snapshot(&ctx).unwrap();
    assert!(
        snap.agents[0].tool_input.is_empty(),
        "non-waiting agent must clear tool_input in snapshot"
    );
    assert_eq!(snap.agents[0].transcript_size, 3);

    // Emit a status event with a new session_id and empty transcript -> clears transcript fields.
    let t1 = Time::date_utc(2026, 10, 24, 14, 0, 1, 500_000_000);
    f.w.record_report(
        &[StateEntry {
            agent_name: "sw".into(),
            key: "_session_id".into(),
            value: "s2".into(),
            updated_at: t1,
        }],
        "sw",
        &[],
        Some(&AgentEvent {
            at: t1,
            kind: AGENT_STATUS.into(),
            agent: "sw".into(),
            status: "processing".into(),
            session_id: "s2".into(),
            ..AgentEvent::default()
        }),
    )
    .unwrap();
    let lines = poll(&mut tr);
    let changed = lines[0].as_agent().unwrap();
    assert_eq!(changed.agent.session_id, "s2");
    assert!(changed.agent.transcript.is_empty());
    assert!(changed.agent.dialect.is_empty());
    assert_eq!(changed.agent.transcript_size, 0);
    assert!(changed.agent.transcript_mtime.is_zero());

    // Round-trip every Line variant through serde_json.
    let sample_lines = vec![
        Line::from(changed.clone()),
        Line::Store(StoreLine {
            r#type: LINE_STORE.into(),
            store: snap.stores[0].clone(),
            agents: Vec::new(),
        }),
        Line::Host(HostLine {
            r#type: LINE_HOST.into(),
            host: Host {
                host: "hub".into(),
                agents: 0,
                error: String::new(),
            },
            agents: Vec::new(),
        }),
        Line::Unrecorded(UnrecordedLine {
            r#type: LINE_UNRECORDED.into(),
            sessions: vec![Session {
                session: "agent-x".into(),
                store: String::new(),
            }],
        }),
    ];
    for l in &sample_lines {
        let json = serde_json::to_string(l).unwrap();
        let decoded: Line = serde_json::from_str(&json).unwrap();
        assert_eq!(&decoded, l, "round-trip failed for {json}");
    }
}

/// Verifies byte-for-byte JSON parity between Go `legacy/internal/export` and
/// Rust `agent-export` (both compact `--json --follow` lines and 2-space
/// indented `--json` snapshots) over the exact same SQLite stores on disk.
/// Skips without the legacy Go module (legacy/README.md) or `go`.
#[test]
fn test_go_byte_for_byte_parity() {
    let legacy = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../legacy");
    let Some(go_module) = legacy
        .canonicalize()
        .ok()
        .filter(|r| r.join("internal/export/export.go").is_file())
    else {
        eprintln!("skipping Go <-> Rust export parity test: no legacy/ Go source in this checkout");
        return;
    };
    if std::process::Command::new("go")
        .arg("version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skipping Go <-> Rust export parity test: `go` is missing");
        return;
    }

    let work = temp_dir();
    let base = work.path().to_str().unwrap();
    let own_dir = filepath::join(&[base, "own", ".gomuxai"]);
    let other_dir = filepath::join(&[base, "other", ".gomuxai"]);
    let old_dir = filepath::join(&[base, "old", ".gomuxai"]);
    let tr_claude = filepath::join(&[
        base,
        ".claude",
        "projects",
        "-w",
        "11111111-2222-3333-4444-555555555555.jsonl",
    ]);
    let tr_agy = filepath::join(&[
        base,
        ".gemini",
        "antigravity-cli",
        "brain",
        "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
        ".system_generated",
        "logs",
        "transcript_full.jsonl",
    ]);
    let tr_legacy = filepath::join(&[
        base,
        ".claude",
        "projects",
        "-w",
        "22222222-3333-4444-5555-666666666666.jsonl",
    ]);
    for p in [&tr_claude, &tr_agy, &tr_legacy] {
        fs::create_dir_all(Path::new(p).parent().unwrap()).unwrap();
    }
    fs::write(&tr_claude, b"{\"a\":1}\n").unwrap();
    fs::write(&tr_agy, b"{\"b\":2}\n").unwrap();
    fs::write(&tr_legacy, b"{\"c\":3}\n").unwrap();

    // Populate primary store (`own_dir`).
    let own = open_store(&own_dir);
    let t_base = Time::date_utc(2026, 11, 1, 9, 0, 0, 0);
    let t_sub = Time::date_utc(2026, 11, 1, 9, 0, 1, 250_000_000);
    let t_rep1 = Time::date_utc(2026, 11, 1, 9, 0, 2, 500_000_000);
    let t_rep2 = Time::date_utc(2026, 11, 1, 9, 0, 3, 750_000);
    let t_adopt = Time::date_utc(2026, 11, 1, 9, 0, 4, 0);
    let t_gone = Time::date_utc(2026, 11, 1, 9, 0, 5, 0);
    let t_now = Time::date_utc(2026, 11, 1, 12, 0, 0, 125_000_000);

    own.create_agent(&StoredAgent {
        name: "lead".into(),
        session_name: "agent-lead".into(),
        backend: "claude".into(),
        cwd: "/srv/app".into(),
        created_at: t_base,
        ..StoredAgent::default()
    })
    .unwrap();
    own.record_report(
        &[
            StateEntry {
                agent_name: "lead".into(),
                key: "_status".into(),
                value: "idle".into(),
                updated_at: t_rep1,
            },
            StateEntry {
                agent_name: "lead".into(),
                key: "_event".into(),
                value: "Stop".into(),
                updated_at: t_rep1,
            },
            StateEntry {
                agent_name: "lead".into(),
                key: "_detail".into(),
                value: "done <ok> & ready".into(),
                updated_at: t_rep1,
            },
            StateEntry {
                agent_name: "lead".into(),
                key: "_session_id".into(),
                value: "11111111-2222-3333-4444-555555555555".into(),
                updated_at: t_rep1,
            },
            StateEntry {
                agent_name: "lead".into(),
                key: "_transcript".into(),
                value: tr_claude.clone(),
                updated_at: t_rep1,
            },
        ],
        "lead",
        &[],
        Some(&AgentEvent {
            at: t_rep1,
            kind: AGENT_STATUS.into(),
            agent: "lead".into(),
            status: "idle".into(),
            event: "Stop".into(),
            detail: "done <ok> & ready".into(),
            session_id: "11111111-2222-3333-4444-555555555555".into(),
            transcript: tr_claude.clone(),
            ..AgentEvent::default()
        }),
    )
    .unwrap();

    own.create_agent(&StoredAgent {
        name: "lead-w1".into(),
        session_name: "agent-lead-w1".into(),
        backend: "agy".into(),
        spawned_by: "lead".into(),
        cwd: "/srv/app/sub".into(),
        created_at: t_sub,
        ..StoredAgent::default()
    })
    .unwrap();
    own.record_report(
        &[
            StateEntry {
                agent_name: "lead-w1".into(),
                key: "_status".into(),
                value: "waiting".into(),
                updated_at: t_rep2,
            },
            StateEntry {
                agent_name: "lead-w1".into(),
                key: "_event".into(),
                value: "PermissionRequest".into(),
                updated_at: t_rep2,
            },
            StateEntry {
                agent_name: "lead-w1".into(),
                key: "_detail".into(),
                value: "permission: Bash: echo a < b && c > d".into(),
                updated_at: t_rep2,
            },
            StateEntry {
                agent_name: "lead-w1".into(),
                key: "_tool".into(),
                value: "Bash".into(),
                updated_at: t_rep2,
            },
            StateEntry {
                agent_name: "lead-w1".into(),
                key: "_tool_input".into(),
                value: r#"{"command":"echo a < b && c > d"}"#.into(),
                updated_at: t_rep2,
            },
            StateEntry {
                agent_name: "lead-w1".into(),
                key: "_transcript".into(),
                value: tr_agy.clone(),
                updated_at: t_rep2,
            },
        ],
        "lead-w1",
        &[],
        Some(&AgentEvent {
            at: t_rep2,
            kind: AGENT_STATUS.into(),
            agent: "lead-w1".into(),
            status: "waiting".into(),
            event: "PermissionRequest".into(),
            detail: "permission: Bash: echo a < b && c > d".into(),
            tool: "Bash".into(),
            tool_input: r#"{"command":"echo a < b && c > d"}"#.into(),
            transcript: tr_agy.clone(),
            ..AgentEvent::default()
        }),
    )
    .unwrap();

    own.create_agent(&StoredAgent {
        name: "adopted".into(),
        session_name: "user-pane".into(),
        backend: "claude".into(),
        adopted: true,
        cwd: "/srv/app".into(),
        created_at: t_adopt,
        ..StoredAgent::default()
    })
    .unwrap();
    own.record_report(
        &[
            StateEntry {
                agent_name: "adopted".into(),
                key: "_status".into(),
                value: "ended".into(),
                updated_at: t_adopt,
            },
            StateEntry {
                agent_name: "adopted".into(),
                key: "_end_reason".into(),
                value: "quit".into(),
                updated_at: t_adopt,
            },
            StateEntry {
                agent_name: "adopted".into(),
                key: "_session_id".into(),
                value: "99999999-2222-3333-4444-555555555555".into(),
                updated_at: t_adopt,
            },
        ],
        "adopted",
        &[],
        Some(&AgentEvent {
            at: t_adopt,
            kind: AGENT_STATUS.into(),
            agent: "adopted".into(),
            status: "ended".into(),
            end_reason: "quit".into(),
            session_id: "99999999-2222-3333-4444-555555555555".into(),
            ..AgentEvent::default()
        }),
    )
    .unwrap();

    own.create_agent(&StoredAgent {
        name: "gone".into(),
        session_name: "agent-gone".into(),
        backend: "claude".into(),
        spawned_by: "lead".into(),
        cwd: "/srv/app".into(),
        created_at: t_gone,
        ..StoredAgent::default()
    })
    .unwrap();
    own.record_report(
        &[StateEntry {
            agent_name: "gone".into(),
            key: "_transcript".into(),
            value: tr_claude.clone(),
            updated_at: t_gone,
        }],
        "gone",
        &[],
        Some(&AgentEvent {
            at: t_gone,
            kind: AGENT_STATUS.into(),
            agent: "gone".into(),
            status: "idle".into(),
            transcript: tr_claude.clone(),
            ..AgentEvent::default()
        }),
    )
    .unwrap();
    own.delete_agent("gone").unwrap();
    drop(own);

    // Populate discovered modern store (`other_dir`).
    let other = open_store(&other_dir);
    other
        .create_agent(&StoredAgent {
            name: "disc".into(),
            session_name: "agent-disc".into(),
            backend: "claude".into(),
            spawned_by: "lead".into(),
            cwd: "/srv/other".into(),
            created_at: t_base,
            ..StoredAgent::default()
        })
        .unwrap();
    drop(other);

    // Populate legacy store (`old_dir`) without `runs` or `agent_events`, plus a
    // logged departed spawn with `logs/leg-gone.transcripts`.
    fs::create_dir_all(filepath::join(&[&old_dir, "logs"])).unwrap();
    fs::write(
        filepath::join(&[&old_dir, "logs", "leg-gone.transcripts"]),
        format!("claude\t{tr_legacy}\n"),
    )
    .unwrap();
    let old_db = Connection::open(filepath::join(&[&old_dir, "gomuxai.db"])).unwrap();
    for stmt in [
        "CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL, spawned_by TEXT)",
        "CREATE TABLE state (agent_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (agent_name, key))",
        "CREATE TABLE command_log (id INTEGER PRIMARY KEY, started_at TEXT, agent TEXT, cwd TEXT, args TEXT, exit_code INTEGER)",
        "INSERT INTO agents VALUES ('leg-live', 'agent-leg-live', 'claude', '', '', '2026-10-24T14:00:00Z', '')",
        "INSERT INTO state VALUES ('leg-live', '_status', 'idle', '2026-10-24T14:01:00Z')",
        r#"INSERT INTO command_log VALUES (1, '2026-10-24T13:00:00Z', 'lead', '/srv/leg', '["spawn","leg-gone","--backend","claude"]', 0)"#,
    ] {
        old_db.execute_batch(stmt).unwrap();
    }
    drop(old_db);

    // Run Rust scenario and collect JSON outputs.
    let run_rust = || -> Vec<(String, String)> {
        let mut out = Vec::new();
        let sessions = Rc::new(RefCell::new(vec![
            "agent-lead".to_string(),
            "agent-lead-w1".to_string(),
            "user-pane".to_string(),
            "agent-leg-live".to_string(),
            "agent-stray".to_string(),
            "agent-untracked".to_string(),
        ]));
        let path_of = Rc::new(RefCell::new(HashMap::from([
            ("agent-disc".to_string(), other_dir.clone()),
            ("agent-untracked".to_string(), own_dir.clone()),
        ])));
        let remote_status = Rc::new(RefCell::new("ended".to_string()));

        let make_deps = || {
            let s = Rc::clone(&sessions);
            let p = Rc::clone(&path_of);
            let rs = Rc::clone(&remote_status);
            Deps {
                host: "box".to_string(),
                namespace: String::new(),
                session_prefix: "agent-".to_string(),
                sessions: Some(Box::new(move || Ok(s.borrow().clone()))),
                session_store: Some(Box::new(move |sess| {
                    p.borrow().get(sess).cloned().unwrap_or_default()
                })),
                list_host: Some(Box::new(move |_ctx, host| match host {
                    "old" => Ok(vec![AgentInfo {
                        name: "far".into(),
                        backend: "claude".into(),
                        session: "agent-far".into(),
                        status: "ended".into(),
                        created_at: BusTime::date_utc(2026, 11, 1, 8, 0, 0, 0),
                        ..AgentInfo::default()
                    }]),
                    "new" => Ok(vec![AgentInfo {
                        name: "far".into(),
                        backend: "claude".into(),
                        session: "agent-far".into(),
                        spawned_by: "lead".into(),
                        status: rs.borrow().clone(),
                        end_reason: if *rs.borrow() == "ended" {
                            "gone".into()
                        } else {
                            String::new()
                        },
                        detail: "remote <detail> & status".into(),
                        session_id: "5f0c".into(),
                        cwd: "/srv/w".into(),
                        attached: Some(false),
                        resumable: Some(*rs.borrow() == "ended"),
                        created_at: BusTime::date_utc(2026, 11, 1, 8, 0, 0, 0),
                        ..AgentInfo::default()
                    }]),
                    _ => anyhow::bail!("no bus"),
                })),
                now: Some(Box::new(move || t_now)),
            }
        };

        let ctx = Context::background();
        let dirs = vec![own_dir.clone(), old_dir.clone()];
        let hosts = vec!["down".to_string(), "new".to_string(), "old".to_string()];

        // 1. Snapshot (all = false), compact + pretty.
        let snap_live = new(make_deps(), &dirs, &hosts, false)
            .snapshot(&ctx)
            .unwrap();
        out.push((
            "snap_live_compact".into(),
            serde_json::to_string(&snap_live).unwrap(),
        ));
        out.push((
            "snap_live_pretty".into(),
            serde_json::to_string_pretty(&snap_live).unwrap(),
        ));

        // 2. Snapshot (all = true) + Resume(1) + Poll + PollSessions + PollHosts.
        let mut tr = new(make_deps(), &dirs, &hosts, true);
        let snap_all = tr.snapshot(&ctx).unwrap();
        out.push((
            "snap_all_compact".into(),
            serde_json::to_string(&snap_all).unwrap(),
        ));
        out.push((
            "snap_all_pretty".into(),
            serde_json::to_string_pretty(&snap_all).unwrap(),
        ));

        let mut resumed = new(make_deps(), &dirs, &hosts, true);
        assert!(resumed.resume(&ctx, 1).unwrap());
        for (i, line) in resumed.poll(&ctx).unwrap().iter().enumerate() {
            out.push((
                format!("resume_poll_{i}"),
                serde_json::to_string(line).unwrap(),
            ));
        }

        // Session change + discovered store + unrecorded change.
        sessions.borrow_mut().retain(|s| s != "agent-lead");
        sessions.borrow_mut().push("agent-disc".to_string());
        for (i, line) in tr.poll_sessions(&ctx).iter().enumerate() {
            out.push((
                format!("poll_sessions_{i}"),
                serde_json::to_string(line).unwrap(),
            ));
        }

        // Remote host change.
        *remote_status.borrow_mut() = "idle".to_string();
        for (i, line) in tr.poll_hosts(&ctx).iter().enumerate() {
            out.push((
                format!("poll_hosts_{i}"),
                serde_json::to_string(line).unwrap(),
            ));
        }
        out
    };

    let rust_out = run_rust();

    // Run identical Go scenario via `go test -overlay` with temp files in `/var/tmp`
    // so the Go working tree stays untouched.
    let go_helper_path = work.path().join("rust_parity_helper_test.go");
    let go_out_path = work.path().join("go_out.jsonl");
    let go_test_src = r#"package export

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"os"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

func encodeCompact(t *testing.T, v any) string {
	t.Helper()
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		t.Fatal(err)
	}
	return string(bytes.TrimSuffix(buf.Bytes(), []byte("\n")))
}

func encodePretty(t *testing.T, v any) string {
	t.Helper()
	var buf bytes.Buffer
	enc := json.NewEncoder(&buf)
	enc.SetIndent("", "  ")
	enc.SetEscapeHTML(false)
	if err := enc.Encode(v); err != nil {
		t.Fatal(err)
	}
	return string(bytes.TrimSuffix(buf.Bytes(), []byte("\n")))
}

func TestRustByteForByteParityHelper(t *testing.T) {
	outFile := os.Getenv("GOMUXAI_PARITY_OUT")
	if outFile == "" {
		return
	}
	ownDir := os.Getenv("GOMUXAI_PARITY_OWN")
	otherDir := os.Getenv("GOMUXAI_PARITY_OTHER")
	oldDir := os.Getenv("GOMUXAI_PARITY_OLD")

	sessions := []string{
		"agent-lead",
		"agent-lead-w1",
		"user-pane",
		"agent-leg-live",
		"agent-stray",
		"agent-untracked",
	}
	pathOf := map[string]string{
		"agent-disc":      otherDir,
		"agent-untracked": ownDir,
	}
	remoteStatus := "ended"
	tNow := time.Date(2026, 11, 1, 12, 0, 0, 125_000_000, time.UTC)
	created := time.Date(2026, 11, 1, 8, 0, 0, 0, time.UTC)

	makeDeps := func() Deps {
		return Deps{
			Host:          "box",
			SessionPrefix: "agent-",
			Sessions:      func() ([]string, error) { return append([]string(nil), sessions...), nil },
			SessionStore:  func(s string) string { return pathOf[s] },
			ListHost: func(_ context.Context, host string) ([]natsbus.AgentInfo, error) {
				switch host {
				case "old":
					return []natsbus.AgentInfo{{
						Name: "far", Backend: "claude", Session: "agent-far",
						Status: "ended", CreatedAt: created,
					}}, nil
				case "new":
					no := false
					res := remoteStatus == "ended"
					er := ""
					if res {
						er = "gone"
					}
					return []natsbus.AgentInfo{{
						Name: "far", Backend: "claude", Session: "agent-far",
						SpawnedBy: "lead", Status: remoteStatus, EndReason: er,
						Detail: "remote <detail> & status", SessionID: "5f0c",
						Cwd: "/srv/w", Attached: &no, Resumable: &res, CreatedAt: created,
					}}, nil
				default:
					return nil, errors.New("no bus")
				}
			},
			Now: func() time.Time { return tNow },
		}
	}

	ctx := context.Background()
	dirs := []string{ownDir, oldDir}
	hosts := []string{"down", "new", "old"}

	type pair struct {
		Name string `json:"name"`
		JSON string `json:"json"`
	}
	var results []pair

	trLive := New(makeDeps(), dirs, hosts, false)
	defer trLive.Close()
	snapLive, err := trLive.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	results = append(results,
		pair{"snap_live_compact", encodeCompact(t, snapLive)},
		pair{"snap_live_pretty", encodePretty(t, snapLive)},
	)

	trAll := New(makeDeps(), dirs, hosts, true)
	defer trAll.Close()
	snapAll, err := trAll.Snapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	results = append(results,
		pair{"snap_all_compact", encodeCompact(t, snapAll)},
		pair{"snap_all_pretty", encodePretty(t, snapAll)},
	)

	resumed := New(makeDeps(), dirs, hosts, true)
	defer resumed.Close()
	ok, err := resumed.Resume(ctx, 1)
	if err != nil || !ok {
		t.Fatalf("Resume(1) = %v, %v", ok, err)
	}
	rLines, err := resumed.Poll(ctx)
	if err != nil {
		t.Fatal(err)
	}
	for i, l := range rLines {
		results = append(results, pair{Name: "resume_poll_" + itoa(i), JSON: encodeCompact(t, l)})
	}

	sessions = []string{
		"agent-lead-w1",
		"user-pane",
		"agent-leg-live",
		"agent-stray",
		"agent-untracked",
		"agent-disc",
	}
	for i, l := range trAll.PollSessions(ctx) {
		results = append(results, pair{Name: "poll_sessions_" + itoa(i), JSON: encodeCompact(t, l)})
	}

	remoteStatus = "idle"
	for i, l := range trAll.PollHosts(ctx) {
		results = append(results, pair{Name: "poll_hosts_" + itoa(i), JSON: encodeCompact(t, l)})
	}

	raw, err := json.Marshal(results)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(outFile, raw, 0o600); err != nil {
		t.Fatal(err)
	}
}

func itoa(n int) string {
	if n == 0 {
		return "0"
	}
	var digits []byte
	for n > 0 {
		digits = append([]byte{byte('0' + n%10)}, digits...)
		n /= 10
	}
	return string(digits)
}
"#;
    fs::write(&go_helper_path, go_test_src).unwrap();

    let virtual_go_path = go_module.join("internal/export/rust_parity_helper_test.go");
    let overlay_path = work.path().join("overlay.json");
    let overlay_json = serde_json::json!({
        "Replace": {
            virtual_go_path.to_str().unwrap(): go_helper_path.to_str().unwrap()
        }
    });
    fs::write(&overlay_path, serde_json::to_vec(&overlay_json).unwrap()).unwrap();

    let status = std::process::Command::new("go")
        .current_dir(&go_module)
        .env("GOTMPDIR", base)
        .env("GOMUXAI_PARITY_OUT", &go_out_path)
        .env("GOMUXAI_PARITY_OWN", &own_dir)
        .env("GOMUXAI_PARITY_OTHER", &other_dir)
        .env("GOMUXAI_PARITY_OLD", &old_dir)
        .args([
            "test",
            "-overlay",
            overlay_path.to_str().unwrap(),
            "-run",
            "^TestRustByteForByteParityHelper$",
            "-count=1",
            "./internal/export/",
        ])
        .output()
        .unwrap();
    assert!(
        status.status.success(),
        "go parity helper failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&status.stdout),
        String::from_utf8_lossy(&status.stderr)
    );

    #[derive(serde::Deserialize)]
    struct GoPair {
        name: String,
        json: String,
    }
    let go_out: Vec<GoPair> = serde_json::from_slice(&fs::read(&go_out_path).unwrap()).unwrap();
    assert_eq!(
        rust_out.len(),
        go_out.len(),
        "item count mismatch: rust={} go={}",
        rust_out.len(),
        go_out.len()
    );
    for ((r_name, r_json), g) in rust_out.iter().zip(&go_out) {
        assert_eq!(r_name, &g.name);
        assert_eq!(
            r_json, &g.json,
            "byte-for-byte JSON mismatch on {r_name}:\nrust: {r_json}\ngo:   {}",
            g.json
        );
    }
}
