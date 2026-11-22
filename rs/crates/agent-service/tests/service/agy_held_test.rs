//! Go tests/service/agy_held_test.go.

use std::sync::{Arc, Mutex};

use agent_backend::PermissionResponse;
use agent_backend::namedagents::{self, AgyConfig};
use agent_service::*;
use agent_store::Time;

use crate::common::*;
use crate::require_termd;

/// fakeAgyScreen is the agy backend with its screen readings set by the
/// test: the test session is a plain shell, which draws none of agy's.
#[derive(Default)]
struct ScreenState {
    at_prompt: bool,
    how: String,
    question: bool,
    input: String,
}

#[derive(Clone, Default)]
struct FakeAgyScreen(Arc<Mutex<ScreenState>>);

impl FakeAgyScreen {
    fn set(&self, f: impl FnOnce(&mut ScreenState)) {
        f(&mut self.0.lock().unwrap());
    }
}

/// spawnAgyHolding spawns a fake agy agent with hooks wired, so messages to
/// it can be held, and puts it mid-turn.
fn spawn_agy_holding(env: &mut TestEnv, name: &str) -> FakeAgyScreen {
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    let fake = FakeAgyScreen::default();
    let (a, b, c) = (fake.clone(), fake.clone(), fake.clone());
    let o = Arc::new(AgyOverrides {
        name: "fakeagyscreen".into(),
        launch_command_with_hooks: Some(Box::new(|_, _| "true".to_string())),
        at_prompt: Some(Box::new(move |_, _| {
            let s = a.0.lock().unwrap();
            s.at_prompt.then(|| s.how.clone())
        })),
        question_prompt_visible: Some(Box::new(move |_, _| b.0.lock().unwrap().question)),
        permission_prompt_visible: Some(Box::new(|_, _| false)),
        input_text: Some(Box::new(move |_, _| {
            Some(c.0.lock().unwrap().input.clone())
        })),
        append_prompt_flag: Some(Box::new(|_, (cmd, _)| cmd)),
        ..Default::default()
    });
    env.backends.register_factory("fakeagyscreen", move |_| {
        Box::new(AgyFake::new(
            namedagents::new_agy_backend_with_config(AgyConfig::default()),
            &o,
        ))
    });
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: name.into(),
                backend: "fakeagyscreen".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));
    agy_hook(
        env,
        name,
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    fake
}

/// agyHook runs an agy hook as the hook itself does, returning its output.
fn agy_hook(env: &TestEnv, name: &str, event: &str, payload: &str) -> Option<Vec<u8>> {
    let (out, err) = env.service.handle_hook(name, "", event, payload.as_bytes());
    if let Err(e) = err {
        panic!("HandleHook {event} {payload}: {e:#}");
    }
    out
}

/// reportedLongAgo makes the last report older than the time the screen
/// gets to catch up with a hook.
fn reported_long_ago(env: &TestEnv, name: &str) {
    let mut e = env
        .store
        .get_state(name, STATE_KEY_STATUS)
        .unwrap()
        .expect("a reported status");
    e.updated_at = Time(chrono::Local::now().fixed_offset() - chrono::Duration::seconds(10));
    env.store.set_state(&e).unwrap();
}

