//! Go tests/service/agent_service_test.go.

use std::time::Duration;

use agent_base::await_;
use agent_service::*;

use crate::common::*;
use crate::require_termd;

fn spawn(env: &TestEnv, name: &str, backend: &str) -> agent_store::Agent {
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: name.into(),
                backend: backend.into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn failed: {e:#}"))
}

// TestSpawn_FullFlow tests the complete spawn operation.
#[test]
fn test_spawn_full_flow() {
    require_termd!();
    let env = setup_test_env();
    let opts = SpawnOptions {
        name: "test-spawn-agent".into(),
        backend: "claude".into(),
        spawned_by: "lead".into(),
        ..Default::default()
    };

    let agent = env
        .service
        .spawn(&bg(), opts.clone())
        .unwrap_or_else(|e| panic!("spawn failed: {e:#}"));

    // Verify agent was created
    assert_eq!(agent.name, opts.name);
    let expected_session = format!("{}{}", env.cfg().session_prefix, opts.name);
    assert_eq!(agent.session_name, expected_session);

    // Verify session exists
    assert!(
        env.tmux.session_exists(&agent.session_name).unwrap(),
        "expected session to exist"
    );

    // Verify agent in database
    let stored = env
        .store
        .get_agent(&opts.name)
        .unwrap()
        .expect("expected agent in database");
    assert_eq!(stored.spawned_by, "lead");
}

// TestSpawn_DuplicateAgent tests that spawning a duplicate agent fails.
#[test]
fn test_spawn_duplicate_agent() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-duplicate", "claude");
    let err = env
        .service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "test-duplicate".into(),
                backend: "claude".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("already exists"),
        "expected 'already exists' error, got: {err:#}"
    );
}

// TestSpawn_InvalidBackend tests spawning with a non-existent backend.
#[test]
fn test_spawn_invalid_backend() {
    require_termd!();
    let env = setup_test_env();
    let err = env
        .service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "test-invalid-backend".into(),
                backend: "nonexistent-backend".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "expected 'not found' error, got: {err:#}"
    );
}

// TestSpawn_WithTaskFile tests spawning with a task file.
#[test]
fn test_spawn_with_task_file() {
    require_termd!();
    let env = setup_test_env();
    let task = env.dir().join("test-task.md");
    std::fs::write(&task, "# Test Task\nDo something.").unwrap();
    let task = task.display().to_string();

    let agent = env
        .service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "test-with-task".into(),
                backend: "claude".into(),
                task_file: task.clone(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn failed: {e:#}"));
    assert_eq!(agent.task_file, task);
}

// Custom JSON backends (--backend-config) have no Provider factory: their
// command is typed into the session as is, and their status comes from
// their patterns.
#[test]
fn test_spawn_custom_backend() {
    require_termd!();
    let env = setup_test_env();
    let cfg_path = env.dir().join("backend.json");
    std::fs::write(
        &cfg_path,
        r#"{"name": "custom", "launch_command": "echo custom-launched", "init_message_format": "echo task %s", "idle_pattern": "custom-launched"}"#,
    )
    .unwrap();
    env.backends
        .load_from_file(&cfg_path.display().to_string())
        .expect("loading backend config");

    let agent = env
        .service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "custom-worker".into(),
                backend: "custom".into(),
                task_file: "plan.md".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn failed: {e:#}"));
    assert!(
        agent.backend == "custom" && agent.task_file == "plan.md",
        "agent = {agent:?}"
    );
    assert!(
        env.store.get_agent("custom-worker").unwrap().is_some(),
        "agent not stored"
    );

    wait_for_session(
        &env,
        &agent.session_name,
        &["echo custom-launched", "echo task plan.md"],
    );
    assert!(
        !std::path::Path::new(&format!("{}/launch.sh", env.agent_dir("custom-worker"))).exists(),
        "custom backend should launch without a script"
    );

    let info = env.service.status_info("custom-worker").unwrap();
    assert!(
        info.status == AGENT_STATUS_IDLE && info.source == STATUS_SOURCE_SCRAPE,
        "got {} from {}, want idle from scrape",
        info.status,
        info.source
    );
}

// TestSend_ToExistingAgent tests sending a message to an existing agent.
#[test]
fn test_send_to_existing_agent() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-send-target", "claude");
    env.service
        .send(
            &bg(),
            "test-send-target",
            "hello world",
            SendOptions::default(),
        )
        .unwrap_or_else(|e| panic!("send failed: {e:#}"));
}

// TestSend_ToMissingAgent tests sending to a non-existent agent.
#[test]
fn test_send_to_missing_agent() {
    require_termd!();
    let env = setup_test_env();
    let err = env
        .service
        .send(&bg(), "nonexistent-agent", "hello", SendOptions::default())
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "expected 'not found' error, got: {err:#}"
    );
}

// TestQueue_MessageQueuing tests queuing a message for delivery.
#[test]
fn test_queue_message_queuing() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-queue-receiver", "claude");

    env.service
        .queue(
            "sender-agent",
            "test-queue-receiver",
            "queued message content",
        )
        .unwrap();

    let messages = env
        .store
        .get_pending_messages("test-queue-receiver", 10)
        .unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].content, "queued message content");
}

