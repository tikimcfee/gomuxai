//! Go tests/service/held_test.go.

use std::time::{Duration, Instant};

use agent_backend::Status;
use agent_service::*;

use crate::common::*;
use crate::require_termd;

/// spawnHolding spawns a fake Claude agent with hooks wired, so messages to it
/// can be held; env.delivering() counts the deliverers sends start.
fn spawn_holding(env: &mut TestEnv, name: &str) -> FakeHook {
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    spawn_fake(env, name)
}

/// busy puts the fake agent mid-turn: its prompt was submitted and native
/// status says busy.
fn busy(env: &TestEnv, fake: &FakeHook, name: &str) {
    fake.set(|s| {
        s.native_found = true;
        s.native = Status::Processing;
        s.prompt_gone = true;
    });
    hook_as(env, fake, name, r#"{"hook_event_name":"UserPromptSubmit"}"#);
}

/// stopHook runs the Stop hook as the hook itself does, returning its output.
fn stop_hook(env: &TestEnv, fake: &FakeHook, name: &str, payload: &str) -> Option<Vec<u8>> {
    let payload = payload.replacen(
        '{',
        &format!(r#"{{"session_id":"{}","#, fake.session_id()),
        1,
    );
    let (out, err) = env.service.handle_hook(name, "", "", payload.as_bytes());
    if let Err(e) = err {
        panic!("HandleHook {payload}: {e:#}");
    }
    out
}

/// additionalContext decodes a Stop hook's delivery output.
fn additional_context(out: Option<Vec<u8>>) -> String {
    let out = out.expect("hook output");
    let v: serde_json::Value = serde_json::from_slice(&out)
        .unwrap_or_else(|e| panic!("hook output {:?}: {e}", String::from_utf8_lossy(&out)));
    assert_eq!(
        v["hookSpecificOutput"]["hookEventName"],
        "Stop",
        "hook output {}: not a Stop hookSpecificOutput",
        String::from_utf8_lossy(&out)
    );
    v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

// A message to a busy agent is held, and the Stop hook hands every held
// message over at once, oldest first, as additionalContext. The agent goes
// on with them, so the status stays processing (a new turn for wait --next)
// and the interim reply isn't the result; the next Stop finds nothing held
// and ends the turn as before.
#[test]
fn test_send_held_until_stop_delivers() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");

    let res = env
        .service
        .send(
            &bg(),
            "worker",
            "[from lead@h] first",
            SendOptions {
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        res.held && res.held_count == 1 && res.typed == 0 && res.status == AGENT_STATUS_PROCESSING,
        "send to a busy agent: {res:?}; want held"
    );
    let res = env
        .service
        .send(
            &bg(),
            "worker",
            "[from a person on h] second",
            SendOptions::default(),
        )
        .unwrap();
    assert!(
        res.held && res.held_count == 2,
        "second send: {res:?}; want held behind the first"
    );
    assert_eq!(
        env.delivering().len(),
        2,
        "started deliverers, want one per held send"
    );
    assert_eq!(
        env.service.status_info("worker").unwrap().held,
        2,
        "status held"
    );
    let sent_at = env.state_value("worker", STATE_KEY_SENT_AT);

    // The status-only path never claims.
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"PostToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
    );
    assert_eq!(
        env.service.held_count("worker"),
        2,
        "ReportHook delivered held messages"
    );

    let out = stop_hook(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","last_assistant_message":"interim"}"#,
    );
    let want = "2 messages arrived while you were working, oldest first:\n\n[from lead@h] first\n\n[from a person on h] second";
    assert_eq!(additional_context(out), want);
    assert_eq!(
        env.service.held_count("worker"),
        0,
        "messages still held after delivery"
    );
    let (st, ev) = (
        env.state_value("worker", STATE_KEY_STATUS),
        env.state_value("worker", STATE_KEY_EVENT),
    );
    assert!(
        st == "processing" && ev == EVENT_DELIVERED,
        "after delivery: {st}/{ev}, want processing/delivered"
    );
    assert_eq!(
        env.state_value("worker", STATE_KEY_DETAIL),
        "2 held messages delivered"
    );
    assert_eq!(
        env.state_value("worker", STATE_KEY_RESULT),
        "",
        "result stored for a turn that goes on"
    );
    let turn_at = env.state_value("worker", STATE_KEY_TURN_AT);
    assert!(
        turn_at > sent_at,
        "turn_at {turn_at} not after sent_at {sent_at}: wait --next would miss the delivered step"
    );

    let out = stop_hook(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#,
    );
    assert!(out.is_none(), "Stop with nothing held printed {out:?}");
    let (st, r) = (
        env.state_value("worker", STATE_KEY_STATUS),
        env.state_value("worker", STATE_KEY_RESULT),
    );
    assert!(
        st == "idle" && r == "done",
        "final Stop: {st}, result {r:?}; want idle, done"
    );
}

// One held message is introduced as one.
#[test]
fn test_stop_hook_one_message() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    env.service
        .send(&bg(), "worker", "[from lead@h] hi", SendOptions::default())
        .unwrap();
    // A Stop with background work running delivers too: the prompt is free.
    let out = stop_hook(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","background_tasks":[{"id":"b1","type":"shell","status":"running"}]}"#,
    );
    assert_eq!(
        additional_context(out),
        "A message arrived while you were working:\n\n[from lead@h] hi"
    );
}

// The turn ended (its Stop found nothing held) before the message was
// stored, though native status, lagging, still says busy: send sees the
// Stop's report after storing, and types the message itself. A Stop that
// lands between send's status check and the store takes the same path.
#[test]
fn test_send_turn_ended_during_send() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    let out = stop_hook(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#,
    );
    assert!(out.is_none(), "nothing held, yet Stop printed {out:?}");
    // Native still says busy.
    let res = env
        .service
        .send(
            &bg(),
            "worker",
            "echo raced-$((6*7))",
            SendOptions::default(),
        )
        .unwrap();
    assert!(
        !res.held && res.typed == 1,
        "send as the turn ended: {res:?}; want typed"
    );
    assert!(
        env.delivering().is_empty(),
        "started a deliverer for a typed message"
    );
    assert_eq!(env.service.held_count("worker"), 0, "message left held");
    wait_for_output(&env, "worker", "raced-42");
}

