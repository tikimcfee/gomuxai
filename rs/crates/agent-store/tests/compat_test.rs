//! The same database, both ways: a store the Go binary wrote reads the same
//! from Rust, a store Rust wrote reads the same from Go, and both write the
//! same text into every column.
//!
//! The Go side is testdata/gocompat/main.go, built against a temp copy of the
//! legacy Go module (legacy/go.mod, go.sum and internal/storage, nothing else)
//! so the repo is untouched; it needs `go` and the module cache (no network:
//! GOPROXY=off). Without them, or without legacy/, the test says so and passes.
//!
//! `AGENT_STORE_COMPAT_DB=<copy of a real store> cargo test -p agent-store
//! --test compat_test -- --ignored` compares the two dumps of that database
//! (dump migrates it, so give it a copy).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::Command;

use agent_store::*;
use chrono::{FixedOffset, TimeDelta, TimeZone};
use rusqlite::Connection;
use rusqlite::types::ValueRef;

/// The Go module the Rust tool replaced (legacy/README.md).
fn legacy_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../legacy")
}

/// Builds the Go side; None (and a note) when it can't be built here.
fn build_go(dir: &Path) -> Option<PathBuf> {
    let root = legacy_root();
    let storage = root.join("internal/storage");
    if !storage.join("sqlite.go").is_file() {
        eprintln!("compat: skipping, no legacy/ Go source (internal/storage) in this checkout");
        return None;
    }
    if Command::new("go").arg("version").output().is_err() {
        eprintln!("compat: no go on PATH; skipping");
        return None;
    }
    let module = dir.join("module");
    let pkg = module.join("internal/storage");
    let cmd = module.join("cmd/gocompat");
    std::fs::create_dir_all(&pkg).unwrap();
    std::fs::create_dir_all(&cmd).unwrap();
    for f in ["go.mod", "go.sum"] {
        std::fs::copy(root.join(f), module.join(f)).unwrap();
    }
    for entry in std::fs::read_dir(&storage).unwrap() {
        let p = entry.unwrap().path();
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        if name.ends_with(".go") && !name.ends_with("_test.go") {
            std::fs::copy(&p, pkg.join(&name)).unwrap();
        }
    }
    std::fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/gocompat/main.go"),
        cmd.join("main.go"),
    )
    .unwrap();
    let bin = dir.join("gocompat");
    let out = Command::new("go")
        .args(["build", "-o"])
        .arg(&bin)
        .arg("./cmd/gocompat")
        .current_dir(&module)
        .env("GOFLAGS", "-mod=mod")
        .env("GOPROXY", "off")
        .env("GOTOOLCHAIN", "local")
        .env("GOWORK", "off")
        .output()
        .unwrap();
    if !out.status.success() {
        eprintln!(
            "compat: building the Go side failed; skipping:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return None;
    }
    Some(bin)
}