// A message to a busy agy agent is held, and the next PostInvocation hands
// every held message over as its own userMessage, with force_continue. With
// nothing held, PostInvocation prints nothing and records nothing.
#[test]
fn test_agy_held_until_post_invocation_delivers() {
    require_termd!();
    let mut env = setup_test_env();
    spawn_agy_holding(&mut env, "agyw");
    agy_hook(
        &env,
        "agyw",
        "PreToolUse",
        r#"{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"sleep 5"}}}"#,
    );

    let out = agy_hook(
        &env,
        "agyw",
        "PostInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    assert!(
        out.is_none(),
        "PostInvocation with nothing held printed {out:?}"
    );
    assert_eq!(
        env.state_value("agyw", STATE_KEY_DETAIL),
        "run_command: sleep 5",
        "PostInvocation with nothing held recorded a detail"
    );

    for (i, m) in ["[from lead@h] first", "[from a person on h] second"]
        .iter()
        .enumerate()
    {
        let res = env
            .service
            .send(
                &bg(),
                "agyw",
                m,
                SendOptions {
                    from: "lead".into(),
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(
            res.held && res.held_count == i + 1 && res.held_until == "its current step ends",
            "send {} to a busy agy agent: {res:?}; want held",
            i + 1
        );
    }
    let sent_at = env.state_value("agyw", STATE_KEY_SENT_AT);
    // Its other hooks only report.
    for ev in ["PostToolUse", "Stop"] {
        let out = agy_hook(
            &env,
            "agyw",
            ev,
            r#"{"conversationId":"c1","fullyIdle":false,"toolCall":{"name":"run_command","args":{"CommandLine":"sleep 5"}}}"#,
        );
        assert!(out.is_none(), "{ev} printed {out:?}");
    }
    agy_hook(
        &env,
        "agyw",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":1}"#,
    );

    let out = agy_hook(
        &env,
        "agyw",
        "PostInvocation",
        r#"{"conversationId":"c1","invocationNum":1}"#,
    )
    .expect("delivery output");
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let steps = v["injectSteps"].as_array().cloned().unwrap_or_default();
    let step_msg = steps
        .first()
        .and_then(|s| s["userMessage"].as_str())
        .unwrap_or_default();
    assert!(
        steps.len() == 1
            && v["terminationBehavior"] == "force_continue"
            && step_msg.contains("[from lead@h] first")
            && step_msg.contains("[from a person on h] second")
            && step_msg.find("[from lead@h] first") < step_msg.find("[from a person on h] second"),
        "delivery output {}",
        String::from_utf8_lossy(&out)
    );
    assert_eq!(
        env.service.held_count("agyw"),
        0,
        "messages still held after delivery"
    );
    let (st, ev, d) = (
        env.state_value("agyw", STATE_KEY_STATUS),
        env.state_value("agyw", STATE_KEY_EVENT),
        env.state_value("agyw", STATE_KEY_DETAIL),
    );
    assert!(
        st == "processing" && ev == EVENT_DELIVERED && d == "2 held messages delivered",
        "after delivery: {st}/{ev}/{d:?}, want processing/delivered/2 held messages delivered"
    );
    let turn_at = env.state_value("agyw", STATE_KEY_TURN_AT);
    assert!(
        turn_at > sent_at,
        "turn_at {turn_at} not after sent_at {sent_at}: wait --next would miss the delivered step"
    );

    let out = agy_hook(
        &env,
        "agyw",
        "PostInvocation",
        r#"{"conversationId":"c1","invocationNum":2}"#,
    );
    assert!(out.is_none(), "second PostInvocation printed {out:?}");
    agy_hook(
        &env,
        "agyw",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(
        env.state_value("agyw", STATE_KEY_STATUS),
        "idle",
        "after Stop"
    );
}

// On agy, `--later` skips PostInvocation and delivers at Stop, while
// `--interrupt` blocks PreToolUse with strict `{"decision":"deny"}` and
// delivers coalesced (with full superseded text) on PostInvocation.
#[test]
fn test_agy_later_and_interrupt_pre_tool_use_into_post_invocation() {
    require_termd!();
    let mut env = setup_test_env();
    spawn_agy_holding(&mut env, "agyint");
    let mut a = env.store.get_agent("agyint").unwrap().unwrap();
    a.spawned_by = "lead".into();
    env.store.replace_agent(&a).unwrap();

    let later = env
        .service
        .send(
            &bg(),
            "agyint",
            "[from peer@h] check docs when done",
            SendOptions {
                from: "peer".into(),
                urgency: "later".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(later.held && later.held_until == "its turn ends");
    assert!(
        agy_hook(
            &env,
            "agyint",
            "PostInvocation",
            r#"{"conversationId":"c1","invocationNum":0}"#
        )
        .is_none(),
        "PostInvocation must skip --later"
    );

    let r1 = env
        .service
        .send(
            &bg(),
            "agyint",
            "[from lead@h] deploy v1\nwith feature flag X",
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
            "agyint",
            "[from lead@h] STOP: abort deploy, keep flag X only",
            SendOptions {
                from: "lead".into(),
                replace: true,
                urgency: "interrupt".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(r2.superseded_ids, vec![r1.message_id]);

    let pre = agy_hook(
        &env,
        "agyint",
        "PreToolUse",
        r#"{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"deploy.sh"}}}"#,
    )
    .expect("PreToolUse interrupt deny");
    let pv: serde_json::Value = serde_json::from_slice(&pre).unwrap();
    assert_eq!(pv["decision"], "deny");

    let post = agy_hook(
        &env,
        "agyint",
        "PostInvocation",
        r#"{"conversationId":"c1","invocationNum":1}"#,
    )
    .expect("PostInvocation coalesced delivery");
    let v: serde_json::Value = serde_json::from_slice(&post).unwrap();
    let steps = v["injectSteps"].as_array().unwrap();
    assert_eq!(steps.len(), 1);
    let msg = steps[0]["userMessage"].as_str().unwrap();
    assert!(msg.contains("URGENT INTERRUPT"));
    assert!(
        msg.contains("with feature flag X"),
        "full superseded body kept:\n{msg}"
    );
    assert_eq!(
        env.service.held_count("agyint"),
        1,
        "--later still held for Stop"
    );

    let stop_out = agy_hook(
        &env,
        "agyint",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    )
    .expect("Stop delivers --later");
    let sv: serde_json::Value = serde_json::from_slice(&stop_out).unwrap();
    assert_eq!(sv["decision"], "continue");
    assert!(
        sv["reason"]
            .as_str()
            .unwrap()
            .contains("check docs when done")
    );
    assert_eq!(env.service.held_count("agyint"), 0);
}

// Esc and Ctrl+C fire no hook: once the last report has stood a while and
// agy is back at its prompt, it is idle, and its held messages get typed.
#[test]
fn test_agy_interrupted_turn_is_idle() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_agy_holding(&mut env, "agyi");
    agy_hook(
        &env,
        "agyi",
        "PreToolUse",
        r#"{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"sleep 30"}}}"#,
    );
    let res = env
        .service
        .send(
            &bg(),
            "agyi",
            "echo typed-$((40+2))",
            SendOptions::default(),
        )
        .unwrap();
    assert!(res.held, "send to a busy agent: {res:?}");

    fake.set(|s| {
        s.at_prompt = true;
        s.how = "interrupted".into();
    });
    // Right after a hook, the screen may not have caught up.
    assert_eq!(
        env.service.status_info("agyi").unwrap().status,
        AGENT_STATUS_PROCESSING,
        "just after a hook"
    );
    reported_long_ago(&env, "agyi");
    let info = env.service.status_info("agyi").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE
            && info.detail == "interrupted; at its prompt"
            && info.source == STATUS_SOURCE_SCRAPE,
        "interrupted: {info:?}; want idle from the screen"
    );
    assert_eq!(
        info.reported_status, AGENT_STATUS_PROCESSING,
        "want the last report kept"
    );

    fake.set(|s| s.input = "a person's text".into());
    let (n, err) = env
        .service
        .deliver_held(&bg(), "agyi", &DeliverOptions::default());
    assert!(
        n == 0 && err.is_err(),
        "text in the input line: typed {n}, err {err:?}; want refused"
    );
    fake.set(|s| s.input = String::new());
    let (n, err) = env
        .service
        .deliver_held(&bg(), "agyi", &DeliverOptions::default());
    assert!(
        n == 1 && err.is_ok(),
        "at its prompt: typed {n}, err {err:?}; want 1"
    );
    wait_for_output(&env, "agyi", "typed-42");

    // A Stop that left background work running isn't overruled.
    agy_hook(
        &env,
        "agyi",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":false}"#,
    );
    reported_long_ago(&env, "agyi");
    fake.set(|s| s.how = String::new());
    assert_eq!(
        env.service.status_info("agyi").unwrap().status,
        AGENT_STATUS_PROCESSING,
        "background work"
    );
    // Without the interrupted line, it says what it knows.
    agy_hook(
        &env,
        "agyi",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    reported_long_ago(&env, "agyi");
    let info = env.service.status_info("agyi").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE && info.detail.contains("no hook reported"),
        "at its prompt: {} ({})",
        info.status,
        info.detail
    );
}

// ask_question's dialog has no hook: after its PreToolUse, the dialog on
// screen makes the agent waiting, so a send holds instead of answering it,
// and approve/deny and typed sends refuse.
#[test]
fn test_agy_question_is_waiting() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = spawn_agy_holding(&mut env, "agyq");
    agy_hook(
        &env,
        "agyq",
        "PreToolUse",
        r#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"options":["red","green"],"question":"Which color?"}]}}}"#,
    );

    assert_eq!(
        env.service.status_info("agyq").unwrap().status,
        AGENT_STATUS_PROCESSING,
        "no dialog on screen yet"
    );
    fake.set(|s| s.question = true);
    let info = env.service.status_info("agyq").unwrap();
    assert!(
        info.status == AGENT_STATUS_WAITING
            && info.detail == "question: ask_question: Which color? (red, green)"
            && info.tool == "ask_question",
        "question on screen: {info:?}"
    );

    let res = env
        .service
        .send(&bg(), "agyq", "[from lead@h] hi", SendOptions::default())
        .unwrap();
    assert!(
        res.held && res.status == AGENT_STATUS_WAITING,
        "send at the question: {res:?}; want held"
    );
    let err = env
        .service
        .send(
            &bg(),
            "agyq",
            "[from lead@h] now",
            SendOptions {
                now: true,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("pick an answer"),
        "send --now at the question: {err:#}; want refused"
    );
    let err = env
        .service
        .respond_permission(
            "agyq",
            PermissionResponse::Approve,
            PermissionOptions::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("question for the user"),
        "approve at the question: {err:#}; want refused"
    );
    // Even after the turn's end: the deliverer doesn't type into it.
    fake.set(|s| s.at_prompt = false);
    agy_hook(
        &env,
        "agyq",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true}"#,
    );
    let (n, err) = env
        .service
        .deliver_held(&bg(), "agyq", &DeliverOptions::default());
    assert!(
        n == 0 && err.is_err(),
        "deliver at a question: typed {n}, err {err:?}; want refused"
    );
}

// agy's Stop hook carries no reply text in its payload: the service reads the
// turn's closing reply from the transcript (from Stop's transcriptPath or the
// recorded _transcript), stores it as _result, skips interim replies while
// background tasks run, and clears _result when the next turn starts.
#[test]
fn test_agy_stop_stores_result_and_clears_on_next_turn() {
    require_termd!();
    let mut env = setup_test_env();
    spawn_agy_holding(&mut env, "agyr");

    let tdir = tempdir();
    let logs = tdir
        .path()
        .join("brain")
        .join("c1")
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let tpath = logs.join("transcript_full.jsonl");
    let tpath_str = tpath.display().to_string();

    let append = |line: &str| {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tpath)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    };

    // Turn 1: prompt -> tool call -> tool result -> closing reply.
    append(
        r#"{"step_index":0,"type":"USER_INPUT","content":"<USER_REQUEST>\nCheck build\n</USER_REQUEST>"}"#,
    );
    append(
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"Running tests first.","tool_calls":[{"name":"run_command","args":{"CommandLine":"cargo test"}}]}"#,
    );
    append(
        r#"{"step_index":2,"type":"GENERIC","status":"DONE","content":"Command exited with code 0."}"#,
    );
    append(
        r#"{"step_index":3,"type":"PLANNER_RESPONSE","thinking":"All green.","content":"All 12 tests passed."}"#,
    );

    let stop1 = serde_json::json!({
        "conversationId": "c1",
        "transcriptPath": tpath_str,
        "fullyIdle": true,
        "terminationReason": "NO_TOOL_CALL",
    })
    .to_string();
    agy_hook(&env, "agyr", "Stop", &stop1);
    assert_eq!(
        env.state_value("agyr", STATE_KEY_RESULT),
        "All 12 tests passed."
    );
    assert_eq!(env.state_value("agyr", STATE_KEY_TRANSCRIPT), tpath_str);

    // Turn 2 starts: PreInvocation(0) clears _result; PreInvocation(1) keeps it empty.
    agy_hook(
        &env,
        "agyr",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    assert_eq!(
        env.state_value("agyr", STATE_KEY_RESULT),
        "",
        "want _result cleared on new turn"
    );
    agy_hook(
        &env,
        "agyr",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":1}"#,
    );
    assert_eq!(env.state_value("agyr", STATE_KEY_RESULT), "");

    // Turn 2 launches background work and stops with fullyIdle=false: interim
    // reply in transcript must NOT be stored as _result.
    append(
        r#"{"step_index":4,"type":"USER_INPUT","content":"<USER_REQUEST>\nRun long job\n</USER_REQUEST>"}"#,
    );
    append(
        r#"{"step_index":5,"type":"PLANNER_RESPONSE","content":"","tool_calls":[{"name":"run_command","args":{"CommandLine":"make slow"}}]}"#,
    );
    append(
        r#"{"step_index":6,"type":"GENERIC","status":"RUNNING","content":"Tool is running as a background task with task id: c1/task-1"}"#,
    );
    append(
        r#"{"step_index":7,"type":"PLANNER_RESPONSE","content":"Started background job, waiting."}"#,
    );
    agy_hook(
        &env,
        "agyr",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":false,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(env.state_value("agyr", STATE_KEY_STATUS), "processing");
    assert_eq!(
        env.state_value("agyr", STATE_KEY_RESULT),
        "",
        "interim reply stored while background task runs"
    );

    // Background job finishes; Stop payload omits transcriptPath, relying on recorded _transcript.
    append(
        r#"{"step_index":8,"type":"SYSTEM_MESSAGE","content":"<SYSTEM_MESSAGE>\n[Message] sender=c1/task-1 content=Task id \"c1/task-1\" finished with result:\nCommand exited with code 0.\n</SYSTEM_MESSAGE>"}"#,
    );
    append(r#"{"step_index":9,"type":"PLANNER_RESPONSE","content":"Long job finished cleanly."}"#);
    agy_hook(
        &env,
        "agyr",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agyr",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(
        env.state_value("agyr", STATE_KEY_RESULT),
        "Long job finished cleanly."
    );
}

// When Stop arrives slightly before agy finishes flushing the turn's closing
// PLANNER_RESPONSE line, the service retries briefly rather than storing an
// empty result or the previous turn's reply, and still finishes well under 1s
// when a turn wrote no reply.
#[test]
fn test_agy_stop_waits_for_unflushed_transcript() {
    require_termd!();
    let mut env = setup_test_env();
    spawn_agy_holding(&mut env, "agyf");

    let tdir = tempdir();
    let logs = tdir
        .path()
        .join("brain")
        .join("c1")
        .join(".system_generated")
        .join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let tpath = logs.join("transcript_full.jsonl");
    let tpath_str = tpath.display().to_string();

    // Turn 1 finished earlier; Turn 2's USER_INPUT is on disk, but its closing
    // PLANNER_RESPONSE flushes 60ms after Stop starts.
    std::fs::write(
        &tpath,
        concat!(
            r#"{"step_index":0,"type":"USER_INPUT","content":"first"}"#,
            "\n",
            r#"{"step_index":1,"type":"PLANNER_RESPONSE","content":"stale turn 1 reply"}"#,
            "\n",
            r#"{"step_index":2,"type":"USER_INPUT","content":"second"}"#,
            "\n",
        ),
    )
    .unwrap();

    let writer_path = tpath.clone();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(60));
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&writer_path)
            .unwrap();
        writeln!(
            f,
            r#"{{"step_index":3,"type":"PLANNER_RESPONSE","content":"flushed turn 2 reply"}}"#
        )
        .unwrap();
    });

    let stop = serde_json::json!({
        "conversationId": "c1",
        "transcriptPath": tpath_str,
        "fullyIdle": true,
        "terminationReason": "NO_TOOL_CALL",
    })
    .to_string();
    agy_hook(&env, "agyf", "Stop", &stop);
    writer.join().unwrap();
    assert_eq!(
        env.state_value("agyf", STATE_KEY_RESULT),
        "flushed turn 2 reply"
    );

    // A turn that ends with no reply at all stays bounded well under a second.
    agy_hook(
        &env,
        "agyf",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&tpath)
            .unwrap();
        writeln!(
            f,
            r#"{{"step_index":4,"type":"USER_INPUT","content":"third"}}"#
        )
        .unwrap();
    }
    let started = std::time::Instant::now();
    agy_hook(
        &env,
        "agyf",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_millis(800),
        "unflushed Stop took {elapsed:?}, want well under 1s"
    );
    assert_eq!(env.state_value("agyf", STATE_KEY_RESULT), "");
}

