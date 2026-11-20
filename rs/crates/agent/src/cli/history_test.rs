//! The command log and `agent history` (Go: internal/cli/history_test.go).

use std::time::SystemTime;

use agent_store::{CommandFilter, CommandRecord, Time};
use chrono::{Local, TimeZone};

use super::exit::ExitError;
use super::history::{self, format_history, record_command};
use super::root::{Ctx, SharedBuf, execute_ctx};
use super::testenv::{TestEnv, temp_dir, test_config};

fn commands(env: &TestEnv) -> Vec<CommandRecord> {
    env.store()
        .list_commands(&CommandFilter::default())
        .unwrap()
}

fn words(s: &[&str]) -> Vec<String> {
    s.iter().map(|w| w.to_string()).collect()
}

#[test]
fn record_command_test() {
    let mut env = TestEnv::new();
    let mut vars = agent_termd::testing::EnvGuard::lock();
    vars.set("AGENT_NAME", "lead");
    let long = "x".repeat(400);
    let run_err = anyhow::Error::new(ExitError::new(2, anyhow::anyhow!("timed out\nmore")));
    env.ctx.path = "send".to_string();
    record_command(
        &env.ctx,
        &words(&["send", "helper", &long]),
        SystemTime::now(),
        Some(&run_err),
    );
    drop(vars);

    let recs = commands(&env);
    assert_eq!(recs.len(), 1, "recorded {} commands, want 1", recs.len());
    let r = &recs[0];
    assert!(
        r.agent == "lead"
            && r.host == "local"
            && r.exit_code == 2
            && r.error == "timed out"
            && !r.build.is_empty()
            && !r.cwd.is_empty(),
        "recorded {r:?}"
    );
    let want = format!("{}…[+100 bytes]", "x".repeat(300));
    assert!(
        r.args.len() == 3 && r.args[2] == want,
        "args = {:?}, want the long one capped",
        r.args
    );
}

#[test]
fn record_command_skips_hooks_and_the_bus_daemon() {
    let env = TestEnv::new();
    // Through whole lines, so report's --hook is parsed as the run sees it.
    // None of these commands is ported yet: each fails, and is recorded or not.
    for line in [
        &["report", "--hook", "--as", "x"][..],
        &["bus", "run"],
        &["hook"],
        &["report", "idle"],
    ] {
        let mut ctx = Ctx::new(SharedBuf::default().writer(), SharedBuf::default().writer());
        ctx.base_cfg = Some(env.cfg.clone());
        let _ = execute_ctx(&mut ctx, &words(line));
    }
    let recs = commands(&env);
    assert!(
        recs.len() == 1 && recs[0].args == ["report", "idle"],
        "recorded {recs:?}, want only the plain report"
    );
    assert!(!history::recorded("bus run", false));
    assert!(!history::recorded("report", true));
    assert!(history::recorded("report", false));
}

// Help, and commands that never asked for the store, never opened it.
#[test]
fn record_command_opens_the_store_itself() {
    let env = TestEnv::new();
    let mut ctx = Ctx::new(SharedBuf::default().writer(), SharedBuf::default().writer());
    ctx.base_cfg = Some(env.cfg.clone());
    execute_ctx(&mut ctx, &words(&["help", "wait"])).unwrap();
    let recs = commands(&env);
    assert!(
        recs.len() == 1 && recs[0].args == ["help", "wait"],
        "recorded {recs:?}, want help"
    );

    // A database that can't be opened: nothing recorded, nothing said.
    let missing = temp_dir("agent-cli-");
    let (out, err) = (SharedBuf::default(), SharedBuf::default());
    let mut ctx = Ctx::new(out.writer(), err.writer());
    ctx.base_cfg = Some(test_config(&missing.path().join("missing")));
    ctx.path = "list".to_string();
    record_command(&ctx, &words(&["list"]), SystemTime::now(), None);
    assert_eq!(out.contents() + &err.contents(), "");
    assert!(!missing.path().join("missing").exists());
}