fn go(bin: &Path, mode: &str, db: &Path) -> String {
    let out = Command::new(bin).arg(mode).arg(db).output().unwrap();
    assert!(
        out.status.success(),
        "gocompat {mode}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// The scenario main.go's write runs, through the Rust store.
fn write(path: &Path) {
    let s = Store::open(path).unwrap();
    let loc = FixedOffset::west_opt(6 * 3600).unwrap();
    let t0 = Time(
        loc.with_ymd_and_hms(2026, 11, 1, 9, 30, 15).unwrap() + TimeDelta::nanoseconds(123_456_789),
    );
    let sec = |n: i64| t0 + TimeDelta::seconds(n);
    let state = |agent: &str, key: &str, value: &str, at: Time| StateEntry {
        agent_name: agent.into(),
        key: key.into(),
        value: value.into(),
        updated_at: at,
    };

    s.create_agent(&Agent {
        name: "lead".into(),
        session_name: "agent-lead".into(),
        backend: "claude".into(),
        task_file: "tasks/a.md".into(),
        log_file: "/logs/lead.log".into(),
        cwd: "/w".into(),
        created_at: t0,
        ..Default::default()
    })
    .unwrap();
    s.create_agent(&Agent {
        name: "lead-t1".into(),
        session_name: "agent-lead-t1".into(),
        backend: "agy".into(),
        spawned_by: "lead@hub".into(),
        cwd: "/w/sub".into(),
        adopted: true,
        created_at: sec(1),
        ..Default::default()
    })
    .unwrap();
    s.create_agent(&Agent {
        name: "gone".into(),
        session_name: "agent-gone".into(),
        backend: "claude".into(),
        created_at: sec(2),
        ..Default::default()
    })
    .unwrap();
    s.set_states(&[
        state("lead", "_status", "idle", sec(3)),
        state("lead", "notes", "a && <b>", sec(3)),
    ])
    .unwrap();
    s.record_report(
        &[
            state("lead-t1", "_status", "processing", sec(4)),
            state("lead-t1", "_transcript", "/t/x.jsonl", sec(4)),
        ],
        "lead-t1",
        &[],
        Some(&AgentEvent {
            at: sec(4),
            kind: AGENT_STATUS.into(),
            agent: "lead-t1".into(),
            status: "processing".into(),
            event: "PreToolUse".into(),
            detail: "d".into(),
            tool: "Bash".into(),
            tool_input: r#"{"command":"ls"}"#.into(),
            session_id: "sid".into(),
            transcript: "/t/x.jsonl".into(),
            ..Default::default()
        }),
    )
    .unwrap();
    s.update_states(
        &[
            state("gone", "_status", "ended", sec(5)),
            state("gone", "_end_reason", "quit", sec(5)),
        ],
        "gone",
        &["notes".to_string()],
    )
    .unwrap();
    s.delete_agent("gone").unwrap();
    s.replace_agent(&Agent {
        name: "lead".into(),
        session_name: "agent-lead".into(),
        backend: "claude".into(),
        task_file: "tasks/a.md".into(),
        log_file: "/logs/lead.log".into(),
        cwd: "/w".into(),
        created_at: sec(6),
        ..Default::default()
    })
    .unwrap();
    s.update_agent_backend("lead-t1", "claude").unwrap();

    for (from, to, content, status, at) in [
        ("lead", "lead-t1", "hello", MESSAGE_STATUS_PENDING, t0),
        ("x", "lead-t1", "second", MESSAGE_STATUS_PENDING, t0),
        ("x", "lead", "long text", MESSAGE_STATUS_HANDOVER, sec(1)),
    ] {
        s.create_message(&mut Message {
            sender_id: from.into(),
            receiver_id: to.into(),
            content: content.into(),
            status,
            created_at: at,
            ..Default::default()
        })
        .unwrap();
    }
    s.claim_message(1).unwrap();

    s.set_task(&Task {
        name: "review".into(),
        content: "# Review\n".into(),
        source_file: "tasks/review.md".into(),
        created_at: t0,
    })
    .unwrap();

    s.log_command(&mut CommandRecord {
        started_at: t0,
        duration_ms: 12,
        host: "hub".into(),
        agent: "lead-t1".into(),
        cwd: "/w".into(),
        args: vec!["send".into(), "lead".into(), "a && b".into()],
        exit_code: 1,
        error: "boom".into(),
        build: "abc".into(),
        ..Default::default()
    })
    .unwrap();
    s.log_command(&mut CommandRecord {
        started_at: sec(1),
        host: "hub".into(),
        build: "abc".into(),
        ..Default::default()
    })
    .unwrap();

    let cmd = AttachedCommand {
        args: vec!["wait".into(), "x".into()],
        exit_code: 2,
        error: "timed out".into(),
        at: t0,
    };
    let mut f1 = Friction {
        uid: "hub-1".into(),
        host: "hub".into(),
        filed_by: "lead".into(),
        parent: "boss".into(),
        cwd: "/w".into(),
        build: "abc".into(),
        created_at: t0,
        updated_at: t0,
        kind: "bug".into(),
        summary: "s1".into(),
        detail: "d".into(),
        command: Some(cmd.clone()),
        status: FRICTION_OPEN.into(),
        ..Default::default()
    };
    let mut f2 = Friction {
        uid: "hub-2".into(),
        host: "hub".into(),
        filed_by: "user".into(),
        build: "abc".into(),
        created_at: sec(1),
        updated_at: sec(1),
        kind: "help".into(),
        summary: "s2".into(),
        status: FRICTION_OPEN.into(),
        ..Default::default()
    };
    s.add_friction(&mut f1).unwrap();
    s.add_friction(&mut f2).unwrap();
    s.add_friction_vote(&mut FrictionVote {
        friction_id: f1.id,
        uid: "v1".into(),
        host: "hub".into(),
        voter: "user".into(),
        build: "abc".into(),
        created_at: t0,
        note: "me too".into(),
        command: Some(cmd),
        ..Default::default()
    })
    .unwrap();
    s.add_friction_link(&mut FrictionLink {
        friction_id: f2.id,
        peer_host: "laptop".into(),
        peer_id: 5,
        linked_by: "user".into(),
        linked_at: t0,
        ..Default::default()
    })
    .unwrap();
    f2.status = FRICTION_DUPLICATE.into();
    f2.duplicate_of = f1.id;
    f2.resolved_by = "user".into();
    f2.resolved_at = t0 + TimeDelta::hours(1);
    f2.resolution = "dup".into();
    f2.updated_at = t0 + TimeDelta::hours(1);
    s.update_friction(&f2).unwrap();
    f1.status = FRICTION_ACCEPTED.into();
    f1.accepted_by = "lead".into();
    f1.accepted_at = t0 + TimeDelta::hours(2);
    f1.accept_note = "soon".into();
    f1.updated_at = t0 + TimeDelta::hours(2);
    s.update_friction(&f1).unwrap();
    s.add_friction_link(&mut FrictionLink {
        friction_id: f1.id,
        peer_host: "laptop".into(),
        peer_id: 34,
        peer_uid: "laptop-34".into(),
        linked_by: "lead".into(),
        linked_at: t0,
        note: "n".into(),
        confirmed_at: t0 + TimeDelta::minutes(1),
        peer_status: "open".into(),
        peer_kind: "bug".into(),
        peer_summary: "s".into(),
        peer_votes: 3,
        peer_checked_at: t0 + TimeDelta::minutes(1),
        ..Default::default()
    })
    .unwrap();
    s.add_friction_link(&mut FrictionLink {
        friction_id: f1.id,
        peer_host: "Mac".into(),
        peer_id: 7,
        linked_by: "lead".into(),
        linked_at: t0,
        ..Default::default()
    })
    .unwrap();
    s.rename_host("mac", "studio").unwrap();

    s.record_session_report(&SessionReport {
        session_id: "s1".into(),
        harness: "claude".into(),
        status: "idle".into(),
        event: "Stop".into(),
        transcript: "/t/s1.jsonl".into(),
        at: t0,
        ..Default::default()
    })
    .unwrap();
    s.record_session_report(&SessionReport {
        session_id: "s2".into(),
        harness: "claude".into(),
        status: "ended".into(),
        end_reason: "quit".into(),
        event: "SessionEnd".into(),
        at: sec(1),
        ..Default::default()
    })
    .unwrap();

    // The Rust build's own tables (RUST_ONLY_TABLES), with rows, so the Go is
    // shown opening a store that uses them.
    write_rust_only(&s, t0);
}

/// The tables only the Rust build creates. A Rust-written store has exactly
/// these beyond the Go's schema, and the Go opens and reads it all the same.
const RUST_ONLY_TABLES: &[&str] = &["host_state", "jobs", "scripts"];

fn rust_only_job(t0: Time) -> Job {
    Job {
        script: "build.sh".into(),
        path: "/w/build.sh".into(),
        sha256: "ab".repeat(32),
        content: "#!/bin/sh\nexit 3\n".into(),
        by: "lead".into(),
        host: "hub".into(),
        cwd: "/w".into(),
        args: vec!["x".into()],
        unit: "agent-job-0a1b2c3d-1.scope".into(),
        ran: "/d/.gomuxai/jobs/1/script".into(),
        memory_high: "24G".into(),
        memory_max: "28G".into(),
        started_at: t0,
        ..Default::default()
    }
}

fn write_rust_only(s: &Store, t0: Time) {
    let mut j = rust_only_job(t0);
    s.create_job(&mut j).unwrap();
    s.finish_job(j.id, &(t0 + TimeDelta::seconds(5)), Some(3))
        .unwrap();
    s.set_script(&PromotedScript {
        name: "nightly".into(),
        sha256: "cd".repeat(32),
        source: "/w/nightly.sh".into(),
        promoted_by: "user".into(),
        promoted_at: t0,
    })
    .unwrap();
    s.set_host_state("focus", "lead").unwrap();
}

/// The Rust-only rows read back as write_rust_only wrote them.
fn check_rust_only(path: &Path, t0: Time) {
    let s = Store::open(path).unwrap();
    let mut want = rust_only_job(t0);
    want.id = 1;
    want.ended_at = Some(t0 + TimeDelta::seconds(5));
    want.exit_code = Some(3);
    let got = s.get_job(1).unwrap().unwrap();
    assert!(
        got.ended_at.unwrap().equal(&want.ended_at.unwrap())
            && got.started_at.equal(&want.started_at)
    );
    assert_eq!(
        Job {
            started_at: want.started_at,
            ended_at: want.ended_at,
            ..got
        },
        want
    );
    assert_eq!(
        s.get_script("nightly").unwrap().unwrap().sha256,
        "cd".repeat(32)
    );
    assert_eq!(s.get_host_state("focus").unwrap().as_deref(), Some("lead"));
}

/// The table (and index) names in a store's schema, grouped by table.
fn table_names(path: &Path) -> BTreeSet<String> {
    let conn = Connection::open(path).unwrap();
    conn.prepare("SELECT tbl_name FROM sqlite_master")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn ts(t: &Time) -> String {
    t.utc().format_rfc3339_nano()
}

fn js<T: serde::Serialize + ?Sized>(v: &T) -> String {
    gojson::marshal(v).unwrap()
}

struct Dump(String);

impl Dump {
    fn line(&mut self, kind: &str, fields: &[&dyn Display]) {
        let parts: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
        self.0 += &format!("{kind} {}\n", parts.join("|"));
    }
    fn agent(&mut self, kind: &str, a: &Agent) {
        self.line(
            kind,
            &[
                &a.name,
                &a.session_name,
                &a.backend,
                &a.task_file,
                &a.log_file,
                &a.spawned_by,
                &a.cwd,
                &a.adopted,
                &ts(&a.created_at),
                &js(a),
            ],
        );
    }
    fn run(&mut self, kind: &str, r: &Run) {
        self.line(
            kind,
            &[
                &r.id,
                &r.name,
                &r.parent,
                &r.backend,
                &r.session,
                &r.cwd,
                &ts(&r.created_at),
                &ts(&r.ended_at),
                &r.status,
                &r.end_reason,
                &r.transcript,
            ],
        );
    }
    fn state(&mut self, kind: &str, e: &StateEntry) {
        self.line(
            kind,
            &[&e.agent_name, &e.key, &e.value, &ts(&e.updated_at), &js(e)],
        );
    }
}

/// What main.go's dump prints, from the Rust store.
fn dump(path: &Path) -> String {
    let s = Store::open(path).unwrap();
    let mut d = Dump(String::new());

    let agents = s.list_agents().unwrap();
    let mut receivers: BTreeSet<String> = BTreeSet::from(["x".to_string()]);
    for a in &agents {
        d.agent("agent", a);
        receivers.insert(a.name.clone());
        let got = s.get_agent(&a.name).unwrap().unwrap();
        d.agent("get", &got);
        for e in s.list_state(&a.name).unwrap() {
            d.state("state", &e);
        }
    }
    for r in &receivers {
        for m in s.get_pending_messages(r, 1000).unwrap() {
            d.line(
                "pending",
                &[
                    &m.id,
                    &m.sender_id,
                    &m.receiver_id,
                    &m.content,
                    &m.status,
                    &ts(&m.created_at),
                    &js(&m),
                ],
            );
        }
    }
    for id in 1..=20i64 {
        if let Ok(st) = s.get_message_status(id) {
            d.line("msgstatus", &[&id, &st]);
        }
    }
    for t in s.list_tasks().unwrap() {
        d.line(
            "task",
            &[
                &t.name,
                &js(&t.content),
                &t.source_file,
                &ts(&t.created_at),
                &js(&t),
            ],
        );
    }
    for c in s.list_commands(&CommandFilter::default()).unwrap() {
        d.line(
            "command",
            &[
                &c.id,
                &ts(&c.started_at),
                &c.duration_ms,
                &c.host,
                &c.agent,
                &c.parent,
                &c.cwd,
                &js(&c.args),
                &c.exit_code,
                &c.error,
                &c.build,
                &js(&c),
            ],
        );
    }
    for f in s.list_friction(&FrictionFilter::default()).unwrap() {
        let cmd = f.command.as_ref().map(js).unwrap_or_default();
        d.line(
            "friction",
            &[
                &f.id,
                &f.uid,
                &f.host,
                &f.filed_by,
                &f.parent,
                &f.cwd,
                &f.build,
                &ts(&f.created_at),
                &ts(&f.updated_at),
                &f.kind,
                &f.summary,
                &f.detail,
                &cmd,
                &f.status,
                &f.accepted_by,
                &ts(&f.accepted_at),
                &f.accept_note,
                &f.resolved_by,
                &ts(&f.resolved_at),
                &f.resolution,
                &f.duplicate_of,
                &f.votes,
                &js(&f),
            ],
        );
        for v in s.list_friction_votes(f.id).unwrap() {
            let cmd = v.command.as_ref().map(js).unwrap_or_default();
            d.line(
                "vote",
                &[
                    &v.id,
                    &v.friction_id,
                    &v.uid,
                    &v.host,
                    &v.voter,
                    &v.parent,
                    &v.build,
                    &ts(&v.created_at),
                    &v.note,
                    &cmd,
                    &js(&v),
                ],
            );
        }
    }
    d.line("lastfriction", &[&s.last_friction_id().unwrap()]);
    for l in s.list_friction_links(0).unwrap() {
        d.line(
            "link",
            &[
                &l.id,
                &l.friction_id,
                &l.peer_host,
                &l.peer_id,
                &l.peer_uid,
                &l.linked_by,
                &ts(&l.linked_at),
                &l.note,
                &ts(&l.confirmed_at),
                &l.peer_status,
                &l.peer_kind,
                &l.peer_summary,
                &l.peer_votes,
                &l.peer_resolved_by,
                &l.peer_resolution,
                &ts(&l.peer_checked_at),
                &js(&l),
            ],
        );
    }
    let reports: BTreeMap<String, SessionReport> =
        s.session_reports().unwrap().into_iter().collect();
    for r in reports.values() {
        d.line(
            "session",
            &[
                &r.session_id,
                &r.harness,
                &r.status,
                &r.end_reason,
                &r.event,
                &r.detail,
                &r.transcript,
                &ts(&r.at),
            ],
        );
    }
    let tree = s.read_tree(true).unwrap();
    d.line("tree", &[&tree.cursor, &tree.events]);
    for a in &tree.agents {
        d.agent("tree.agent", a);
    }
    let runs: BTreeMap<&String, &Run> = tree.runs.iter().collect();
    for r in runs.values() {
        d.run("tree.run", r);
    }
    for e in &tree.state {
        d.state("tree.state", e);
    }
    for r in &tree.ended {
        d.run("tree.ended", r);
    }
    let (events, oldest) = s.agent_events_after(0, 1_000_000).unwrap();
    d.line("oldest", &[&oldest]);
    for ev in &events {
        d.line(
            "event",
            &[
                &ev.id,
                &ts(&ev.at),
                &ev.kind,
                &ev.agent,
                &ev.run,
                &ev.status,
                &ev.event,
                &ev.detail,
                &ev.tool,
                &js(&ev.tool_input),
                &ev.session_id,
                &ev.transcript,
                &ev.end_reason,
            ],
        );
        if ev.run != 0
            && let Some(r) = s.get_run(ev.run).unwrap()
        {
            d.run("run", &r);
        }
    }
    d.0
}

/// Every table's rows, as stored, and the schema's SQL, leaving out the
/// tables in skip (and their indexes). Values that depend on when the
/// scenario ran (a run's end, a removed event's time) read "NOW".
fn raw(path: &Path, skip: &[&str]) -> String {
    let conn = Connection::open(path).unwrap();
    let mut out = String::new();
    let tables: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_master ORDER BY type, name",
        )
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for (kind, name, table, sql) in &tables {
        if skip.contains(&table.as_str()) {
            continue;
        }
        out += &format!("{kind} {name}: {sql}\n");
        if kind != "table" {
            continue;
        }
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM \"{name}\" ORDER BY rowid"))
            .unwrap();
        let cols: Vec<String> = stmt.column_names().map(|c| c.to_string()).collect();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let mut vals = Vec::new();
            for (i, col) in cols.iter().enumerate() {
                let v = match row.get_ref(i).unwrap() {
                    ValueRef::Null => "NULL".to_string(),
                    ValueRef::Integer(n) => format!("i:{n}"),
                    ValueRef::Real(f) => format!("r:{f}"),
                    ValueRef::Text(t) => format!("t:{:?}", String::from_utf8_lossy(t)),
                    ValueRef::Blob(b) => format!("b:{b:?}"),
                };
                let now_dependent = name == "runs" && col == "ended_at" && v != "t:\"\"";
                vals.push((
                    col.clone(),
                    if now_dependent { "NOW".to_string() } else { v },
                ));
            }
            // agent_events' kind comes after at: look again once the row is read.
            if name == "agent_events"
                && vals
                    .iter()
                    .any(|(c, v)| c == "kind" && v == "t:\"removed\"")
            {
                for (c, v) in vals.iter_mut() {
                    if c == "at" {
                        *v = "NOW".to_string();
                    }
                }
            }
            // A skipped table's AUTOINCREMENT counter goes with it.
            if name == "sqlite_sequence"
                && vals
                    .iter()
                    .any(|(c, v)| c == "name" && skip.iter().any(|t| *v == format!("t:{t:?}")))
            {
                continue;
            }
            let row: Vec<String> = vals.into_iter().map(|(c, v)| format!("{c}={v}")).collect();
            out += &format!("  {}\n", row.join(" "));
        }
    }
    out
}

