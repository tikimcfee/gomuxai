//! Go tests/service/handover_test.go.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use agent_service::*;
use agent_store::{
    MESSAGE_STATUS_DELIVERED, MESSAGE_STATUS_FAILED, MESSAGE_STATUS_HANDOVER,
    MESSAGE_STATUS_PENDING, Message, Time,
};

use crate::common::*;

/// setupHookedClaude is setup_claude_terminal with worker's hooks wired at
/// spawn (so it holds messages), and term running its UserPromptSubmit hook
/// unless no_hook.
fn setup_hooked_claude(term: &Arc<ClaudeTerminal>, no_hook: bool) -> ClaudeEnv {
    let ce = setup_claude_terminal(term, SUBMIT_TEST_WAIT);
    // No session id: the fake has no registry for native status to read.
    ce.set_state(STATE_KEY_SPAWN_ID, "s1");
    ce.svc
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    if !no_hook {
        term.set_on_submit(prompt_submit_hook(&ce.svc, "s1"));
    }
    ce
}

fn handover_line_re() -> regex::Regex {
    regex::Regex::new(r"^(.*) \[gomuxai message #([0-9]+), ([0-9.]+ K?B), ([0-9]+ lines?): the whole message follows as hook context\]$").unwrap()
}

fn message(content: &str, receiver: &str, status: agent_store::MessageStatus) -> Message {
    Message {
        sender_id: "x".into(),
        receiver_id: receiver.into(),
        content: content.into(),
        status,
        created_at: Time::now(),
        ..Default::default()
    }
}