// TestQueue_InvalidReceiver tests queuing to a non-existent receiver.
#[test]
fn test_queue_invalid_receiver() {
    require_termd!();
    let env = setup_test_env();
    let err = env
        .service
        .queue("sender", "nonexistent-receiver", "message")
        .unwrap_err();
    assert!(
        err.to_string().contains("not found"),
        "expected 'not found' error, got: {err:#}"
    );
}

// TestCapture_Output tests capturing output from an agent's session.
#[test]
fn test_capture_output() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-capture", "claude");
    // Output may be empty initially, that's OK.
    env.service
        .capture("test-capture", 50, false)
        .unwrap_or_else(|e| panic!("capture failed: {e:#}"));
}

// TestCapture_DefaultLineLimit tests that default line limit is applied.
#[test]
fn test_capture_default_line_limit() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-capture-default", "claude");
    env.service
        .capture("test-capture-default", 0, false)
        .unwrap_or_else(|e| panic!("capture with default lines failed: {e:#}"));
}

// TestCapture_MissingAgent tests capturing from non-existent agent.
#[test]
fn test_capture_missing_agent() {
    require_termd!();
    let env = setup_test_env();
    assert!(
        env.service.capture("nonexistent", 50, false).is_err(),
        "expected error for missing agent"
    );
}

// TestStatus_Detection tests status detection using backend patterns.
#[test]
fn test_status_detection() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-status", "claude");

    // Might be unknown, idle, or processing depending on timing.
    let status = env
        .service
        .status("test-status")
        .unwrap_or_else(|e| panic!("status failed: {e:#}"));
    assert!(
        [
            AGENT_STATUS_IDLE,
            AGENT_STATUS_PROCESSING,
            AGENT_STATUS_UNKNOWN
        ]
        .contains(&status),
        "unexpected status: {status}"
    );
}

// TestStatus_MissingAgent tests status of non-existent agent.
#[test]
fn test_status_missing_agent() {
    require_termd!();
    let env = setup_test_env();
    assert!(
        env.service.status("nonexistent").is_err(),
        "expected error for missing agent"
    );
}

// TestList_EnumeratesAgents tests listing all agents.
#[test]
fn test_list_enumerates_agents() {
    require_termd!();
    let env = setup_test_env();
    assert_eq!(
        env.service.list().unwrap().len(),
        0,
        "expected 0 agents initially"
    );
    for name in ["list-agent-1", "list-agent-2"] {
        spawn(&env, name, "claude");
    }
    assert_eq!(env.service.list().unwrap().len(), 2);
}

/// isCleanupError checks if an error is a CleanupError (non-fatal cleanup issues).
fn is_cleanup_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<CleanupError>().is_some()
}

// TestCleanup_FullFlow tests the complete cleanup operation.
#[test]
fn test_cleanup_full_flow() {
    require_termd!();
    let env = setup_test_env();
    let agent = spawn(&env, "test-cleanup", "claude");
    assert!(
        env.tmux.session_exists(&agent.session_name).unwrap(),
        "session should exist before cleanup"
    );

    // Cleanup - may return CleanupError for non-fatal issues
    if let Err(e) = env.service.cleanup("test-cleanup") {
        assert!(is_cleanup_error(&e), "cleanup failed: {e:#}");
    }
    assert!(
        !env.tmux.session_exists(&agent.session_name).unwrap(),
        "session should not exist after cleanup"
    );
    assert!(
        env.store.get_agent("test-cleanup").unwrap().is_none(),
        "agent should not exist in DB after cleanup"
    );
}