// Setup opens the store for every command but help and version, as Go's
// does, even one that needs none (statuses): a fresh install's first
// commands then have a database to be recorded in (review 5, finding 5).
#[test]
fn setup_opens_the_store() {
    let dir = temp_dir("agent-cli-");
    let cfg = test_config(&dir.path().join("data"));
    let mut ctx = Ctx::new(SharedBuf::default().writer(), SharedBuf::default().writer());
    ctx.base_cfg = Some(cfg.clone());
    let cmd = super::root::command("statuses");
    ctx.setup(&cmd).unwrap();
    assert_eq!(ctx.cfg().host, "local");
    assert!(ctx.opened_store().is_some(), "setup opened no store");
    assert!(
        std::path::Path::new(&cfg.database_path).exists(),
        "setup created no database"
    );
}

// A fresh install's first commands go in the command log, whatever they
// are: hooks, statuses, one for another host that fails (Go records all
// three; the store used to be opened only by commands that read it).
#[test]
fn fresh_install_records_its_first_commands() {
    let dir = temp_dir("agent-cli-");
    let cfg = test_config(&dir.path().join("data"));
    assert!(!dir.path().join("data").exists());
    for line in [
        &["statuses"][..],
        &[
            "hooks",
            "status",
            "--settings",
            "/nonexistent/settings.json",
        ],
        &["status", "worker@hub"],
    ] {
        let mut ctx = Ctx::new(SharedBuf::default().writer(), SharedBuf::default().writer());
        ctx.base_cfg = Some(cfg.clone());
        ctx.set_bus_connect(|| Err(agent_bus::ErrNoBus.into()));
        let _ = execute_ctx(&mut ctx, &words(line));
    }
    let store = agent_store::Store::open(&cfg.database_path).expect("the data dir setup made");
    let recs = store.list_commands(&CommandFilter::default()).unwrap();
    // Newest first.
    let args: Vec<Vec<String>> = recs.iter().map(|r| r.args.clone()).collect();
    assert_eq!(
        args,
        vec![
            words(&["status", "worker@hub"]),
            words(&[
                "hooks",
                "status",
                "--settings",
                "/nonexistent/settings.json"
            ]),
            words(&["statuses"]),
        ],
        "recorded {recs:?}"
    );
    assert_eq!(recs[0].exit_code, 1, "status worker@hub failed: {recs:?}");
}

// A data dir under a file fails every command in setup, as Go's EnsureDirs
// does, the ones that read nothing there included (hooks status, a command
// for another host).
#[test]
fn a_bad_data_dir_fails_every_command() {
    let dir = temp_dir("agent-cli-");
    let file = dir.path().join("not_a_dir");
    std::fs::write(&file, "x").unwrap();
    let cfg = test_config(&file.join("data"));
    for line in [
        &[
            "hooks",
            "status",
            "--settings",
            "/nonexistent/settings.json",
        ][..],
        &["status", "w@hub"],
        &["statuses"],
    ] {
        let mut ctx = Ctx::new(SharedBuf::default().writer(), SharedBuf::default().writer());
        ctx.base_cfg = Some(cfg.clone());
        ctx.set_bus_connect(|| Err(agent_bus::ErrNoBus.into()));
        let err = format!(
            "{:#}",
            execute_ctx(&mut ctx, &words(line)).expect_err("bad data dir")
        );
        assert!(
            err.starts_with("creating directories: ") && err.ends_with("not a directory"),
            "{line:?}: err = {err}"
        );
    }
}

#[test]
fn wait_timeout_returns_exit_code() {
    crate::require_termd!();
    let env = super::testenv::TestEnv::setup_cli();
    env.no_bus();
    // Long enough for a first check on a loaded machine: the error names the
    // last status seen.
    let err = super::wait::run(
        &env.ctx,
        &super::wait::Args {
            any: false,
            follow: false,
            for_: "idle".to_string(),
            interval: std::time::Duration::from_millis(10),
            next: false,
            timeout: std::time::Duration::from_millis(500),
            args: words(&["worker"]),
        },
    )
    .unwrap_err();
    assert_eq!(
        super::exit::exit_code(&err),
        super::wait::WAIT_TIMEOUT_EXIT_CODE,
        "err = {err:#}"
    );
    assert_eq!(
        format!("{err:#}"),
        "timed out after 500ms waiting for worker to be idle (last status: waiting)"
    );
}