// Claude agents get --name <agent> at both spawn and resume, without changing
// any other byte of the launch command, and cli_session_name / session_name
// read the recorded session name back for both claude and agy.
#[test]
fn test_claude_spawn_and_resume_name_and_cli_session_name() {
    require_termd!();
    let mut env = setup_test_env();
    let bin = env.dir().join("agent");
    std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    env.cfg_mut().agent_binary = bin.display().to_string();

    let settings = namedagents::claude_settings_path().unwrap();
    let backup = env.dir().join("backups").display().to_string();
    namedagents::install_claude_hooks(&settings, &env.cfg().agent_binary, &backup, true).unwrap();

    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "named-worker".into(),
                backend: "claude".into(),
                model: "opus".into(),
                skip_permissions: true,
                ..Default::default()
            },
        )
        .unwrap();

    let sid = env.state_value("named-worker", STATE_KEY_SESSION_ID);
    assert!(!sid.is_empty(), "want pinned session id");
    let launch_path = format!("{}/launch.sh", env.agent_dir("named-worker"));
    let spawn_script = std::fs::read_to_string(&launch_path).unwrap();
    let want_spawn_head = format!(
        "claude --dangerously-skip-permissions --settings '{}' --session-id '{}' --name 'named-worker' --model 'opus' --append-system-prompt '",
        namedagents::CLAUDE_SPAWN_SETTINGS,
        sid
    );
    assert!(
        spawn_script.contains(&want_spawn_head),
        "spawn launch.sh:\n{spawn_script}\nwant substring:\n{want_spawn_head}"
    );

    // End its terminal session and resume it on its conversation: --resume and
    // --name both ride the relaunch, with no --session-id.
    env.tmux
        .kill_session(&env.session_name("named-worker"))
        .unwrap();
    env.service
        .resume(
            &bg(),
            ResumeOptions {
                name: "named-worker".into(),
                session_id: sid.clone(),
                skip_permissions: true,
                ..Default::default()
            },
        )
        .unwrap();
    let resume_script = std::fs::read_to_string(&launch_path).unwrap();
    let want_resume_head = format!(
        "claude --dangerously-skip-permissions --settings '{}' --resume '{}' --name 'named-worker' --model 'opus' --append-system-prompt '",
        namedagents::CLAUDE_SPAWN_SETTINGS,
        sid
    );
    assert!(
        resume_script.contains(&want_resume_head) && !resume_script.contains("--session-id"),
        "resume launch.sh:\n{resume_script}\nwant substring:\n{want_resume_head}"
    );

    // Recorded session name lookup through the service for claude and agy.
    let claude_jsonl = env.dir().join(format!("{sid}.jsonl"));
    std::fs::write(
        &claude_jsonl,
        format!("{{\"type\":\"custom-title\",\"sessionId\":\"{sid}\",\"customTitle\":\"named-worker\"}}\n"),
    )
    .unwrap();
    env.set_state(
        "named-worker",
        STATE_KEY_TRANSCRIPT,
        &claude_jsonl.display().to_string(),
    );
    assert_eq!(env.service.cli_session_name("named-worker"), "named-worker");
    assert_eq!(
        session_name("claude", &sid, &claude_jsonl.display().to_string()),
        "named-worker"
    );

    spawn_agy_holding(&mut env, "agyn");
    let agy_root = env.dir().join("agy-data");
    let agy_logs = agy_root
        .join("brain")
        .join("c1")
        .join(".system_generated")
        .join("logs");
    let agy_ann = agy_root.join("annotations");
    std::fs::create_dir_all(&agy_logs).unwrap();
    std::fs::create_dir_all(&agy_ann).unwrap();
    let agy_tpath = agy_logs.join("transcript_full.jsonl");
    std::fs::write(
        &agy_tpath,
        "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hi\"}\n",
    )
    .unwrap();
    std::fs::write(agy_ann.join("c1.pbtxt"), "title:\"Auto Generated Title\"\n").unwrap();
    assert_eq!(
        session_name("agy", "c1", &agy_tpath.display().to_string()),
        "Auto Generated Title"
    );
}

