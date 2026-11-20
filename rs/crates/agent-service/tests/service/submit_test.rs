//! Go tests/service/submit_test.go (claudeTerminal is in common.rs).

use std::time::{Duration, Instant};

use agent_service::*;
use agent_store::{MESSAGE_STATUS_PENDING, Message, Time};

use crate::common::*;

// A message typed while Claude is busy reached it in one read with its
// Enter, which became a newline: the message sat in the input box until a
// person pressed Enter. Send now waits for the box to show the text before
// pressing Enter.
#[test]
fn test_submit_waits_for_the_box_to_show_the_text() {
    for msg in [
        "[from lead@hub] run the tests".to_string(),
        "[from lead@hub] Screenshot:\n/tmp/shot.png\nlook at it".to_string(),
        format!("[from lead@hub] {}", "a long line ".repeat(40)),
    ] {
        // Without looking, as before: the stall merges text and Enter.
        let term = ClaudeTerminal::new(|s| s.stalled = 3);
        let ce = setup_claude_terminal(&term, Duration::ZERO);
        ce.svc.send_unchecked(&bg(), "worker", &msg).unwrap();
        term.capture_now();
        term.capture_now();
        term.capture_now();
        let (_, submitted, boxed) = term.state();
        assert!(
            submitted.is_empty() && !boxed.is_empty(),
            "the fake should reproduce the bug: submitted {submitted:?}, box {boxed:?}"
        );

        let term = ClaudeTerminal::new(|s| s.stalled = 3);
        let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
        ce.svc
            .send_unchecked(&bg(), "worker", &msg)
            .unwrap_or_else(|e| panic!("{msg:?}: {e:#}"));
        let (enters, submitted, boxed) = term.state();
        assert!(
            submitted.len() == 1 && boxed.is_empty() && enters == 1,
            "{msg:?}: submitted {submitted:?} with {enters} Enters, box {boxed:?}; want it submitted with one"
        );
        if !msg.starts_with("[from lead@hub] Screenshot") {
            assert!(
                !submitted[0].contains('\n'),
                "{msg:?}: submitted with a newline: {:?}",
                submitted[0]
            );
        }
    }
}

