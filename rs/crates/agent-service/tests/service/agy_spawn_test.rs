//! Go tests/service/agy_spawn_test.go.

use agent_backend::PermissionResponse;
use agent_base::await_;
use agent_service::*;

use crate::common::*;
use crate::require_termd;

#[test]
fn test_agy_spawn_generates_hooks_bundle_and_reports_lifecycle() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into();

    let dir = tempdir();
    let task_file = dir.path().join("task.md").display().to_string();
    std::fs::write(&task_file, "Say hi").unwrap();

    let fake = spawn_fake_agy(
        &env,
        SpawnOptions {
            name: "agy-worker".into(),
            task_file,
            ..Default::default()
        },
    );

    // Verify .gomuxai/agents/agy-worker/.agents/hooks.json exists
    let hooks_path = format!("{}/.agents/hooks.json", env.agent_dir("agy-worker"));
    assert!(
        std::path::Path::new(&hooks_path).exists(),
        "expected hooks bundle at {hooks_path}"
    );
    let spawn_id = spawn_id_of(&fake.report_cmd());

    // Simulate PreToolUse hook report (with --spawn-id and --event)
    let pre = r#"{"conversationId":"conv-99","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo hi"}}}"#;
    env.service
        .report_hook("agy-worker", &spawn_id, "PreToolUse", pre.as_bytes())
        .unwrap();
    let info = env.service.status_info("agy-worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_PROCESSING
            && info.detail == "run_command: echo hi"
            && info.tool == "run_command",
        "after PreToolUse: got {} ({:?}, tool={:?})",
        info.status,
        info.detail,
        info.tool
    );
    assert_eq!(
        env.state_value("agy-worker", STATE_KEY_SESSION_ID),
        "conv-99"
    );

    // Simulate Stop hook report
    let stop = r#"{"conversationId":"conv-99","error":"","fullyIdle":true,"terminationReason":"NO_TOOL_CALL"}"#;
    env.service
        .report_hook("agy-worker", &spawn_id, "Stop", stop.as_bytes())
        .unwrap();
    let info = env.service.status_info("agy-worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE && info.detail == "turn finished",
        "after Stop: got {} ({:?})",
        info.status,
        info.detail
    );

    // Reject a stale spawn id
    assert!(
        env.service
            .report_hook(
                "agy-worker",
                "stale-spawn-id",
                "PreInvocation",
                br#"{"conversationId":"old"}"#
            )
            .is_err(),
        "expected stale spawn id to be rejected"
    );

    // Cleanup should remove the per-agent bundle directory
    env.service.cleanup("agy-worker").unwrap();
    assert!(
        !std::path::Path::new(&env.agent_dir("agy-worker")).exists(),
        "expected agent dir to be removed on cleanup"
    );
}

// Non-hook `agent report exited --as <name> --spawn-id <id>` is run by the
// shell wrapper after agy exits. If the agent was cleaned up and respawned
// with the same name in the meantime, the old wrapper must not mark the new
// spawn exited.
#[test]
fn test_report_rejects_stale_spawn_id() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/bin/agent".into();

    let old = spawn_fake_agy(
        &env,
        SpawnOptions {
            name: "worker".into(),
            ..Default::default()
        },
    );
    let old_spawn = spawn_id_of(&old.report_cmd());
    env.service.cleanup("worker").unwrap();

    let fresh = spawn_fake_agy(
        &env,
        SpawnOptions {
            name: "worker".into(),
            ..Default::default()
        },
    );
    let new_spawn = spawn_id_of(&fresh.report_cmd());
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

    // Late exit wrapper from the previous spawn arrives: must be rejected.
    let _ = env.service.report(
        "worker",
        StatusReport {
            status: AGENT_STATUS_ENDED,
            event: "report".into(),
            spawn_id: old_spawn,
            ..Default::default()
        },
    );
    assert_eq!(
        env.service.status("worker").unwrap(),
        AGENT_STATUS_IDLE,
        "stale exit report overwrote respawn"
    );

    // Current spawn's exit wrapper succeeds.
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                event: "report".into(),
                spawn_id: new_spawn,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(env.service.status("worker").unwrap(), AGENT_STATUS_ENDED);
}

// agy fires PreToolUse before showing its permission dialog and fires nothing
// while the dialog is up. When the dialog is visible at the bottom of the
// pane after PreToolUse, status_info should report waiting and
// respond_permission should work without --force.
#[test]
fn test_agy_waiting_and_approve_when_permission_dialog_visible() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/gomuxai/agent".into();

    let fake = spawn_fake_agy(
        &env,
        SpawnOptions {
            name: "agy-perm".into(),
            ..Default::default()
        },
    );
    let session = env.session_name("agy-perm");
    let spawn_id = spawn_id_of(&fake.report_cmd());

    // PreToolUse fires while no dialog is on screen: status is processing,
    // and approve without --force refuses.
    let pre = r#"{"conversationId":"conv-1","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo gated"}}}"#;
    env.service
        .report_hook("agy-perm", &spawn_id, "PreToolUse", pre.as_bytes())
        .unwrap();
    assert_eq!(
        env.service.status("agy-perm").unwrap(),
        AGENT_STATUS_PROCESSING,
        "without dialog on screen"
    );
    assert!(
        env.service
            .respond_permission(
                "agy-perm",
                PermissionResponse::Approve,
                PermissionOptions::default()
            )
            .is_err(),
        "approve succeeded without dialog on screen"
    );

    // Render agy's permission dialog at the bottom of the agent's pane and
    // block on read so the shell prompt doesn't appear below "esc to cancel".
    let dialog_cmd = r"printf '\nRun this command?\n> 1. Yes, run command\n  2. Yes, and always allow\n  4. No, cancel\nesc to cancel\n'; read -r -n 1 _";
    env.tmux.send_keys(&session, dialog_cmd).unwrap();
    env.tmux.send_special_key(&session, "Enter").unwrap();

    // Wait for the shell to print the dialog lines.
    let mut info = None;
    await_::until("waiting, with the dialog on screen", || {
        let i = env.service.status_info("agy-perm").unwrap();
        let screen = env.tmux.capture(&session, 0).unwrap_or_default();
        let s = format!("status {}; screen:\n{screen}", i.status);
        let ok = i.status == AGENT_STATUS_WAITING;
        info = Some(i);
        (s, ok)
    });
    let info = info.unwrap();
    assert!(
        info.detail == "permission: run_command: echo gated" && info.tool == "run_command",
        "got detail={:?} tool={:?}",
        info.detail,
        info.tool
    );

    // Approve without --force (and with --expect run_command) now succeeds.
    env.service
        .respond_permission(
            "agy-perm",
            PermissionResponse::Approve,
            PermissionOptions {
                expect_tool: "run_command".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("RespondPermission without --force failed: {e:#}"));
    assert_eq!(
        env.service.status("agy-perm").unwrap(),
        AGENT_STATUS_PROCESSING,
        "after approve"
    );
}