// After a Stop that left background work running, the prompt takes input
// though native status says busy: type, don't hold.
#[test]
fn test_send_types_after_background_stop() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","background_tasks":[{"id":"b1","type":"shell","status":"running"}]}"#,
    );
    let res = env
        .service
        .send(&bg(), "worker", "echo bg", SendOptions::default())
        .unwrap();
    assert!(
        !res.held && res.typed == 1,
        "send during background work: {res:?}; want typed"
    );
}

// --now, and slash commands, are typed into a busy agent as before.
#[test]
fn test_send_now_and_slash_commands_are_typed() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    for (msg, opts) in [
        (
            "echo now",
            SendOptions {
                now: true,
                ..Default::default()
            },
        ),
        ("/compact", SendOptions::default()),
    ] {
        let res = env
            .service
            .send(&bg(), "worker", msg, opts.clone())
            .unwrap();
        assert!(
            !res.held && res.typed == 1,
            "send {msg:?} {opts:?}: {res:?}; want typed"
        );
    }
}

// A message to an agent at its prompt is typed after the ones still held
// (a turn that ended with no Stop), as one prompt, oldest first.
#[test]
fn test_send_types_held_messages_first() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    for m in ["echo one-$((1))", "echo two-$((2))"] {
        env.service
            .send(&bg(), "worker", m, SendOptions::default())
            .unwrap();
    }
    fake.set(|s| s.native = Status::Idle); // Esc: no hook fired
    let res = env
        .service
        .send(&bg(), "worker", "echo three-$((3))", SendOptions::default())
        .unwrap();
    assert!(
        !res.held && res.typed == 3,
        "send at the prompt: {res:?}; want 3 typed"
    );
    let screen = wait_for_output(&env, "worker", "three-3");
    let (i, j, k) = (
        screen.find("one-1"),
        screen.find("two-2"),
        screen.find("three-3"),
    );
    assert!(
        i.is_some() && i < j && j < k,
        "typed out of order:\n{screen}"
    );
}

