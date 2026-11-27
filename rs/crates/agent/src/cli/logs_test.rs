//! Go: internal/cli/logs_test.go (and remote_ops_test.go's cases of agent
//! logs name@host, which run build_logs on the serving side).
use std::sync::atomic::Ordering;

use agent_transcript::{
    Image, KIND_PROMPT, KIND_SAY, KIND_SYSTEM, KIND_THINK, KIND_TOOL, OP_UPDATE, PromptEvent, RawJson, SayEvent,
    ThinkEvent, ToolResult, ToolSummary,
};

use super::*;
use crate::cli::root::execute_ctx;
use crate::cli::testenv::{TestEnv, temp_dir};

fn line_range(from: usize, to: usize) -> Vec<String> {
    (from..to).map(|i| format!("line {i}")).collect()
}

#[test]
fn test_tail_log_lines() {
    struct Case {
        name: &'static str,
        padding: usize, // bytes of escape sequences after each line's text
        lines: usize,
        n: usize,
        expected: Vec<String>,
    }
    let cases = [
        Case {
            name: "small file",
            padding: 0,
            lines: 3,
            n: 50,
            expected: line_range(0, 3),
        },
        Case {
            name: "last n of a small file",
            padding: 0,
            lines: 10,
            n: 2,
            expected: line_range(8, 10),
        },
        // Larger than the first window: the line cut off at its start must not
        // show up as a line of its own.
        Case {
            name: "tail of a large file",
            padding: 0,
            lines: 100_000,
            n: 3,
            expected: line_range(99_997, 100_000),
        },
        // So much padding that the first window holds fewer than n lines.
        Case {
            name: "window grows",
            padding: 10_000,
            lines: 200,
            n: 40,
            expected: line_range(160, 200),
        },
    ];
    for tt in cases {
        let mut b = String::new();
        let pad = "\x1b[K".repeat(tt.padding / 3);
        for i in 0..tt.lines {
            b.push_str(&format!("\x1b[38;5;246mline {i}\x1b[39m{pad}\r\n"));
        }
        let dir = temp_dir("agent-logs-");
        let path = dir.path().join("agent.log").to_string_lossy().into_owned();
        std::fs::write(&path, b).unwrap();
        let got = tail_log_lines(&path, tt.n).unwrap_or_else(|e| panic!("{}: tail_log_lines: {e:#}", tt.name));
        assert_eq!(got, tt.expected, "{}: tail_log_lines(n={})", tt.name, tt.n);
    }
}

#[test]
fn test_tail_log_lines_missing_file() {
    let dir = temp_dir("agent-logs-");
    let path = dir.path().join("none.log").to_string_lossy().into_owned();
    let err = tail_log_lines(&path, 10).expect_err("a missing file");
    assert!(goos::is_not_exist(&err), "err = {err:#}, want a not-exist error");
}

/// A synthetic Claude Code transcript: one prompt, a tool call, a reply.
pub(crate) const TEST_TRANSCRIPT: &str = r#"{"type":"user","sessionId":"sess-1","cwd":"/home/user/proj","timestamp":"2026-08-01T12:04:31Z","message":{"role":"user","content":"run the tests"}}
{"type":"assistant","timestamp":"2026-08-01T12:04:33Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"go test ./... && go vet ./..."}}]}}
{"type":"user","timestamp":"2026-08-01T12:04:40Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok  a\nok  b\nok  c"}]}}
{"type":"assistant","timestamp":"2026-08-01T12:05:02Z","message":{"role":"assistant","content":[{"type":"text","text":"All tests pass.\nNothing else to do."}]}}
"#;

/// setup_cli, with the bus forbidden and the log dir made (Go: setupLogs;
/// times print in UTC: testenv's TZ).
pub(crate) fn setup_logs() -> TestEnv {
    let env = TestEnv::setup_cli();
    env.forbid_bus();
    std::fs::create_dir_all(&env.cfg.log_dir).unwrap();
    env
}

/// Reports a hook naming path as worker's transcript (Go: hookTranscript).
pub(crate) fn hook_transcript(env: &TestEnv, path: &str) {
    let payload = serde_json::json!({"hook_event_name": "UserPromptSubmit", "transcript_path": path});
    env.service()
        .report_hook("worker", "", "", payload.to_string().as_bytes())
        .unwrap();
}