// Cleanup of an idle agent saves its screen before typing the exit command:
// a CLI that quits on it ends the pane (and the session) before a later
// snapshot could be taken, and cleanup promises "its log stays". (The Go
// snapshotted after the exit, and saved nothing for such an agent.)
#[test]
fn test_cleanup_saves_the_log_before_the_cli_exits() {
    require_termd!();
    let env = setup_test_env();
    let _fake = spawn_fake(&env, "test-cleanup-log");
    let session = env.session_name("test-cleanup-log");
    // A CLI that prints, then quits on the line it reads (cleanup's /exit).
    env.tmux
        .send_keys(
            &session,
            "echo cleanup-log-$((6*7)); exec sh -c 'read l; exit 0'",
        )
        .unwrap();
    env.tmux.send_special_key(&session, "Enter").unwrap();
    wait_pane(&env, &session, "the marker", |s| {
        has_line(s, "cleanup-log-42")
    });
    env.service
        .report(
            "test-cleanup-log",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                ..Default::default()
            },
        )
        .unwrap();
    let info = env.service.status_info("test-cleanup-log").unwrap();
    assert_eq!(info.status, AGENT_STATUS_IDLE, "{info:?}");

    if let Err(e) = env.service.cleanup("test-cleanup-log") {
        assert!(is_cleanup_error(&e), "cleanup failed: {e:#}");
    }
    assert!(
        !env.tmux.session_exists(&session).unwrap(),
        "session should be gone"
    );
    let log = std::fs::read_to_string(env.cfg().log_path("test-cleanup-log"))
        .expect("cleanup saved the agent's terminal log");
    assert!(has_line(&log, "cleanup-log-42"), "log = {log:?}");
}

// TestCleanup_MissingAgent tests cleanup of non-existent agent.
#[test]
fn test_cleanup_missing_agent() {
    require_termd!();
    let env = setup_test_env();
    assert!(
        env.service.cleanup("nonexistent").is_err(),
        "expected error for missing agent"
    );
}

// TestCleanup_CleansMessages tests that cleanup removes associated messages.
#[test]
fn test_cleanup_cleans_messages() {
    require_termd!();
    let env = setup_test_env();
    spawn(&env, "test-cleanup-messages", "claude");
    env.service
        .queue("sender", "test-cleanup-messages", "test message")
        .unwrap();
    assert_eq!(
        env.store
            .get_pending_messages("test-cleanup-messages", 10)
            .unwrap()
            .len(),
        1,
        "expected 1 message before cleanup"
    );

    if let Err(e) = env.service.cleanup("test-cleanup-messages") {
        assert!(is_cleanup_error(&e), "cleanup failed: {e:#}");
    }
    assert_eq!(
        env.store
            .get_pending_messages("test-cleanup-messages", 10)
            .unwrap()
            .len(),
        0,
        "expected 0 messages after cleanup"
    );
}

// TestSpawn_CleanupOnFailure tests that session is cleaned up if storage fails.
#[test]
#[ignore = "requires storage error injection - covered by unit tests with mocks (skipped in the Go too)"]
fn test_spawn_cleanup_on_failure() {}

// TestSpawn_ContextCancellation tests that spawn respects context cancellation.
#[test]
fn test_spawn_context_cancellation() {
    require_termd!();
    let mut env = setup_test_env();
    // A longer startup delay for this test
    env.cfg_mut().startup_delay = Duration::from_secs(5);

    // A context that will be cancelled
    let (ctx, _cancel) = Context::with_timeout(&bg(), Duration::from_millis(100));
    let opts = SpawnOptions {
        name: "test-context-cancel".into(),
        backend: "claude".into(),
        ..Default::default()
    };
    let err = env
        .service
        .spawn(&ctx, opts.clone())
        .expect_err("expected error due to context cancellation");

    // The deadline can land in any step, so match it whatever wraps it.
    assert!(
        err.downcast_ref::<ContextError>().is_some(),
        "expected context error, got: {err:#}"
    );

    // Whichever step it hit, the session doesn't outlive the failed spawn.
    let session = env.session_name(&opts.name);
    await_::until("the failed spawn's session to be gone", || {
        let r = env.tmux.session_exists(&session);
        let ok = matches!(r, Ok(false));
        (format!("{r:?}"), ok)
    });
}
