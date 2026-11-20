//! Go tests/backend/agy_hooks_test.go. TestAgyWorksInItsProject, which sets
//! TMPDIR, is in agy_workspace_test.rs, a test binary of its own.

mod common;

use agent_backend::namedagents::{self, AgyConfig};
use agent_backend::*;
use common::*;
use serde_json::Value;

const AGY_PREFIX: &str = r#"env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy --settings '{"showFeedbackSurvey":false}'"#;

/// Payloads trimmed from real Antigravity CLI (agy) hook invocations (see
/// exploration/agy-hooks-findings.md).
#[test]
fn test_agy_parse_hook_event() {
    let provider = namedagents::new_agy_backend();
    // (name, event, payload, ok, status, detail, tool)
    let tests: &[(&str, &str, &str, bool, Status, &str, &str)] = &[
        (
            "pre invocation starts processing",
            "PreInvocation",
            r#"{"conversationId":"c1","executionNum":0,"invocationNum":0,"lastUserInput":"hi"}"#,
            true,
            Status::Processing,
            "prompt received",
            "",
        ),
        (
            "pre tool use summarizes command",
            "PreToolUse",
            r#"{"conversationId":"c1","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo probe","toolSummary":"Echo"}}}"#,
            true,
            Status::Processing,
            "run_command: echo probe",
            "run_command",
        ),
        (
            "pre tool use summarizes file path",
            "PreToolUse",
            r#"{"conversationId":"c1","stepIdx":2,"toolCall":{"name":"read_file","args":{"Path":"/tmp/x.go"}}}"#,
            true,
            Status::Processing,
            "read_file: /tmp/x.go",
            "read_file",
        ),
        (
            "pre tool use summarizes view_file's path, not its toolSummary",
            "PreToolUse",
            r#"{"conversationId":"c1","stepIdx":2,"toolCall":{"name":"view_file","args":{"AbsolutePath":"/tmp/x.go","toolSummary":"Reading x"}}}"#,
            true,
            Status::Processing,
            "view_file: /tmp/x.go",
            "view_file",
        ),
        (
            "pre tool use summarizes a message whole",
            "PreToolUse",
            r#"{"conversationId":"c1","stepIdx":2,"toolCall":{"name":"send_message","args":{"Recipient":"main","Message":"done"}}}"#,
            true,
            Status::Processing,
            "send_message: → main\ndone",
            "send_message",
        ),
        (
            "post tool use success",
            "PostToolUse",
            r#"{"conversationId":"c1","error":"","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo probe"}}}"#,
            true,
            Status::Processing,
            "run_command: echo probe",
            "run_command",
        ),
        (
            "post tool use failure",
            "PostToolUse",
            r#"{"conversationId":"c1","error":"exit status 1","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"false"}}}"#,
            true,
            Status::Processing,
            "failed: run_command: false",
            "run_command",
        ),
        (
            "stop fully idle",
            "Stop",
            r#"{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":true,"lastUserInput":"hi","terminationReason":"NO_TOOL_CALL"}"#,
            true,
            Status::Idle,
            "turn finished",
            "",
        ),
        (
            "stop with background tasks still running",
            "Stop",
            r#"{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":false,"lastUserInput":"hi","terminationReason":"NO_TOOL_CALL"}"#,
            true,
            Status::Processing,
            "background tasks running",
            "",
        ),
        (
            "stop with error message",
            "Stop",
            r#"{"conversationId":"c1","error":"API rate limit exceeded","executionNum":0,"fullyIdle":true,"terminationReason":"ERROR"}"#,
            true,
            Status::Error,
            "API rate limit exceeded",
            "",
        ),
        (
            "stop with max invocations termination reason",
            "Stop",
            r#"{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":true,"terminationReason":"MAX_INVOCATIONS"}"#,
            true,
            Status::Error,
            "MAX_INVOCATIONS",
            "",
        ),
        (
            "stop with user canceled",
            "Stop",
            r#"{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":true,"terminationReason":"USER_CANCELED"}"#,
            true,
            Status::Idle,
            "USER_CANCELED",
            "",
        ),
        (
            "session start is ignored",
            "SessionStart",
            r#"{"conversationId":"c1","executionNum":0,"invocationNum":0,"lastUserInput":"hi"}"#,
            false,
            Status::Unknown,
            "",
            "",
        ),
        (
            "post invocation is reported only to deliver",
            "PostInvocation",
            r#"{"conversationId":"c1","initialNumSteps":3,"invocationNum":1}"#,
            true,
            Status::Processing,
            "",
            "",
        ),
        (
            "failed post invocation is left to stop",
            "PostInvocation",
            r#"{"conversationId":"c1","error":"quota exceeded","invocationNum":0}"#,
            false,
            Status::Unknown,
            "",
            "",
        ),
        (
            "cancelled post invocation is left to stop",
            "PostInvocation",
            r#"{"conversationId":"c1","invocationNum":0,"terminationReason":"USER_CANCELED"}"#,
            false,
            Status::Unknown,
            "",
            "",
        ),
        (
            "unknown event is ignored",
            "SomethingElse",
            r#"{"conversationId":"c1"}"#,
            false,
            Status::Unknown,
            "",
            "",
        ),
    ];

    for &(name, event, payload, want_ok, status, detail, tool) in tests {
        let r = provider
            .parse_hook_event(event, payload.as_bytes())
            .unwrap_or_else(|e| panic!("{name}: parse_hook_event: {e:#}"));
        assert_eq!(r.is_some(), want_ok, "{name}: ok");
        let Some(r) = r else { continue };
        assert_eq!(r.event, event, "{name}: event");
        assert_eq!(r.status, status, "{name}: status");
        assert_eq!(r.detail, detail, "{name}: detail");
        assert_eq!(r.tool, tool, "{name}: tool");
        assert_eq!(r.session_id, "c1", "{name}: session id");
    }
}

