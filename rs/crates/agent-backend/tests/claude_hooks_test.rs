//! Go tests/backend/claude_hooks_test.go.

mod common;

use agent_backend::namedagents::{self, ClaudeConfig};
use agent_backend::*;
use agent_claudereg::Entry;
use common::*;

fn parse(payload: &str) -> anyhow::Result<Option<HookReport>> {
    namedagents::new_claude_backend().parse_hook_event("", payload.as_bytes())
}

/// Payloads trimmed from real Claude Code v2.1.310 hook invocations (see
/// exploration/claude-hooks-findings.md).
#[test]
fn test_claude_parse_hook_event() {
    struct Case {
        name: &'static str,
        payload: &'static str,
        want_ok: bool,
        want_status: Status,
        want_detail: &'static str,
        want_result: &'static str,
    }
    let skip = |name, payload| Case {
        name,
        payload,
        want_ok: false,
        want_status: Status::Unknown,
        want_detail: "",
        want_result: "",
    };
    let ok = |name, payload, want_status, want_detail, want_result| Case {
        name,
        payload,
        want_ok: true,
        want_status,
        want_detail,
        want_result,
    };
    let tests = [
        ok(
            "session start",
            r#"{"hook_event_name":"SessionStart","session_id":"s1","source":"startup"}"#,
            Status::Idle,
            "session startup",
            "",
        ),
        ok(
            "prompt submitted",
            r#"{"hook_event_name":"UserPromptSubmit","session_id":"s1","prompt":"hi"}"#,
            Status::Processing,
            "prompt received",
            "",
        ),
        ok(
            "pre tool use summarizes the command",
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_input":{"command":"echo probe","description":"Echo"}}"#,
            Status::Processing,
            "Bash: echo probe",
            "",
        ),
        ok(
            "permission request waits on a human",
            r#"{"hook_event_name":"PermissionRequest","session_id":"s1","tool_name":"Edit","tool_input":{"file_path":"/tmp/x.go"}}"#,
            Status::Waiting,
            "permission: Edit: /tmp/x.go",
            "",
        ),
        ok(
            "pre tool use summarizes from the tool table",
            r#"{"hook_event_name":"PreToolUse","session_id":"s1","cwd":"/tmp","tool_name":"Skill","tool_input":{"skill":"pdf"}}"#,
            Status::Processing,
            "Skill: pdf",
            "",
        ),
        skip(
            "delayed permission notification is ignored",
            r#"{"hook_event_name":"Notification","session_id":"s1","notification_type":"permission_prompt","message":"Claude needs your permission"}"#,
        ),
        ok(
            "idle notification",
            r#"{"hook_event_name":"Notification","session_id":"s1","notification_type":"idle_prompt"}"#,
            Status::Idle,
            "waiting for input",
            "",
        ),
        ok(
            "stop carries the result",
            r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"done","stop_hook_active":false}"#,
            Status::Idle,
            "turn finished",
            "done",
        ),
        // Observed: the agent launched a background shell, replied
        // "started", and Stop fired while the shell kept running.
        ok(
            "stop with a background shell running is not idle",
            r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"started","background_tasks":[{"id":"berqn6wy0","type":"shell","status":"running","description":"Sleep 45 seconds then echo finished","command":"sleep 45; echo finished"}],"session_crons":[]}"#,
            Status::Processing,
            "turn finished, 1 background task(s) running",
            "",
        ),
        ok(
            "stop with a background subagent running is not idle",
            r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"launched","background_tasks":[{"id":"adfca3363a3dc3bba","type":"subagent","status":"running","description":"Run sleep 30 and reply ok","agent_type":"general-purpose"}],"session_crons":[]}"#,
            Status::Processing,
            "turn finished, 1 background task(s) running",
            "",
        ),
        ok(
            "stop after background work finished",
            r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"The subagent replied ok.","background_tasks":[],"session_crons":[]}"#,
            Status::Idle,
            "turn finished",
            "The subagent replied ok.",
        ),
        ok(
            "session end",
            r#"{"hook_event_name":"SessionEnd","session_id":"s1","reason":"prompt_input_exit"}"#,
            Status::Ended,
            "prompt_input_exit",
            "",
        ),
        // Observed: /clear fires SessionEnd (reason clear), then SessionStart
        // (source clear). Reporting exited in between would fail queued messages.
        skip(
            "session end on /clear is not an exit",
            r#"{"hook_event_name":"SessionEnd","session_id":"s1","reason":"clear"}"#,
        ),
        skip(
            "unknown events are ignored",
            r#"{"hook_event_name":"SubagentStart","session_id":"s1"}"#,
        ),
    ];

    for tt in tests {
        let r =
            parse(tt.payload).unwrap_or_else(|e| panic!("{}: parse_hook_event: {e:#}", tt.name));
        assert_eq!(r.is_some(), tt.want_ok, "{}: ok", tt.name);
        let Some(r) = r else { continue };
        assert_eq!(r.status, tt.want_status, "{}: status", tt.name);
        assert_eq!(r.detail, tt.want_detail, "{}: detail", tt.name);
        assert_eq!(r.result, tt.want_result, "{}: result", tt.name);
        assert_eq!(r.session_id, "s1", "{}: session id", tt.name);
    }
    // A session end says how it ended.
    let r = parse(r#"{"hook_event_name":"SessionEnd","session_id":"s1","reason":"other"}"#)
        .unwrap()
        .unwrap();
    assert_eq!(r.end_reason, END_QUIT);
}