// A message over 1022 bytes typed into Claude on macOS reached it in two
// reads, and Claude kept only the second: an agent got the last 44 bytes
// of a 1066-byte message, and the tail of a --now message (friction #36,
// #37). Messages are pasted with paste brackets now, which make the pieces
// one paste.
#[test]
fn test_submit_long_message_arrives_whole() {
    for size in [PTY_QUEUE + 44, 5000] {
        let msg = format!(
            "[from lead@hub] the instructions come first. {}",
            "x".repeat(size)
        );
        for strange in [false, true] {
            // checked, and typed without checking
            let term = ClaudeTerminal::new(|s| s.strange = strange);
            let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
            ce.svc
                .send(
                    &bg(),
                    "worker",
                    &msg,
                    SendOptions {
                        now: true,
                        ..Default::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{} bytes: {e:#}", msg.len()));
            let (_, submitted, _) = term.state();
            assert!(
                submitted.len() == 1 && submitted[0] == msg,
                "{} bytes (unchecked {strange}): Claude got {} bytes; want the whole message",
                msg.len(),
                submitted.first().map_or(0, |s| s.len())
            );
        }
    }
}

// The same over the bus, as the reports came: a --now message to a busy
// agent (#36), and a message to an agent whose turn had stopped with
// background work running, which send types in at once (#37).
#[test]
fn test_submit_long_message_over_the_bus_arrives_whole() {
    let msg = format!(
        "[from lead@hub] the instructions come first. {}",
        "y".repeat(1100)
    );
    for (name, now, ev) in [
        ("--now while it works", true, "PreToolUse"),
        ("held, its turn over but for background work", false, "Stop"),
    ] {
        let bus = crate::require_bus!();
        let term = ClaudeTerminal::new(|_| {});
        let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
        ce.set_state(STATE_KEY_SPAWN_ID, "s1");
        ce.svc
            .report(
                "worker",
                StatusReport {
                    status: AGENT_STATUS_PROCESSING,
                    event: ev.into(),
                    ..Default::default()
                },
            )
            .unwrap();
        term.set_on_submit(prompt_submit_hook(&ce.svc, "s1"));
        let (_server, client) = serve(&bus, &ce.svc, "h1", agent_bus::DEFAULT_REMOTE_OPS);

        let resp = client
            .send_message(
                &bg(),
                &agent_bus::AgentRef::new("worker", "h1"),
                agent_bus::SendRequest {
                    message: msg.clone(),
                    now,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|e| panic!("{name}: SendMessage: {e:#}"));
        assert!(
            resp.as_ref().is_some_and(|r| r.typed == 1),
            "{name}: SendMessage = {resp:?}; want it typed"
        );
        // Its hook reaches it, so the message goes as a short line, its
        // whole text handed over by the hook (see handover_test.rs).
        let got = term.got();
        assert!(
            got.len() == 1 && got[0].ends_with(&format!("\n\n{msg}")),
            "{name}: Claude got {got:?}; want the whole message"
        );
    }
}

// Claude stalled longer than send waits for the text: the text and Enter
// arrive together, and the next Enter is eaten. Send keeps pressing Enter
// while the box shows the message.
#[test]
fn test_submit_presses_enter_until_the_box_empties() {
    let term = ClaudeTerminal::new(|s| s.stalled = 10);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    ce.svc
        .send_unchecked(&bg(), "worker", "[from lead@hub] run the tests")
        .unwrap();
    let (enters, submitted, boxed) = term.state();
    assert!(
        submitted.len() == 1 && boxed.is_empty(),
        "submitted {submitted:?}, box {boxed:?}; want submitted"
    );
    assert_eq!(enters, 3, "Enters; want 3 (one a newline, one eaten)");
    assert!(
        ce.svc.unsubmitted("worker").is_none(),
        "Unsubmitted after it was submitted"
    );
}

// When no Enter submits the message, send says so, and so do status and
// the next send, until the box empties.
#[test]
fn test_submit_says_when_the_message_sits_in_the_box() {
    let term = ClaudeTerminal::new(|s| s.deaf_enter = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    let err = ce
        .svc
        .send_unchecked(&bg(), "worker", "[from lead@hub] run the tests")
        .unwrap_err();
    assert!(
        err.downcast_ref::<UnsubmittedError>().is_some(),
        "err {err:#}; want UnsubmittedError"
    );
    let s = err.to_string();
    assert!(
        s.contains("agent key test-worker Enter") && s.contains("don't send it again"),
        "error doesn't say what to do: {s}"
    );
    assert_eq!(term.state().0, 3, "Enters; want 3");
    let (at, session) = ce.svc.unsubmitted("worker").expect("Unsubmitted");
    assert!(
        session == "test-worker" && Time::now().unix_nano() - at.unix_nano() < 60_000_000_000,
        "Unsubmitted = {session:?}; want it, just now"
    );
    let why = ce.svc.input_blocked("worker");
    assert!(
        why.contains("wasn't submitted"),
        "InputBlocked = {why:?}; want it to name the unsubmitted message"
    );

    // A person presses Enter.
    term.set(|s| s.deaf_enter = false);
    agent_termd::Ops::send_special_key(term.as_ref(), "", "Enter").unwrap();
    assert!(
        ce.svc.unsubmitted("worker").is_none(),
        "Unsubmitted after the box emptied"
    );
    assert_eq!(
        ce.svc.input_blocked("worker"),
        "",
        "InputBlocked after the box emptied"
    );
}

// An Enter that brings up a dialog (or anything that hides the box) ends
// the checking: another Enter would answer the dialog.
#[test]
fn test_submit_no_enter_once_the_box_is_gone() {
    let term = ClaudeTerminal::new(|s| {
        s.deaf_enter = true;
        s.dialog_on = 1;
    });
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    ce.svc
        .send_unchecked(&bg(), "worker", "[from lead@hub] run the tests")
        .unwrap_or_else(|e| panic!("err {e:#}; want none (it can't tell)"));
    assert_eq!(term.state().0, 1, "Enters; want 1");
}

// Held messages typed in but not submitted sit in the input box: they
// aren't held again, which would deliver them twice.
#[test]
fn test_submit_unsubmitted_held_messages_stay_delivered() {
    let term = ClaudeTerminal::new(|s| s.deaf_enter = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    ce.set_state(STATE_KEY_SPAWN_ID, "s1");
    ce.store
        .create_message(&mut Message {
            sender_id: "lead".into(),
            receiver_id: "worker".into(),
            content: "[from lead@hub] run the tests".into(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    ce.svc
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Notification".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let (n, err) = ce
        .svc
        .deliver_held(&bg(), "worker", &DeliverOptions::default());
    assert!(
        n == 1
            && err
                .as_ref()
                .is_err_and(|e| e.downcast_ref::<UnsubmittedError>().is_some()),
        "DeliverHeld = {n}, {err:?}; want 1, UnsubmittedError"
    );
    assert_eq!(ce.svc.held_count("worker"), 0, "held after typing");
}

// When the input box can't be read before typing, send still types the
// message and presses Enter, but says it couldn't check, and status shows
// that until a turn starts: a box that goes unrecognized (as a named
// session's once did) mustn't fail quietly.
#[test]
fn test_submit_says_when_it_cannot_check() {
    let term = ClaudeTerminal::new(|s| s.strange = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    turn_started(&ce.store, "worker"); // it isn't starting up: no wait for the box
    let res = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions {
                now: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(res.typed, 1);
    assert!(
        res.unchecked.contains("input box isn't on screen"),
        "Unchecked = {:?}; want why it couldn't check",
        res.unchecked
    );
    let (enters, submitted, _) = term.state();
    assert!(
        enters == 1 && submitted.len() == 1,
        "{enters} Enters, submitted {submitted:?}; want typed and Enter pressed once, as before"
    );
    let (why, at) = ce.svc.unchecked("worker").expect("Unchecked recorded");
    assert!(
        why == res.unchecked && Time::now().unix_nano() - at.unix_nano() < 60_000_000_000,
        "Unchecked = {why:?}"
    );

    // A turn that starts after it shows the message was taken. Not a
    // wait: the clock moves, so the turn's time is after the send's.
    std::thread::sleep(Duration::from_millis(10));
    ce.svc
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                event: "UserPromptSubmit".into(),
                clear_result: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        ce.svc.unchecked("worker").is_none(),
        "Unchecked after a turn started; want none"
    );

    // A message it could check clears the record.
    term.set(|s| s.strange = false);
    ce.svc
        .send_unchecked(&bg(), "worker", "[from lead@hub] again")
        .unwrap();
    assert!(
        ce.store
            .get_state("worker", STATE_KEY_UNCHECKED)
            .unwrap()
            .is_none(),
        "{STATE_KEY_UNCHECKED} after a checked message; want it deleted"
    );
}

// A send right after spawn, to a Claude that hadn't drawn its input box
// yet, was typed in without checking: the text and its Enter waited in the
// terminal until Claude read them together, as a paste, and the message sat
// in the box unsent (friction #38). With no turn started yet, send now
// waits for the box to come up, then types and checks as usual.
#[test]
fn test_submit_waits_for_a_starting_clis_input_box() {
    let term = ClaudeTerminal::new(|s| s.starting = 8);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    let res = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions::default(),
        )
        .unwrap();
    assert!(
        res.typed == 1 && res.unchecked.is_empty(),
        "Send = {res:?}; want typed and checked"
    );
    let (enters, submitted, boxed) = term.state();
    assert!(
        submitted.len() == 1 && boxed.is_empty(),
        "submitted {submitted:?} with {enters} Enters, box {boxed:?}; want it submitted"
    );
    assert!(
        ce.svc.unchecked("worker").is_none(),
        "Unchecked recorded for a checked message"
    );
}

// A box that never comes up is waited for only while no turn has started,
// and only so long; then the message is typed unchecked, as before, saying
// so.
#[test]
fn test_submit_wait_for_a_starting_cli_is_bounded() {
    let term = ClaudeTerminal::new(|s| s.strange = true);
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    let start = Instant::now();
    let res = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions::default(),
        )
        .unwrap();
    assert_eq!(res.typed, 1);
    let took = start.elapsed();
    assert!(
        took >= SUBMIT_TEST_WAIT * 10 && took <= SUBMIT_TEST_WAIT * 20,
        "took {took:?}; want about {:?}",
        SUBMIT_TEST_WAIT * 10
    );
    assert!(
        res.unchecked.contains("input box isn't on screen")
            && res.unchecked.contains("after waiting"),
        "Unchecked = {:?}; want why, and that it waited",
        res.unchecked
    );
}

// A dialog that comes up while send waits for a starting CLI's box ends
// the wait: the message's Enter would answer it. --force (send_unchecked)
// types anyway, as it always has.
#[test]
fn test_submit_starting_clis_dialog_stops_the_send() {
    let term = ClaudeTerminal::new(|s| {
        s.starting = 4;
        s.dialog = true;
    });
    let ce = setup_claude_terminal(&term, SUBMIT_TEST_WAIT);
    let err = ce
        .svc
        .send(
            &bg(),
            "worker",
            "[from lead@hub] run the tests",
            SendOptions::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("permission prompt on screen"),
        "err {err:#}; want the dialog named"
    );
    let (_, _, boxed) = term.state();
    let unread = term.mu.lock().unwrap().unread.clone();
    assert!(
        boxed.is_empty() && unread.is_empty(),
        "typed into the dialog: box {boxed:?}, unread {unread:?}"
    );
}