pub(crate) fn write_test_file(path: &str, content: &str) {
    std::fs::write(path, content).unwrap();
}

/// Runs the command line words in env's Ctx, and returns its result, its
/// stdout, and its stderr; the hint printed is forgotten, as the Go tests'
/// hinted = false does.
pub(crate) fn run_words(env: &mut TestEnv, words: &[&str]) -> (anyhow::Result<()>, String, String) {
    env.ctx.hinted.store(false, Ordering::Relaxed);
    let argv: Vec<String> = words.iter().map(|w| w.to_string()).collect();
    let res = execute_ctx(&mut env.ctx, &argv);
    (res, env.take_out(), env.take_err())
}

/// A temp dir's path joined with name, as a string.
pub(crate) fn temp_path(dir: &tempfile::TempDir, name: &str) -> String {
    dir.path().join(name).to_string_lossy().into_owned()
}

#[test]
fn test_logs_transcript() {
    crate::require_termd!();
    let mut env = setup_logs();
    let dir = temp_dir("agent-logs-");
    let path = temp_path(&dir, "sess-1.jsonl");
    write_test_file(&path, TEST_TRANSCRIPT);
    hook_transcript(&env, &path);

    let (res, out, hints) = run_words(&mut env, &["logs", "worker"]);
    res.unwrap();
    let want = "12:04:31 prompt run the tests
12:04:33 tool   Bash: go test ./... && go vet ./... → ok (3 lines)
12:05:02 say    All tests pass.…
";
    assert_eq!(out, want);
    assert!(hints.is_empty(), "hint for a readable transcript: {hints}");

    let (_, out, _) = run_words(&mut env, &["logs", "worker", "--lines", "1", "--full"]);
    assert_eq!(out, "12:05:02 say    All tests pass.\n                Nothing else to do.\n", "--lines 1 --full");

    // --json: the stream, a session line then the events, the tool's result an
    // update of its call.
    let (_, out, _) = run_words(&mut env, &["logs", "worker", "--json"]);
    let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
    assert_eq!(lines.len(), 5, "--json:\n{out}");
    let session: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(session["type"], "session", "--json:\n{out}");
    assert_eq!(session["harness"], "claude");
    assert_eq!(session["path"], path.as_str());
    assert_eq!(session["cwd"], "/home/user/proj");
    assert_eq!(session["project"], "proj");
    let mut ops = Vec::new();
    for line in &lines[1..] {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(v["type"], "event", "--json line {line}");
        let e: Event = serde_json::from_value(v).unwrap();
        ops.push(format!("{} {}", e.op, e.kind));
        if e.op == OP_UPDATE {
            let result = e.tool.as_ref().and_then(|t| t.result.as_ref()).expect("a result");
            assert_eq!(result.output, "ok  a\nok  b\nok  c", "--json tool result");
        }
    }
    assert_eq!(ops, ["add prompt", "add tool", "update tool", "add say"]);
    assert!(out.contains(r#""go test ./... && go vet ./...""#), "--json escapes the command:\n{out}");
}

/// The list of transcripts outlives the agent record, and spans sessions.
#[test]
fn test_logs_after_cleanup_and_across_sessions() {
    crate::require_termd!();
    let mut env = setup_logs();
    let dir = temp_dir("agent-logs-");
    let (first, second) = (temp_path(&dir, "sess-1.jsonl"), temp_path(&dir, "sess-2.jsonl"));
    write_test_file(&first, TEST_TRANSCRIPT);
    write_test_file(
        &second,
        "{\"type\":\"user\",\"sessionId\":\"sess-2\",\"timestamp\":\"2026-08-01T13:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"again\"}}\n",
    );
    hook_transcript(&env, &first);
    hook_transcript(&env, &first);
    hook_transcript(&env, &second);
    env.store().delete_agent("worker").unwrap();

    let (res, out, _) = run_words(&mut env, &["logs", "worker", "--lines", "2"]);
    res.unwrap();
    let want = "12:05:02 say    All tests pass.…
--- session sess-2 (claude)
13:00:00 prompt again
";
    assert_eq!(out, want);
}

#[test]
fn test_logs_falls_back_to_terminal_log() {
    crate::require_termd!();
    let mut env = setup_logs();
    write_test_file(&env.cfg.log_path("worker"), "$ claude\r\nhello from the terminal\r\n");

    let (res, out, hints) = run_words(&mut env, &["logs", "worker"]);
    assert!(res.is_ok() && out.contains("hello from the terminal"), "no transcript: {out:?}, {res:?}");
    assert_eq!(hints, "hint: printing worker's terminal log: no transcript recorded for it\n");

    let dir = temp_dir("agent-logs-");
    let gone = temp_path(&dir, "gone.jsonl");
    hook_transcript(&env, &gone);
    let (_, out, hints) = run_words(&mut env, &["logs", "worker"]);
    assert!(
        out.contains("hello from the terminal") && hints.contains(&format!("its transcript {gone} is gone")),
        "missing transcript: {out:?}, hint {hints:?}"
    );

    let (res, _, _) = run_words(&mut env, &["logs", "worker", "--json"]);
    let err = res.expect_err("--json without a transcript");
    assert!(format!("{err:#}").contains("--terminal"), "--json without a transcript: {err:#}");

    let (_, out, hints) = run_words(&mut env, &["logs", "worker", "--terminal"]);
    assert!(
        out.contains("hello from the terminal") && hints.is_empty(),
        "--terminal: {out:?}, hint {hints:?}"
    );
}

#[test]
fn test_logs_unparseable() {
    crate::require_termd!();
    let mut env = setup_logs();
    write_test_file(&env.cfg.log_path("worker"), "terminal text\n");
    let dir = temp_dir("agent-logs-");
    let path = temp_path(&dir, "s.jsonl");
    write_test_file(&path, "not a transcript\n");
    hook_transcript(&env, &path);

    let (_, out, hints) = run_words(&mut env, &["logs", "worker"]);
    assert!(
        out.contains("terminal text") && hints.contains("can't be read: unparseable transcript"),
        "unparseable: {out:?}, hint {hints:?}"
    );
}

/// A name with neither a log nor an agent suggests close names.
#[test]
fn test_logs_no_log() {
    crate::require_termd!();
    let mut env = setup_logs();
    let (res, _, _) = run_words(&mut env, &["logs", "workr"]);
    let err = format!("{:#}", res.expect_err("no such agent"));
    assert_eq!(
        err,
        "no log for \"workr\": no agent by that name has run on host local; did you mean worker?"
    );
    let (res, _, _) = run_words(&mut env, &["logs", "zzz"]);
    let err = format!("{:#}", res.expect_err("no such agent"));
    assert_eq!(
        err,
        format!(
            "no log for \"zzz\": no agent by that name has run on host local (logs are in {})",
            env.cfg.log_dir
        )
    );
}

/// A host whose bus doesn't serve logs (an older agent) is no bus answering,
/// and the hint points at capture.
#[test]
fn test_logs_host_without_logs_op() {
    crate::require_termd!();
    let mut env = setup_logs();
    if env
        .serve_as_h1_with(&[agent_bus::OP_LIST, agent_bus::OP_STATUS, agent_bus::OP_CAPTURE])
        .is_none()
    {
        eprintln!("skipping: no agent-busd");
        return;
    }
    let (res, _, hints) = run_words(&mut env, &["logs", "worker@h1"]);
    let err = format!("{:#}", res.expect_err("no logs op"));
    assert!(
        err.contains("does it allow \"logs\"") && hints.contains("agent capture worker@h1"),
        "remote: {err}, hint {hints:?}"
    );
}

/// agent logs name@host prints what agent logs prints on that host, with the
/// same flags, and its hint (Go: TestRemoteLogsMatchLocal).
#[test]
fn test_remote_logs_match_local() {
    crate::require_termd!();
    let mut env = setup_logs();
    if env.serve_as_h1().is_none() {
        eprintln!("skipping: no agent-busd");
        return;
    }
    let dir = temp_dir("agent-logs-");
    let path = temp_path(&dir, "sess-1.jsonl");
    write_test_file(&path, TEST_TRANSCRIPT);
    hook_transcript(&env, &path);

    for flags in [
        &["--lines", "50"][..],
        &["--lines", "1", "--full"],
        &["--lines", "50", "--json"],
        &["--lines", "2", "--terminal"],
    ] {
        let local_words: Vec<&str> = ["logs", "worker"].iter().chain(flags).copied().collect();
        let (res, local, local_hints) = run_words(&mut env, &local_words);
        res.unwrap_or_else(|e| panic!("{flags:?}: local: {e:#}"));
        let remote_words: Vec<&str> = ["logs", "worker@h1"].iter().chain(flags).copied().collect();
        let (res, remote, remote_hints) = run_words(&mut env, &remote_words);
        res.unwrap_or_else(|e| panic!("{flags:?}: remote: {e:#}"));
        assert_eq!(remote, local, "{flags:?}: output differs");
        assert_eq!(remote_hints, local_hints, "{flags:?}: hints differ");
    }
}

/// With no transcript, logs falls back to the terminal log and says why, on
/// either host (Go: TestRemoteLogsHint).
#[test]
fn test_remote_logs_hint() {
    crate::require_termd!();
    let mut env = setup_logs();
    if env.serve_as_h1().is_none() {
        eprintln!("skipping: no agent-busd");
        return;
    }
    write_test_file(&env.cfg.log_path("worker"), "line one\r\nline two\r\n");
    let (res, out, hints) = run_words(&mut env, &["logs", "worker@h1"]);
    res.unwrap();
    assert_eq!(out, "line one\nline two\n");
    assert!(
        hints.contains("printing worker's terminal log: no transcript recorded for it"),
        "hint {hints:?}"
    );
}

/// A log too big for a bus message keeps its newest lines and says how much
/// was left out (Go: TestRemoteLogsTruncated).
#[test]
fn test_remote_logs_truncated() {
    crate::require_termd!();
    let mut env = setup_logs();
    if env.serve_as_h1().is_none() {
        eprintln!("skipping: no agent-busd");
        return;
    }
    let mut b = String::new();
    for i in 0..40_000 {
        b.push_str(&format!("terminal line {i:06} {}\r\n", "x".repeat(20)));
    }
    write_test_file(&env.cfg.log_path("worker"), &b);
    let (res, out, hints) = run_words(&mut env, &["logs", "worker@h1", "--terminal", "--lines", "40000"]);
    res.unwrap();
    let last = format!("terminal line 039999 {}\n", "x".repeat(20));
    assert!(
        out.len() <= MAX_LOGS_REPLY && out.ends_with(&last),
        "remote logs: {} bytes ending {:?}",
        out.len(),
        &out[out.len().saturating_sub(60)..]
    );
    assert!(out.starts_with("terminal line "), "remote logs start mid-line: {:?}", &out[..40.min(out.len())]);
    assert!(hints.contains("were cut to fit a bus message: the oldest"), "no word of the cut: {hints:?}");
}

#[test]
fn test_format_event() {
    const AT: &str = "2026-08-01T09:05:07.000Z";
    let tool = |status: &str, output: &str| -> Event {
        let input = br#"{"command":"make\nlint"}"#;
        Event {
            ts: AT.into(),
            kind: KIND_TOOL.into(),
            tool: Some(ToolEvent {
                name: "Bash".into(),
                input: Some(RawJson::from_json_str(std::str::from_utf8(input).unwrap()).unwrap()),
                result: Some(ToolResult {
                    status: status.into(),
                    output: output.into(),
                    ..Default::default()
                }),
                tool_summary: transcript::summarize_tool("Bash", input, None, ""),
                ..Default::default()
            }),
            ..Default::default()
        }
    };
    let edit = Event {
        ts: AT.into(),
        kind: KIND_TOOL.into(),
        tool: Some(ToolEvent {
            name: "Edit".into(),
            result: Some(ToolResult {
                status: transcript::STATUS_OK.into(),
                ..Default::default()
            }),
            tool_summary: ToolSummary {
                action: "edit".into(),
                target: "src/a.go".into(),
                meta: Some(RawJson::from_json_str(r#"{"added":3,"removed":1}"#).unwrap()),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    let system = |subtype: &str, text: &str, data: &str| -> Event {
        Event {
            ts: AT.into(),
            kind: KIND_SYSTEM.into(),
            system: Some(SystemEvent {
                subtype: subtype.into(),
                text: text.into(),
                data: (!data.is_empty()).then(|| RawJson::from_json_str(data).unwrap()),
            }),
            ..Default::default()
        }
    };
    let prompt = |ts: &str, text: &str, origin: &str| -> Event {
        Event {
            ts: ts.into(),
            kind: KIND_PROMPT.into(),
            prompt: Some(PromptEvent {
                text: text.into(),
                origin: origin.into(),
                ..Default::default()
            }),
            ..Default::default()
        }
    };
    let think_redacted = |abandoned: bool| Event {
        ts: AT.into(),
        abandoned,
        kind: KIND_THINK.into(),
        think: Some(ThinkEvent {
            redacted: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let long = "word ".repeat(40);
    let mut command = prompt(AT, "/review", "command");
    if let Some(p) = &mut command.prompt {
        p.command = "/review".into();
        p.images = vec![Image {
            media_type: "image/png".into(),
            data: "AA==".into(),
        }];
    }
    let mut abandoned = prompt(AT, "Relaoded!", "human");
    abandoned.abandoned = true;
    let long_command = Event {
        ts: AT.into(),
        kind: KIND_TOOL.into(),
        tool: Some(ToolEvent {
            name: "Bash".into(),
            result: Some(ToolResult {
                status: "ok".into(),
                ..Default::default()
            }),
            tool_summary: ToolSummary {
                action: "bash".into(),
                detail: format!(
                    "cd /home/user/src/widget && {} && go test ./internal/server -run TestSpin",
                    "x".repeat(60)
                ),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    let cases: Vec<(Event, bool, String)> = vec![
        (prompt("", "hi", "human"), false, "--:--:-- prompt hi".into()),
        (prompt(AT, "and the docs", "queued"), false, "09:05:07 prompt (queued) and the docs".into()),
        (command, false, "09:05:07 prompt /review [1 image]".into()),
        (
            prompt(
                AT,
                "<task-notification>\n<task-id>b1</task-id>\n<summary>Background command \"Serve\" completed</summary>\n</task-notification>",
                "task-notification",
            ),
            false,
            r#"09:05:07 prompt (task-notification) Background command "Serve" completed"#.into(),
        ),
        (
            prompt(
                AT,
                "[SYSTEM NOTIFICATION]\n<task-notification><summary>a done</summary></task-notification>\n<task-notification><summary>b failed</summary></task-notification>",
                "task-notification",
            ),
            false,
            "09:05:07 prompt (task-notification) a done; b failed".into(),
        ),
        // A delivery shows the message under gomuxai's head line.
        (
            prompt(
                AT,
                "A message arrived while you were working:\n\n[from lead@hub] skip cmd/legacy",
                "delivered",
            ),
            false,
            "09:05:07 prompt (delivered) [from lead@hub] skip cmd/legacy".into(),
        ),
        (think_redacted(false), false, "09:05:07 think".into()),
        // A prompt sent again edited: the old one, and what answered it.
        (abandoned, false, "09:05:07 prompt (abandoned) Relaoded!".into()),
        (think_redacted(true), false, "09:05:07 think  (abandoned)".into()),
        (
            Event {
                ts: AT.into(),
                kind: KIND_SAY.into(),
                say: Some(SayEvent {
                    text: long.clone(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            false,
            format!("09:05:07 say    {}…", long[..119].trim()),
        ),
        (tool("ok", ""), false, "09:05:07 tool   Bash: make lint → ok".into()),
        (tool("ok", "1 issue\n"), false, "09:05:07 tool   Bash: make lint → ok: 1 issue".into()),
        (
            tool("error", "Exit code 2\nlint: 1 issue"),
            false,
            "09:05:07 tool   Bash: make lint → error: Exit code 2… (2 lines)".into(),
        ),
        (
            tool("background", "started"),
            false,
            "09:05:07 tool   Bash: make lint → running in the background".into(),
        ),
        (tool("pending", ""), false, "09:05:07 tool   Bash: make lint → no result yet".into()),
        (tool("interrupted", ""), false, "09:05:07 tool   Bash: make lint → interrupted".into()),
        // A long command keeps its head and its tail.
        (
            long_command,
            false,
            "09:05:07 tool   Bash: cd /home/user/src/widget && xxxxxxxxxxxx…go test ./internal/server -run TestSpin → ok"
                .into(),
        ),
        (
            system("model_fallback", "", r#"{"from":"model-a","to":"model-b"}"#),
            false,
            "09:05:07 system model fallback: model-a → model-b".into(),
        ),
        (
            tool("ok", "a\nb"),
            true,
            "09:05:07 tool   Bash: make\n                lint → ok (2 lines)\n                a\n                b".into(),
        ),
        (edit, false, "09:05:07 tool   Edit: src/a.go (+3 -1) → ok".into()),
        (
            system("turn_duration", "", r#"{"durationMs":18234,"messageCount":9}"#),
            false,
            "09:05:07 system turn took 18.2s".into(),
        ),
        (
            system("interrupt", "[Request interrupted by user]", ""),
            false,
            "09:05:07 system [Request interrupted by user]".into(),
        ),
        (
            system("compact_boundary", "Conversation compacted", "{}"),
            false,
            "09:05:07 system compact_boundary: Conversation compacted".into(),
        ),
        (
            system("stop_hook_summary", "", r#"{"hookCount":1}"#),
            false,
            "09:05:07 system stop_hook_summary".into(),
        ),
    ];
    crate::cli::testenv::isolate(); // TZ=UTC
    for (e, full, want) in cases {
        assert_eq!(format_event(&e, full), want, "format_event(full={full})");
    }
}

/// Typed lines put "type" first, as Go's struct embedding does, and escape
/// U+2028 and U+2029 but not <, > and & (SetEscapeHTML(false)).
#[test]
fn test_typed_json_line() {
    let e = Event {
        id: "a".into(),
        op: "add".into(),
        kind: KIND_SAY.into(),
        say: Some(SayEvent {
            text: "a && <b>\u{2028}".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        typed_json_line("event", &e).unwrap(),
        "{\"type\":\"event\",\"id\":\"a\",\"op\":\"add\",\"seq\":0,\"at\":0,\"line\":0,\"kind\":\"say\",\"say\":{\"text\":\"a && <b>\\u2028\"}}\n"
    );
}

/// A transcript with a secret in a prompt, a second tool call, and a reply:
/// what reading in parts and scrubbing are tested on.
const PARTS_TRANSCRIPT: &str = r#"{"type":"user","sessionId":"sess-p","cwd":"/home/user/proj","timestamp":"2026-08-01T12:00:00Z","message":{"role":"user","content":"deploy it\nmy api_key is abc123\nthen report"}}
{"type":"assistant","timestamp":"2026-08-01T12:00:01Z","message":{"role":"assistant","content":[{"type":"thinking","thinking":"plan the deploy","signature":"s"}]}}
{"type":"assistant","timestamp":"2026-08-01T12:00:02Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}
{"type":"user","timestamp":"2026-08-01T12:00:03Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"test a ... ok\nAuthorization: Bearer zzzz9999yyyy\ntest b ... ok"}]}}
{"type":"assistant","timestamp":"2026-08-01T12:00:04Z","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"make deploy"}}]}}
{"type":"user","timestamp":"2026-08-01T12:00:05Z","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","content":"deployed"}]}}
{"type":"assistant","timestamp":"2026-08-01T12:00:06Z","message":{"role":"assistant","content":[{"type":"text","text":"Deployed; tests pass."}]}}
"#;

/// agent logs reads any session by its transcript's path or id, an agent's
/// or not, and in parts: --talk, --grep and --start, numbered, with a hint
/// that says how to read on.
#[test]
fn test_logs_session_in_parts() {
    let mut env = setup_logs();
    let dir = temp_dir("agent-logs-");
    let path = temp_path(&dir, "sess-p.jsonl");
    write_test_file(&path, PARTS_TRANSCRIPT);

    let (res, out, _) = run_words(&mut env, &["logs", &path, "--lines", "1"]);
    res.unwrap();
    assert_eq!(out, "12:00:06 say    Deployed; tests pass.\n", "by path");

    // --talk: no thinking, calls in one line.
    let (res, out, hint) = run_words(&mut env, &["logs", &path, "--talk"]);
    res.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "--talk:\n{out}");
    assert!(lines[0].starts_with("#1 12:00:00 prompt deploy it"), "{out}");
    assert!(lines[1].starts_with("#2 12:00:02 tool   Bash: cargo test → ok (3 lines)"), "{out}");
    assert!(!out.contains("plan the deploy"), "--talk leaves thinking out:\n{out}");
    assert!(hint.contains("talk events 1-4 of 4"), "hint: {hint}");

    // --full with --talk prints prompts whole, never tool output.
    let (_, out, _) = run_words(&mut env, &["logs", &path, "--talk", "--full"]);
    assert!(out.contains("then report"), "--talk --full prompt whole:\n{out}");
    assert!(!out.contains("test a ... ok"), "--talk --full prints no tool output:\n{out}");

    // Paging.
    let (_, out, hint) = run_words(&mut env, &["logs", &path, "--talk", "--start", "2", "--lines", "2"]);
    assert_eq!(out.lines().count(), 2, "{out}");
    assert!(out.starts_with("#2 "), "{out}");
    assert!(hint.contains("2-3 of 4; --start 4 reads on"), "hint: {hint}");
    let (_, out, hint) = run_words(&mut env, &["logs", &path, "--start", "9"]);
    assert!(out.is_empty(), "{out}");
    assert!(hint.contains("past the last"), "hint: {hint}");

    // --grep looks in tool output too.
    let (_, out, hint) = run_words(&mut env, &["logs", &path, "--grep", "DEPLOYED"]);
    assert_eq!(out.lines().count(), 2, "--grep:\n{out}");
    assert!(hint.contains("matching \"DEPLOYED\" 1-2 of 2"), "hint: {hint}");

    // --scrub redacts the secrets in place, in the prompt and the output,
    // and keeps every line.
    let (_, out, _) = run_words(&mut env, &["logs", &path, "--full", "--scrub"]);
    assert!(!out.contains("abc123") && !out.contains("zzzz9999yyyy"), "--scrub:\n{out}");
    assert!(out.contains("my api_key is [redacted: secret]"), "{out}");
    assert!(out.contains("Authorization: Bearer [redacted: token]"), "{out}");
    assert!(out.contains("then report") && out.contains("test b ... ok"), "--scrub keeps the rest:\n{out}");
    let (_, out, _) = run_words(&mut env, &["logs", &path, "--json", "--scrub"]);
    assert!(!out.contains("abc123") && !out.contains("zzzz9999yyyy"), "--json --scrub:\n{out}");
    assert!(out.contains(r#""input""#) && out.contains("make deploy"), "--json --scrub keeps tool input:\n{out}");

    // What doesn't go together.
    let (res, _, _) = run_words(&mut env, &["logs", &path, "--talk", "--json"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("don't go with --json"));
    let (res, _, _) = run_words(&mut env, &["logs", &path, "--terminal"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("isn't an agent here"));
    let (res, _, _) = run_words(&mut env, &["logs", "no/such/sess.jsonl"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("no transcript at"));
}

/// A peer reads only agents' logs through the bus's op: never a session or
/// a path, and never in parts.
#[test]
fn test_logs_parts_and_scrub_stay_local() {
    let mut env = setup_logs();
    let (res, _, _) = run_words(&mut env, &["logs", "worker@elsewhere", "--talk"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("not over the bus"));
    let (res, _, _) = run_words(&mut env, &["logs", "worker@elsewhere", "--scrub"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("reads this host's only"));
    // What a peer's request gets: a path or a session id names no agent.
    let dir = temp_dir("agent-logs-");
    let path = temp_path(&dir, "sess-p.jsonl");
    write_test_file(&path, PARTS_TRANSCRIPT);
    for agent in [path.as_str(), "sess-p", "claude:sess-p"] {
        let req = agent_bus::LogsRequest {
            agent: agent.to_string(),
            lines: 5,
            ..Default::default()
        };
        let res = serve_logs(&env.ctx, &agent_bus::Context::background(), req);
        assert!(res.is_err(), "a peer read {agent}: {:?}", res.map(|r| r.output));
    }
}

/// What review found: a path with '@' is a path; a short name says no agent
/// has it either; a window starting in a later session names it; a tool's
/// command and a session's title are scrubbed to one bounded line.
#[test]
fn test_logs_paths_names_windows_and_scrubbed_lines() {
    crate::require_termd!();
    let mut env = setup_logs();
    let dir = temp_dir("agent-logs-");
    let sub = dir.path().join("a@b");
    std::fs::create_dir_all(&sub).unwrap();
    let path = sub.join("sess-p.jsonl").to_string_lossy().into_owned();
    write_test_file(&path, PARTS_TRANSCRIPT);
    let (res, out, _) = run_words(&mut env, &["logs", &path, "--lines", "1"]);
    res.unwrap();
    assert!(out.contains("Deployed"), "a path with @:\n{out}");

    let other = temp_path(&dir, "sess-q.jsonl");
    write_test_file(&other, &PARTS_TRANSCRIPT.replace("sess-p", "sess-q"));

    // Two sessions of one agent; a page that starts in the second names it.
    hook_transcript(&env, &path);
    hook_transcript(&env, &other);
    let (_, out, _) = run_words(&mut env, &["logs", "worker", "--talk", "--start", "6", "--lines", "1"]);
    assert!(out.starts_with("--- session sess-q (claude)\n#6 "), "{out}");

    let mut e = Event {
        kind: KIND_TOOL.into(),
        tool: Some(agent_transcript::ToolEvent {
            name: "Bash".into(),
            tool_summary: ToolSummary {
                detail: "agent send helper \"line one\nline two with more\"".into(),
                ..Default::default()
            },
            ..Default::default()
        }),
        ..Default::default()
    };
    redact_event(&mut e);
    assert_eq!(
        e.tool.unwrap().tool_summary.detail,
        "agent send helper \"line one\nline two with more\"",
        "a command is kept whole"
    );
    assert_eq!(
        redact_line("curl -H 'Authorization: Bearer abcdef123456'"),
        "curl -H 'Authorization: Bearer [redacted: token]'"
    );
    assert_eq!(redact_line(&"x".repeat(500)).chars().count(), 500, "nothing cut");
}

/// A short name that no agent has, and that starts several sessions' ids,
/// says both.
#[test]
fn test_logs_short_name_says_no_agent_and_which_sessions() {
    let (mut env, home) = crate::cli::sessions::sessions_test::setup_sessions();
    let day = std::time::Duration::from_secs(3600);
    crate::cli::sessions::sessions_test::claude_transcript(&home, "abc-1", "/home/user/shop", "", day);
    crate::cli::sessions::sessions_test::claude_transcript(&home, "abc-2", "/home/user/shop", "", day);
    let (res, _, _) = run_words(&mut env, &["logs", "abc"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.starts_with("no agent named \"abc\" here"), "{err}");
    assert!(err.contains("starts the ids of 2 sessions"), "{err}");
    let (res, out, _) = run_words(&mut env, &["logs", "claude:abc-2", "--lines", "1"]);
    res.unwrap();
    assert!(out.contains("prompt hi"), "harness:id:\n{out}");
}

/// A capped read (--scrub --full, more than one event) cuts a long piece
/// and names the command that reads its event whole; that command cuts
/// nothing.
#[test]
fn test_logs_caps_long_pieces_with_a_pointer() {
    let mut env = setup_logs();
    let dir = temp_dir("agent-logs-");
    let path = temp_path(&dir, "sess-c.jsonl");
    let long = "y".repeat(5000);
    let user = r#"{"type":"user","sessionId":"sess-c","cwd":"/home/user/proj","timestamp":"2026-08-01T12:00:00Z","message":{"role":"user","content":"go"}}"#;
    let reply = format!(
        r#"{{"type":"assistant","timestamp":"2026-08-01T12:00:01Z","message":{{"role":"assistant","content":[{{"type":"text","text":"{long}"}}]}}}}"#
    );
    write_test_file(&path, &format!("{user}\n{reply}\n"));
    let (res, out, _) = run_words(&mut env, &["logs", &path, "--full", "--scrub"]);
    res.unwrap();
    let marker = format!("[… 1000 more characters cut; the whole event: agent logs {path} --start 2 --lines 1 --full]");
    assert!(out.contains(&marker), "{out}");
    let (_, out, _) = run_words(&mut env, &["logs", &path, "--start", "2", "--lines", "1", "--full", "--scrub"]);
    assert!(out.contains(&long) && !out.contains("more characters cut"), "one event whole:\n{}", &out[..200.min(out.len())]);
    let (_, out, _) = run_words(&mut env, &["logs", &path, "--full"]);
    assert!(out.contains(&long), "a person's --full isn't capped");
}

/// A picture can't be redacted: a reader for mining gets what it was, not
/// its data; the prompt's text is kept.
#[test]
fn test_redact_event_marks_images() {
    let mut e = Event {
        kind: KIND_PROMPT.into(),
        prompt: Some(PromptEvent {
            text: "see this".into(),
            images: vec![Image {
                media_type: "image/png".into(),
                data: "iVBORw0KGgo".into(),
            }],
            ..Default::default()
        }),
        ..Default::default()
    };
    redact_event(&mut e);
    let p = e.prompt.unwrap();
    assert_eq!(p.text, "see this");
    assert_eq!(p.images[0].data, "[redacted: image, image/png, 11 bytes of base64]");
}