/// Every payload names the conversation's transcript; agent logs reads it.
#[test]
fn test_agy_parse_hook_event_transcript() {
    let path =
        "/home/user/.gemini/antigravity-cli/brain/c1/.system_generated/logs/transcript_full.jsonl";
    let payload = format!(
        r#"{{"conversationId":"c1","transcriptPath":"{path}","stepIdx":1,"toolCall":{{"name":"run_command","args":{{"CommandLine":"ls"}}}}}}"#
    );
    let r = namedagents::new_agy_backend()
        .parse_hook_event("PostToolUse", payload.as_bytes())
        .unwrap()
        .unwrap();
    assert_eq!(r.transcript, path);
}

#[test]
fn test_agy_parse_hook_event_invalid_json() {
    assert!(
        namedagents::new_agy_backend()
            .parse_hook_event("PreInvocation", b"not json")
            .is_err(),
        "expected error for invalid payload"
    );
}

/// agy's permission dialog follows PreToolUse, so that event keeps the call
/// whole.
#[test]
fn test_agy_parse_hook_event_keeps_tool_call_whole() {
    let payload = format!(
        r#"{{"conversationId":"c1","toolCall":{{"name":"run_command","args":{{"CommandLine":"{}"}}}}}}"#,
        "é".repeat(300)
    );
    let r = namedagents::new_agy_backend()
        .parse_hook_event("PreToolUse", payload.as_bytes())
        .unwrap()
        .unwrap();
    assert_eq!(r.detail, format!("run_command: {}", "é".repeat(300)));
    assert_eq!(
        r.tool_input,
        format!(r#"{{"CommandLine":"{}"}}"#, "é".repeat(300))
    );
}

#[test]
fn test_agy_launch_command_with_hooks() {
    let tmp = temp_dir();
    let agent_dir = path_in(&tmp, "my agent's dir");
    let provider = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: agent_dir.clone(),
        ..Default::default()
    });
    let report_cmd = r#"'/opt/my tools/it'\''s/agent' report --hook --as 'helper' --spawn-id '11111111-2222-3333-4444-555555555555'"#;

    let cmd =
        provider.launch_command_with_hooks(report_cmd, "11111111-2222-3333-4444-555555555555");
    // agy must launch without SSH_* vars, or it ignores the saved login.
    assert!(
        cmd.starts_with(&format!("{AGY_PREFIX} --add-dir ")),
        "unexpected launch command: {cmd}"
    );
    let want_exit = r#"; '/opt/my tools/it'\''s/agent' report ended --end-reason quit --as 'helper' --spawn-id '11111111-2222-3333-4444-555555555555' "agy exited with code $?""#;
    assert!(
        cmd.ends_with(want_exit),
        "launch command missing exit wrapper:\ngot:  {cmd}\nwant suffix: {want_exit}"
    );

    // Let a real shell split the agy invocation arguments (before ';').
    let agy_part = cmd.strip_suffix(want_exit).unwrap();
    let args = shell_args(
        agy_part
            .strip_prefix("env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy ")
            .unwrap(),
    );
    assert_eq!(
        args,
        [
            "--settings",
            r#"{"showFeedbackSurvey":false}"#,
            "--add-dir",
            agent_dir.as_str()
        ]
    );

    // append_prompt_flag must place -i before the '; ... report exited' suffix.
    let with_prompt = provider.append_prompt_flag(&cmd, "Run task");
    assert!(
        with_prompt.ends_with(&format!(" -i 'Run task'{want_exit}")),
        "append_prompt_flag with exit wrapper = {with_prompt}"
    );

    // Verify .agents/hooks.json was generated in agent_dir.
    let hooks_path = std::path::Path::new(&agent_dir)
        .join(".agents")
        .join("hooks.json");
    let data = std::fs::read_to_string(&hooks_path).expect("reading generated hooks.json");
    let doc: Value = serde_json::from_str(&data).unwrap();
    let g = &doc["gomuxai"];
    for event in ["PreInvocation", "PostInvocation", "Stop"] {
        let hooks = g[event].as_array().unwrap();
        assert!(
            hooks.len() == 1
                && hooks[0]["command"] == format!("{report_cmd} --event {event}")
                && hooks[0]["type"] == "command",
            "{event} = {hooks:?}"
        );
    }
    for event in ["PreToolUse", "PostToolUse"] {
        let groups = g[event].as_array().unwrap();
        assert!(
            groups.len() == 1
                && groups[0]["matcher"] == "*"
                && groups[0]["hooks"].as_array().unwrap().len() == 1
                && groups[0]["hooks"][0]["command"] == format!("{report_cmd} --event {event}"),
            "{event} = {groups:?}"
        );
    }
    // The bytes as Go's json.MarshalIndent writes them: keys sorted, two
    // spaces, HTML characters escaped.
    let want = r#"{
  "gomuxai": {
    "PostInvocation": [
      {
        "type": "command",
        "command": "a & b --event PostInvocation",
        "timeout": 10
      }
    ],
    "PostToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "a & b --event PostToolUse",
            "timeout": 10
          }
        ]
      }
    ],
    "PreInvocation": [
      {
        "type": "command",
        "command": "a & b --event PreInvocation",
        "timeout": 10
      }
    ],
    "PreToolUse": [
      {
        "matcher": "*",
        "hooks": [
          {
            "type": "command",
            "command": "a & b --event PreToolUse",
            "timeout": 10
          }
        ]
      }
    ],
    "Stop": [
      {
        "type": "command",
        "command": "a & b --event Stop",
        "timeout": 10
      }
    ]
  }
}"#;
    let dir2 = temp_dir();
    let p2 = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: dir2.path().to_str().unwrap().to_string(),
        ..Default::default()
    });
    p2.launch_command_with_hooks("a & b", "s");
    let got = std::fs::read_to_string(dir2.path().join(".agents").join("hooks.json")).unwrap();
    // (The "&" is written escaped, as Go escapes it.)
    assert_eq!(got, want.replace(" & ", " \\u0026 "));
}

