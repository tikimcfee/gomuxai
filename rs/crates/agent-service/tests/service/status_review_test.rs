//! Go tests/service/status_review_test.go: regression tests from the
//! status-from-hooks review.

use std::io::Write as _;
use std::sync::Arc;

use agent_backend::{PaneSession, PermissionResponse, Status};
use agent_service::*;

use crate::common::*;
use crate::require_termd;

// The session id may change (/clear, /resume): the hook's is recorded. (A
// stale hook from a previous spawn's CLI can't reach the agent: place_hook
// finds no pane above a CLI cleanup killed.)
#[test]
fn test_report_hook_accepts_session_change_within_spawn() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    spawn_fake(&env, "worker");

    let payload =
        r#"{"hook_event_name":"SessionStart","source":"clear","session_id":"after-clear"}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    assert_eq!(
        env.state_value("worker", STATE_KEY_SESSION_ID),
        "after-clear"
    );
}

// Repro (real claude): spawn, wait for idle, `kill -9 <claude pid>`. The pane
// drops to bash, Claude's session registry no longer lists the session, and
// `agent status` said "idle (source: report)". `agent watch` then typed a
// queued message into bash, which ran it as a shell command.
#[test]
fn test_status_info_crashed_session_is_not_idle() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");

    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| s.native_gone = true); // the CLI answered, but the session is gone

    let info = env.service.status_info("worker").unwrap();
    assert_ne!(
        info.status, AGENT_STATUS_IDLE,
        "crashed agent reported idle (source {}); the watcher would deliver into the shell",
        info.source
    );
    assert_eq!(info.status, AGENT_STATUS_ENDED);
}

// Repro (real claude): the CLI quits (/exit, SessionEnd), and a person types
// `claude` at the shell left in the pane. The new CLI was launched by hand,
// so no hook reports it; the registry and the process tree say it runs there.
#[test]
fn test_status_info_adopts_the_session_the_pane_runs() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");
    let old = fake.session_id();

    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                result: "old reply".into(),
                transcript: "/t/old.jsonl".into(),
                ..Default::default()
            },
        )
        .unwrap();
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                event: "SessionEnd".into(),
                detail: "prompt_input_exit".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| {
        s.native_gone = true;
        s.pane_session = Some(PaneSession {
            id: "by-hand".into(),
            pid: 4242,
            status: Status::Idle,
            detail: "at its prompt".into(),
        });
    });

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE
            && info.event == EVENT_ADOPTED
            && info.source == STATUS_SOURCE_NATIVE,
        "got {}/{}/{} ({}), want idle/adopted/native",
        info.status,
        info.event,
        info.source,
        info.detail
    );
    for want in ["by-hand", "pid 4242", old.as_str()] {
        assert!(
            info.detail.contains(want),
            "detail {:?} doesn't name {want:?}",
            info.detail
        );
    }
    for (key, want) in [
        (STATE_KEY_SESSION_ID, "by-hand"),
        (STATE_KEY_TRANSCRIPT, ""), // the new session's transcript is no hook's yet
        (STATE_KEY_RESULT, ""),     // nor is the old reply its reply
    ] {
        let e = env.store.get_state("worker", key).unwrap();
        assert!(
            e.as_ref().is_some_and(|e| e.value == want),
            "{key} = {e:?}, want {want:?}"
        );
    }

    // A CLI started by hand carries no hooks until the global hook reports
    // it, so nothing could hand it held messages: they are typed.
    assert!(
        !env.service.holds_messages_named("worker"),
        "holds messages for a session no hook has reached"
    );

    // Recorded once: now the session on record is the one running.
    fake.set(|s| {
        s.native_gone = false;
        s.native_found = true;
        s.native = Status::Processing;
        s.session_id = "by-hand".into();
    });
    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_PROCESSING && info.source == STATUS_SOURCE_NATIVE,
        "after adopting: {}/{}, want processing/native",
        info.status,
        info.source
    );
}

// A pane that can't be read says so, beside the exit it can't overrule.
#[test]
fn test_status_info_pane_error_is_shown() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");

    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| {
        s.native_gone = true;
        s.pane_err = Some("two Claude Code processes run at the same depth".into());
    });

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_ENDED
            && info.detail.contains("its pane: two Claude Code processes"),
        "got {} ({})",
        info.status,
        info.detail
    );
}

// failingNativeBackend can't reach the backend CLI at all (e.g. claude not on
// PATH for the watcher): the last report is still the best answer.
#[test]
fn test_status_info_native_unavailable_keeps_report() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");
    let mut o = fake_hook_overrides("fakehooks", &fake);
    o.native_status = Some(Box::new(|_| {
        (
            Status::Unknown,
            false,
            Some(
                agent_base::gostd::os::PathError::new(
                    "stat",
                    "claude",
                    std::io::Error::from(std::io::ErrorKind::NotFound),
                )
                .into(),
            ),
        )
    }));
    register_claude_fake_as(&env.backends, "fakehooks", &Arc::new(o));

    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE && info.source == STATUS_SOURCE_REPORT,
        "got {} from {}, want idle from report",
        info.status,
        info.source
    );
}