// A message Claude would take as a paste (long, or 3+ line breaks) goes to
// an agent whose hook reaches it as one short line, and the hook hands the
// model its whole text with the prompt: whatever its size and characters.
#[test]
fn test_handover_long_message_arrives_as_hook_context() {
    let strange = "quotes \"double\" 'single', $HOME $(id) `uname`, héllo wörld 日本語 🙂, a tab\tand \\backslash";
    for size in [1000, 10000, 100000] {
        let mut b = format!("[from lead@hub] Do these things, in order:\n{strange}\n");
        let mut i = 1;
        while b.len() < size {
            let _ = writeln!(
                b,
                "{i}. step {i} of the plan, with ~tildes~ and <angle> & ampersands"
            );
            i += 1;
        }
        let msg = b.trim_end_matches('\n').to_string();

        let term = ClaudeTerminal::new(|_| {});
        let ce = setup_hooked_claude(&term, false);
        let res = ce
            .svc
            .send(
                &bg(),
                "worker",
                &msg,
                SendOptions {
                    from: "lead".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            !res.held && res.typed == 1 && res.unchecked.is_empty(),
            "{} bytes: Send = {res:?}; want it typed",
            msg.len()
        );
        let (_, submitted, boxed) = term.state();
        assert!(
            submitted.len() == 1 && boxed.is_empty(),
            "{} bytes: submitted {submitted:?}, box {boxed:?}; want one prompt",
            msg.len()
        );
        let line = &submitted[0];
        let re = handover_line_re();
        let m = re.captures(line);
        assert!(
            m.is_some() && !line.contains('\n') && line.chars().count() <= 400,
            "{} bytes: typed {line:?}; want one short handover line",
            msg.len()
        );
        let m = m.unwrap();
        assert_eq!(
            &m[1], "[from lead@hub] Do these things, in order: …",
            "line starts wrong; want the message's first line"
        );
        let got = term.got();
        let want = format!("{line}\ngomuxai message #{}, whole:\n\n{msg}", &m[2]);
        assert!(
            got.len() == 1 && got[0] == want,
            "{} bytes: the model got {} bytes, want {}: the line, then the whole message",
            msg.len(),
            got[0].len(),
            want.len()
        );
        let id: i64 = m[2].parse().unwrap();
        assert_eq!(
            ce.store.get_message_status(id).unwrap(),
            MESSAGE_STATUS_DELIVERED
        );
    }
}

// Short single-line messages, and slash commands, are typed as they are.
#[test]
fn test_handover_short_messages_are_typed() {
    for msg in [
        "[from lead@hub] run the tests".to_string(),
        "[from lead@hub] two lines:\nthe second".to_string(),
        "/model claude-fable-5-1".to_string(),
        format!("[from lead@hub] {}", "é".repeat(600)), // 600 characters, 1200 bytes
    ] {
        let term = ClaudeTerminal::new(|_| {});
        let ce = setup_hooked_claude(&term, false);
        ce.svc
            .send(&bg(), "worker", &msg, SendOptions::default())
            .unwrap_or_else(|e| panic!("{msg:.30?}: {e:#}"));
        let got = term.got();
        assert!(
            got.len() == 1 && got[0] == msg,
            "{msg:.30?}: the model got {got:?}; want it as typed"
        );
    }
}

// Held messages typed together, oldest first, are one long prompt: handed
// over the same way.
#[test]
fn test_handover_held_messages_typed_together() {
    let term = ClaudeTerminal::new(|_| {});
    let ce = setup_hooked_claude(&term, false);
    for m in ["[from a@h] one", "[from b@h] two"] {
        ce.store
            .create_message(&mut message(m, "worker", MESSAGE_STATUS_PENDING))
            .unwrap();
    }
    let res = ce
        .svc
        .send(&bg(), "worker", "[from c@h] three", SendOptions::default())
        .unwrap();
    assert_eq!(res.typed, 3, "Send = {res:?}; want 3 typed");
    let got = term.got();
    assert!(
        got.len() == 1
            && got[0].contains("[from a@h] one")
            && got[0].contains("[from b@h] two")
            && got[0].contains("[from c@h] three")
            && got[0].find("[from a@h] one") < got[0].find("[from b@h] two")
            && got[0].find("[from b@h] two") < got[0].find("[from c@h] three"),
        "the model got {got:?}; want all three coalesced, oldest first"
    );
}

// If the hook never takes the message, send says the agent got only the
// line, and a late hook hands over nothing; held messages typed with it
// aren't held again, so the deliverer can't type the line over and over.
#[test]
fn test_handover_hook_missing_is_reported() {
    let term = ClaudeTerminal::new(|_| {});
    let ce = setup_hooked_claude(&term, true);
    ce.store
        .create_message(&mut message(
            "[from a@h] held",
            "worker",
            MESSAGE_STATUS_PENDING,
        ))
        .unwrap();
    let msg = format!("[from lead@hub] {}", "long ".repeat(300));
    let start = Instant::now();
    let err = ce
        .svc
        .send(&bg(), "worker", &msg, SendOptions::default())
        .unwrap_err();
    let he = err
        .downcast_ref::<HandoverError>()
        .unwrap_or_else(|| panic!("Send err = {err:#}; want a HandoverError"));
    assert!(
        err.to_string().contains("got only that line"),
        "Send err = {err:#}"
    );
    assert!(
        start.elapsed() >= SUBMIT_TEST_WAIT * 3,
        "gave up after {:?}; want 3 SubmitWaits",
        start.elapsed()
    );
    assert_eq!(
        ce.store.get_message_status(he.id).unwrap(),
        MESSAGE_STATUS_FAILED
    );
    assert_eq!(
        ce.svc.held_count("worker"),
        0,
        "held after the failed handover; want none"
    );
    let (_, submitted, _) = term.state();
    let out = prompt_submit_hook(&ce.svc, "s1")(&submitted[0]);
    assert!(
        out.is_none(),
        "a late hook handed over {out:?}; want nothing"
    );
}

// A line Enter didn't submit sits in the input box: whoever submits it
// later gets the whole message from the hook.
#[test]
fn test_handover_unsubmitted_line_still_hands_over() {
    let term = ClaudeTerminal::new(|s| s.deaf_enter = true);
    let ce = setup_hooked_claude(&term, false);
    let msg = "[from lead@hub] a\nb\nc\nd";
    let err = ce
        .svc
        .send(&bg(), "worker", msg, SendOptions::default())
        .unwrap_err();
    assert!(
        err.downcast_ref::<UnsubmittedError>().is_some(),
        "Send err = {err:#}; want UnsubmittedError"
    );
    term.set(|s| s.deaf_enter = false);
    agent_termd::Ops::send_special_key(term.as_ref(), "", "Enter").unwrap();
    let got = term.got();
    assert!(
        got.len() == 1 && got[0].ends_with(&format!("\n\n{msg}")),
        "the model got {got:?}; want the whole message once submitted"
    );
}

// Only the prompt naming a message hands it over, and only to the agent it
// was sent to.
#[test]
fn test_handover_token_names_the_agents_own_message() {
    let term = ClaudeTerminal::new(|_| {});
    let ce = setup_hooked_claude(&term, false);
    let mut other = message("secret", "someone-else", MESSAGE_STATUS_HANDOVER);
    ce.store.create_message(&mut other).unwrap();
    let hook = prompt_submit_hook(&ce.svc, "s1");
    for prompt in [
        format!(
            "x [gomuxai message #{}, 1 KB, 3 lines: the whole message follows as hook context]",
            other.id
        ),
        "just a prompt".to_string(),
    ] {
        let out = hook(&prompt);
        assert!(
            out.is_none(),
            "prompt {prompt:?} got hook output {out:?}; want none"
        );
    }
    assert_eq!(
        ce.store.get_message_status(other.id).unwrap(),
        MESSAGE_STATUS_HANDOVER,
        "another agent's message changed"
    );
}