fn assert_same(what: &str, a: &str, b: &str) {
    if a == b {
        return;
    }
    let (al, bl): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    for i in 0..al.len().max(bl.len()) {
        let (x, y) = (
            al.get(i).copied().unwrap_or("<none>"),
            bl.get(i).copied().unwrap_or("<none>"),
        );
        if x != y {
            panic!("{what}: first difference at line {}:\n  {x}\n  {y}", i + 1);
        }
    }
    panic!("{what}: differ");
}

#[test]
fn go_and_rust_share_a_database() {
    let dir = common::temp_dir();
    let Some(bin) = build_go(dir.path()) else {
        return;
    };
    let a = dir.path().join("go-written.db");
    let b = dir.path().join("rust-written.db");

    // Go writes; Go and Rust read the same; Go still reads it after Rust
    // opened (and migrated) it.
    go(&bin, "write", &a);
    // As the Go left it, before Rust opens it.
    let a_go = dir.path().join("go-only.db");
    std::fs::copy(&a, &a_go).unwrap();
    let go_a = go(&bin, "dump", &a);
    let rust_a = dump(&a);
    assert_same("Go-written store, Go's dump vs Rust's", &go_a, &rust_a);
    assert_same(
        "Go-written store, Go's dump before and after Rust opened it",
        &go_a,
        &go(&bin, "dump", &a),
    );
    assert!(
        go_a.lines().count() > 40,
        "the dump is too short to mean much:\n{go_a}"
    );
    eprintln!(
        "compat: Go and Rust dumps agree, {} lines",
        go_a.lines().count()
    );

    // Rust writes; Rust and Go read the same.
    write(&b);
    let rust_b = dump(&b);
    assert_same(
        "Rust-written store, Rust's dump vs Go's",
        &rust_b,
        &go(&bin, "dump", &b),
    );

    // The two stores hold the same text in every column, under the same
    // schema, but for the Rust build's own tables: a Rust-written store has
    // exactly those beyond the Go's, and Rust opening a Go store adds exactly
    // those and changes nothing else.
    let raw_a = raw(&a_go, &[]);
    if std::env::var_os("AGENT_STORE_COMPAT_SHOW").is_some() {
        eprintln!("{go_a}\n{raw_a}");
    }
    assert_same(
        "Go-written vs Rust-written rows",
        &raw_a,
        &raw(&b, RUST_ONLY_TABLES),
    );
    assert_same(
        "Go-written store before and after Rust opened it",
        &raw_a,
        &raw(&a, RUST_ONLY_TABLES),
    );
    let go_tables = table_names(&a_go);
    let want: BTreeSet<String> = RUST_ONLY_TABLES.iter().map(|t| t.to_string()).collect();
    for (what, path) in [("Rust-written", &b), ("Go-written, opened by Rust", &a)] {
        let extra: BTreeSet<String> = table_names(path).difference(&go_tables).cloned().collect();
        assert_eq!(extra, want, "{what} store's tables beyond the Go's");
    }
    assert!(
        go_tables.is_subset(&table_names(&b)),
        "the Rust-written store lacks a Go table"
    );

    // The Go opened (and migrated) the Rust-written store above; the
    // Rust-only rows are still there, as written.
    let t0 = Time(
        FixedOffset::west_opt(6 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 11, 1, 9, 30, 15)
            .unwrap()
            + TimeDelta::nanoseconds(123_456_789),
    );
    check_rust_only(&b, t0);

    // A read-only open reads the same tree as a migrated one.
    let ro = open_read_only(&a).unwrap();
    let rw = Store::open(&a).unwrap();
    let (t1, t2) = (ro.read_tree(true).unwrap(), rw.read_tree(true).unwrap());
    assert_eq!(
        (t1.cursor, &t1.agents, &t1.state, &t1.ended),
        (t2.cursor, &t2.agents, &t2.state, &t2.ended)
    );
    assert_eq!(t1.runs, t2.runs);
}

#[test]
#[ignore = "needs AGENT_STORE_COMPAT_DB: a copy of a real store"]
fn go_and_rust_read_a_real_store_alike() {
    let Ok(db) = std::env::var("AGENT_STORE_COMPAT_DB") else {
        panic!("set AGENT_STORE_COMPAT_DB to a copy of a store");
    };
    let dir = common::temp_dir();
    let Some(bin) = build_go(dir.path()) else {
        panic!("can't build the Go side")
    };
    let db = PathBuf::from(db);
    let go_dump = go(&bin, "dump", &db);
    let rust_dump = dump(&db);
    eprintln!("compat: {} dump lines", go_dump.lines().count());
    assert_same("real store, Go's dump vs Rust's", &go_dump, &rust_dump);
}