/// Registers an agy fake (`backend_name`) whose pane runs `stty -echo; exec cat`
/// after printing `READY`, so tests can read the exact lines typed into the
/// pane in order.
fn register_agy_cat(env: &mut TestEnv, backend_name: &str) -> FakeAgyScreen {
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    let fake = FakeAgyScreen::default();
    let (a, b, c) = (fake.clone(), fake.clone(), fake.clone());
    let name_str = backend_name.to_string();
    let o = Arc::new(AgyOverrides {
        name: name_str.clone(),
        launch_command_with_hooks: Some(Box::new(|_, _| {
            "stty -echo; printf 'RDY''_MARK\\n'; exec cat #".to_string()
        })),
        at_prompt: Some(Box::new(move |_, _| {
            let s = a.0.lock().unwrap();
            s.at_prompt.then(|| s.how.clone())
        })),
        question_prompt_visible: Some(Box::new(move |_, _| b.0.lock().unwrap().question)),
        permission_prompt_visible: Some(Box::new(|_, _| false)),
        input_text: Some(Box::new(move |_, _| {
            Some(c.0.lock().unwrap().input.clone())
        })),
        append_prompt_flag: Some(Box::new(|_, (cmd, _)| cmd)),
        ..Default::default()
    });
    let o_reg = Arc::clone(&o);
    env.backends.register(Arc::new(AgyFake::new(
        namedagents::new_agy_backend_with_config(AgyConfig::default()),
        &o_reg,
    )));
    env.backends.register_factory(&name_str, move |cfg| {
        Box::new(AgyFake::new(
            namedagents::new_agy_backend_with_config(AgyConfig {
                skip_permissions: cfg.skip_permissions,
                agent_dir: cfg.agent_dir,
                cwd: cfg.cwd,
                resume: cfg.resume,
            }),
            &o,
        ))
    });
    fake
}

