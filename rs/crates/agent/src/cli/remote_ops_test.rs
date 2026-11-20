//! Go: internal/cli/remote_ops_test.go. The Go's fakeClaude records the
//! session a resume continues; here the agent's launch.sh says it
//! (resume_all_test::launched_resume), and the claude CLI is testenv's
//! stand-in.

use agent_backend::EndReason;
use agent_bus::{Context, ResumeRequest};
use agent_service::{AGENT_STATUS_ENDED, AGENT_STATUS_PROCESSING, StatusReport};
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, Time};

use super::logs::logs_test::{TEST_TRANSCRIPT, hook_transcript, setup_logs, write_test_file};
use super::remote_ops::{LogsChunk, fit_logs};
use super::resume;
use super::resume_all_test::launched_resume;
use super::spawn_continue_test::{claude_transcript, setup_continue};
use super::testenv::{TestEnv, parse_args, temp_dir};

fn chunk(text: &str, session: bool) -> LogsChunk {
    LogsChunk {
        text: text.into(),
        session,
    }
}

// agent logs name@host prints what agent logs prints on that host, with the
// same flags, and its hint.
#[test]
fn test_remote_logs_match_local() {
    crate::require_termd!();
    let mut env = setup_logs();
    crate::require_bus!(env.serve_as_h1());
    let dir = temp_dir("agent-cli-logs-");
    let path = dir
        .path()
        .join("sess-1.jsonl")
        .to_string_lossy()
        .into_owned();
    write_test_file(&path, TEST_TRANSCRIPT);
    hook_transcript(&env, &path);

    for flags in [
        vec!["--lines", "50"],
        vec!["--lines", "1", "--full"],
        vec!["--lines", "50", "--json"],
        vec!["--lines", "2", "--terminal"],
    ] {
        let mut local_line = vec!["logs", "worker"];
        local_line.extend(flags.iter().copied());
        env.take_err();
        let (local, res) = env.run(&local_line);
        res.unwrap_or_else(|e| panic!("{flags:?}: local: {e:#}"));
        let local_hints = env.take_err();
        let mut remote_line = vec!["logs", "worker@h1"];
        remote_line.extend(flags.iter().copied());
        let (remote, res) = env.run(&remote_line);
        res.unwrap_or_else(|e| panic!("{flags:?}: remote: {e:#}"));
        let remote_hints = env.take_err();
        assert_eq!(
            (remote, remote_hints),
            (local, local_hints),
            "{flags:?} differs"
        );
    }
}

// With no transcript, logs falls back to the terminal log and says why, on
// either host.
#[test]
fn test_remote_logs_hint() {
    crate::require_termd!();
    let mut env = setup_logs();
    crate::require_bus!(env.serve_as_h1());
    write_test_file(&env.cfg.log_path("worker"), "line one\r\nline two\r\n");

    let (out, res) = env.run(&["logs", "worker@h1"]);
    res.unwrap_or_else(|e| panic!("{e:#}"));
    let hints = env.take_err();
    assert!(
        out == "line one\nline two\n"
            && hints.contains("printing worker's terminal log: no transcript recorded for it"),
        "remote logs = {out:?}, hint {hints:?}"
    );
}

// A log too big for a bus message keeps its newest lines and says how much
// was left out.
#[test]
fn test_remote_logs_truncated() {
    crate::require_termd!();
    let mut env = setup_logs();
    crate::require_bus!(env.serve_as_h1());
    let mut b = String::new();
    for i in 0..40_000 {
        b += &format!("terminal line {i:06} {}\r\n", "x".repeat(20));
    }
    write_test_file(&env.cfg.log_path("worker"), &b);

    let (out, res) = env.run(&["logs", "worker@h1", "--terminal", "--lines", "40000"]);
    res.unwrap_or_else(|e| panic!("{e:#}"));
    let tail = format!("terminal line 039999 {}\n", "x".repeat(20));
    assert!(
        out.len() <= agent_bus::MAX_LOGS_REPLY && out.ends_with(&tail),
        "remote logs: {} bytes, want at most {} ending with the last line",
        out.len(),
        agent_bus::MAX_LOGS_REPLY
    );
    assert!(
        out.starts_with("terminal line "),
        "remote logs start mid-line: {:?}",
        &out[..out.len().min(40)]
    );
    let hints = env.take_err();
    assert!(
        hints.contains("were cut to fit a bus message: the oldest"),
        "no word of the cut: {hints:?}"
    );
}