#[test]
fn test_agy_launch_command_with_hooks_keeps_skip_permissions() {
    let tmp = temp_dir();
    let provider = namedagents::new_agy_backend_with_config(AgyConfig {
        skip_permissions: true,
        agent_dir: tmp.path().to_str().unwrap().to_string(),
        ..Default::default()
    });
    let cmd = provider.launch_command_with_hooks("agent report --hook", "sid");
    assert!(
        cmd.starts_with(&format!(
            "{AGY_PREFIX} --dangerously-skip-permissions --add-dir "
        )),
        "unexpected launch command: {cmd}"
    );
}

/// A resumed agy continues its conversation, and the exit report still ends
/// the command (it runs after agy, in the shell).
#[test]
fn test_agy_launch_command_with_hooks_resume() {
    let tmp = temp_dir();
    let provider = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: tmp.path().to_str().unwrap().to_string(),
        resume: "83d95154-d7a6-496b-a811-2496b2c39a84".to_string(),
        ..Default::default()
    });
    let cmd = provider.launch_command_with_hooks("agent report --hook", "sid");
    assert!(
        cmd.starts_with(&format!(
            "{AGY_PREFIX} --conversation '83d95154-d7a6-496b-a811-2496b2c39a84' --add-dir "
        )),
        "unexpected launch command: {cmd}"
    );
    assert!(
        cmd.contains("; agent report ended --end-reason quit"),
        "the exit report is gone: {cmd}"
    );
}

