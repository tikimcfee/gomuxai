//! export's cases: exportHostList in remote_test.go's
//! TestHostNamesMatchIgnoringCase, termd_attach_test.go's
//! TestTermdLogsAndExportDeps, and the command's own output.
use std::sync::mpsc;

use agent_bus::CancelFn;
use agent_service::{AGENT_STATUS_IDLE, AgentService, SendOptions, SpawnOptions, StatusReport};
use agent_store::Store as AgentStore;

use super::*;
use crate::cli::logs::logs_test::{run_words, setup_logs, temp_path};
use crate::cli::testenv::{TestEnv, temp_dir};

/// Makes this host's bus trust a peer named host (Go: trustPeers).
fn trust_peer(env: &TestEnv, host: &str) {
    let dir = temp_dir("agent-export-peer-");
    let id = agent_bus::init_identity(dir.path(), host).unwrap();
    agent_bus::trust_peer(env.cfg.bus_dir(), id.public_bundle("")).unwrap();
}

/// --hosts names hosts as the bus spells them, leaving out this host and
/// repeats (Go: TestHostNamesMatchIgnoringCase's exportHostList).
#[test]
fn test_export_host_list() {
    let env = TestEnv::new();
    std::fs::create_dir_all(env.cfg.bus_dir()).unwrap();
    let err = export_host_list(&env.ctx, "all").unwrap_err();
    assert!(format!("{err:#}").contains("this host trusts no peers"), "{err:#}");
    trust_peer(&env, "Mac-Mini");
    assert_eq!(export_host_list(&env.ctx, "mac-mini,LOCAL,Mac-Mini").unwrap(), ["Mac-Mini"]);
    assert_eq!(export_host_list(&env.ctx, "all").unwrap(), ["Mac-Mini"]);
    assert_eq!(export_host_list(&env.ctx, "zeta, alpha").unwrap(), ["alpha", "zeta"]);
    assert!(export_host_list(&env.ctx, "").unwrap().is_empty());
    let err = export_host_list(&env.ctx, "a b").unwrap_err();
    assert_eq!(format!("{err:#}"), "--hosts: invalid host name \"a b\": use letters, digits, '-' and '_'");
}

#[test]
fn test_data_dir_for() {
    let dir = temp_dir("agent-export-");
    let bin = temp_path(&dir, "bin");
    let data = format!("{bin}/.gomuxai");
    std::fs::create_dir_all(&data).unwrap();
    AgentStore::open(format!("{data}/gomuxai.db")).unwrap();
    std::fs::write(format!("{bin}/agent"), "").unwrap();
    let real = filepath::eval_symlinks(&data).unwrap();
    for p in [data.as_str(), bin.as_str(), &format!("{bin}/agent")] {
        assert_eq!(data_dir_for(p), real, "data_dir_for({p})");
    }
    assert_eq!(data_dir_for(""), "");
    assert_eq!(data_dir_for(&temp_path(&dir, "none")), "");
}

#[test]
fn test_byte_size_and_scope() {
    assert_eq!(byte_size(512), "512 B");
    assert_eq!(byte_size(1536), "1.5 KB");
    assert_eq!(byte_size(3 << 20), "3.0 MB");
    assert_eq!(follow_scope(&["a".into()], &[]), "this store and the stores its sessions point at");
    assert_eq!(
        follow_scope(&["a".into(), "b".into()], &["hub".into(), "mac".into()]),
        "2 stores and the stores its sessions point at, and hub, mac"
    );
}