// KNOWN BUG. Repro (real claude): ask the agent to use AskUserQuestion. Hooks
// report PermissionRequest "permission: AskUserQuestion", native says waiting,
// and `agent approve --always` pressed "2", answering the question ("Blue")
// on the user's behalf. approve should only answer real permission dialogs.
#[test]
fn test_respond_permission_refuses_question_dialogs() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Favorite color?"}]}}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    match env.service.respond_permission(
        "worker",
        PermissionResponse::Approve,
        PermissionOptions::default(),
    ) {
        Ok(_) => {
            panic!("approve answered an AskUserQuestion dialog; want a refusal without --force")
        }
        Err(e) => eprintln!("refused: {e:#}"),
    }
}

// KNOWN BUG (fixed). Report wrote _status/_event/_detail as separate
// statements, so concurrent reports interleaved: a stress run of 30
// concurrent `agent report --hook` processes left 170/300 report groups torn
// in a DB trace.
#[test]
fn test_report_concurrent_reports_stay_consistent() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let reports = [
        StatusReport {
            status: AGENT_STATUS_WAITING,
            event: "PermissionRequest".into(),
            detail: "permission: Bash".into(),
            ..Default::default()
        },
        StatusReport {
            status: AGENT_STATUS_PROCESSING,
            event: "PostToolUse".into(),
            detail: "Read: f".into(),
            ..Default::default()
        },
    ];
    for round in 0..50 {
        std::thread::scope(|s| {
            for r in &reports {
                let svc = &env.service;
                s.spawn(move || {
                    let _ = svc.report("worker", r.clone());
                });
            }
        });
        let (st, ev) = (
            env.state_value("worker", STATE_KEY_STATUS),
            env.state_value("worker", STATE_KEY_EVENT),
        );
        assert_eq!(
            st == "waiting",
            ev == "PermissionRequest",
            "round {round}: torn report: _status={st} _event={ev}"
        );
    }
}

#[test]
fn test_respond_permission_force_answers_question_dialogs() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"AskUserQuestion","tool_input":{}}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("--force should answer the dialog: {e:#}"));
}

#[test]
fn test_respond_permission_expect_tool() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash","tool_input":{"command":"rm -rf build"}}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    assert_eq!(
        env.service.status_info("worker").unwrap().tool,
        "Bash",
        "pending tool"
    );

    // The prompt you inspected was replaced by a different one: refuse.
    assert!(
        env.service
            .respond_permission(
                "worker",
                PermissionResponse::Approve,
                PermissionOptions {
                    expect_tool: "Edit".into(),
                    ..Default::default()
                }
            )
            .is_err(),
        "approved a Bash prompt while expecting Edit"
    );
    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions {
                expect_tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("expected Bash approval to go through: {e:#}"));
}

// Repro (real claude): reply "A" to one prompt, then send another (or /clear).
// _result kept saying "A" until the next Stop, so a coordinator polling it
// read the previous turn's reply as this one's. And a turn that launched
// background work stopped with an interim reply ("started") stored as the
// result and status idle, while the work and the real reply were still to come.
#[test]
fn test_report_hook_result_tracks_current_turn() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let result = || env.state_value("worker", STATE_KEY_RESULT);
    let hook = |payload: &str| {
        env.service
            .report_hook("worker", "", "", payload.as_bytes())
            .unwrap()
    };

    hook(
        r#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"A","background_tasks":[]}"#,
    );
    assert_eq!(result(), "A");

    hook(
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"now launch the job"}"#,
    );
    assert_eq!(
        result(),
        "",
        "after a new prompt, want empty (stale reply from the previous turn)"
    );

    hook(
        r#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"started","background_tasks":[{"id":"b1","type":"shell","status":"running","command":"make"}],"session_crons":[]}"#,
    );
    assert_eq!(
        env.service.status("worker").unwrap(),
        AGENT_STATUS_PROCESSING,
        "with a background task running"
    );
    assert_eq!(
        result(),
        "",
        "want empty (interim reply recorded as the result)"
    );

    // The task's completion starts a turn of its own.
    hook(
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"<task-notification>b1 completed</task-notification>"}"#,
    );
    hook(
        r#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"build passed","background_tasks":[],"session_crons":[]}"#,
    );
    assert_eq!(result(), "build passed", "want the final reply");

    hook(r#"{"hook_event_name":"SessionStart","session_id":"s2","source":"clear"}"#);
    assert_eq!(result(), "", "after /clear, want empty");
}