#[test]
fn test_agy_append_prompt_flag() {
    let provider = namedagents::new_agy_backend();
    assert_eq!(
        provider.append_prompt_flag("agy --add-dir '/tmp/a'", "Please run viewer's task"),
        r#"agy --add-dir '/tmp/a' -i 'Please run viewer'\''s task'"#
    );
    assert_eq!(provider.append_prompt_flag("agy", ""), "agy");
}

#[test]
fn test_agy_permission_key() {
    let provider = namedagents::new_agy_backend();
    for (resp, key) in [
        (PermissionResponse::Approve, "1"),
        (PermissionResponse::ApproveAlways, "2"),
        (PermissionResponse::Deny, "Escape"),
    ] {
        assert_eq!(
            provider.permission_key(resp, "run_command").unwrap(),
            key,
            "{resp:?}"
        );
    }
}

#[test]
fn test_agy_permission_key_refuses_question_dialogs() {
    let provider = namedagents::new_agy_backend();
    assert!(
        provider
            .permission_key(PermissionResponse::Approve, "ask_question")
            .is_err(),
        "permission_key(approve, ask_question): want an error"
    );
    assert!(
        provider
            .permission_key(PermissionResponse::Approve, "")
            .is_ok(),
        "empty tool should be treated as a plain prompt"
    );
}

#[test]
fn test_agy_permission_prompt_visible() {
    let provider = namedagents::new_agy_backend();

    let cmd_prompt = [
        "Requesting permission for:",
        "   echo interactive-approval-test",
        "Run this command?",
        "> 1. Yes, run command",
        "  2. Yes, and always allow in this conversation for commands that start with 'echo'",
        "  3. Yes, and always allow for commands that start with 'echo' (Persist to settings.json)",
        "  4. No, cancel",
        "  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command",
        "esc to cancel                                                     Gemini 3.8 Flash · low",
    ]
    .join("\n");
    assert!(
        provider.permission_prompt_visible(&cmd_prompt),
        "expected command permission dialog to be visible"
    );

    let edit_prompt = [
        "Accept this file edit?",
        "  1. Yes, accept this change",
        "> 2. Yes, and always allow in this conversation",
        "  3. No, reject this change",
        "esc to cancel",
    ]
    .join("\n");
    assert!(
        provider.permission_prompt_visible(&edit_prompt),
        "expected file edit permission dialog to be visible"
    );

    let idle_screen = [
        ">",
        "? for shortcuts                                                   Gemini 3.8 Flash · low",
    ]
    .join("\n");
    assert!(
        !provider.permission_prompt_visible(&idle_screen),
        "idle screen should not match permission dialog"
    );

    let generating_screen = "Generating... (Enter/Esc to cancel)\nesc to cancel";
    assert!(
        !provider.permission_prompt_visible(generating_screen),
        "generating spinner without Yes/No options should not match"
    );

    let scrolled_past = format!(
        "{cmd_prompt}\n{}? for shortcuts",
        "output line\n".repeat(15)
    );
    assert!(
        !provider.permission_prompt_visible(&scrolled_past),
        "dialog scrolled off the bottom of the screen should not match"
    );
}