#[test]
fn test_fit_logs() {
    let chunks = vec![
        chunk("S1\n", true),
        chunk("a1\n", false),
        chunk("a2\n", false),
        chunk("S2\n", true),
        chunk("b1\n", false),
        chunk("b2\n", false),
        chunk("b3\n", false),
    ];
    for (budget, want, dropped) in [
        // Each chunk takes 4 bytes encoded: "\n" escapes to two.
        (100, "S1\na1\na2\nS2\nb1\nb2\nb3\n", 0),
        (12, "S2\nb2\nb3\n", 12), // b2's session line is put back
        (16, "S2\nb1\nb2\nb3\n", 9),
        (20, "S2\nb1\nb2\nb3\n", 9), // a2 would need S1 too
        (24, "S1\na2\nS2\nb1\nb2\nb3\n", 3),
    ] {
        assert_eq!(
            fit_logs(&chunks, budget, false),
            (want.to_string(), dropped),
            "budget {budget}"
        );
    }

    // One chunk too big alone: text keeps its end, JSON drops it.
    let big = vec![chunk(&format!("{}end\n", "y".repeat(100)), false)];
    let (got, dropped) = fit_logs(&big, 60, true);
    assert!(
        !got.is_empty() && got.ends_with("end\n") && dropped == 104 - got.len(),
        "text too big: {got:?}, {dropped} dropped"
    );
    assert_eq!(
        fit_logs(&big, 60, false),
        (String::new(), 104),
        "JSON line too big"
    );
    // Sizes count JSON escaping: quotes take two bytes each, 8 in all.
    let quoted = vec![chunk("\"\"\"\n", false), chunk("ok\n", false)];
    assert_eq!(fit_logs(&quoted, 8, false).0, "ok\n", "escaped size");
}

