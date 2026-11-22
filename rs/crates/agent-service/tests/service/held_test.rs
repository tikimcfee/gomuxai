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
    let ctx_text = additional_context(out);
    assert!(
        ctx_text.starts_with("2 messages arrived while you were working, oldest first")
            && ctx_text.contains("[from lead@h] first")
            && ctx_text.contains("[from a person on h] second")
            && ctx_text.find("[from lead@h] first") < ctx_text.find("[from a person on h] second"),
        "unexpected coalesced Stop output: {ctx_text}"
    );
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

// Claude PostToolBatch delivers held messages mid-turn (coalesced, keeping full
// superseded text, skipping `--later` messages), ignores empty/subagent batches,
// and PreToolUse interrupt denies the tool, marks the turn interrupted, and
// lets deliver_held type with 0s grace.
#[test]
fn test_claude_post_tool_batch_coalescing_full_superseded_text_and_pre_tool_interrupt() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    let mut a = env.store.get_agent("worker").unwrap().unwrap();
    a.spawned_by = "lead".into();
    env.store.replace_agent(&a).unwrap();
    busy(&env, &fake, "worker");

    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"cargo test"}}"#,
    );
    assert!(
        stop_hook(
            &env,
            &fake,
            "worker",
            r#"{"hook_event_name":"PostToolBatch"}"#
        )
        .is_none(),
        "empty PostToolBatch must print nothing"
    );
    assert_eq!(
        env.state_value("worker", STATE_KEY_DETAIL),
        "Bash: cargo test"
    );

    // `--later` message must skip mid-turn PostToolBatch.
    let later = env
        .service
        .send(
            &bg(),
            "worker",
            "[from peer@h] run clippy at end of turn",
            SendOptions {
                from: "peer".into(),
                urgency: "later".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(later.held && later.held_until == "its turn ends");

    // Multi-line draft replaced by a relative follow-up: full superseded text is kept!
    let r1 = env
        .service
        .send(
            &bg(),
            "worker",
            "[from lead@h] Step 1: refactor auth.rs\nStep 2: keep Option<Token> on parse failure",
            SendOptions {
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let r2 = env
        .service
        .send(
            &bg(),
            "worker",
            "[from lead@h] Skip Step 1, only do Step 2 from above",
            SendOptions {
                from: "lead".into(),
                replace: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r2.superseded_ids, vec![r1.message_id]);
    assert_eq!(
        r2.held_count, 2,
        "only active pending messages (later + r2) counted"
    );

    // Subagent PostToolBatch must NOT steal main session messages.
    assert!(
        stop_hook(
            &env,
            &fake,
            "worker",
            r#"{"hook_event_name":"PostToolBatch","agent_id":"sub-agent-1"}"#
        )
        .is_none()
    );

    // Main-session PostToolBatch claims r2 + superseded r1, leaving `later` for Stop.
    let out = stop_hook(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"PostToolBatch"}"#,
    )
    .expect("PostToolBatch output");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PostToolBatch");
    assert_eq!(
        v["systemMessage"],
        "[gomuxai] Delivered 1 held message (1 superseded) mid-turn"
    );
    let ctx_str = v["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert!(
        ctx_str.contains("Step 2: keep Option<Token> on parse failure"),
        "full superseded multiline text must be preserved:\n{ctx_str}"
    );
    assert!(ctx_str.contains(&format!(
        "[SUPERSEDED by #{} — context only, do NOT act on this]",
        r2.message_id
    )));
    assert_eq!(env.service.held_count("worker"), 1, "--later still held");

    // `--replace` after r2 was delivered warns with `already_delivered_id == r2.message_id`!
    let r3 = env
        .service
        .send(
            &bg(),
            "worker",
            "echo urgent_stop-$((20+2))",
            SendOptions {
                from: "lead".into(),
                replace: true,
                urgency: "interrupt".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r3.already_delivered_id, r2.message_id);
    assert_eq!(r3.already_delivered_via, "PostToolBatch");

    // PreToolUse with interrupt pending returns deny + continue:false AND flips
    // reported status to idle/interrupted so deliver_held delivers with 0s grace
    // even when native registry status is unavailable!
    fake.set(|s| {
        s.native_found = false;
    });
    let pre_out = stop_hook(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"PreToolUse","cwd":"/work","tool_name":"Bash","tool_input":{"command":"/opt/x/agent list"}}"#,
    )
    .expect("PreToolUse interrupt deny");
    let pv: serde_json::Value = serde_json::from_slice(&pre_out).unwrap();
    assert_eq!(pv["continue"], false);
    assert_eq!(pv["hookSpecificOutput"]["permissionDecision"], "deny");
    assert_eq!(env.state_value("worker", STATE_KEY_STATUS), "idle");

    let (typed, err) = env.service.deliver_held(
        &bg(),
        "worker",
        &DeliverOptions {
            wait: true,
            interval: Duration::from_millis(20),
            grace: Duration::from_secs(10),
        },
    );
    assert!(typed == 2 && err.is_ok(), "typed={typed}, err={err:?}");
    wait_for_output(&env, "worker", "urgent_stop-22");
}

// `send --now` always records a SQLite receipt ID, sweeps held messages ahead
// of itself when the input line is empty, and leaves held messages waiting when
// a human has draft text in the input box.
#[test]
fn test_send_now_records_receipt_and_sweeps_held_messages_when_input_empty() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");

    // 1. `held_count == 0` -> `--now` still records `message_id > 0` in SQLite!
    let r0 = env
        .service
        .send(
            &bg(),
            "worker",
            "echo now_zero-$((10+1))",
            SendOptions {
                now: true,
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r0.message_id > 0 && r0.typed == 1);
    let rec0 = env
        .service
        .message_receipt("worker", r0.message_id)
        .unwrap();
    assert_eq!(rec0.status, "delivered");
    assert_eq!(rec0.delivery_via, "now");

    // 2. Pending message held -> `--now` with empty input sweeps and coalesces it!
    let r1 = env
        .service
        .send(
            &bg(),
            "worker",
            "echo held_before-$((10+2))",
            SendOptions {
                from: "peer".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r1.held);
    let r2 = env
        .service
        .send(
            &bg(),
            "worker",
            "echo now_sweeps-$((10+3))",
            SendOptions {
                now: true,
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r2.message_id > 0 && !r2.held && r2.typed == 2);
    assert_eq!(env.service.held_count("worker"), 0);
    let screen = wait_for_output(&env, "worker", "now_sweeps-13");
    assert!(screen.find("held_before-12") < screen.find("now_sweeps-13"));
}

// Idle `send` records real message IDs and applies `--replace` / `--supersede`
// against pending messages left over from an interrupted turn.
#[test]
fn test_idle_send_applies_replace_with_real_message_id() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    busy(&env, &fake, "worker");

    let r1 = env
        .service
        .send(
            &bg(),
            "worker",
            "echo stale_draft",
            SendOptions {
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r1.held);

    // Turn ends via Esc (native status turns idle with r1 still pending).
    fake.set(|s| s.native = Status::Idle);
    let r2 = env
        .service
        .send(
            &bg(),
            "worker",
            "echo fresh_plan-$((30+4))",
            SendOptions {
                from: "lead".into(),
                replace: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert!(r2.message_id > 0 && r2.typed == 1);
    assert_eq!(r2.superseded_ids, vec![r1.message_id]);
    let rec1 = env
        .service
        .message_receipt("worker", r1.message_id)
        .unwrap();
    assert_eq!(rec1.status, "superseded");
    assert_eq!(rec1.superseded_by, r2.message_id);
    wait_for_output(&env, "worker", "fresh_plan-34");
}

// `check_can_interrupt` allows people and true `spawned_by` ancestors, and
// rejects peers, self, and bare local names colliding with a remote parent!
#[test]
fn test_interrupt_enforces_host_aware_spawned_by_ancestry() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_holding(&mut env, "worker");
    // Spawn a local agent also named "lead" on this host ("local" / ""), while
    // `worker`'s true parent is `lead@laptop-2` on another host!
    let _local_lead = spawn_holding(&mut env, "lead");
    let mut w = env.store.get_agent("worker").unwrap().unwrap();
    w.spawned_by = "lead@laptop-2".into();
    env.store.replace_agent(&w).unwrap();
    busy(&env, &fake, "worker");

    // Local `lead` must NOT match `lead@laptop-2`!
    let local_err = env
        .service
        .send(
            &bg(),
            "worker",
            "stop",
            SendOptions {
                from: "lead".into(),
                urgency: "interrupt".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        local_err.to_string().contains("cannot interrupt"),
        "local collision allowed: {local_err:#}"
    );

    // Self-interrupt rejected.
    assert!(
        env.service
            .send(
                &bg(),
                "worker",
                "stop",
                SendOptions {
                    from: "worker".into(),
                    urgency: "interrupt".into(),
                    ..Default::default()
                },
            )
            .is_err()
    );

    // True remote parent `lead@laptop-2` is allowed!
    let ok = env
        .service
        .send(
            &bg(),
            "worker",
            "[from lead@laptop-2] stop",
            SendOptions {
                from: "lead@laptop-2".into(),
                urgency: "interrupt".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(ok.held && ok.interrupted);
}