/// Returns every non-empty line printed after the `RDY_MARK` marker in `name`'s pane.
fn typed_lines(env: &TestEnv, name: &str) -> Vec<String> {
    let screen = env.service.capture(name, 100, false).unwrap_or_default();
    let mut after_ready = false;
    let mut out = Vec::new();
    for line in screen.lines() {
        let t = line.trim();
        if !after_ready {
            if t == "RDY_MARK" {
                after_ready = true;
            }
            continue;
        }
        if !t.is_empty() {
            out.push(t.to_string());
        }
    }
    out
}

// After an agy agent's first completed turn (idle Stop), the service types
// `/rename <agent.name>` once per conversation id without advancing `_sent_at`
// past `_turn_at`, skips interim/cancelled Stops, never re-types on later turns
// of the same conversation, and names a new conversation id if one starts.
#[test]
fn test_agy_names_conversation_after_first_turn_once_per_session() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = register_agy_cat(&mut env, "agycat");
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "agy-namer".into(),
                backend: "agycat".into(),
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_output(&env, "agy-namer", "RDY_MARK");
    // At its idle prompt before its first hook: status reads that off the
    // screen (an input box under a busy footer would read processing).
    fake.set(|s| s.at_prompt = true);

    // Send a prompt (sets _sent_at) and start turn 1 (sets _turn_at > _sent_at).
    env.service
        .send(&bg(), "agy-namer", "first prompt", SendOptions::default())
        .unwrap();
    wait_for_output(&env, "agy-namer", "first prompt");
    std::thread::sleep(std::time::Duration::from_millis(5));
    agy_hook(
        &env,
        "agy-namer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );

    // Interim Stop (background tasks running) and USER_CANCELED Stop do not name yet.
    agy_hook(
        &env,
        "agy-namer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":false,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(env.state_value("agy-namer", STATE_KEY_NAMED), "");
    agy_hook(
        &env,
        "agy-namer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"USER_CANCELED"}"#,
    );
    assert_eq!(env.state_value("agy-namer", STATE_KEY_NAMED), "");
    assert_eq!(typed_lines(&env, "agy-namer"), vec!["first prompt"]);

    // First completed turn's Stop names c1 once and keeps _turn_at > _sent_at.
    agy_hook(
        &env,
        "agy-namer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agy-namer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    wait_for_output(&env, "agy-namer", "/rename agy-namer");
    assert_eq!(env.state_value("agy-namer", STATE_KEY_NAMED), "c1");
    assert_eq!(
        typed_lines(&env, "agy-namer"),
        vec!["first prompt", "/rename agy-namer"]
    );

    let info = env.service.status_info("agy-namer").unwrap();
    assert_eq!(info.status, AGENT_STATUS_IDLE);
    assert!(
        info.turn_at.after(&info.sent_at),
        "typing /rename advanced sent_at ({:?}) past turn_at ({:?}), which would stall wait --next",
        info.sent_at,
        info.turn_at
    );

    // Subsequent turns of c1 do NOT type `/rename` again.
    agy_hook(
        &env,
        "agy-namer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agy-namer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert_eq!(
        typed_lines(&env, "agy-namer"),
        vec!["first prompt", "/rename agy-namer"]
    );

    // A new conversation c2 (e.g. after /clear) is named once too.
    agy_hook(
        &env,
        "agy-namer",
        "PreInvocation",
        r#"{"conversationId":"c2","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agy-namer",
        "Stop",
        r#"{"conversationId":"c2","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    agent_base::await_::until("second /rename for c2", || {
        let lines = typed_lines(&env, "agy-namer");
        let ok = lines == vec!["first prompt", "/rename agy-namer", "/rename agy-namer"];
        (format!("{lines:?}"), ok)
    });
    assert_eq!(env.state_value("agy-namer", STATE_KEY_NAMED), "c2");
}

// `/rename` is never typed into a dialog, into a non-empty input line, or
// ahead of held messages waiting to be delivered: each defers without looping
// until a later Stop finds the agent idle at an empty prompt with nothing held.
#[test]
fn test_agy_naming_defers_for_dialogs_nonempty_input_and_held_messages() {
    require_termd!();
    let mut env = setup_test_env();
    let fake = register_agy_cat(&mut env, "agydeferbe");
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "agy-defer".into(),
                backend: "agydeferbe".into(),
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_output(&env, "agy-defer", "RDY_MARK");

    // 1. Dialog on screen at Stop -> deferred.
    fake.set(|s| s.question = true);
    agy_hook(
        &env,
        "agy-defer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agy-defer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(env.state_value("agy-defer", STATE_KEY_NAMED), "");
    assert!(
        typed_lines(&env, "agy-defer").is_empty(),
        "typed into question dialog"
    );

    // 2. Dialog gone, but a person's draft sits in the input line -> deferred.
    fake.set(|s| {
        s.question = false;
        s.input = "person typing".into();
    });
    agy_hook(
        &env,
        "agy-defer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agy-defer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(env.state_value("agy-defer", STATE_KEY_NAMED), "");
    assert!(
        typed_lines(&env, "agy-defer").is_empty(),
        "typed into non-empty input line"
    );

    // 3. Input empty, but a held message is waiting -> `/rename` must NOT jump
    //    ahead of the held message; deliver_held types the held message first,
    //    and the next Stop types `/rename` after it.
    fake.set(|s| s.input = String::new());
    agy_hook(
        &env,
        "agy-defer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    let held_res = env
        .service
        .send(
            &bg(),
            "agy-defer",
            "[from lead@h] held-first",
            SendOptions {
                from: "lead".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(held_res.held);

    // Suppose Stop arrives with the message still held (e.g. held right after PostInvocation).
    agy_hook(
        &env,
        "agy-defer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    assert_eq!(
        env.state_value("agy-defer", STATE_KEY_NAMED),
        "",
        "named ahead of held message"
    );
    assert!(
        typed_lines(&env, "agy-defer").is_empty(),
        "typed /rename ahead of held message"
    );

    // Existing deliverer delivers the held message first.
    let (n, err) = env
        .service
        .deliver_held(&bg(), "agy-defer", &DeliverOptions::default());
    assert!(n == 1 && err.is_ok(), "deliver_held: n={n}, err={err:?}");
    wait_for_output(&env, "agy-defer", "[from lead@h] held-first");
    assert_eq!(
        typed_lines(&env, "agy-defer"),
        vec!["[from lead@h] held-first"]
    );

    // Turn started by the delivered held message ends -> now `/rename` is typed, after it.
    agy_hook(
        &env,
        "agy-defer",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "agy-defer",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    wait_for_output(&env, "agy-defer", "/rename agy-defer");
    assert_eq!(env.state_value("agy-defer", STATE_KEY_NAMED), "c1");
    assert_eq!(
        typed_lines(&env, "agy-defer"),
        vec!["[from lead@h] held-first", "/rename agy-defer"]
    );
}

// Conversations whose recorded title (`session_name`) already equals the
// agent's name skip `/rename` (including after `resume`), while unnamed resumed
// conversations and adopted agy sessions get named at their first Stop.
#[test]
fn test_agy_naming_skips_already_named_and_handles_resume_and_adopt() {
    require_termd!();
    let mut env = setup_test_env();
    register_agy_cat(&mut env, "agy");

    let agy_root = env.dir().join("agy-store");
    let make_conv = |cid: &str, title: &str| -> String {
        let logs = agy_root
            .join("brain")
            .join(cid)
            .join(".system_generated")
            .join("logs");
        let ann = agy_root.join("annotations");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::create_dir_all(&ann).unwrap();
        let tpath = logs.join("transcript_full.jsonl");
        std::fs::write(
            &tpath,
            "{\"step_index\":0,\"type\":\"USER_INPUT\",\"content\":\"hi\"}\n",
        )
        .unwrap();
        if !title.is_empty() {
            std::fs::write(
                ann.join(format!("{cid}.pbtxt")),
                format!("title:{title:?}\n"),
            )
            .unwrap();
        }
        tpath.display().to_string()
    };

    // 1. Conversation c1 already has title == agent.name -> sets _named without typing /rename.
    let c1_tpath = make_conv("c1", "agy-skip");
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "agy-skip".into(),
                backend: "agy".into(),
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_output(&env, "agy-skip", "RDY_MARK");
    agy_hook(
        &env,
        "agy-skip",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    let stop_c1 = serde_json::json!({
        "conversationId": "c1",
        "transcriptPath": c1_tpath,
        "fullyIdle": true,
        "terminationReason": "NO_TOOL_CALL",
    })
    .to_string();
    agy_hook(&env, "agy-skip", "Stop", &stop_c1);
    assert_eq!(env.state_value("agy-skip", STATE_KEY_NAMED), "c1");
    assert_eq!(env.service.cli_session_name("agy-skip"), "agy-skip");
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        typed_lines(&env, "agy-skip").is_empty(),
        "re-typed /rename when title already matched"
    );

    // 2. End and resume on c1 (already named): resume clears _named, and the
    //    resumed run's first Stop sees title == "agy-skip" and still skips typing.
    env.tmux
        .kill_session(&env.session_name("agy-skip"))
        .unwrap();
    env.service
        .resume(
            &bg(),
            ResumeOptions {
                name: "agy-skip".into(),
                session_id: "c1".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        env.state_value("agy-skip", STATE_KEY_NAMED),
        "",
        "want _named cleared on resume"
    );
    wait_for_output(&env, "agy-skip", "RDY_MARK");
    agy_hook(
        &env,
        "agy-skip",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":0}"#,
    );
    agy_hook(&env, "agy-skip", "Stop", &stop_c1);
    assert_eq!(env.state_value("agy-skip", STATE_KEY_NAMED), "c1");
    std::thread::sleep(std::time::Duration::from_millis(30));
    assert!(
        typed_lines(&env, "agy-skip").is_empty(),
        "resumed already-named session typed /rename"
    );

    // 3. End and resume on c2 whose title is an auto-generated summary != "agy-skip" -> renames!
    let c2_tpath = make_conv("c2", "Fix Build Warnings");
    env.tmux
        .kill_session(&env.session_name("agy-skip"))
        .unwrap();
    env.service
        .resume(
            &bg(),
            ResumeOptions {
                name: "agy-skip".into(),
                session_id: "c2".into(),
                ..Default::default()
            },
        )
        .unwrap();
    wait_for_output(&env, "agy-skip", "RDY_MARK");
    agy_hook(
        &env,
        "agy-skip",
        "PreInvocation",
        r#"{"conversationId":"c2","invocationNum":0}"#,
    );
    let stop_c2 = serde_json::json!({
        "conversationId": "c2",
        "transcriptPath": c2_tpath,
        "fullyIdle": true,
        "terminationReason": "NO_TOOL_CALL",
    })
    .to_string();
    agy_hook(&env, "agy-skip", "Stop", &stop_c2);
    wait_for_output(&env, "agy-skip", "/rename agy-skip");
    assert_eq!(env.state_value("agy-skip", STATE_KEY_NAMED), "c2");
    assert_eq!(typed_lines(&env, "agy-skip"), vec!["/rename agy-skip"]);

    // 4. Adopted agy session follows the exact same rule at its first Stop.
    let user_sess = format!("{}adopted-pane", env.cfg().session_prefix);
    env.tmux
        .create_session(&user_sess, &std::collections::HashMap::new())
        .unwrap();
    env.tmux
        .send_keys(&user_sess, "stty -echo; printf 'RDY''_MARK\\n'; exec cat #")
        .unwrap();
    env.tmux.send_special_key(&user_sess, "Enter").unwrap();
    wait_for_session(&env, &user_sess, &["RDY_MARK"]);
    env.service
        .adopt(AdoptOptions {
            name: "adopted-agy".into(),
            session_name: user_sess,
            backend: "agy".into(),
            ..Default::default()
        })
        .unwrap();
    agy_hook(
        &env,
        "adopted-agy",
        "PreInvocation",
        r#"{"conversationId":"c-adopted","invocationNum":0}"#,
    );
    agy_hook(
        &env,
        "adopted-agy",
        "Stop",
        r#"{"conversationId":"c-adopted","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );
    wait_for_output(&env, "adopted-agy", "/rename adopted-agy");
    assert_eq!(env.state_value("adopted-agy", STATE_KEY_NAMED), "c-adopted");
    assert_eq!(
        typed_lines(&env, "adopted-agy"),
        vec!["/rename adopted-agy"]
    );
    // Kill the adopted user session cleanly before drop (adopt cleanup leaves user sessions running).
    let _ = env
        .tmux
        .kill_session(&format!("{}adopted-pane", env.cfg().session_prefix));
}

// On agy with `queuedMessages: "send-immediately"`, `send --now` mid-turn is
// picked up by `PreInvocation` (`invocationNum >= 1`) in the running turn
// without firing a separate prompt-submit hook: `_turn_at` must advance past
// `_sent_at` so `wait --next` returns on `Stop` instead of timing out, whereas
// an ordinary `PreInvocation(>=1)` with only a `--later` message held must NOT
// advance `_turn_at` prematurely.
#[test]
fn test_agy_send_now_mid_turn_advances_turn_at_for_wait_next() {
    require_termd!();
    let mut env = setup_test_env();
    spawn_agy_holding(&mut env, "agynow");

    // 1. Hold a `--later` message mid-turn: `PreInvocation(1)` must NOT advance
    // `_turn_at` past `_sent_at` while `--later` is still held for turn end.
    env.service
        .send(
            &bg(),
            "agynow",
            "[from peer@h] after turn",
            SendOptions {
                from: "peer@h".into(),
                urgency: "later".into(),
                ..Default::default()
            },
        )
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    agy_hook(
        &env,
        "agynow",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":1}"#,
    );
    let info_later = env.service.status_info("agynow").unwrap();
    assert!(
        !info_later.turn_at.after(&info_later.sent_at),
        "PreInvocation(1) without --now advanced turn_at ({:?}) past --later sent_at ({:?})",
        info_later.turn_at,
        info_later.sent_at
    );

    // 2. `send --now` mid-turn sweeps held messages and sets `_sent_at`; the
    // following `PreInvocation(2)` consumes `_now_at`, advancing `_turn_at >
    // _sent_at`, so `Stop` immediately satisfies `wait --next`.
    let now_res = env
        .service
        .send(
            &bg(),
            "agynow",
            "[from lead@h] steer right now",
            SendOptions {
                now: true,
                from: "lead@h".into(),
                urgency: "now".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(!now_res.held && now_res.typed == 2);
    std::thread::sleep(std::time::Duration::from_millis(5));
    agy_hook(
        &env,
        "agynow",
        "PreInvocation",
        r#"{"conversationId":"c1","invocationNum":2}"#,
    );
    std::thread::sleep(std::time::Duration::from_millis(5));
    agy_hook(
        &env,
        "agynow",
        "Stop",
        r#"{"conversationId":"c1","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#,
    );

    let info = env.service.status_info("agynow").unwrap();
    assert!(
        info.sent_at.before(&info.turn_at) && info.turn_at.before(&info.reported_at),
        "want sent < turn < reported after mid-turn --now, got {:?}, {:?}, {:?}",
        info.sent_at.0,
        info.turn_at.0,
        info.reported_at.0
    );

    let (wait_ctx, _cancel) =
        agent_service::Context::with_timeout(&bg(), std::time::Duration::from_secs(1));
    let waited = agent_bus::wait(
        &wait_ctx,
        None,
        agent_bus::WaitOptions {
            host: String::new(),
            agent: "agynow".into(),
            for_statuses: vec!["idle".into()],
            fail_statuses: Vec::new(),
            next: true,
            after: agent_bus::Time::default(),
            current: Some(Box::new(|_| {
                let i = env.service.status_info("agynow")?;
                Ok(agent_bus::Observation {
                    status: i.status.to_string(),
                    detail: i.detail,
                    reported_status: i.reported_status.to_string(),
                    reported_at: agent_bus::Time(i.reported_at.0),
                    sent_at: agent_bus::Time(i.sent_at.0),
                    turn_at: agent_bus::Time(i.turn_at.0),
                })
            })),
            interval: std::time::Duration::from_millis(20),
        },
    );
    assert_eq!(
        waited.map_err(|(s, e)| format!("{s}: {e:#}")),
        Ok("idle".to_string()),
        "wait --next should immediately see the completed --now turn"
    );
}