// agent resume name@host runs that host's resume: its refusals, and a resume
// it starts there.
#[test]
fn test_remote_resume() {
    crate::require_termd!();
    let mut c = setup_continue(None);
    let (home, dir) = (c.home.clone(), c.a.clone());
    let env = &mut c.env;

    // Not served: resume is off for other hosts by default.
    crate::require_bus!(env.serve_as_h1_with(agent_bus::DEFAULT_REMOTE_OPS));
    let err = resume::run(&env.ctx, &parse_args("resume", &["worker@h1"])).expect_err("not served");
    let hints = env.take_err();
    assert!(
        format!("{err:#}").contains(r#"does it allow "resume""#)
            && hints.contains("--remote-ops listing resume"),
        "resume not served: err {err:#}, hint {hints:?}"
    );

    let svc = std::sync::Arc::clone(env.service());
    env.serve_svc_as("h1", svc, agent_bus::ALL_OPS);
    // Refusals are the serving host's resume's own.
    for name in ["worker", "ghost"] {
        let local = resume::run(&env.ctx, &parse_args("resume", &[name])).expect_err("refused");
        let remote_target = format!("{name}@h1");
        let remote =
            resume::run(&env.ctx, &parse_args("resume", &[&remote_target])).expect_err("refused");
        let want = format!(
            "h1 couldn't resume {name}: {}",
            format!("{local:#}").replacen(
                "(1 agent there: agent list)",
                "(1 agent there: agent list --host local)",
                1
            )
        );
        assert_eq!(format!("{remote:#}"), want, "resume {name}");
    }
    // --attach goes over ssh (ssh_test.rs), which needs a terminal: a test
    // has none.
    env.ctx.attach_terminal = Some(false);
    let err = resume::run(&env.ctx, &parse_args("resume", &["worker@h1", "--attach"]))
        .expect_err("no terminal");
    assert!(
        format!("{err:#}").contains("over ssh, so it needs one"),
        "resume --attach of another host's agent with no terminal: {err:#}"
    );
    env.ctx.attach_terminal = None;

    let id = "11111111-2222-3333-4444-555555555555";
    let path = claude_transcript(&home, id, &dir, "", std::time::Duration::ZERO);
    env.store()
        .create_agent(&Agent {
            name: "done".into(),
            session_name: "rpc-cli-done".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.service()
        .report(
            "done",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                end_reason: EndReason::from("quit"),
                transcript: path,
                ..Default::default()
            },
        )
        .unwrap();
    let wd = std::env::current_dir().unwrap();
    resume::run(&env.ctx, &parse_args("resume", &["done@h1"])).unwrap_or_else(|e| panic!("{e:#}"));
    let out = env.take_out();
    let want = format!(
        "Resumed agent done@h1 (session rpc-cli-done, backend claude) on session {id}, in {dir}.\n"
    );
    assert!(
        out.starts_with(&want),
        "remote resume printed:\n{out}\nwant it to start:\n{want}"
    );
    assert_eq!(launched_resume(env, "done"), id, "the session resumed");
    let a = env.store().get_agent("done").unwrap().expect("done");
    assert_eq!(a.cwd, dir, "want it started where its session ran");
    assert_eq!(
        std::env::current_dir().unwrap(),
        wd,
        "resume changed this process's directory: the bus serves requests concurrently"
    );
}

// A remote resume says which agent asks, as name@host; the serving host lets
// an agent resume only its own helpers, as locally, and refuses a caller that
// names the serving host itself.
#[test]
fn test_remote_resume_only_your_helpers() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    crate::require_bus!(env.serve_as_h1_with(agent_bus::ALL_OPS));
    // h1 is this test's own service, whose host is "local": the request says
    // lead@local, which h1 takes for one of its own agents.
    env.set_env("AGENT_NAME", "lead");
    let err = resume::run(&env.ctx, &parse_args("resume", &["worker@h1"])).expect_err("refused");
    assert!(
        format!("{err:#}").contains("resumed_by lead@local names an agent on this host (local)"),
        "err = {err:#}, want lead@local refused by the host it names"
    );

    let handler = super::remote_ops::bus_handler(&env.ctx).unwrap();
    // review-4 S2: this host under a name it had before agent bus rename, or
    // in another case, is this host too.
    agent_bus::init_identity(env.cfg.bus_dir(), "alpha").unwrap();
    agent_bus::rename_identity(env.cfg.bus_dir(), "alpha", "local").unwrap();
    for by in ["lead@alpha", "lead@ALPHA", "lead@Local"] {
        let err = agent_bus::Handler::resume(
            &handler,
            &Context::background(),
            ResumeRequest {
                agent: "worker".into(),
                resumed_by: by.into(),
            },
        )
        .expect_err("refused");
        assert!(
            format!("{err:#}").contains(&format!(
                "resumed_by {by} names an agent on this host (local)"
            )),
            "{by} resuming worker: {err:#}"
        );
    }
    let err = agent_bus::Handler::resume(
        &handler,
        &Context::background(),
        ResumeRequest {
            agent: "worker".into(),
            resumed_by: "lead@laptop".into(),
        },
    )
    .expect_err("refused");
    assert!(
        format!("{err:#}")
            .contains(r#"it was spawned by a person, not by you ("lead@laptop") or your helpers"#),
        "lead@laptop resuming worker: {err:#}, want refused"
    );
}

// The JSON lines a remote logs --json prints are whole: a cut never splits
// one.
#[test]
fn test_remote_logs_json_lines_whole() {
    let mut chunks = vec![chunk("{\"type\":\"session\"}\n", true)];
    for i in 0..50 {
        chunks.push(chunk(&format!("{{\"type\":\"event\",\"n\":{i}}}\n"), false));
    }
    let (out, dropped) = fit_logs(&chunks, 200, false);
    assert!(dropped > 0, "nothing was cut");
    let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
    assert_eq!(
        lines[0], "{\"type\":\"session\"}",
        "want the session line put back"
    );
    for l in lines {
        assert!(
            serde_json::from_str::<serde_json::Value>(l).is_ok(),
            "line {l:?} isn't JSON"
        );
    }
}

// As for a remote spawn, --skip-permissions and --trust-workspace are
// refused for another host's agent before anything is sent: a peer never
// starts a process with its permission checks bypassed.
#[test]
fn test_remote_resume_refuses_permission_flags() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    env.forbid_bus();
    for flag in ["--skip-permissions", "--trust-workspace"] {
        let err = resume::run(&env.ctx, &parse_args("resume", &["worker@h1", flag]))
            .expect_err("refused");
        assert_eq!(
            format!("{err:#}"),
            format!(
                "{flag} works only for an agent on this host: to resume worker with it, run agent resume on h1"
            )
        );
    }
}

// agent deliver name@host goes over the bus, under the respond op: messages
// are held on the agent's host, and a sender elsewhere had to ssh there to
// type them in (friction #42).
#[test]
fn test_remote_deliver() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.create_live_agent(&Agent {
        name: "dworker".into(),
        session_name: "rpc-cli-dworker".into(),
        backend: "claude".into(),
        created_at: Time::now(),
        ..Default::default()
    });

    // Not served: a host whose --remote-ops leave out respond.
    crate::require_bus!(env.serve_as_h1_with(&[agent_bus::OP_LIST]));
    let (_, res) = env.run(&["deliver", "dworker@h1"]);
    let err = format!("{:#}", res.expect_err("not served"));
    assert!(err.contains("respond"), "respond not served: {err}");

    let svc = std::sync::Arc::clone(env.service());
    env.serve_svc_as("h1", svc, agent_bus::DEFAULT_REMOTE_OPS);
    let (out, res) = env.run(&["deliver", "dworker@h1"]);
    assert!(
        res.is_ok() && out == "Nothing is held for dworker@h1.\n",
        "nothing held: {out:?}, {res:?}"
    );
    env.service()
        .report(
            "dworker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                event: "UserPromptSubmit".into(),
                ..Default::default()
            },
        )
        .unwrap();
    env.store()
        .create_message(&mut Message {
            sender_id: "lead".into(),
            receiver_id: "dworker".into(),
            content: "[from lead@local] hi".into(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    let (out, res) = env.run(&["deliver", "dworker@h1"]);
    assert!(
        res.is_ok() && out.starts_with("Still holding 1 message for dworker@h1 (it is processing)"),
        "held for a busy agent: {out:?}, {res:?}"
    );
    // The same as here.
    let (local, res) = env.run(&["deliver", "dworker"]);
    assert!(
        res.is_ok() && out.replacen("dworker@h1", "dworker", 1) == local,
        "local deliver {local:?}, {res:?}; remote {out:?}"
    );
    // --wait is the agent's host's own deliverer.
    let (_, res) = env.run(&["deliver", "dworker@h1", "--wait"]);
    let err = format!("{:#}", res.expect_err("refused"));
    assert!(
        err.contains("runs on the agent's host"),
        "--wait over the bus: {err}"
    );
}