/// A new prompt or conversation makes the stored result stale.
#[test]
fn test_claude_parse_hook_event_clear_result() {
    for (payload, want) in [
        (
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#,
            true,
        ),
        (
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"<task-notification>..."}"#,
            true,
        ),
        (
            r#"{"hook_event_name":"SessionStart","source":"clear"}"#,
            true,
        ),
        (
            r#"{"hook_event_name":"SessionStart","source":"compact"}"#,
            false,
        ),
        (
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{}}"#,
            false,
        ),
        (
            r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#,
            false,
        ),
        (
            r#"{"hook_event_name":"Stop","last_assistant_message":"","background_tasks":[]}"#,
            false,
        ),
    ] {
        let r = parse(payload)
            .unwrap()
            .unwrap_or_else(|| panic!("{payload}: not ok"));
        assert_eq!(r.clear_result, want, "{payload}: clear_result");
    }
}

/// Each PermissionRequest opens a pending prompt that its call's PostToolUse
/// (same tool and input) resolves; a subagent's carries its id and says
/// whose it is, as Claude's dialog does.
#[test]
fn test_claude_parse_hook_event_pending_prompts() {
    let p = |payload: &str| {
        parse(payload)
            .unwrap()
            .unwrap_or_else(|| panic!("{payload}: not ok"))
    };
    let req = p(
        r#"{"hook_event_name":"PermissionRequest","agent_id":"a1","agent_type":"general-purpose","tool_name":"Bash","tool_input":{"command":"rmdir sub1"}}"#,
    );
    assert!(
        req.prompt == PromptChange::Opened && req.agent == "a1" && !req.call_key.is_empty(),
        "PermissionRequest: {req:?}"
    );
    assert_eq!(
        req.detail,
        "permission: Bash: rmdir sub1 (from the general-purpose agent)"
    );
    let post = p(
        r#"{"hook_event_name":"PostToolUse","agent_id":"a1","tool_name":"Bash","tool_input":{ "command": "rmdir sub1" },"tool_response":{}}"#,
    );
    assert_eq!(post.prompt, PromptChange::Resolved);
    assert_eq!(post.call_key, req.call_key);
    let other = p(
        r#"{"hook_event_name":"PostToolUseFailure","tool_name":"Bash","tool_input":{"command":"rmdir sub2"}}"#,
    );
    assert_ne!(
        other.call_key, req.call_key,
        "another call has the same call key"
    );
    let sub = p(
        r#"{"hook_event_name":"PermissionRequest","agent_id":"a1","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
    );
    assert_eq!(sub.detail, "permission: Bash: ls (from a subagent)");
    for (payload, want) in [
        (
            r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{}}"#,
            PromptChange::None,
        ),
        (
            r#"{"hook_event_name":"UserPromptSubmit","prompt":"hi"}"#,
            PromptChange::MainEnded,
        ),
        (
            r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#,
            PromptChange::AllEnded,
        ),
        (
            r#"{"hook_event_name":"Stop","background_tasks":[{"type":"subagent","status":"running"}]}"#,
            PromptChange::None,
        ),
        (
            r#"{"hook_event_name":"SessionEnd","reason":"other"}"#,
            PromptChange::AllEnded,
        ),
        (
            r#"{"hook_event_name":"StopFailure","error":"rate_limit"}"#,
            PromptChange::AllEnded,
        ),
        (
            r#"{"hook_event_name":"SessionStart","source":"startup"}"#,
            PromptChange::AllEnded,
        ),
    ] {
        assert_eq!(p(payload).prompt, want, "{payload}: prompt change");
    }
}