/// export prints the tree for a person, and as JSON.
#[test]
fn test_export_snapshot() {
    crate::require_termd!();
    let mut env = setup_logs();
    let (res, out, _) = run_words(&mut env, &["export"]);
    res.unwrap();
    let data_dir = filepath::dir(&env.cfg.database_path);
    assert!(
        out.starts_with(&format!("Store {data_dir} (this binary's): events up to ")),
        "export:\n{out}"
    );
    assert!(out.contains("\nNAME                     STATUS      PARENT"), "export:\n{out}");
    assert!(
        out.contains("worker                   waiting     -                  rpc-cli-worker           2026-10-01 09:30:00 -\n"),
        "export:\n{out}"
    );

    let (res, out, _) = run_words(&mut env, &["export", "--json"]);
    res.unwrap();
    let snap: Snapshot = serde_json::from_str(&out).unwrap();
    assert_eq!(snap.r#type, "snapshot");
    assert_eq!(snap.agents.len(), 1, "{out}");
    assert_eq!(snap.agents[0].name, "worker");
    assert!(out.starts_with("{\n  \"type\": \"snapshot\",\n"), "indented JSON:\n{out}");

    let (res, _, _) = run_words(&mut env, &["export", "--since", "3"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("--since resumes a follow"));
}

/// export --json --follow prints the snapshot, then a line per change, until
/// it is stopped.
#[test]
fn test_export_follow() {
    crate::require_termd!();
    let env = setup_logs();
    let share = env.ctx.share().unwrap();
    let out = crate::cli::root::SharedBuf::default();
    let (bctx, cancel): (Context, CancelFn) = Context::with_cancel(&Context::background());
    let (tx, done) = mpsc::channel();
    let w = out.clone();
    let thread = std::thread::spawn(move || {
        let ctx = share.ctx(w.writer(), Box::new(std::io::sink()));
        let args = Args {
            all: false,
            follow: true,
            hosts: String::new(),
            json: true,
            since: 0,
            store: Vec::new(),
            args: Vec::new(),
        };
        let iv = ExportIntervals {
            poll: Duration::from_millis(10),
            session: Duration::from_millis(50),
            host: Duration::from_secs(3600),
        };
        let _ = tx.send(run_export(&ctx, &bctx, &args, iv));
    });
    let wait_for = |what: &str, f: &dyn Fn(&str) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let s = out.contents();
            if f(&s) {
                return s;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}:\n{s}");
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    wait_for("the snapshot", &|s| s.starts_with("{\"type\":\"snapshot\",") && s.ends_with('\n'));
    env.service()
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                ..Default::default()
            },
        )
        .unwrap();
    let s = wait_for("worker's change", &|s| {
        s.lines()
            .any(|l| l.contains(r#""type":"changed""#) && l.contains(r#""reason":"status""#) && l.contains(r#""status":"idle""#))
    });
    cancel.cancel();
    done.recv_timeout(Duration::from_secs(5)).expect("the follow ends").unwrap();
    let _ = thread.join();
    for line in s.lines() {
        serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| panic!("line {line}: {e}"));
    }
}

/// A follow line for a person.
#[test]
fn test_format_export_line() {
    crate::cli::testenv::isolate(); // TZ=UTC
    let at = agent_store::Time::date_utc(2026, 10, 1, 9, 30, 0, 250_000_000);
    let agent = ExportAgent {
        name: "worker".into(),
        host: "local".into(),
        backend: "claude".into(),
        cwd: "/srv/x".into(),
        status: "waiting".into(),
        event: "PermissionRequest".into(),
        detail: "permission: Bash:\n ls  -la".into(),
        session: "agent-worker".into(),
        attached: Some(true),
        ..Default::default()
    };
    let line = |typ: &str, reason: &str| {
        Line::from(agent_export::AgentLine {
            r#type: typ.into(),
            at,
            reason: reason.into(),
            agent: agent.clone(),
            ..Default::default()
        })
    };
    assert_eq!(
        format_export_line(&line(LINE_SPAWNED, "")),
        "09:30:00.250 spawned worker@local (claude, by a person, in /srv/x)"
    );
    assert_eq!(
        format_export_line(&line(LINE_REMOVED, "")),
        "09:30:00.250 removed worker@local (last status waiting)"
    );
    assert_eq!(
        format_export_line(&line(agent_export::LINE_CHANGED, REASON_SESSION)),
        "09:30:00.250 worker@local: session agent-worker back"
    );
    assert_eq!(
        format_export_line(&line(agent_export::LINE_CHANGED, REASON_STATUS)),
        "09:30:00.250 worker@local waiting event=PermissionRequest detail=\"permission: Bash: ls -la\""
    );
    assert_eq!(
        format_export_line(&line(agent_export::LINE_CHANGED, agent_export::REASON_TRANSCRIPT)),
        ""
    );
}

/// Go: TestTermdLogsAndExportDeps: logs --terminal reads a live agent's
/// screen (no log file), export_deps lists termd's sessions, carries the
/// configured namespace, and finds a session's store from its PATH; after
/// cleanup, logs --terminal reads the saved scrollback.
#[test]
fn test_termd_logs_and_export_deps() {
    crate::require_termd!();
    let mut env = setup_logs();

    // Another store, whose binary goes first on the session's PATH.
    let other = temp_dir("agent-export-other-");
    let other_bin = temp_path(&other, "bin");
    let other_data = format!("{other_bin}/.gomuxai");
    std::fs::create_dir_all(&other_data).unwrap();
    AgentStore::open(format!("{other_data}/gomuxai.db")).unwrap();

    let mut cfg = env.cfg.clone();
    cfg.agent_binary = format!("{other_bin}/agent");
    let svc = Arc::new(AgentService::new(
        cfg,
        Arc::clone(env.ctx.store().unwrap()),
        Arc::clone(env.ctx.termd().unwrap()),
        Arc::clone(env.ctx.backends()),
    ));
    env.ctx.set_service(Arc::clone(&svc));
    let bctx = Context::background();
    let a = svc
        .spawn(
            &bctx,
            SpawnOptions {
                name: "live-log".into(),
                backend: "shell".into(),
                ..Default::default()
            },
        )
        .unwrap();
    svc.send(&bctx, "live-log", "echo cli-log-$((40+2))", SendOptions::default())
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let s = svc.capture("live-log", 20, false).unwrap_or_default();
        if s.contains("cli-log-42") {
            break;
        }
        assert!(Instant::now() < deadline, "cli-log-42 never showed:\n{s}");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Live logs --terminal reads the termd scrollback, with no log file.
    let (res, out, _) = run_words(&mut env, &["logs", "live-log", "--terminal"]);
    assert!(res.is_ok() && out.contains("cli-log-42"), "live logs --terminal = {out:?}, {res:?}");

    let mut deps = export_deps(&env.ctx);
    assert_eq!(deps.namespace, env.cfg.termd_namespace, "export_deps' namespace");
    let names = (deps.sessions.as_mut().unwrap())().unwrap();
    assert!(names.contains(&a.session_name), "export_deps' sessions = {names:?}; want {}", a.session_name);
    let real_other = filepath::eval_symlinks(&other_data).unwrap();
    assert_eq!(
        (deps.session_store.as_mut().unwrap())(&a.session_name),
        real_other,
        "export_deps' session_store({})",
        a.session_name
    );

    // After cleanup, logs --terminal still reads the saved scrollback.
    svc.cleanup("live-log").unwrap();
    let (res, out, _) = run_words(&mut env, &["logs", "live-log", "--terminal"]);
    assert!(res.is_ok() && out.contains("cli-log-42"), "post-cleanup logs --terminal = {out:?}, {res:?}");
}
