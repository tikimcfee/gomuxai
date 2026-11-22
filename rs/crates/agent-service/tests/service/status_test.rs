//! Go tests/service/status_test.go.

use agent_backend::{PermissionResponse, Status};
use agent_service::*;

use crate::common::*;
use crate::require_termd;

#[test]
fn test_spawn_wires_hooks_to_agent_binary() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/gomux ai/agent".into();

    let fake = spawn_fake(&env, "hooked");

    let sid = fake.session_id();
    assert!(!sid.is_empty(), "expected a session id to be pinned");
    assert_eq!(env.state_value("hooked", STATE_KEY_SESSION_ID), sid);
    // Its hooks are every session's, found by pane: no spawn id.
    assert!(
        env.store
            .get_state("hooked", STATE_KEY_SPAWN_ID)
            .unwrap()
            .is_none(),
        "a Claude agent got a spawn id"
    );
}

// Without the global hook a Claude agent couldn't report: spawn says so and
// how to fix it, rather than start an agent nothing can follow.
#[test]
fn test_spawn_refuses_without_global_hook() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/agent".into();

    let fake = register_fake_hook(&env);
    fake.set(|s| s.hooks_err = Some("no hook on Stop: agent hooks install registers it".into()));
    let err = env
        .service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "unhooked".into(),
                backend: "fakehooks".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("agent hooks install"),
        "spawn err = {err:#}"
    );
}

#[test]
fn test_spawn_no_hooks_without_agent_binary() {
    require_termd!();
    let env = setup_test_env();

    let fake = spawn_fake(&env, "plain");

    assert_eq!(
        fake.session_id(),
        "",
        "no session should be pinned without AgentBinary"
    );
    assert!(
        env.store
            .get_state("plain", STATE_KEY_SESSION_ID)
            .unwrap()
            .is_none()
    );
}

#[test]
fn test_report_hook_updates_status() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    spawn_fake(&env, "worker");

    let payload = r#"{"hook_event_name":"PermissionRequest","session_id":"s-2","tool_name":"Bash","tool_input":{"command":"rm -rf build"}}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_WAITING && info.source == STATUS_SOURCE_REPORT,
        "got {} from {}, want waiting from report",
        info.status,
        info.source
    );
    assert_eq!(info.detail, "permission: Bash: rm -rf build");
    // The hook's session id replaces the spawn-time one (e.g. after /clear).
    assert_eq!(
        env.state_value("worker", STATE_KEY_SESSION_ID),
        "s-2",
        "session id not updated from hook"
    );
}

#[test]
fn test_report_hook_stop_stores_result() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let payload =
        r#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"Found 3 issues."}"#;
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    assert_eq!(
        env.state_value("worker", STATE_KEY_RESULT),
        "Found 3 issues."
    );
}

// Claude's idle_prompt Notification, ~60s into background work after a
// Stop, must not record idle (with no result) before that work is done.
#[test]
fn test_report_hook_idle_prompt_during_background_work() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");
    let hook = |payload: &str| {
        env.service
            .report_hook("worker", "", "", payload.as_bytes())
            .unwrap();
        env.service.status_info("worker").unwrap()
    };
    let idle_prompt =
        r#"{"hook_event_name":"Notification","session_id":"s","notification_type":"idle_prompt"}"#;

    hook(
        r#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"started","background_tasks":[{"type":"subagent","status":"running"}]}"#,
    );
    let info = hook(idle_prompt);
    assert!(
        info.status == AGENT_STATUS_PROCESSING && info.detail.contains("background"),
        "after idle_prompt: {} ({}), want processing with the background detail",
        info.status,
        info.detail
    );

    // Once the work is done and its turn has ended, idle_prompt records as usual.
    hook(
        r#"{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"done","background_tasks":[]}"#,
    );
    let info = hook(idle_prompt);
    assert!(
        info.status == AGENT_STATUS_IDLE && info.event == "Notification",
        "after the last Stop: {} from {}, want idle from Notification",
        info.status,
        info.event
    );
}

#[test]
fn test_report_hook_stores_transcript() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let path = "/home/user/.claude/projects/p/s.jsonl";
    let payload = format!(
        r#"{{"hook_event_name":"UserPromptSubmit","session_id":"s","transcript_path":"{path}"}}"#
    );
    env.service
        .report_hook("worker", "", "", payload.as_bytes())
        .unwrap();
    // A payload without a path keeps the one stored.
    env.service
        .report_hook(
            "worker",
            "",
            "",
            br#"{"hook_event_name":"Stop","session_id":"s"}"#,
        )
        .unwrap();
    assert_eq!(env.state_value("worker", STATE_KEY_TRANSCRIPT), path);
}

