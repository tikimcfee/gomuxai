//! `agent approve` and `agent deny` answering a Claude agent's permission
//! prompt, as the CLI runs them: what they say they answered, the hint after
//! a deny, the status they record, and the refusals once nothing waits.

use std::time::{Duration, Instant};

use agent_service::{AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING, AGENT_STATUS_WAITING, StatusReport};

use crate::cli::testenv::TestEnv;

/// Draws a Claude permission dialog in worker's bare shell, its footer the
/// last line on screen, and leaves cat reading the keys that answer it
/// (unechoed, so the dialog stays as drawn).
fn draw_prompt(env: &TestEnv) {
    let t = env.termd_ops();
    t.send_keys(
        "rpc-cli-worker",
        r"clear; stty -echo; printf 'Do you want to proceed?\n\342\235\257 1. Yes\n  2. No\n\nEsc to cancel\n'; cat >/dev/null",
    )
    .unwrap();
    t.send_special_key("rpc-cli-worker", "Enter").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let screen = t.capture("rpc-cli-worker", 0).unwrap_or_default();
        let last = screen.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
        if last.trim() == "Esc to cancel" && screen.contains("2. No") {
            return;
        }
        assert!(Instant::now() < deadline, "the dialog never showed:\n{screen}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn waiting_on(env: &TestEnv, detail: &str) {
    env.service()
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                detail: detail.into(),
                tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap();
}

#[test]
fn approve_and_deny_answer_the_prompt() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    draw_prompt(&env);

    let (out, res) = env.run(&["approve", "worker", "--expect", "Bash"]);
    res.unwrap();
    assert_eq!(out, "Approved worker's Bash prompt (permission: Bash: ls).\n");
    assert_eq!(env.take_err(), "");
    let st = env.service().status_info("worker").unwrap();
    assert_eq!(st.status, AGENT_STATUS_PROCESSING, "after approve: {st:?}");

    waiting_on(&env, "permission: Bash: rm -rf build");
    let (out, res) = env.run(&["approve", "worker", "--always"]);
    res.unwrap();
    assert_eq!(
        out,
        "Always approved worker's Bash prompt (permission: Bash: rm -rf build).\n"
    );

    waiting_on(&env, "permission: Bash: git push");
    let (out, res) = env.run(&["deny", "worker"]);
    res.unwrap();
    assert_eq!(out, "Denied worker's Bash prompt (permission: Bash: git push).\n");
    assert_eq!(
        env.take_err(),
        "hint: denying cancels Claude's turn: tell it what to do instead with agent send worker \"...\"\n"
    );
    let st = env.service().status_info("worker").unwrap();
    assert_eq!(st.status, AGENT_STATUS_IDLE, "after deny: {st:?}");

    // Nothing waits now: both refuse, and say why.
    for cmd in ["approve", "deny"] {
        let (out, res) = env.run(&[cmd, "worker"]);
        let err = format!("{:#}", res.unwrap_err());
        assert!(
            err.starts_with("agent worker is idle, not waiting on a permission prompt, so there is nothing to answer"),
            "{cmd} with nothing waiting: {err}"
        );
        assert_eq!(out, "");
    }
}

// The prompt status describes may be gone from the screen (answered in the
// meantime): no key is sent.
#[test]
fn approve_refuses_a_prompt_not_on_screen() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    let (out, res) = env.run(&["approve", "worker"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.starts_with("agent worker has no permission prompt on screen; it may have just been answered"),
        "{err}"
    );
    assert_eq!(out, "");
    let st = env.service().status_info("worker").unwrap();
    assert_eq!(st.status, AGENT_STATUS_WAITING, "a refused approve changed the status");
}