#[test]
fn format_history_test() {
    let at = Time(
        Local
            .with_ymd_and_hms(2026, 10, 24, 14, 3, 11)
            .unwrap()
            .fixed_offset(),
    );
    let rec = |agent: &str, args: &[&str], code: i64, error: &str| CommandRecord {
        started_at: at,
        agent: agent.to_string(),
        args: words(args),
        exit_code: code,
        error: error.to_string(),
        ..Default::default()
    };
    let y = "y".repeat(200);
    let e = "e".repeat(200);
    let recs = [
        rec("", &["list"], 0, ""),
        rec("lead", &["send", "helper", "review auth.go"], 0, ""),
        rec(
            "lead",
            &["wait", "helper", "--for", "idel"],
            1,
            r#"invalid status "idel""#,
        ),
        rec("", &["send", "helper", &y], 1, &e),
    ];
    let want = format!(
        "10-24 14:03:11 -      0 list
10-24 14:03:11 lead   0 send helper 'review auth.go'
10-24 14:03:11 lead   1 wait helper --for idel  # invalid status \"idel\"
10-24 14:03:11 -      1 send helper {}…  # {}…
",
        "y".repeat(87),
        "e".repeat(79)
    );
    assert_eq!(format_history(&recs), want);
}

#[test]
fn history_lists_filters_and_prints_json() {
    let env = TestEnv::new();
    // A person's commands (the test may run under an agent).
    let mut vars = agent_termd::testing::EnvGuard::lock();
    vars.remove("AGENT_NAME");
    let run = |line: &[&str]| -> (String, Option<String>) {
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let mut ctx = Ctx::new(out.writer(), err.writer());
        ctx.base_cfg = Some(env.cfg.clone());
        let res = execute_ctx(&mut ctx, &words(line));
        (out.contents(), res.err().map(|e| format!("{e:#}")))
    };
    let (out, err) = run(&["history"]);
    assert_eq!(
        (out.as_str(), err),
        ("No commands recorded on host local yet.\n", None)
    );
    let (out, _) = run(&["history", "--failed"]);
    assert_eq!(
        out,
        "No recorded commands match (agent history without --agent or --failed lists them all).\n"
    );
    let (_, err) = run(&["history", "-n", "0"]);
    assert_eq!(err.as_deref(), Some("--lines must be at least 1"));
    let _ = run(&["task", "get", "nope"]);

    // Newest last: the runs above, each recorded after it ended.
    let (out, _) = run(&["history", "-n", "3"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].ends_with("  0 history --failed"), "{out}");
    assert!(
        lines[1].ends_with("  1 history -n 0  # --lines must be at least 1"),
        "{out}"
    );
    assert!(
        lines[2].contains("  1 task get nope  # no task \"nope\": there are no tasks yet"),
        "{out}"
    );
    let (out, _) = run(&["history", "--failed", "--agent", "-"]);
    assert_eq!(out.lines().count(), 2, "{out}");
    let (out, _) = run(&["history", "--agent", "lead"]);
    assert!(out.starts_with("No recorded commands match"), "{out}");

    // One line of JSON, newest last, as Go's json.Encoder writes it.
    let (out, _) = run(&["history", "--json", "-n", "2", "--failed"]);
    assert!(out.ends_with("]\n") && out.lines().count() == 1, "{out}");
    let recs: Vec<CommandRecord> = serde_json::from_str(&out).unwrap();
    assert_eq!(recs.len(), 2);
    assert_eq!(recs[1].args, ["task", "get", "nope"]);
    assert_eq!(recs[1].exit_code, 1);
}
