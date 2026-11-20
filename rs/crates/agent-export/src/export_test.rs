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