/// Every payload names the session's transcript; agent logs reads it.
#[test]
fn test_claude_parse_hook_event_transcript() {
    let payload = r#"{"hook_event_name":"PreToolUse","session_id":"s1","transcript_path":"/home/user/.claude/projects/-home-user-proj/s1.jsonl","tool_name":"Bash","tool_input":{"command":"ls"}}"#;
    let r = parse(payload).unwrap().unwrap();
    assert_eq!(
        r.transcript,
        "/home/user/.claude/projects/-home-user-proj/s1.jsonl"
    );
}

#[test]
fn test_claude_parse_hook_event_invalid_json() {
    let err = parse("not json").expect_err("expected error for invalid payload");
    assert_eq!(
        format!("{err:#}"),
        "decoding hook payload: invalid character 'o' in literal null (expecting 'u')"
    );
}

/// A permission prompt's detail and input are kept whole: whoever answers
/// it needs to see everything it allows.
#[test]
fn test_claude_parse_hook_event_keeps_permission_input_whole() {
    let command = format!("{}{}", "é".repeat(300), r#"\necho \"two\""#); // as escaped in JSON
    let payload = format!(
        r#"{{"hook_event_name":"PermissionRequest","tool_name":"Bash","tool_input":{{"command":"{command}","description":"d"}}}}"#
    );
    let r = parse(&payload).unwrap().unwrap();
    assert_eq!(
        r.detail,
        format!("permission: Bash: {}\necho \"two\"", "é".repeat(300))
    );
    assert_eq!(
        r.tool_input,
        format!(r#"{{"command":"{command}","description":"d"}}"#)
    );

    // Other events don't keep the input.
    let r = parse(
        r#"{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(r.tool_input, "", "PreToolUse kept tool input");
}

/// A new Claude agent's session id is pinned; its hooks come from Claude
/// Code's user settings, so the launch command carries none.
#[test]
fn test_claude_launch_command_with_session() {
    let flag = format!(" --settings '{}'", namedagents::CLAUDE_SPAWN_SETTINGS);
    let cmd = namedagents::new_claude_backend()
        .launch_command_with_session("11111111-2222-3333-4444-555555555555");
    assert_eq!(
        cmd,
        format!("claude{flag} --session-id '11111111-2222-3333-4444-555555555555'")
    );
    let cmd = namedagents::new_claude_backend_with_config(ClaudeConfig {
        skip_permissions: true,
        ..Default::default()
    })
    .launch_command_with_session("sid");
    assert!(
        cmd.starts_with(&format!(
            "claude --dangerously-skip-permissions{flag} --session-id "
        )),
        "unexpected launch command: {cmd}"
    );
}

/// A resumed agent continues its session: --resume names it, and the
/// spawn's own id isn't passed (Claude refuses --session-id with --resume).
#[test]
fn test_claude_launch_command_with_session_resume() {
    let provider = namedagents::new_claude_backend_with_config(ClaudeConfig {
        resume: "3f34adff-9ebe-405a-aac0-8f69e8fd85f5".to_string(),
        ..Default::default()
    });
    let cmd = provider.launch_command_with_session("spawn-id");
    assert_eq!(
        cmd,
        format!(
            "claude --settings '{}' --resume '3f34adff-9ebe-405a-aac0-8f69e8fd85f5'",
            namedagents::CLAUDE_SPAWN_SETTINGS
        ),
        "a resumed session takes no --session-id"
    );
}

#[test]
fn test_registry_status() {
    let entry = |status: &str, waiting_for: &str, unknown: &str| Entry {
        status: status.to_string(),
        waiting_for: waiting_for.to_string(),
        unknown: unknown.to_string(),
        ..Default::default()
    };
    for (e, want, detail) in [
        (
            entry("busy", "", ""),
            Status::Processing,
            "a turn is running",
        ),
        (entry("idle", "", ""), Status::Idle, "at its prompt"),
        // The turn is over, but a shell it started runs on, and its end
        // starts the next turn: not done.
        (
            entry("shell", "", ""),
            Status::Processing,
            "turn over; a background shell it started is still running, and its end starts the next turn",
        ),
        (
            entry("waiting", "permission", ""),
            Status::Waiting,
            "waiting for permission",
        ),
        (
            entry("waiting", "", ""),
            Status::Waiting,
            "waiting for a person",
        ),
        (
            entry("", "", ""),
            Status::Unknown,
            "the CLI has not recorded a status yet",
        ),
        (
            entry(
                "compacting",
                "",
                r#"status "compacting" is not one gomuxai knows"#,
            ),
            Status::Unknown,
            r#"status "compacting" is not one gomuxai knows"#,
        ),
    ] {
        let (st, d) = namedagents::registry_status(&e);
        assert_eq!((st, d.as_str()), (want, detail), "{:?}", e.status);
    }
}

#[test]
fn test_claude_permission_key() {
    let provider = namedagents::new_claude_backend();
    for (resp, key) in [
        (PermissionResponse::Approve, "1"),
        (PermissionResponse::ApproveAlways, "2"),
        (PermissionResponse::Deny, "Escape"),
    ] {
        assert_eq!(
            provider.permission_key(resp, "Bash").unwrap(),
            key,
            "{resp:?}"
        );
    }
}

/// Question dialogs also raise PermissionRequest, but "1" would pick an
/// answer.
#[test]
fn test_claude_permission_key_refuses_question_dialogs() {
    let provider = namedagents::new_claude_backend();
    for tool in ["AskUserQuestion", "ExitPlanMode"] {
        let err = provider
            .permission_key(PermissionResponse::Approve, tool)
            .expect_err("want an error");
        assert_eq!(
            err.to_string(),
            format!(
                "{tool} is a question for the user, not a permission prompt; answer it with agent keys (see it with agent capture), or --force"
            )
        );
    }
    assert!(
        provider
            .permission_key(PermissionResponse::Approve, "")
            .is_ok(),
        "empty tool should be treated as a plain prompt"
    );
}

/// deny's Escape cancels a running turn, so it is only sent while the
/// dialog is actually on screen.
#[test]
fn test_claude_permission_prompt_visible() {
    let b = namedagents::new_claude_backend();
    for (name, screen, want) in [
        ("bash dialog, No is 4", CLAUDE_BASH_DIALOG, true),
        ("write dialog, No is 3", CLAUDE_WRITE_DIALOG, true),
        ("running turn", CLAUDE_RUNNING, false),
        (
            "idle, reply mentions options",
            CLAUDE_IDLE_AFTER_LIST,
            false,
        ),
        ("empty", "", false),
    ] {
        assert_eq!(b.permission_prompt_visible(screen), want, "{name}");
    }
}

/// The briefing goes inline, quoted as one word whatever it holds.
#[test]
fn test_claude_with_instructions() {
    let brief = "# You're agent \"worker\"\n\nRun `agent send` when done; $HOME stays literal.\n";
    let dir = temp_dir();
    let path = path_in(&dir, "instructions.md");
    std::fs::write(&path, brief).unwrap();
    let cmd = namedagents::new_claude_backend()
        .with_instructions("claude --session-id x", &path)
        .unwrap();
    let args = shell_args(cmd.strip_prefix("claude ").unwrap());
    assert!(
        args.len() == 4 && args[2] == "--append-system-prompt" && args[3] == brief,
        "args = {args:?}"
    );

    // A briefing that can't be read is an error, never an agent without it.
    let missing = path_in(&dir, "missing.md");
    let err = namedagents::new_claude_backend()
        .with_instructions("claude", &missing)
        .expect_err("missing briefing launched");
    assert_eq!(
        format!("{err:#}"),
        format!(
            "reading the instructions for claude's system prompt: open {missing}: no such file or directory"
        )
    );
}

/// Every launch flag gomuxai gives claude must be one Claude Code's /restart
/// carries over; after any of CLAUDE_RESTART_BLOCKING_FLAGS it refuses to
/// restart in place. The command is built as spawn builds it, every option
/// on.
#[test]
fn test_claude_launch_restartable() {
    let dir = temp_dir();
    let path = path_in(&dir, "instructions.md");
    std::fs::write(&path, "# briefing\n").unwrap();
    for cfg in [
        ClaudeConfig::default(),
        ClaudeConfig {
            skip_permissions: true,
            resume: "r1".to_string(),
        },
    ] {
        let b = namedagents::new_claude_backend_with_config(cfg.clone());
        let cmd = b.with_session_name(&b.launch_command_with_session("s1"), "worker");
        let cmd = b.with_model(&cmd, "opus");
        let cmd = b.with_instructions(&cmd, &path).unwrap();
        let cmd = b.allow_commands(
            &cmd,
            &["agent send".to_string(), "/opt/x/agent status".to_string()],
        );
        for arg in shell_args(cmd.strip_prefix("claude ").unwrap()) {
            for f in namedagents::CLAUDE_RESTART_BLOCKING_FLAGS {
                assert!(
                    arg != *f && !arg.starts_with(&format!("{f}=")),
                    "{cfg:?}: launch uses {f}, which Claude Code's /restart refuses: {cmd}"
                );
            }
        }
    }
}

#[test]
fn test_claude_allow_tools() {
    let b = namedagents::new_claude_backend();
    let cmd = b.allow_tools(
        "claude",
        &["agent status".to_string()],
        &["Read(//srv/app/**)".to_string()],
        &["Edit".to_string(), "Write".to_string()],
    );
    let args = shell_args(cmd.strip_prefix("claude ").unwrap());
    assert_eq!(
        args,
        [
            "--allowedTools",
            "Bash(agent status:*)",
            "Read(//srv/app/**)",
            "--disallowedTools",
            "Edit",
            "Write"
        ]
    );
    assert_eq!(b.allow_tools("claude", &[], &[], &[]), "claude");
}

#[test]
fn test_claude_allow_commands() {
    let cmd = namedagents::new_claude_backend().allow_commands(
        "claude",
        &["agent status".to_string(), "/opt/x/agent send".to_string()],
    );
    let args = shell_args(cmd.strip_prefix("claude ").unwrap());
    assert_eq!(
        args,
        [
            "--allowedTools",
            "Bash(agent status:*)",
            "Bash(/opt/x/agent send:*)"
        ]
    );
    assert_eq!(
        namedagents::new_claude_backend().allow_commands("claude", &[]),
        "claude"
    );
}

/// Screens captured with escapes (tmux capture-pane -e) from Claude Code
/// v2.1.310: its input box sits between two rules, the placeholder drawn
/// dim.
#[test]
fn test_claude_input_text() {
    let rule = format!("\x1b[38;5;244m{}", "─".repeat(60));
    let footer =
        "\x1b[39m  \x1b[38;5;220m⏵⏵ auto mode on\x1b[38;5;246m (shift+tab to cycle)\x1b[39m";
    let screen = |boxed: &[&str]| {
        let mut lines = vec!["● ok", "", rule.as_str()];
        lines.extend_from_slice(boxed);
        lines.extend_from_slice(&[rule.as_str(), footer]);
        lines.join("\n") + "\n"
    };
    let top = format!("\x1b[38;5;244m{} worker ─", "─".repeat(50));
    let named = |boxed: &[&str]| {
        let mut lines = vec!["● ok", "", top.as_str()];
        lines.extend_from_slice(boxed);
        lines.extend_from_slice(&[rule.as_str(), footer]);
        lines.join("\n") + "\n"
    };
    let label_without_rule = ["● ok", "─ worker ─", "\x1b[39m❯ ", rule.as_str(), footer].join("\n");
    let tests: Vec<(&str, String, Option<&str>)> = vec![
        (
            "empty, dim placeholder",
            screen(&["\x1b[39m❯ \x1b[2mTry \"fix lint errors\"\x1b[0m"]),
            Some(""),
        ),
        ("empty", screen(&["\x1b[39m❯ "]), Some("")),
        (
            "typed",
            screen(&["\x1b[39m❯ half typed by a person"]),
            Some("half typed by a person"),
        ),
        (
            "two lines",
            screen(&["\x1b[39m❯ half typed by a person", "  second line"]),
            Some("half typed by a person\nsecond line"),
        ),
        (
            "typed, then a dim hint",
            screen(&["\x1b[39m❯ yes\x1b[2m (enter to send)\x1b[22m"]),
            Some("yes"),
        ),
        ("a shell", "$ ls\nfile\n$ \n".to_string(), None),
        (
            "a dialog",
            "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel\n".to_string(),
            None,
        ),
        ("rules without a prompt", screen(&["  1. Apple"]), None),
        // A named session (--name, /rename) has its name in the top rule.
        // Missing it left every message to an idle agent held until a
        // person started its next turn.
        ("named session, empty", named(&["\x1b[39m❯ "]), Some("")),
        (
            "named session, typed",
            named(&["\x1b[39m❯ half typed"]),
            Some("half typed"),
        ),
        ("a label without its rule", label_without_rule, None),
        // What sending reads to tell a message took in, then submitted.
        // Prompts are drawn with a no-break space after the "❯".
        (
            "a paste's placeholder",
            screen(&["\x1b[39m❯\u{a0}[Pasted text #17 +60 lines]"]),
            Some("[Pasted text #17 +60 lines]"),
        ),
        (
            "an image path, converted",
            screen(&["\x1b[39m❯\u{a0}[Image #21] [Pasted text #3 +4 lines]"]),
            Some("[Image #21] [Pasted text #3 +4 lines]"),
        ),
        // An Enter that arrived in one read with the text: a newline, and
        // the message sits there.
        (
            "a message with its Enter as a newline",
            screen(&["\x1b[39m❯\u{a0}[from lead@hub] run the tests", ""]),
            Some("[from lead@hub] run the tests"),
        ),
        // Mid-turn, a submitted message is queued, and the box says so dim.
        (
            "queued, dim hint",
            screen(&[
                "\x1b[38;5;246m❯\u{a0}\x1b[2m\x1b[39mPress up to edit queued messages\x1b[0m",
            ]),
            Some(""),
        ),
    ];
    let b = namedagents::new_claude_backend();
    for (name, screen, want) in tests {
        assert_eq!(b.input_text(&screen).as_deref(), want, "{name}");
    }
}

/// Held messages go back as Stop's additionalContext: Claude goes on with
/// it, shown as "Stop hook feedback". {"decision":"block"} would show as an
/// error, and only Stop can deliver.
#[test]
fn test_claude_delivery() {
    let b = namedagents::new_claude_backend();
    for (messages, want) in [
        (
            vec![r#"[from lead@hub] "quoted" <tag>"#.to_string()],
            "A message arrived while you were working:\n\n[from lead@hub] \"quoted\" <tag>",
        ),
        (
            vec![
                "[from a@hub] one".to_string(),
                "[from b@hub] two".to_string(),
            ],
            "2 messages arrived while you were working, oldest first:\n\n[from a@hub] one\n\n[from b@hub] two",
        ),
    ] {
        let out = b.delivery_output(&messages);
        let got: serde_json::Map<String, serde_json::Value> = serde_json::from_slice(&out).unwrap();
        let h = got["hookSpecificOutput"].as_object().unwrap();
        assert!(
            got.len() == 1
                && h.len() == 2
                && h["hookEventName"] == "Stop"
                && h["additionalContext"] == want,
            "delivery_output = {}",
            String::from_utf8_lossy(&out)
        );
    }
    // The bytes as Go writes them: keys sorted, HTML characters escaped.
    assert_eq!(
        String::from_utf8(b.delivery_output(&["<a>".to_string()])).unwrap(),
        "{\"hookSpecificOutput\":{\"additionalContext\":\"A message arrived while you were working:\\n\\n\\u003ca\\u003e\",\"hookEventName\":\"Stop\"}}"
    );
    for (event, agent, want) in [
        ("Stop", "", true),
        ("PostToolBatch", "", true),
        ("PostToolBatch", "sub-1", false),
        ("PostToolUse", "", false),
        ("StopFailure", "", false),
        ("Notification", "", false),
    ] {
        let r = HookReport {
            event: event.to_string(),
            agent: agent.to_string(),
            ..Default::default()
        };
        assert_eq!(
            b.can_deliver(&r),
            want,
            "can_deliver({event}, agent={agent:?})"
        );
    }
    assert_eq!(b.delivery_point(), "its next tool batch or turn ends");

    let post_batch = parse(r#"{"hook_event_name":"PostToolBatch","session_id":"s1"}"#)
        .unwrap()
        .unwrap();
    assert!(post_batch.delivery_only && post_batch.status == Status::Processing);

    let batch = [
        CoalescedMessage {
            id: 1,
            sender: "lead".into(),
            content: "[from lead@h] draft".into(),
            age_secs: 20,
            urgency: "soon".into(),
            superseded_by: 2,
            topic: String::new(),
        },
        CoalescedMessage {
            id: 2,
            sender: "lead".into(),
            content: "[from lead@h] final".into(),
            age_secs: 5,
            urgency: "soon".into(),
            superseded_by: 0,
            topic: String::new(),
        },
    ];
    let ptb_out: serde_json::Value =
        serde_json::from_slice(&b.delivery_output_for("PostToolBatch", &batch)).unwrap();
    assert_eq!(
        ptb_out["hookSpecificOutput"]["hookEventName"],
        "PostToolBatch"
    );
    assert_eq!(
        ptb_out["systemMessage"],
        "[gomuxai] Delivered 1 held message (1 superseded) mid-turn"
    );

    let pre = HookReport {
        event: "PreToolUse".into(),
        ..Default::default()
    };
    let deny: serde_json::Value =
        serde_json::from_slice(&b.interrupt_output(&pre, "urgent stop").unwrap()).unwrap();
    assert_eq!(deny["continue"], false);
    assert_eq!(deny["hookSpecificOutput"]["permissionDecision"], "deny");
    let sub_pre = HookReport {
        event: "PreToolUse".into(),
        agent: "sub-1".into(),
        ..Default::default()
    };
    assert!(b.interrupt_output(&sub_pre, "urgent stop").is_none());
}

#[test]
fn test_claude_allow() {
    let b = namedagents::new_claude_backend();
    let r = parse(
        r#"{"hook_event_name":"PreToolUse","cwd":"/work","tool_name":"Bash","tool_input":{"command":"./bin/agent list","description":"List agents"}}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (r.command.as_str(), r.cwd.as_str()),
        ("./bin/agent list", "/work")
    );
    for payload in [
        r#"{"hook_event_name":"PreToolUse","cwd":"/work","tool_name":"Read","tool_input":{"command":"./bin/agent list"}}"#,
        r#"{"hook_event_name":"PermissionRequest","cwd":"/work","tool_name":"Bash","tool_input":{"command":"./bin/agent list"}}"#,
    ] {
        let r = parse(payload).unwrap().unwrap();
        assert_eq!(
            r.command, "",
            "{payload}: only PreToolUse for Bash can approve"
        );
    }

    let out = b.allow_output("safe");
    assert_eq!(
        String::from_utf8(out).unwrap(),
        r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","permissionDecisionReason":"safe"}}"#
    );
}

/// Lone UTF-16 surrogates, raw non-UTF-8 bytes, and null array elements in
/// hook payloads decode like Go's encoding/json instead of dropping the report.
#[test]
fn test_claude_parse_hook_event_lenient_utf8_and_null_array_elements() {
    let claude = namedagents::new_claude_backend();

    // 2.1(1): lone surrogate and raw non-UTF-8 in Stop's last_assistant_message.
    let r = parse(
        r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"cut \ud83d"}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (r.status, r.result.as_str(), r.prompt),
        (Status::Idle, "cut \u{fffd}", PromptChange::AllEnded)
    );

    let r = claude
        .parse_hook_event(
            "",
            b"{\"hook_event_name\":\"Stop\",\"session_id\":\"s1\",\"last_assistant_message\":\"cut \xff\"}",
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        (r.status, r.result.as_str()),
        (Status::Idle, "cut \u{fffd}")
    );

    // 2.1(3): lone surrogate in PreToolUse Bash tool_input (command and description).
    let r = parse(
        r#"{"hook_event_name":"PreToolUse","cwd":"/w","tool_name":"Bash","tool_input":{"command":"./bin/agent send x \"cut \ud83d\""}}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(r.command, "./bin/agent send x \"cut \u{fffd}\"");
    assert_eq!(r.detail, "Bash: ./bin/agent send x \"cut \u{fffd}\"");

    let r = parse(
        r#"{"hook_event_name":"PreToolUse","cwd":"/w","tool_name":"Bash","tool_input":{"command":"./bin/agent list","description":"emoji \ud83d"}}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(r.command, "./bin/agent list");
    assert_eq!(r.detail, "Bash: ./bin/agent list");

    // 2.2(1): null elements in background_tasks decode to zero values.
    let r = parse(
        r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"ok","background_tasks":[null]}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!((r.status, r.result.as_str()), (Status::Idle, "ok"));

    let r = parse(
        r#"{"hook_event_name":"Stop","session_id":"s1","last_assistant_message":"wait","background_tasks":[null,{"status":"running"}]}"#,
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        (r.status, r.detail.as_str()),
        (
            Status::Processing,
            "turn finished, 1 background task(s) running"
        )
    );
}