// Repro (race): the agent is waiting on a Bash prompt; someone else answers
// it while `agent deny` is between its status check and the keypress. deny
// then sent Escape into the turn now running, cancelling the work that was
// just approved. deny must re-check the screen and refuse.
#[test]
fn test_respond_permission_refuses_when_prompt_left_screen() {
    require_termd!();
    let env = setup_test_env();
    let fake = spawn_fake(&env, "worker");

    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash","tool_input":{"command":"go test ./..."}}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    fake.set(|s| s.prompt_gone = true); // answered meanwhile; status still says waiting

    assert!(
        env.service
            .respond_permission(
                "worker",
                PermissionResponse::Deny,
                PermissionOptions::default()
            )
            .is_err(),
        "deny sent Escape with no prompt on screen; it would cancel the running turn"
    );
    assert_eq!(
        env.state_value("worker", STATE_KEY_EVENT),
        "PermissionRequest",
        "a refused deny must not record itself"
    );
    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions {
                force: true,
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("--force should skip the screen check: {e:#}"));
}

// Claude writes a status word gomuxai doesn't know: that agent's report
// stands, and status -v can say why.
#[test]
fn test_status_info_unknown_registry_value() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| {
        s.native_found = true;
        s.native = Status::Unknown;
        s.native_err = Some(
            r#"status "compacting" is not one gomuxai knows, written by Claude Code 2.2.0"#.into(),
        );
    });

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE
            && info.source == STATUS_SOURCE_REPORT
            && info.native_err.contains(r#""compacting""#),
        "got {}/{}, native {:?}",
        info.status,
        info.source,
        info.native_err
    );
}

// Repro (Claude Code 2.1.319): Claude moved an agent's conversation into a
// background session its daemon runs under the agent's CLI. The old
// session's registry entry kept "busy", its transcript ended with
// continued-in, and status said processing until someone noticed. Status
// follows the move to the session the pane now runs, keeping the agent's
// reply.
#[test]
fn test_status_info_follows_a_session_claude_moved() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");
    let old = fake.session_id();

    let dir = tempdir();
    let old_path = dir.path().join(format!("{old}.jsonl"));
    std::fs::write(
        &old_path,
        format!(r#"{{"type":"user","sessionId":"{old}"}}"#) + "\n",
    )
    .unwrap();
    let old_path_s = old_path.display().to_string();
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                event: "PostToolUse".into(),
                transcript: old_path_s,
                session_id: old.clone(),
                ..Default::default()
            },
        )
        .unwrap();
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "Stop".into(),
                result: "the reply".into(),
                ..Default::default()
            },
        )
        .unwrap();
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                event: "PostToolUse".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| {
        s.native_found = true;
        s.native = Status::Processing; // the old entry, frozen at busy
    });

    // Not moved yet: the pane's session is the one on record.
    fake.set(|s| {
        s.pane_session = Some(PaneSession {
            id: old.clone(),
            pid: 830233,
            status: Status::Processing,
            detail: String::new(),
        })
    });
    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_PROCESSING && info.event != EVENT_SESSION_MOVED,
        "before the move: {}/{}",
        info.status,
        info.event
    );

    // The move: the transcript says so, and the pane runs the new session.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&old_path)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"continued-in","sessionId":"{old}","continuedInSessionId":"moved"}}"#
    )
    .unwrap();
    drop(f);
    // A continued-in the pane doesn't run (another process took it) isn't followed.
    let info = env.service.status_info("worker").unwrap();
    assert_ne!(
        info.event, EVENT_SESSION_MOVED,
        "followed a session its pane doesn't run: {}",
        info.detail
    );
    fake.set(|s| {
        s.pane_session = Some(PaneSession {
            id: "moved".into(),
            pid: 1776758,
            status: Status::Idle,
            detail: "at its prompt".into(),
        });
        s.session_id = "moved".into(); // the registry lists the new session, idle
        s.native = Status::Idle;
    });

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE && info.source == STATUS_SOURCE_NATIVE,
        "after the move: {}/{} ({}), want idle/native",
        info.status,
        info.source,
        info.detail
    );
    let moved_path = dir.path().join("moved.jsonl").display().to_string();
    for (key, want) in [
        (STATE_KEY_SESSION_ID, "moved"),
        (STATE_KEY_TRANSCRIPT, moved_path.as_str()),
        (STATE_KEY_RESULT, "the reply"), // the same conversation: its reply stands
        (STATE_KEY_EVENT, EVENT_SESSION_MOVED),
    ] {
        let e = env.store.get_state("worker", key).unwrap();
        assert!(
            e.as_ref().is_some_and(|e| e.value == want),
            "{key} = {e:?}, want {want:?}"
        );
    }
    // Followed once: the next check finds the session on record running.
    assert_eq!(
        env.service.status_info("worker").unwrap().status,
        AGENT_STATUS_IDLE,
        "second check"
    );
}
