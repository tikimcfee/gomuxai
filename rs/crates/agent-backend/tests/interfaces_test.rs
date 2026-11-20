//! Go tests/backend/interfaces_test.go: the service finds these
//! capabilities by the `as_*` accessors, so a backend that forgets to
//! override one silently loses the whole capability. Pin them.

use agent_backend::namedagents;
use agent_backend::*;

/// The `as_*` accessors that return `Some`, by trait name.
fn capabilities(p: &dyn Provider) -> Vec<&'static str> {
    let mut out = Vec::new();
    let mut has = |name: &'static str, yes: bool| {
        if yes {
            out.push(name);
        }
    };
    has("HookProvider", p.as_hook_provider().is_some());
    has("SpawnHooker", p.as_spawn_hooker().is_some());
    has("GlobalHooker", p.as_global_hooker().is_some());
    has("HookDeliverer", p.as_hook_deliverer().is_some());
    has("PromptHandover", p.as_prompt_handover().is_some());
    has("HookAllower", p.as_hook_allower().is_some());
    has("InputReader", p.as_input_reader().is_some());
    has("SubmitChecker", p.as_submit_checker().is_some());
    has("TurnEndReader", p.as_turn_end_reader().is_some());
    has("QuestionPrompter", p.as_question_prompter().is_some());
    has(
        "InstructionsProvider",
        p.as_instructions_provider().is_some(),
    );
    has("CommandAllower", p.as_command_allower().is_some());
    has("TrustPrompter", p.as_trust_prompter().is_some());
    has("ModelChooser", p.as_model_chooser().is_some());
    has("SessionNamer", p.as_session_namer().is_some());
    has("DialogReader", p.as_dialog_reader().is_some());
    has("PromptFlagProvider", p.as_prompt_flag_provider().is_some());
    has(
        "NativeStatusProvider",
        p.as_native_status_provider().is_some(),
    );
    has(
        "PaneSessionProvider",
        p.as_pane_session_provider().is_some(),
    );
    has("PermissionResponder", p.as_permission_responder().is_some());
    has("TranscriptProvider", p.as_transcript_provider().is_some());
    out
}

#[test]
fn test_interfaces() {
    let mut claude = vec![
        "HookProvider",
        "GlobalHooker",
        "PaneSessionProvider",
        "NativeStatusProvider",
        "PermissionResponder",
        "InstructionsProvider",
        "TrustPrompter",
        "DialogReader",
        "CommandAllower",
        "HookDeliverer",
        "InputReader",
        "SubmitChecker",
        "ModelChooser",
        "SessionNamer",
        "PromptHandover",
        // Not pinned in the Go test, but implemented there.
        "HookAllower",
        "TranscriptProvider",
    ];
    let mut agy = vec![
        "HookProvider",
        "SpawnHooker",
        "PermissionResponder",
        "PromptFlagProvider",
        "InstructionsProvider",
        "TrustPrompter",
        "HookDeliverer",
        "InputReader",
        "TurnEndReader",
        "QuestionPrompter",
        "ModelChooser",
        "TranscriptProvider",
    ];
    let mut got_claude = capabilities(&namedagents::new_claude_backend());
    let mut got_agy = capabilities(&namedagents::new_agy_backend());
    for v in [&mut claude, &mut agy, &mut got_claude, &mut got_agy] {
        v.sort_unstable();
    }
    assert_eq!(got_claude, claude);
    assert_eq!(got_agy, agy);
    assert_eq!(
        capabilities(&namedagents::new_aider_backend()),
        vec!["ModelChooser"]
    );
    assert!(capabilities(&namedagents::new_shell_backend()).is_empty());

    // Every built-in backend is also a Backend.
    let r = new_registry();
    for name in ["claude", "agy", "aider", "shell"] {
        assert_eq!(r.get(name).unwrap().name(), name);
        assert_eq!(
            r.create_provider(name, ProviderConfig::default())
                .unwrap()
                .name(),
            name
        );
    }
}
