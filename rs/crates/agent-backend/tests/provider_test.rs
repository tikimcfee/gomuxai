//! Go tests/backend/provider_test.go.

mod common;

use agent_backend::namedagents::{self, AgyConfig, ClaudeConfig};
use agent_backend::*;
use agent_base::util::shell_quote;

/// What every Claude launch command carries: the settings that keep startup
/// and between-turn dialogs from coming up.
fn claude_settings_flag() -> String {
    format!(" --settings '{}'", namedagents::CLAUDE_SPAWN_SETTINGS)
}

#[test]
fn test_claude_exit_command() {
    let provider = namedagents::new_claude_backend();
    assert!(
        !provider.exit_command().is_empty(),
        "exit_command() returned empty string"
    );
}

#[test]
fn test_aider_exit_command() {
    let provider = namedagents::new_aider_backend();
    assert!(
        !provider.exit_command().is_empty(),
        "exit_command() returned empty string"
    );
}

#[test]
fn test_status_string() {
    for (status, expected) in [
        (Status::Unknown, "unknown"),
        (Status::Idle, "idle"),
        (Status::Processing, "processing"),
        (Status::Error, "error"),
    ] {
        assert_eq!(status.to_string(), expected);
    }
}

#[test]
fn test_claude_config_skip_permissions() {
    // Default - no flag
    let default_backend = namedagents::new_claude_backend();
    assert_eq!(
        default_backend.launch_command(),
        format!("claude{}", claude_settings_flag())
    );

    // With skip permissions
    let skip_backend = namedagents::new_claude_backend_with_config(ClaudeConfig {
        skip_permissions: true,
        ..Default::default()
    });
    assert_eq!(
        skip_backend.launch_command(),
        format!(
            "claude --dangerously-skip-permissions{}",
            claude_settings_flag()
        )
    );
}

#[test]
fn test_registry_create_provider() {
    let registry = new_registry();

    // Create without skip permissions
    let p1 = registry
        .create_provider("claude", ProviderConfig::default())
        .unwrap();
    assert_eq!(
        p1.launch_command(),
        format!("claude{}", claude_settings_flag())
    );

    // Create with skip permissions
    let p2 = registry
        .create_provider(
            "claude",
            ProviderConfig {
                skip_permissions: true,
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        p2.launch_command(),
        format!(
            "claude --dangerously-skip-permissions{}",
            claude_settings_flag()
        )
    );

    // Verify they're different instances
    assert!(
        !std::ptr::eq(
            &*p1 as *const dyn Provider as *const u8,
            &*p2 as *const dyn Provider as *const u8
        ),
        "create_provider should return new instances"
    );

    // Create agy provider via registry
    let p_agy = registry
        .create_provider(
            "agy",
            ProviderConfig {
                skip_permissions: true,
                agent_dir: "/var/tmp/agy-test-dir".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        p_agy.launch_command(),
        r#"env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy --settings '{"queuedMessages":"send-immediately","showFeedbackSurvey":false}' --dangerously-skip-permissions"#
    );
}

#[test]
fn test_registry_create_provider_unknown() {
    let registry = new_registry();
    assert!(
        registry
            .create_provider("unknown", ProviderConfig::default())
            .is_err(),
        "create_provider() should error for unknown provider"
    );
    let err = registry
        .create_provider("unknown", ProviderConfig::default())
        .err()
        .unwrap();
    assert_eq!(
        err.to_string(),
        "no factory registered for provider \"unknown\""
    );
}

#[test]
fn test_with_model() {
    let claude = namedagents::new_claude_backend();
    assert_eq!(
        claude.with_model(
            &claude.launch_command_with_session("s1"),
            "claude-fable-5-1"
        ),
        format!(
            "claude --settings {} --session-id 's1' --model 'claude-fable-5-1'",
            shell_quote(namedagents::CLAUDE_SPAWN_SETTINGS)
        )
    );
    let aider = namedagents::new_aider_backend();
    assert_eq!(
        aider.with_model(&aider.launch_command(), "sonnet"),
        "aider --model 'sonnet'"
    );
    // agy's launch line ends with the report of its exit: the model goes to
    // agy, ahead of it.
    let dir = common::temp_dir();
    let agy = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: dir.path().to_str().unwrap().to_string(),
        ..Default::default()
    });
    let launch = agy.with_model(
        &agy.launch_command_with_hooks("/bin/agent report --hook --as a", "sp1"),
        "gemini-3.8-flash-high",
    );
    let (before, after) = launch.split_once("; ").expect("an exit report");
    assert!(
        before.ends_with(" --model 'gemini-3.8-flash-high'") && !after.contains("--model"),
        "agy: {launch}"
    );
}

#[test]
fn test_agy_footer_model() {
    let idle = include_str!("../testdata/agy-1.5-idle.txt");
    assert_eq!(
        namedagents::agy_footer_model(idle).as_deref(),
        Some("Gemini 4 Argon · high")
    );
    let busy = include_str!("../testdata/agy-1.5-busy.txt");
    assert_eq!(
        namedagents::agy_footer_model(busy).as_deref(),
        Some("Gemini 4 Argon · high")
    );
    let with_mode = "────\n>\n────\n? for shortcuts            auto · Gemini 4 Carbon · high\n";
    assert_eq!(
        namedagents::agy_footer_model(with_mode).as_deref(),
        Some("auto · Gemini 4 Carbon · high")
    );
    for none in [
        "",
        "? for shortcuts\n",
        " Select login method:\n > 1. Google OAuth\n",
    ] {
        assert_eq!(namedagents::agy_footer_model(none), None, "{none:?}");
    }
}