// The deliverer types held messages once the agent is at its prompt with
// them still held (an interrupted turn: the report says processing, native
// idle), after the grace; it reports nothing held, and refuses an agent
// that has exited.
#[test]
fn test_deliver_held() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");

    let (_, err) = env
        .service
        .deliver_held(&bg(), "worker", &DeliverOptions::default());
    assert!(
        err.unwrap_err().downcast_ref::<ErrNothingHeld>().is_some(),
        "nothing held: want ErrNothingHeld"
    );

    busy(&env, &fake, "worker");
    env.service
        .send(
            &bg(),
            "worker",
            "echo delivered-$((40+2))",
            SendOptions::default(),
        )
        .unwrap();
    // Still busy: one check types nothing.
    let (n, err) = env
        .service
        .deliver_held(&bg(), "worker", &DeliverOptions::default());
    assert!(n == 0 && err.is_ok(), "busy agent: typed {n}, err {err:?}");

    fake.set(|s| s.native = Status::Idle);
    let start = Instant::now();
    let (n, err) = env.service.deliver_held(
        &bg(),
        "worker",
        &DeliverOptions {
            wait: true,
            interval: Duration::from_millis(50),
            grace: Duration::from_millis(300),
        },
    );
    assert!(
        n == 1 && err.is_ok(),
        "interrupted turn: typed {n}, err {err:?}; want 1"
    );
    assert!(
        start.elapsed() >= Duration::from_millis(300),
        "typed after {:?}, before the grace",
        start.elapsed()
    );
    wait_for_output(&env, "worker", "delivered-42");

    busy(&env, &fake, "worker");
    env.service
        .send(&bg(), "worker", "echo never", SendOptions::default())
        .unwrap();
    fake.set(|s| {
        s.native_found = false;
        s.native_gone = true; // the CLI died
    });
    let (_, err) = env.service.deliver_held(
        &bg(),
        "worker",
        &DeliverOptions {
            wait: true,
            interval: Duration::from_millis(50),
            ..Default::default()
        },
    );
    let err = err.unwrap_err();
    assert!(
        err.to_string().contains("has ended"),
        "exited agent: err {err:#}"
    );
}

// Held messages are never typed into a person's half-written input: the
// deliverer and send wait for an empty input line, and a send that finds
// messages held and the line in use joins them.
#[test]
fn test_held_waits_for_an_empty_input_line() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    env.service
        .send(&bg(), "worker", "echo first-$((1))", SendOptions::default())
        .unwrap();
    fake.set(|s| {
        s.native = Status::Idle; // Esc: no hook fired
        s.input = "half a thought".into();
    });

    let (n, err) = env
        .service
        .deliver_held(&bg(), "worker", &DeliverOptions::default());
    assert!(
        n == 0
            && err
                .as_ref()
                .is_err_and(|e| e.downcast_ref::<InputBlockedError>().is_some()),
        "deliver into a half-typed input: typed {n}, err {err:?}; want InputBlockedError"
    );
    assert_eq!(env.service.input_blocked("worker"), "it has text in it");
    let res = env
        .service
        .send(
            &bg(),
            "worker",
            "echo second-$((2))",
            SendOptions::default(),
        )
        .unwrap();
    assert!(
        res.held && res.held_count == 2 && !res.input_blocked.is_empty(),
        "send with messages held and the input in use: {res:?}; want held behind the first"
    );
    fake.set(|s| s.input = String::new());
    let start = Instant::now();
    let (n, err) = env.service.deliver_held(
        &bg(),
        "worker",
        &DeliverOptions {
            wait: true,
            interval: Duration::from_millis(50),
            grace: Duration::from_millis(200),
        },
    );
    assert!(
        n == 2 && err.is_ok(),
        "input cleared: typed {n}, err {err:?}; want 2"
    );
    assert!(
        start.elapsed() >= Duration::from_millis(200),
        "typed before the grace"
    );
    let screen = wait_for_output(&env, "worker", "second-2");
    let (i, j) = (screen.find("first-1"), screen.find("second-2"));
    assert!(i.is_some() && i < j, "typed out of order:\n{screen}");

    // With nothing held too, a send to an agent at its prompt holds rather
    // than type into a person's input; --now types anyway.
    fake.set(|s| s.input = "more".into());
    let res = env
        .service
        .send(&bg(), "worker", "echo third", SendOptions::default())
        .unwrap();
    assert!(
        res.held && !res.input_blocked.is_empty(),
        "send with nothing held, input in use: {res:?}; want held"
    );
    let res = env
        .service
        .send(
            &bg(),
            "worker",
            "echo fourth",
            SendOptions {
                now: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        res.typed, 1,
        "send --now, input in use: {res:?}; want typed"
    );
}

// The deliverer's grace starts when the input line empties, not at the
// first check after a blocked attempt.
#[test]
fn test_deliver_held_grace_starts_when_input_empties() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");
    env.service
        .send(&bg(), "worker", "echo late", SendOptions::default())
        .unwrap();
    let cleared = Instant::now() + Duration::from_millis(400);
    fake.set(|s| {
        s.native = Status::Idle;
        s.input = "typing".into();
        s.input_until = Some(cleared);
    });

    let (n, err) = env.service.deliver_held(
        &bg(),
        "worker",
        &DeliverOptions {
            wait: true,
            interval: Duration::from_millis(20),
            grace: Duration::from_millis(300),
        },
    );
    assert!(n == 1 && err.is_ok(), "typed {n}, err {err:?}; want 1");
    let since = Instant::now().saturating_duration_since(cleared);
    assert!(
        since >= Duration::from_millis(300),
        "typed {since:?} after the input line emptied, before the grace"
    );
}