/// The briefing reaches agy as AGENTS.md at the root of an --add-dir
/// directory, which is where agy looks (verified against agy 1.4.x: not
/// .agents/rules).
#[test]
fn test_agy_with_instructions() {
    let tmp = temp_dir();
    let dir = tmp.path().to_str().unwrap().to_string();
    let path = path_in(&tmp, "instructions.md");
    std::fs::write(&path, "# briefing\n").unwrap();

    // Hooks already added the agent's dir: no second --add-dir, wrapper kept.
    let provider = namedagents::new_agy_backend_with_config(AgyConfig {
        agent_dir: dir.clone(),
        ..Default::default()
    });
    let with_hooks = provider.launch_command_with_hooks("agent report --hook", "sid");
    let cmd = provider.with_instructions(&with_hooks, &path).unwrap();
    assert!(
        cmd.matches("--add-dir").count() == 1
            && cmd.ends_with(r#"; agent report ended --end-reason quit "agy exited with code $?""#),
        "with hooks: {cmd}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("AGENTS.md")).unwrap(),
        "# briefing\n"
    );

    // Without hooks, the directory is added, before nothing else.
    let plain = namedagents::new_agy_backend();
    let cmd = plain
        .with_instructions(&plain.launch_command(), &path)
        .unwrap();
    assert!(
        cmd.ends_with(&format!(" --add-dir '{dir}'")),
        "without hooks: {cmd}"
    );
    // Asked again, it isn't added twice.
    assert_eq!(plain.with_instructions(&cmd, &path).unwrap(), cmd);

    // A briefing that can't be read is an error, never an agent without it.
    let missing = path_in(&tmp, "missing.md");
    let err = plain
        .with_instructions(&plain.launch_command(), &missing)
        .expect_err("missing briefing launched");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "reading the instructions for agy's AGENTS.md: open {missing}: no such file or directory"
        )
    );
}

/// Lone UTF-16 surrogates, raw non-UTF-8 bytes, and null array elements in
/// agy hook payloads decode like Go's encoding/json.
#[test]
fn test_agy_parse_hook_event_lenient_utf8_and_null_array_elements() {
    let provider = namedagents::new_agy_backend();

    // 2.1(2): lone surrogate and raw non-UTF-8 in Stop error.
    let r = provider
        .parse_hook_event(
            "Stop",
            br#"{"conversationId":"c1","error":"oops \ud800","terminationReason":"ERROR"}"#,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (r.status, r.detail.as_str()),
        (Status::Error, "oops \u{fffd}")
    );

    let r = provider
        .parse_hook_event(
            "Stop",
            b"{\"conversationId\":\"c1\",\"error\":\"oops \xff\",\"terminationReason\":\"ERROR\"}",
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (r.status, r.detail.as_str()),
        (Status::Error, "oops \u{fffd}")
    );

    // 2.2(2): null elements in ask_question options and questions decode to zero values.
    let r = provider
        .parse_hook_event(
            "PreToolUse",
            br#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"question":"Q1","options":["a",null]}]}}}"#,
        )
        .unwrap()
        .unwrap();
    assert_eq!(r.detail, "ask_question: Q1 (a, )");

    let r = provider
        .parse_hook_event(
            "PreToolUse",
            br#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[null]}}}"#,
        )
        .unwrap()
        .unwrap();
    assert_eq!(r.detail, "ask_question");

    let r = provider
        .parse_hook_event(
            "PreToolUse",
            br#"{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"question":"Q1","options":["a","b"]},null]}}}"#,
        )
        .unwrap()
        .unwrap();
    assert_eq!(r.detail, "ask_question: Q1 (a, b); 1 more question");
}
