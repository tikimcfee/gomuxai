//! Go: TestFromAgent (internal/cli/remote_test.go) and
//! TestSenderIsTheAgentTheProcessesShow (internal/cli/adopt_test.go): who a
//! message says it is from.

use agent_store::{Agent, Time};

use super::{from_agent, sender_id};
use crate::cli::adopt_test::stand_under;
use crate::cli::testenv::TestEnv;

#[test]
fn test_from_agent() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    env.change_cfg(|c| c.host = "h1".into());
    // Outside every agent's pane, so only the environment says who sends.
    stand_under(&env, Some(1));
    env.set_env("AGENT_NAME", "");
    env.set_env("AI_AGENT", "");
    env.set_env("CLAUDECODE", "");
    assert_eq!(from_agent(&env.ctx, "hi"), "[from a person on h1] hi");
    assert_eq!(from_agent(&env.ctx, "/compact"), "/compact", "person's slash command changed");

    // An AI CLI a person started has no agent record either, but isn't a
    // person: its environment says what it is.
    for ((ai_agent, claudecode), want) in [
        (("", "1"), "[from a Claude session on h1] hi"),
        (("claude-code_2-1-310_agent", ""), "[from a Claude session on h1] hi"),
        (("cursor_1-4_agent", ""), "[from a cursor session on h1] hi"),
        // AI_AGENT names the tool
        (("cursor_1-4_agent", "1"), "[from a cursor session on h1] hi"),
        (("$(rm -rf ~)_1_agent", ""), "[from an AI session on h1] hi"),
    ] {
        env.set_env("AI_AGENT", ai_agent);
        env.set_env("CLAUDECODE", claudecode);
        assert_eq!(
            from_agent(&env.ctx, "hi"),
            want,
            "AI_AGENT={ai_agent:?} CLAUDECODE={claudecode:?}"
        );
    }
    env.set_env("AI_AGENT", "");
    env.set_env("CLAUDECODE", "");
    env.set_env("AGENT_NAME", "lead");
    assert_eq!(from_agent(&env.ctx, "hi"), "[from lead@h1] hi");
    for cmd in ["/model claude-fable-5-1", "/compact", "/plugin:skill some args"] {
        assert_eq!(from_agent(&env.ctx, cmd), cmd, "agent's slash command changed");
    }
    // A path isn't a slash command: it's a message like any other.
    assert_eq!(
        from_agent(&env.ctx, "/tmp/out.txt is ready"),
        "[from lead@h1] /tmp/out.txt is ready"
    );
}

// Review 5, finding 4: a variable that isn't UTF-8 is set, as Go's os.Getenv
// reads it, never "": AI_AGENT of stray bytes is still an AI session, and
// AGENT_NAME of stray bytes is still an agent (whom send --force refuses,
// and the command log names).
#[test]
fn test_env_that_isnt_utf8_is_set() {
    use std::os::unix::ffi::OsStrExt;
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    env.change_cfg(|c| c.host = "h1".into());
    stand_under(&env, Some(1));
    env.set_env("AGENT_NAME", "");
    env.set_env("CLAUDECODE", "");
    env.set_env("AI_AGENT", std::ffi::OsStr::from_bytes(b"\xff_1"));
    assert_eq!(from_agent(&env.ctx, "hello"), "[from an AI session on h1] hello");
    env.set_env("AI_AGENT", "");

    env.set_env("AGENT_NAME", std::ffi::OsStr::from_bytes(b"w\xff"));
    assert_eq!(crate::cli::root::getenv("AGENT_NAME"), "w\u{fffd}");
    assert_eq!(env.ctx.caller_agent(), "w\u{fffd}");
    assert_eq!(env.ctx.non_interactive_caller_agent(), "w\u{fffd}");
    let (_, res) = env.run(&["send", "--force", "worker", "forced"]);
    assert_eq!(
        format!("{:#}", res.expect_err("refused")),
        "--force needs a person at a terminal; agents answer dialogs through a person (agent approve/deny)"
    );
    env.set_env("AGENT_NAME", "");
}

// AGENT_NAME is inherited by whatever an agent starts, so a process under
// one agent's pane can carry another's (friction #48: a background session
// Claude Code ran under another agent's pane signed its messages with that
// agent's name). The sender is the agent the processes show; AGENT_NAME
// counts only where they show none.
#[test]
fn test_sender_is_the_agent_the_processes_show() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    let session = env.ctx.cfg().session_name("sender-probe");
    env.create_live_agent(&Agent {
        name: "sender-probe".into(),
        session_name: session.clone(),
        backend: "claude".into(),
        created_at: Time::now(),
        ..Default::default()
    });
    let pane = env.termd_ops().first_pane(&session).expect("the probe's pane");
    env.set_env("AGENT_NAME", "lead");
    stand_under(&env, Some(pane.pid));
    assert_eq!(
        from_agent(&env.ctx, "hi"),
        "[from sender-probe@local] hi",
        "under the probe's pane with AGENT_NAME=lead"
    );
    assert_eq!(sender_id(&env.ctx), "sender-probe");
    // Outside every agent's pane, AGENT_NAME still names the sender.
    stand_under(&env, Some(1));
    assert_eq!(from_agent(&env.ctx, "hi"), "[from lead@local] hi", "outside the panes");
}

#[test]
fn test_cli_send_replace_supersede_json_and_status_receipt() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    let session = env.ctx.cfg().session_name("busy-worker");
    env.create_live_agent(&Agent {
        name: "busy-worker".into(),
        session_name: session,
        backend: "agy".into(),
        spawned_by: "lead".into(),
        created_at: Time::now(),
        ..Default::default()
    });
    env.ctx
        .store()
        .unwrap()
        .set_state(&agent_store::StateEntry {
            agent_name: "busy-worker".into(),
            key: "_spawn_id".into(),
            value: "sp1".into(),
            updated_at: Time::now(),
        })
        .unwrap();
    let svc = env.ctx.service().unwrap();
    svc.report(
        "busy-worker",
        agent_service::StatusReport {
            status: agent_service::AGENT_STATUS_PROCESSING,
            event: "PreInvocation".into(),
            ..Default::default()
        },
    )
    .unwrap();

    stand_under(&env, Some(1));
    env.set_env("AGENT_NAME", "lead");

    let (out1, res1) = env.run(&[
        "send",
        "--key",
        "plan",
        "--json",
        "busy-worker",
        "first draft",
    ]);
    res1.expect("first send");
    let v1: serde_json::Value = serde_json::from_str(&out1).unwrap();
    let id1 = v1["id"].as_i64().unwrap();
    assert!(id1 > 0 && v1["held"] == true);

    let (out2, res2) = env.run(&[
        "send",
        "--replace",
        "--key",
        "plan",
        "busy-worker",
        "revised instruction",
    ]);
    res2.expect("replace send");
    assert!(
        out2.contains(&format!("superseded #{id1}")),
        "expected superseded note in output: {out2}"
    );

    let (rec_out, rec_res) = env.run(&["status", "busy-worker", "--receipt", &id1.to_string()]);
    rec_res.expect("status --receipt");
    assert!(
        rec_out.contains(&format!(
            "message #{id1} to busy-worker (from lead): superseded, superseded by #"
        )),
        "unexpected receipt output: {rec_out}"
    );
    env.set_env("AGENT_NAME", "");
}