#[test]
fn test_with_effort() {
    let claude = namedagents::new_claude_backend();
    let ec = claude.as_effort_chooser().expect("claude takes --effort");
    assert_eq!(
        ec.with_effort(&claude.launch_command_with_session("s1"), "high"),
        format!(
            "claude --settings {} --session-id 's1' --effort 'high'",
            shell_quote(namedagents::CLAUDE_SPAWN_SETTINGS)
        )
    );
    // claude takes a model and --effort as they are; agy's model ids carry
    // the effort.
    assert_eq!(ec.model_at_effort("opus", "high"), None);
    // aider takes a model but no effort.
    assert!(
        namedagents::new_aider_backend()
            .as_effort_chooser()
            .is_none()
    );
    // agy's goes ahead of the report of its exit, as its model does.
    let dir = common::temp_dir();
    let agy = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: dir.path().to_str().unwrap().to_string(),
        ..Default::default()
    });
    let launch = agy
        .as_effort_chooser()
        .expect("agy takes --effort")
        .with_effort(
            &agy.launch_command_with_hooks("/bin/agent report --hook --as a", "sp1"),
            "xhigh",
        );
    let (before, after) = launch.split_once("; ").expect("an exit report");
    assert!(
        before.ends_with(" --effort 'xhigh'") && !after.contains("--effort"),
        "agy: {launch}"
    );
    assert_eq!(
        agy.as_effort_chooser()
            .unwrap()
            .model_at_effort("gemini-4-carbon", "high")
            .as_deref(),
        Some("gemini-4-carbon-high")
    );
}

#[test]
fn test_with_session_name() {
    let claude = namedagents::new_claude_backend();
    assert_eq!(
        claude.with_session_name(&claude.launch_command_with_session("s1"), "worker-1"),
        format!(
            "claude --settings {} --session-id 's1' --name 'worker-1'",
            shell_quote(namedagents::CLAUDE_SPAWN_SETTINGS)
        )
    );
    let resumed = namedagents::new_claude_backend_with_config(ClaudeConfig {
        skip_permissions: true,
        resume: "r1".to_string(),
    });
    assert_eq!(
        resumed.with_session_name(&resumed.launch_command_with_session("ignored"), "worker-1"),
        format!(
            "claude --dangerously-skip-permissions --settings {} --resume 'r1' --name 'worker-1'",
            shell_quote(namedagents::CLAUDE_SPAWN_SETTINGS)
        )
    );
    assert_eq!(claude.with_session_name("claude", ""), "claude");
}

/// Claude takes text over 800 characters, or with 3+ line breaks, as a
/// paste (2.1.319); typed_as_paste says so from 640 characters, a margin.
#[test]
fn test_claude_typed_as_paste() {
    let b = namedagents::new_claude_backend();
    for (text, want) in [
        ("run the tests".to_string(), false),
        ("one\ntwo\nthree".to_string(), false),
        ("one\ntwo\nthree\nfour".to_string(), true),
        ("a\r\nb\r\nc".to_string(), false),
        ("a\r\nb\r\nc\r\nd".to_string(), true),
        ("x".repeat(640), false),
        ("x".repeat(641), true),
        ("é".repeat(640), false), // characters, not bytes
        ("🙂".repeat(321), true), // two UTF-16 units each, as Claude counts
        ("/model claude-fable-5-1".to_string(), false),
        ("[from lead@h] see\n/tmp/plan.md".to_string(), false),
    ] {
        assert_eq!(
            b.typed_as_paste(&text),
            want,
            "typed_as_paste({:.20?}, {} bytes)",
            text,
            text.len()
        );
    }
}

/// UserPromptSubmit carries the prompt, and its output's additionalContext
/// goes to the model with it.
#[test]
fn test_claude_prompt_handover() {
    let b = namedagents::new_claude_backend();
    let r = b
        .parse_hook_event(
            "",
            br#"{"hook_event_name":"UserPromptSubmit","session_id":"s","prompt":"hi [gomuxai message #4, x"}"#,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        b.submitted_prompt(&r).as_deref(),
        Some("hi [gomuxai message #4, x")
    );
    let stop = b
        .parse_hook_event("", br#"{"hook_event_name":"Stop","session_id":"s"}"#)
        .unwrap()
        .unwrap();
    assert!(
        b.submitted_prompt(&stop).is_none(),
        "Stop counted as a submitted prompt"
    );
    let out: serde_json::Value =
        serde_json::from_slice(&b.handover_output("the whole text")).unwrap();
    assert_eq!(
        out["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert_eq!(
        out["hookSpecificOutput"]["additionalContext"],
        "the whole text"
    );
    assert_eq!(
        b.handover_output("<the whole text>"),
        b"{\"hookSpecificOutput\":{\"additionalContext\":\"\\u003cthe whole text\\u003e\",\"hookEventName\":\"UserPromptSubmit\"}}"
    );
}