// The transcripts an agent used are listed beside its log, so they outlive
// cleanup like the log does.
#[test]
fn test_transcripts_outlive_cleanup() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    for path in ["/t/one.jsonl", "/t/one.jsonl", "/t/two.jsonl"] {
        let payload = format!(r#"{{"hook_event_name":"PostToolUse","transcript_path":"{path}"}}"#);
        env.service
            .report_hook("worker", "", "", payload.as_bytes())
            .unwrap();
    }
    let want = vec![
        TranscriptRef {
            backend: "fakehooks".into(),
            path: "/t/one.jsonl".into(),
        },
        TranscriptRef {
            backend: "fakehooks".into(),
            path: "/t/two.jsonl".into(),
        },
    ];
    assert_eq!(env.service.transcripts("worker"), want);
    env.service.cleanup("worker").unwrap();
    assert_eq!(env.service.transcripts("worker"), want, "after cleanup");
}

#[test]
fn test_report_hook_unknown_agent() {
    require_termd!();
    let env = setup_test_env();

    let err = env
        .service
        .report_hook("ghost", "", "", br#"{"hook_event_name":"Stop"}"#)
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "expected not found error, got {err:#}"
    );
}

#[test]
fn test_status_info_native_overrides_stale_report() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    let fake = spawn_fake(&env, "worker");

    // A permission prompt was interrupted: no hook fired, the CLI is idle again.
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                event: "PermissionRequest".into(),
                detail: "permission: Bash".into(),
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| {
        s.native = Status::Idle;
        s.native_found = true;
    });

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE && info.source == STATUS_SOURCE_NATIVE,
        "got {} from {}, want idle from native",
        info.status,
        info.source
    );
    assert_eq!(info.detail, "", "stale detail should be dropped");

    // When they agree, the hook detail rides along.
    fake.set(|s| s.native = Status::Waiting);
    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_WAITING && info.detail == "permission: Bash",
        "got {} {:?}, want waiting with detail",
        info.status,
        info.detail
    );
}

#[test]
fn test_status_info_falls_back_to_report_when_native_missing() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();
    spawn_fake(&env, "worker"); // native_found stays false

    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                event: "SessionEnd".into(),
                ..Default::default()
            },
        )
        .unwrap();

    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_ENDED && info.source == STATUS_SOURCE_REPORT,
        "got {} from {}, want exited from report",
        info.status,
        info.source
    );
}

#[test]
fn test_status_info_scrapes_without_reports() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    let info = env.service.status_info("worker").unwrap();
    assert_eq!(info.source, STATUS_SOURCE_SCRAPE);
}

#[test]
fn test_respond_permission_requires_waiting() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_PROCESSING,
                ..Default::default()
            },
        )
        .unwrap();

    let err = env
        .service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions::default(),
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("not waiting"),
        "expected refusal, got {err:#}"
    );
}

#[test]
fn test_respond_permission_approve_and_deny() {
    require_termd!();
    let env = setup_test_env();
    spawn_fake(&env, "worker");

    for (resp, want) in [
        (PermissionResponse::Approve, AGENT_STATUS_PROCESSING),
        (PermissionResponse::Deny, AGENT_STATUS_IDLE),
    ] {
        env.service
            .report(
                "worker",
                StatusReport {
                    status: AGENT_STATUS_WAITING,
                    ..Default::default()
                },
            )
            .unwrap();
        env.service
            .respond_permission("worker", resp, PermissionOptions::default())
            .unwrap_or_else(|e| panic!("RespondPermission({resp:?}): {e:#}"));
        let st = env.service.status("worker").unwrap();
        assert_eq!(st, want, "after {resp:?}");
    }
}

#[test]
fn test_parse_agent_status() {
    assert!(
        parse_agent_status("waiting").is_ok(),
        "waiting should be valid"
    );
    assert!(
        parse_agent_status("done").is_err(),
        "done should be rejected"
    );
}

