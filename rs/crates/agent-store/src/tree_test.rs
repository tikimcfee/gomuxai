use std::path::PathBuf;

use chrono::TimeDelta;
use rusqlite::Connection;

use crate::commands::COMMAND_LOG_SCHEMA;
use crate::*;

/// A temp dir under /var/tmp (/tmp is RAM on the machines this runs on).
pub(crate) fn temp_dir() -> tempfile::TempDir {
    let base = if std::path::Path::new("/var/tmp").is_dir() {
        PathBuf::from("/var/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix("agent-store-test-")
        .tempdir_in(base)
        .unwrap()
}

pub(crate) fn new_test_store() -> (Store, PathBuf, tempfile::TempDir) {
    let dir = temp_dir();
    let path = dir.path().join("gomuxai.db");
    let s = Store::open(&path).unwrap();
    (s, path, dir)
}

fn event_kinds(s: &Store, after: i64) -> Vec<String> {
    let (events, _) = s.agent_events_after(after, 100).unwrap();
    events
        .iter()
        .map(|ev| format!("{}:{}:{}", ev.kind, ev.agent, ev.status))
        .collect()
}

fn secs(n: i64) -> TimeDelta {
    TimeDelta::seconds(n)
}

#[test]
fn tree_spawn_report_cleanup() {
    let (s, _, _dir) = new_test_store();
    let created = Time::date_utc(2026, 10, 24, 14, 0, 0, 0);
    s.create_agent(&Agent {
        name: "lead".into(),
        session_name: "agent-lead".into(),
        backend: "claude".into(),
        cwd: "/w".into(),
        created_at: created,
        ..Default::default()
    })
    .unwrap();
    s.create_agent(&Agent {
        name: "lead-t1".into(),
        session_name: "agent-lead-t1".into(),
        backend: "agy".into(),
        spawned_by: "lead".into(),
        cwd: "/w/sub".into(),
        created_at: created + secs(1),
        ..Default::default()
    })
    .unwrap();
    let report = |name: &str, status: &str, transcript: &str| {
        let at = Time::now();
        let mut entries = vec![StateEntry {
            agent_name: name.into(),
            key: "_status".into(),
            value: status.into(),
            updated_at: at,
        }];
        if !transcript.is_empty() {
            entries.push(StateEntry {
                agent_name: name.into(),
                key: "_transcript".into(),
                value: transcript.into(),
                updated_at: at,
            });
        }
        let ev = AgentEvent {
            at,
            kind: AGENT_STATUS.into(),
            agent: name.into(),
            status: status.into(),
            transcript: transcript.into(),
            ..Default::default()
        };
        s.record_report(&entries, name, &[], Some(&ev)).unwrap();
    };
    report("lead-t1", "processing", "/t/one.jsonl");
    report("lead-t1", "idle", "");

    let tree = s.read_tree(false).unwrap();
    assert!(
        tree.agents.len() == 2 && tree.agents[0].name == "lead" && tree.agents[1].cwd == "/w/sub",
        "live agents = {:?}, want lead then lead-t1 (cwd /w/sub)",
        tree.agents
    );
    let r = tree.runs.get("lead-t1");
    assert!(
        matches!(r, Some(r) if r.parent == "lead" && r.backend == "agy" && r.ended_at.is_zero()),
        "lead-t1's run = {r:?}, want an open run with parent lead"
    );
    assert!(
        tree.cursor == 4 && tree.events,
        "cursor = {} (events {}), want 4: two spawns and two reports",
        tree.cursor,
        tree.events
    );

    s.delete_agent("lead-t1").unwrap();
    s.delete_agent("nobody")
        .expect("deleting an unknown agent: want no error and no event");
    let want = [
        "spawned:lead:",
        "spawned:lead-t1:",
        "status:lead-t1:processing",
        "status:lead-t1:idle",
        "removed:lead-t1:idle",
    ];
    assert_eq!(event_kinds(&s, 0), want);
    assert_eq!(event_kinds(&s, 3), want[3..]);

    let tree = s.read_tree(true).unwrap();
    assert!(
        tree.agents.len() == 1 && tree.ended.len() == 1,
        "after cleanup: {} live, {} ended, want 1 and 1",
        tree.agents.len(),
        tree.ended.len()
    );
    let r = &tree.ended[0];
    assert!(
        r.name == "lead-t1"
            && !r.ended_at.is_zero()
            && r.status == "idle"
            && r.transcript == "/t/one.jsonl"
            && r.cwd == "/w/sub",
        "ended run = {r:?}, want lead-t1, ended, idle, with its transcript and cwd"
    );
    assert_eq!(r.created_at, created + secs(1));
    let (events, _) = s.agent_events_after(4, 10).unwrap();
    assert!(
        events.len() == 1 && events[0].run == r.id,
        "removed event = {events:?}, want one naming run {}",
        r.id
    );
}

#[test]
fn tree_events_pruned() {
    let (mut s, _, _dir) = new_test_store();
    s.agent_events_keep = 3;
    s.agent_events_prune_every = 2;
    s.create_agent(&Agent {
        name: "a".into(),
        session_name: "agent-a".into(),
        backend: "claude".into(),
        created_at: Time::now(),
        ..Default::default()
    })
    .unwrap();
    for _ in 0..5 {
        let ev = AgentEvent {
            at: Time::now(),
            kind: AGENT_STATUS.into(),
            agent: "a".into(),
            status: "idle".into(),
            ..Default::default()
        };
        s.record_report(&[], "a", &[], Some(&ev)).unwrap();
    }
    let (_, oldest) = s.agent_events_after(0, 10).unwrap();
    assert_eq!(oldest, 4, "oldest kept, want 4 (event 6 pruned up to 6-3)");
}

/// A store from before runs and agent events (and agents.cwd): its live
/// agents still read, with cwd from the command log, and its departed agents
/// come from the logged spawns.
#[test]
fn tree_old_store_read_only() {
    let dir = temp_dir();
    let path = dir.path().join("gomuxai.db");
    let db = Connection::open(&path).unwrap();
    for stmt in [
        "CREATE TABLE agents (name TEXT PRIMARY KEY, session_name TEXT NOT NULL, backend TEXT NOT NULL, task_file TEXT, log_file TEXT, created_at TEXT NOT NULL, spawned_by TEXT)",
        "CREATE TABLE state (agent_name TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, updated_at TEXT NOT NULL, PRIMARY KEY (agent_name, key))",
        COMMAND_LOG_SCHEMA,
        "INSERT INTO agents VALUES ('live', 'agent-live', 'claude', '', '', '2026-10-24T14:00:00Z', 'boss')",
        "INSERT INTO state VALUES ('live', '_status', 'idle', '2026-10-24T14:01:00Z')",
        r#"INSERT INTO command_log (started_at, duration_ms, host, agent, parent, cwd, args, exit_code, error, build)
			VALUES ('2026-10-24T13:00:00.000Z', 1, 'h', 'boss', '', '/old', '["spawn","gone","--backend","agy"]', 0, '', 'b'),
			       ('2026-10-24T13:30:00.000Z', 1, 'h', 'boss', '', '/w', '["spawn","--backend","claude","live","task.md"]', 0, '', 'b'),
			       ('2026-10-24T13:40:00.000Z', 1, 'h', 'boss', '', '/w', '["spawn","failed"]', 1, 'no', 'b')"#,
    ] {
        db.execute_batch(stmt)
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    drop(db);

    let s = open_read_only(&path).unwrap();
    let tree = s.read_tree(true).unwrap();
    assert!(
        !tree.events && tree.cursor == 0,
        "events = {}, cursor {}; want no events in an old store",
        tree.events,
        tree.cursor
    );
    assert!(
        tree.agents.len() == 1 && tree.agents[0].spawned_by == "boss" && tree.agents[0].cwd == "/w",
        "live = {:?}, want live, parent boss, cwd /w from its logged spawn",
        tree.agents
    );
    assert_eq!(tree.state.len(), 1, "state entries");
    assert!(
        tree.ended.len() == 1
            && tree.ended[0].name == "gone"
            && tree.ended[0].backend == "agy"
            && tree.ended[0].cwd == "/old"
            && tree.ended[0].id == 0,
        "ended = {:?}, want only gone (agy, /old, no run id); the failed spawn isn't one",
        tree.ended
    );
    s.agent_events_after(0, 10)
        .expect("events of an old store: want none and no error");
    assert!(
        s.create_agent(&Agent {
            name: "x".into(),
            session_name: "agent-x".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .is_err(),
        "a read-only store took a write"
    );
}

#[test]
fn spawn_args() {
    for (raw, name, backend) in [
        (
            r#"["spawn","helper","--backend","claude"]"#,
            "helper",
            "claude",
        ),
        (
            r#"["spawn","--backend=agy","--task","review","helper"]"#,
            "helper",
            "agy",
        ),
        (
            r#"["spawn","--skip-permissions","helper","task.md"]"#,
            "helper",
            "",
        ),
        (r#"["list"]"#, "", ""),
        ("not json", "", ""),
    ] {
        assert_eq!(
            crate::tree::spawn_args(raw),
            (name.to_string(), backend.to_string()),
            "spawn_args({raw})"
        );
    }
}

/// A store from before end reasons: opening it renames "exited" to "ended"
/// once, with "gone" as the reason (nothing recorded how those ended), and
/// leaves later rows alone.
#[test]
fn migrate_exited_to_ended() {
    let dir = temp_dir();
    let path = dir.path().join("gomuxai.db");
    Store::open(&path).unwrap().close().unwrap();
    let db = Connection::open(&path).unwrap();
    for stmt in [
        // As the binary before end reasons left it.
        "ALTER TABLE agent_events DROP COLUMN end_reason",
        "ALTER TABLE runs DROP COLUMN end_reason",
        "INSERT INTO agents (name, session_name, backend, created_at) VALUES ('w', 'agent-w', 'claude', '2026-10-24T14:00:00Z')",
        "INSERT INTO state VALUES ('w', '_status', 'exited', '2026-10-24T14:01:00Z')",
        "INSERT INTO runs (name, parent, backend, session, cwd, created_at, ended_at, status) VALUES ('old', '', 'claude', 's', '/', 'x', 'y', 'exited')",
        "INSERT INTO agent_events (at, kind, agent, run, status) VALUES ('2026-10-24T14:01:00Z', 'status', 'w', 0, 'exited')",
    ] {
        db.execute_batch(stmt)
            .unwrap_or_else(|e| panic!("{stmt}: {e}"));
    }
    drop(db);

    let s = Store::open(&path).unwrap();
    let st = s.get_state("w", "_status").unwrap();
    let why = s.get_state("w", "_end_reason").unwrap();
    assert!(
        st.as_ref().is_some_and(|e| e.value == "ended")
            && why.as_ref().is_some_and(|e| e.value == "gone"),
        "state: {st:?} {why:?}"
    );
    let (run, ev) = {
        let conn = s.conn();
        let q = |sql: &str| -> (String, String) {
            conn.query_row(sql, [], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
        };
        (
            q("SELECT status, end_reason FROM runs WHERE name = 'old'"),
            q("SELECT status, end_reason FROM agent_events WHERE agent = 'w'"),
        )
    };
    assert_eq!(run, ("ended".to_string(), "gone".to_string()), "run");
    assert_eq!(ev, ("ended".to_string(), "gone".to_string()), "event");
    // Once: a later "ended" row with its own reason is not touched again.
    s.close().unwrap();
    let s2 = Store::open(&path).unwrap();
    let why = s2.get_state("w", "_end_reason").unwrap();
    assert!(
        why.as_ref().is_some_and(|e| e.value == "gone"),
        "reopen: {why:?}"
    );
}

/// A resume closes the agent's run with its last status and opens a new one
/// under the same name, keeping its state.
#[test]
fn tree_replace_agent() {
    let (s, _, _dir) = new_test_store();
    let created = Time::date_utc(2026, 10, 31, 9, 0, 0, 0);
    let lead = |at: Time| Agent {
        name: "lead".into(),
        session_name: "agent-lead".into(),
        backend: "agy".into(),
        spawned_by: "boss".into(),
        cwd: "/w".into(),
        created_at: at,
        ..Default::default()
    };
    s.create_agent(&lead(created)).unwrap();
    let at = Time::now();
    let entry = |key: &str, value: &str| StateEntry {
        agent_name: "lead".into(),
        key: key.into(),
        value: value.into(),
        updated_at: at,
    };
    s.set_states(&[
        entry("_status", "ended"),
        entry("_end_reason", "gone"),
        entry("notes", "keep me"),
    ])
    .unwrap();

    let resumed = created + TimeDelta::hours(1);
    s.replace_agent(&lead(resumed)).unwrap();

    let tree = s.read_tree(true).unwrap();
    assert!(
        tree.agents.len() == 1
            && tree.agents[0].created_at == resumed
            && tree.agents[0].spawned_by == "boss",
        "live agents = {:?}, want lead, resumed at {resumed:?}, still boss's",
        tree.agents
    );
    let r = tree.runs.get("lead");
    assert!(
        matches!(r, Some(r) if r.ended_at.is_zero() && r.created_at == resumed),
        "lead's live run = {r:?}, want an open one from the resume"
    );
    assert!(
        tree.ended.len() == 1
            && tree.ended[0].status == "ended"
            && tree.ended[0].end_reason == "gone"
            && tree.ended[0].created_at == created,
        "ended runs = {:?}, want the first run, ended (gone)",
        tree.ended
    );
    assert_eq!(
        event_kinds(&s, 0),
        ["spawned:lead:", "removed:lead:ended", "spawned:lead:"]
    );
    let e = s.get_state("lead", "notes").unwrap();
    assert!(
        e.is_some_and(|e| e.value == "keep me"),
        "notes after replace, want kept"
    );
}

/// agent revive's take-over: the old run closes with the status written
/// before it, the new run opens with the state written after it, in one
/// transaction; a failure anywhere leaves everything as it was (review 2.5).
#[test]
fn tree_replace_agent_with() {
    let (s, _, _dir) = new_test_store();
    let created = Time::date_utc(2026, 10, 31, 9, 0, 0, 0);
    let ag = |name: &str, at: Time| Agent {
        name: name.into(),
        session_name: format!("agent-{name}"),
        backend: "agy".into(),
        cwd: "/w".into(),
        created_at: at,
        ..Default::default()
    };
    s.create_agent(&ag("ag", created)).unwrap();
    let at = Time::now();
    let entry = |key: &str, value: &str| StateEntry {
        agent_name: "ag".into(),
        key: key.into(),
        value: value.into(),
        updated_at: at,
    };
    s.set_states(&[
        entry("_status", "idle"),
        entry("_result", "old reply"),
        entry("notes", "keep me"),
    ])
    .unwrap();
    let before = StateUpdate {
        entries: vec![entry("_status", "ended"), entry("_end_reason", "quit")],
        agent_name: "ag".into(),
        ..Default::default()
    };
    let after = StateUpdate {
        entries: vec![entry("_revived_cli", "42 7")],
        agent_name: "ag".into(),
        delete: vec!["_status".into(), "_end_reason".into(), "_result".into()],
        ..Default::default()
    };

    // A failure (here: no such agent to replace) after `before` rolls it back.
    let ghost = StateUpdate {
        agent_name: "ghost".into(),
        ..before.clone()
    };
    let err = s
        .replace_agent_with(&ag("ghost", created), &ghost, &after)
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "no agent ghost to replace");
    let status = s.get_state("ag", "_status").unwrap().map(|e| e.value);
    assert_eq!(
        status.as_deref(),
        Some("idle"),
        "a failed take-over changed ag"
    );

    let revived = created + TimeDelta::hours(1);
    s.replace_agent_with(&ag("ag", revived), &before, &after)
        .unwrap();
    let tree = s.read_tree(true).unwrap();
    assert!(
        tree.ended.len() == 1
            && tree.ended[0].status == "ended"
            && tree.ended[0].end_reason == "quit",
        "ended runs = {:?}, want the first run, ended (quit)",
        tree.ended
    );
    assert!(
        matches!(tree.runs.get("ag"), Some(r) if r.ended_at.is_zero() && r.created_at == revived)
    );
    let get = |k: &str| s.get_state("ag", k).unwrap().map(|e| e.value);
    assert_eq!(
        (
            get("_status"),
            get("_result"),
            get("_revived_cli"),
            get("notes")
        ),
        (None, None, Some("42 7".into()), Some("keep me".into()))
    );
}

#[test]
fn replace_agent_unknown() {
    let (s, _, _dir) = new_test_store();
    let err = s
        .replace_agent(&Agent {
            name: "ghost".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap_err();
    assert_eq!(format!("{err:#}"), "no agent ghost to replace");
}

/// The stored text of every timestamp is the Go's: RFC3339 in the time's own
/// offset for agents, messages and tasks, RFC3339Nano for state, UTC with
/// nanoseconds for the tree and session reports, UTC milliseconds for the
/// command log and friction.
#[test]
fn stored_time_formats() {
    let (s, _, _dir) = new_test_store();
    let west = chrono::FixedOffset::west_opt(6 * 3600).unwrap();
    let t = Time(
        Time::date_utc(2026, 10, 24, 14, 0, 5, 120_000_900)
            .0
            .with_timezone(&west),
    );
    s.create_agent(&Agent {
        name: "a".into(),
        session_name: "agent-a".into(),
        backend: "claude".into(),
        created_at: t,
        ..Default::default()
    })
    .unwrap();
    s.set_state(&StateEntry {
        agent_name: "a".into(),
        key: "k".into(),
        value: "v".into(),
        updated_at: t,
    })
    .unwrap();
    s.create_message(&mut Message {
        sender_id: "x".into(),
        receiver_id: "a".into(),
        content: "hi".into(),
        status: MESSAGE_STATUS_PENDING,
        created_at: t,
        ..Default::default()
    })
    .unwrap();
    s.log_command(&mut CommandRecord {
        started_at: t,
        args: vec!["list".into()],
        ..Default::default()
    })
    .unwrap();
    s.record_session_report(&SessionReport {
        session_id: "s".into(),
        harness: "claude".into(),
        status: "idle".into(),
        at: t,
        ..Default::default()
    })
    .unwrap();
    let conn = s.conn();
    let one = |sql: &str| -> String { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
    assert_eq!(
        one("SELECT created_at FROM agents"),
        "2026-10-24T08:00:05-06:00"
    );
    assert_eq!(
        one("SELECT updated_at FROM state"),
        "2026-10-24T08:00:05.1200009-06:00"
    );
    assert_eq!(
        one("SELECT created_at FROM messages"),
        "2026-10-24T08:00:05-06:00"
    );
    assert_eq!(
        one("SELECT created_at FROM runs"),
        "2026-10-24T14:00:05.1200009Z"
    );
    assert_eq!(
        one("SELECT at FROM agent_events"),
        "2026-10-24T14:00:05.1200009Z"
    );
    assert_eq!(
        one("SELECT started_at FROM command_log"),
        "2026-10-24T14:00:05.120Z"
    );
    assert_eq!(
        one("SELECT at FROM session_reports"),
        "2026-10-24T14:00:05.1200009Z"
    );
    let mode: String = conn
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
}

/// A store's connection has the Go DSN's pragmas: busy_timeout(5000),
/// foreign_keys(1), synchronous(NORMAL); and a read-only one busy_timeout.
#[test]
fn store_pragmas() {
    let (s, path, _dir) = new_test_store();
    let conn = s.conn();
    let p = |name: &str| -> i64 {
        conn.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))
            .unwrap()
    };
    assert_eq!(p("busy_timeout"), 5000);
    assert_eq!(p("foreign_keys"), 1);
    assert_eq!(p("synchronous"), 1, "synchronous NORMAL");
    drop(conn);
    let ro = open_read_only(&path).unwrap();
    let conn = ro.conn();
    let t: i64 = conn
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(t, 5000);
    drop(conn);
    // data_version moves when another connection commits.
    let before = ro.data_version().unwrap();
    s.set_task(&Task {
        name: "t".into(),
        content: "c".into(),
        created_at: Time::now(),
        ..Default::default()
    })
    .unwrap();
    assert_ne!(ro.data_version().unwrap(), before);
}

#[test]
fn open_read_only_not_a_store() {
    let dir = temp_dir();
    let path = dir.path().join("empty.db");
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE TABLE x (y)")
        .unwrap();
    let err = open_read_only(&path)
        .err()
        .expect("opened a database with no agents table");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "reading {}: no agents table: not an agent store",
            path.display()
        )
    );
}

/// Session reports keep the newest report per session, and an empty
/// transcript leaves the stored one.
#[test]
fn session_reports() {
    let (s, _, _dir) = new_test_store();
    let t0 = Time::date_utc(2026, 11, 1, 10, 0, 0, 5);
    let report = |status: &str, transcript: &str, at: Time| SessionReport {
        session_id: "abc".into(),
        harness: "claude".into(),
        status: status.into(),
        transcript: transcript.into(),
        at,
        ..Default::default()
    };
    s.record_session_report(&report("processing", "/t/abc.jsonl", t0))
        .unwrap();
    s.record_session_report(&report("idle", "", t0 + secs(1)))
        .unwrap();
    s.record_session_report(&report("stale", "/t/old.jsonl", t0))
        .unwrap();
    let all = s.session_reports().unwrap();
    let r = &all["abc"];
    assert_eq!(
        (r.status.as_str(), r.transcript.as_str()),
        ("idle", "/t/abc.jsonl")
    );
    assert_eq!(r.at, t0 + secs(1));
}