// Cleanup removes .gomuxai/agents/<name>, so names must not escape that dir.
#[test]
fn test_spawn_rejects_unsafe_names() {
    require_termd!();
    let env = setup_test_env();

    for name in [
        "",
        "..",
        "../..",
        "a/b",
        ".hidden",
        "-flag",
        "has space",
        "semi;colon",
    ] {
        let err = env
            .service
            .spawn(
                &bg(),
                SpawnOptions {
                    name: name.into(),
                    backend: "claude".into(),
                    ..Default::default()
                },
            )
            .unwrap_err();
        assert!(
            err.to_string().contains("invalid agent name"),
            "Spawn({name:?}): err = {err:#}"
        );
    }
    for name in ["helper", "api-analyzer", "rv_2", "A1"] {
        validate_agent_name(name).unwrap_or_else(|e| panic!("ValidateAgentName({name:?}): {e:#}"));
    }
}

// Every spawned agent gets a briefing on reaching other agents, loaded at
// launch without writing into its project.
#[test]
fn test_spawn_briefs_the_agent() {
    use std::os::unix::fs::PermissionsExt;
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/gomux/agent".into();
    env.cfg_mut().host = "h1".into();

    spawn_fake_with(
        &env,
        "worker",
        SpawnOptions {
            spawned_by: "lead".into(),
            ..Default::default()
        },
    );

    let path = format!("{}/instructions.md", env.agent_dir("worker"));
    let data = std::fs::read_to_string(&path).expect("instructions written");
    for want in [
        r#"agent "worker" on host "h1""#,
        r#"agent "lead""#,
        "full path, /opt/gomux/agent",
        "[from NAME@HOST]",
        "agent send",
        "agent spawn worker-review --backend agy",
        "agent roster pick",
        "agent cleanup worker-review",
    ] {
        assert!(data.contains(want), "instructions missing {want:?}");
    }
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // The launch command loads them (the fake backend launches `true`). It
    // runs from launch.sh in the agent's directory, so the shell is only
    // typed a short command.
    let launch = std::fs::read_to_string(format!("{}/launch.sh", env.agent_dir("worker")))
        .expect("launch script written");
    assert!(
        launch.contains("--append-system-prompt '")
            && launch.contains(r#"agent "worker" on host "h1""#),
        "launch command doesn't load the instructions:\n{launch}"
    );
    wait_for_session(&env, &env.session_name("worker"), &["launch.sh"]);

    // Cleanup removes them with the agent's directory.
    env.service.cleanup("worker").unwrap();
    assert!(
        !std::path::Path::new(&path).exists(),
        "instructions survived cleanup"
    );
}

// A message's Enter answers whatever dialog is on screen (Yes at a permission
// prompt, "No, exit" at Claude's trust prompt), so send refuses at dialogs.
#[test]
fn test_send_refuses_at_dialogs() {
    require_termd!();
    let env = setup_test_env();
    let fake = spawn_fake(&env, "worker");

    // A permission dialog on screen.
    fake.set(|s| s.prompt_gone = false);
    let err = env
        .service
        .send(&bg(), "worker", "hello", SendOptions::default())
        .unwrap_err();
    assert!(
        err.to_string().contains("permission prompt on screen"),
        "send at a permission dialog: err = {err:#}"
    );
    fake.set(|s| s.prompt_gone = true);

    // Claude's trust dialog on screen (shown here by the shell echoing it).
    env.service
        .send_unchecked(
            &bg(),
            "worker",
            r"printf ' Quick safety check: Is this a project you created or one you trust?\n\n ❯ No, exit\n   Yes, I trust this folder\n\n Enter to confirm\n'; read -r -n 1 _",
        )
        .unwrap();
    // The printed line, not the typed command line, which holds it too.
    let session = env.session_name("worker");
    wait_pane(&env, &session, "the trust dialog printed", |screen| {
        has_line(screen, "Yes, I trust this folder")
    });
    let err = env
        .service
        .send(&bg(), "worker", "hello", SendOptions::default())
        .unwrap_err();
    assert!(
        err.to_string().contains("trust prompt"),
        "send at a trust dialog: err = {err:#}"
    );

    // No dialog: send goes through. Send reads the visible screen, which
    // `clear` empties. The Enter first ends the read that kept the shell's
    // prompt off the dialog.
    env.tmux.send_special_key(&session, "Enter").unwrap();
    env.service
        .send_unchecked(&bg(), "worker", "clear")
        .unwrap();
    wait_pane(
        &env,
        &session,
        "the dialog cleared off the screen",
        |screen| !screen.contains("trust this folder"),
    );
    env.service
        .send(&bg(), "worker", "echo ok", SendOptions::default())
        .unwrap();
}
