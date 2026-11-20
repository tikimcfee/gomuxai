use std::collections::HashMap;
use std::io::Write;

use agent_base::gostd::{filepath, strconv};

use crate::agy::{agy_args, agy_response, agy_spawn_calls};
use crate::{
    Agy, Claude, ErrUnparseable, Event, Kimi, OP_UPDATE, ORIGIN_PEER, Options, ParentSession,
    Parser, Reader, STATUS_PENDING, Session, TurnRef, claude_subagent_session,
    claude_subagent_turns, detect, file_options, harvest_claude, latest, read_file, resume_after,
    session_id_from_path, summarize_tool, task_notifications,
};

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .tempdir_in("/var/tmp")
        .expect("tempdir in /var/tmp")
}

pub(crate) fn summary(e: &Event) -> String {
    let turn = match e.turn {
        Some(ref t) => t.index.to_string(),
        None => "-".to_string(),
    };
    let head = format!("{turn} {} {}", e.op, e.kind);
    if let Some(ref p) = e.prompt {
        return format!("{head}({}) {}", p.origin, strconv::quote(&p.text));
    }
    if let Some(ref th) = e.think {
        if th.redacted {
            return format!("{head} redacted");
        }
        return format!("{head} {}", strconv::quote(&th.text));
    }
    if let Some(ref s) = e.say {
        if s.r#final {
            return format!("{head} final {}", strconv::quote(&s.text));
        }
        return format!("{head} {}", strconv::quote(&s.text));
    }
    if let Some(ref t) = e.tool {
        let about = if !t.target.is_empty() {
            &t.target
        } else {
            &t.detail
        };
        let res = t.result.as_ref().unwrap();
        return format!(
            "{head} {}({about}) {} {}",
            t.name,
            res.status,
            strconv::quote(&res.output)
        );
    }
    if let Some(ref sys) = e.system {
        return format!("{head} {} {}", sys.subtype, strconv::quote(&sys.text));
    }
    head
}

pub(crate) fn summaries(events: &[Event]) -> Vec<String> {
    events.iter().map(summary).collect()
}

fn read_fixture(name: &str) -> Vec<u8> {
    let path = filepath::join(&[env!("CARGO_MANIFEST_DIR"), "testdata", name]);
    std::fs::read(path).expect("read fixture")
}

fn corpus_path(rel: &str) -> String {
    filepath::join(&[env!("CARGO_MANIFEST_DIR"), "testdata", "corpus", rel])
}

fn parse(r: &dyn Reader, data: &[u8]) -> (Parser, Vec<Event>) {
    let mut p = r.new_parser(Options::default());
    let events = p.feed(data);
    (p, events)
}

#[test]
fn test_claude_parse() {
    let (p, events) = parse(&Claude, &read_fixture("claude.jsonl"));
    let want = [
        r#"1 add prompt(human) "run the tests""#,
        r#"1 add think "go test should do it""#,
        r#"1 add tool Bash(go test ./...) pending """#,
        r#"1 add tool Read(go.mod) pending """#,
        r#"1 update tool Bash(go test ./...) ok "ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s""#,
        r#"1 update tool Read(go.mod) ok "1\tmodule example.com/proj""#,
        r#"1 add say final "All tests pass.""#,
        r#"2 add prompt(command) "/review high""#,
        r#"2 add tool Bash(make lint) pending """#,
        r#"2 update tool Bash(make lint) error "Exit code 2\nlint: 1 issue""#,
        r#"2 add prompt(task-notification) "<task-notification>background task finished</task-notification>""#,
        r#"2 add say "One lint issue.\nSee above.""#,
        r#"2 add tool Grep(TODO) pending """#,
    ];
    assert_eq!(summaries(&events), want);
    for (i, e) in events.iter().enumerate() {
        assert_eq!(e.seq, i);
    }
    let e = &events[2];
    assert_eq!(e.id, "tu-bash");
    assert_eq!(e.ts, "2026-08-01T10:00:01.000Z");
    assert_eq!(e.line, 7);

    let s = p.session();
    assert_eq!(s.harness, "claude");
    assert_eq!(s.cwd, "/home/user/proj");
    assert_eq!(s.project, "proj");
    assert_eq!(s.session_id, "00000000-0000-0000-0000-00000000c1a0");
    assert_eq!(s.version, "2.1.400");
    assert_eq!(s.git_branch, "main");
    assert_eq!(s.title, "Run the tests");
    assert_eq!(s.model, "claude-test-1");
    assert_eq!(p.lines, 20);
    assert_eq!(p.skipped, 1);
}

#[test]
fn test_claude_parse_pairing() {
    let lines = [
        r#"{"type":"assistant","uuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"Edit","input":{"file_path":"/r/a.js"}}]}}"#,
        r#"{"type":"assistant","uuid":"u2","message":{"role":"assistant","content":[{"type":"tool_use","id":"b","name":"Bash","input":{"command":"ls"}}]}}"#,
        r#"{"type":"user","uuid":"u3","toolUseResult":{"stdout":"one\ntwo\n"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"b","content":[{"type":"text","text":"one"},{"type":"text","text":"two"}]}]}}"#,
        r#"{"type":"user","uuid":"u4","toolUseResult":{"structuredPatch":[]},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"a","content":"The file has been updated"}]}}"#,
        r#"{"type":"user","uuid":"u5","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"unseen","content":"its call came before"}]}}"#,
    ];
    let (_, events) = parse(&Claude, format!("{}\n", lines.join("\n")).as_bytes());
    let want = [
        r#"- add tool Edit(/r/a.js) pending """#,
        r#"- add tool Bash(ls) pending """#,
        r#"- update tool Bash(ls) ok "one\ntwo""#,
        r#"- update tool Edit(/r/a.js) ok "The file has been updated""#,
    ];
    assert_eq!(summaries(&events), want);
    assert_eq!(
        events[2]
            .tool
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .structured
            .as_ref()
            .unwrap()
            .as_str(),
        r#"{"stdout":"one\ntwo\n"}"#
    );
    assert_eq!(events[2].line, 2);
    assert_eq!(
        events[1]
            .tool
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .status,
        STATUS_PENDING
    );
}

#[test]
fn test_claude_user_records() {
    let cases = [
        (
            "",
            r#""  Fix the fog  ""#,
            r#"1 add prompt(human) "  Fix the fog  ""#,
        ),
        (
            "",
            r#"[{"type":"text","text":"pasted"}]"#,
            r#"1 add prompt(human) "pasted""#,
        ),
        (
            r#""promptSource":"sdk","#,
            r#""from the SDK""#,
            r#"1 add prompt(human) "from the SDK""#,
        ),
        (
            "",
            r#""<command-name>/model</command-name>\n<command-args></command-args>""#,
            r#"1 add prompt(command) "/model""#,
        ),
        (
            "",
            r#""<command-name>review</command-name>\n<command-args>high</command-args>""#,
            r#"1 add prompt(command) "/review high""#,
        ),
        (
            r#""origin":{"kind":"human"},"#,
            r#""<pasted_content id=\"a1\">log</pasted_content>""#,
            r#"1 add prompt(human) "<pasted_content id=\"a1\">log</pasted_content>""#,
        ),
        (
            "",
            r#""<bash-input>ls -la</bash-input>""#,
            r#"1 add prompt(command) "! ls -la""#,
        ),
        (
            "",
            r#""<bash-stdout>a.go</bash-stdout><bash-stderr></bash-stderr>""#,
            r#"- add system local_command "<bash-stdout>a.go</bash-stdout><bash-stderr></bash-stderr>""#,
        ),
        (
            r#""promptSource":"typed","origin":{"kind":"human"},"#,
            r#""[Request interrupted by user]""#,
            r#"1 add prompt(human) "[Request interrupted by user]""#,
        ),
        (
            r#""promptSource":"typed","origin":{"kind":"human"},"#,
            r#""<bash-input>ls</bash-input>""#,
            r#"1 add prompt(human) "<bash-input>ls</bash-input>""#,
        ),
        (
            r#""promptSource":"typed","origin":{"kind":"human"},"#,
            r#""<task-notification>quoted</task-notification>""#,
            r#"1 add prompt(human) "<task-notification>quoted</task-notification>""#,
        ),
        (
            r#""promptSource":"sdk","#,
            r#""/compact now""#,
            r#"1 add prompt(human) "/compact now""#,
        ),
        (
            "",
            r#""[Request interrupted by user] and more""#,
            r#"1 add prompt(human) "[Request interrupted by user] and more""#,
        ),
        (
            r#""isMeta":true,"origin":{"kind":"peer"},"#,
            r#""<agent-message from=\"x\">hi</agent-message>""#,
            r#"- add prompt(peer) "<agent-message from=\"x\">hi</agent-message>""#,
        ),
        (
            r#""isMeta":true,"origin":{"kind":"channel"},"#,
            r#""<channel source=\"chat\">hi</channel>""#,
            r#"- add prompt(peer) "<channel source=\"chat\">hi</channel>""#,
        ),
        (
            r#""isMeta":true,"origin":{"kind":"coordinator"},"#,
            r#""next step""#,
            r#"- add prompt(peer) "next step""#,
        ),
        (
            r#""isMeta":true,"origin":{"kind":"auto-continuation"},"#,
            r#""continue""#,
            r#"- add prompt(system) "continue""#,
        ),
        (
            r#""isMeta":true,"promptSource":"system","promptId":"w1","turnPosition":{"promptIndex":4},"#,
            r#""[Cross-session idle notice] nothing new""#,
            r#"4 add prompt(system) "[Cross-session idle notice] nothing new""#,
        ),
        (
            r#""isMeta":true,"promptSource":"system","#,
            r#""Stop hook feedback:\nrun the linter""#,
            r#"- add prompt(system) "Stop hook feedback:\nrun the linter""#,
        ),
        (
            r#""isMeta":true,"origin":{"kind":"task-notification"},"#,
            r#""<task-notification>done</task-notification>""#,
            r#"1 add prompt(task-notification) "<task-notification>done</task-notification>""#,
        ),
        (
            "",
            r#""<local-command-stdout>Set model</local-command-stdout>""#,
            r#"- add system local_command "<local-command-stdout>Set model</local-command-stdout>""#,
        ),
        (
            "",
            r#"[{"type":"text","text":"[Request interrupted by user]"}]"#,
            r#"- add system interrupt "[Request interrupted by user]""#,
        ),
        (
            r#""isMeta":true,"#,
            r#""<local-command-caveat>Caveat</local-command-caveat>""#,
            "",
        ),
        (r#""isMeta":true,"#, r#""[Image: original 100x100]""#, ""),
        (
            "",
            r#"[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]"#,
            "",
        ),
        ("", r#""   ""#, ""),
    ];
    for (fields, content, want) in cases {
        let line = format!(
            r#"{{"type":"user","uuid":"u",{fields}"message":{{"role":"user","content":{content}}}}}"#
        ) + "\n";
        let (_, events) = parse(&Claude, line.as_bytes());
        let got = summaries(&events).join("\n");
        assert_eq!(got, want, "{line}");
    }
}

#[test]
fn test_claude_mid_turn() {
    let lines = [
        r#"{"type":"user","uuid":"u1","promptId":"p1","turnPosition":{"promptIndex":3},"message":{"role":"user","content":"start it and lint"}}"#,
        r#"{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"tool_use","id":"bg","name":"Bash","input":{"command":"make serve","run_in_background":true}}],"stop_reason":"tool_use"}}"#,
        r#"{"type":"user","uuid":"u2","promptId":"p1","toolUseResult":{"stdout":"","backgroundTaskId":"t9"},"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"bg","content":"Command running in background with ID: t9."}]}}"#,
        r#"{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"tool_use","id":"lint","name":"Bash","input":{"command":"make lint"}}],"stop_reason":"tool_use"}}"#,
        r#"{"type":"attachment","uuid":"at1","attachment":{"type":"queued_command","prompt":"<task-notification>\n<tool-use-id>bg</tool-use-id>\n<status>failed</status>\n<summary>exit 2</summary>\n</task-notification>","commandMode":"task-notification"}}"#,
        r#"{"type":"attachment","uuid":"at2","attachment":{"type":"hook_additional_context","content":["lint is slow today"],"hookEvent":"PostToolUse"}}"#,
        r#"{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":[{"type":"tool_use","id":"vet","name":"Bash","input":{"command":"go vet"}}],"stop_reason":"tool_use"}}"#,
        r#"{"type":"user","uuid":"u3","promptId":"p1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"lint","content":"ok"},{"type":"text","text":"[Request interrupted by user for tool use]"}]}}"#,
    ];
    let (_, events) = parse(&Claude, format!("{}\n", lines.join("\n")).as_bytes());
    let want = [
        r#"3 add prompt(human) "start it and lint""#,
        r#"3 add tool Bash(make serve) pending """#,
        r#"3 update tool Bash(make serve) background "Command running in background with ID: t9.""#,
        r#"3 add tool Bash(make lint) pending """#,
        r#"3 add prompt(task-notification) "<task-notification>\n<tool-use-id>bg</tool-use-id>\n<status>failed</status>\n<summary>exit 2</summary>\n</task-notification>""#,
        r#"3 update tool Bash(make serve) error "exit 2""#,
        r#"3 add prompt(system) "lint is slow today""#,
        r#"3 add tool Bash(go vet) pending """#,
        r#"3 add system interrupt "[Request interrupted by user for tool use]""#,
        r#"3 update tool Bash(make lint) ok "ok""#,
        r#"3 update tool Bash(go vet) interrupted """#,
    ];
    assert_eq!(summaries(&events), want);
    assert_eq!(events[8].id, "u3#1");
}

#[test]
fn test_claude_task_notifications() {
    let bg = |n: usize| -> String {
        format!(
            r#"{{"type":"assistant","uuid":"a{n}","message":{{"role":"assistant","content":[{{"type":"tool_use","id":"bg{n}","name":"Bash","input":{{"command":"job {n}"}}}}],"stop_reason":"tool_use"}}}}
{{"type":"user","uuid":"r{n}","promptId":"p1","toolUseResult":{{"backgroundTaskId":"t{n}"}},"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"bg{n}","content":"started"}}]}}}}"#
        )
    };
    let notes = concat!(
        r#"[SYSTEM NOTIFICATION - two tasks finished]\n"#,
        r#"<task-notification>\n<tool-use-id>bg1</tool-use-id>\n<status>completed</status>\n<summary>job 1 done</summary>\n</task-notification>\n"#,
        r#"<task-notification>\n<tool-use-id>bg2</tool-use-id>\n<status>killed</status>\n<summary>job 2 stopped</summary>\n</task-notification>"#
    );
    let lines = [
        r#"{"type":"user","uuid":"u1","promptId":"p1","turnPosition":{"promptIndex":1},"message":{"role":"user","content":"run both"}}"#.to_string(),
        bg(1),
        bg(2),
        format!(
            r#"{{"type":"user","uuid":"u2","promptId":"p2","origin":{{"kind":"task-notification"}},"message":{{"role":"user","content":"{notes}"}}}}"#
        ),
    ];
    let (_, events) = parse(&Claude, format!("{}\n", lines.join("\n")).as_bytes());
    let got = summaries(&events);
    let want = [
        r#"1 update tool Bash(job 1) ok "job 1 done""#,
        r#"1 update tool Bash(job 2) interrupted "job 2 stopped""#,
    ];
    assert_eq!(&got[got.len() - 2..], &want);
    assert!(got[got.len() - 3].starts_with("1 add prompt(task-notification)"));
    let parsed = task_notifications(&notes.replace(r"\n", "\n"));
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[1].tool_use_id, "bg2");
    assert_eq!(parsed[1].status, "killed");
}

#[test]
fn test_claude_closing_replies() {
    let say = |uuid: &str, agent: &str, text: &str, stop: &str| -> String {
        format!(
            r#"{{"type":"assistant","uuid":"{uuid}","agentId":"{agent}","message":{{"role":"assistant","content":[{{"type":"text","text":"{text}"}}],"stop_reason":{stop}}}}}"#
        )
    };
    let prompt = |uuid: &str, agent: &str, turn: &str, text: &str| -> String {
        format!(
            r#"{{"type":"user","uuid":"{uuid}","agentId":"{agent}","promptId":"{turn}","message":{{"role":"user","content":"{text}"}}}}"#
        )
    };
    let sub = [
        prompt("u1", "s1", "p1", "one"),
        say("a1", "s1", "thinking aloud", "null"),
        say("a2", "s1", "done with one", "null"),
        r#"{"type":"system","subtype":"turn_duration","uuid":"t1","agentId":"s1","durationMs":5}"#
            .to_string(),
        prompt("u2", "s1", "p2", "two"),
        say("a3", "s1", "done with two", "null"),
        prompt("u3", "s1", "p3", "three"),
        say("a4", "s1", "partial", "null"),
        prompt("u4", "s1", "p3", "[Request interrupted by user]"),
        r#"{"type":"assistant","uuid":"a5","agentId":"s1","message":{"role":"assistant","content":[{"type":"fallback","from":"model-a","to":"model-b"},{"type":"text","text":"checked"}],"stop_reason":"stop_sequence"}}"#.to_string(),
    ];
    let (_, events) = parse(&Claude, format!("{}\n", sub.join("\n")).as_bytes());
    let want = [
        r#"0 add prompt(peer) "one""#,
        r#"0 add say "thinking aloud""#,
        r#"0 add say "done with one""#,
        r#"0 add system turn_duration """#,
        r#"0 update say final "done with one""#,
        r#"0 add prompt(peer) "two""#,
        r#"0 add say "done with two""#,
        r#"0 add prompt(peer) "three""#,
        r#"0 update say final "done with two""#,
        r#"0 add say "partial""#,
        r#"0 add system interrupt "[Request interrupted by user]""#,
        r#"0 add system model_fallback """#,
        r#"0 add say final "checked""#,
    ];
    assert_eq!(summaries(&events), want);
    assert_eq!(events[8].line, 6);
    assert_eq!(events[8].id, "a3#0");
    assert_eq!(events[8].turn.as_ref().unwrap().id, "p2");
    assert_eq!(
        events[11].system.as_ref().unwrap().data.as_ref().unwrap(),
        r#"{"from":"model-a","to":"model-b"}"#
    );

    let main = [
        prompt("u1", "", "p1", "one"),
        say("a1", "", "cut short", "null"),
        r#"{"type":"system","subtype":"turn_duration","uuid":"t1","durationMs":5}"#.to_string(),
        prompt("u2", "", "p2", "two"),
    ];
    let (_, events) = parse(&Claude, format!("{}\n", main.join("\n")).as_bytes());
    let want_main = [
        r#"1 add prompt(human) "one""#,
        r#"1 add say "cut short""#,
        r#"1 add system turn_duration """#,
        r#"2 add prompt(human) "two""#,
    ];
    assert_eq!(summaries(&events), want_main);
}

#[test]
fn test_claude_request_blobs() {
    let blob = r#"{"role":"assistant","content":[{"type":"text","text":"hi"}],"output_config":{"effort":"high"}}"#;
    let lines = [
        format!(r#"{{"type":"api-request-blob","hash":"h1","message":{blob}}}"#),
        r#"{"type":"api-request","id":"q1","params":{"model":"m"},"tail":["h1"]}"#.to_string(),
    ];
    let events = Claude
        .new_parser(Options {
            requests: true,
            ..Options::default()
        })
        .feed(format!("{}\n", lines.join("\n")).as_bytes());
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0]
            .request
            .as_ref()
            .unwrap()
            .blobs
            .get("h1")
            .unwrap()
            .as_str(),
        blob
    );
}

#[test]
fn test_harvest_claude() {
    let data = read_fixture("claude.jsonl");
    let mut lines: Vec<(&[u8], i64)> = Vec::new();
    let mut at = 0usize;
    while at < data.len() {
        let n = data[at..]
            .iter()
            .position(|&b| b == b'\n')
            .unwrap_or(data.len() - at);
        lines.push((&data[at..at + n], at as i64));
        at += n + 1;
    }
    let (p, _) = parse(&Claude, &data);
    let want = p.session();
    let mut forward = Session::default();
    let mut backward = Session::default();
    for i in 0..lines.len() {
        harvest_claude(&mut forward, lines[i].0, lines[i].1);
        let j = lines.len() - 1 - i;
        harvest_claude(&mut backward, lines[j].0, lines[j].1);
    }
    forward.harness = want.harness.clone();
    backward.harness = want.harness.clone();
    assert_eq!(forward, want);
    assert_eq!(backward, want);

    let mut s = want.clone();
    let end = data.len() as i64;
    for (i, line) in [
        r#"{"type":"custom-title","customTitle":"mine"}"#,
        r#"{"type":"ai-title","aiTitle":"later AI title"}"#,
        r#"{"type":"agent-name","agentName":"fixer"}"#,
        r#"{"type":"continued-in","continuedInSessionId":"next"}"#,
        r#"{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":[]}}"#,
        r#"{"type":"user","sessionId":"s","cwd":"/elsewhere","version":"9.9.9","gitBranch":""}"#,
        r#"{"type":"assistant","isSidechain":true,"version":"9.9.10","message":{"model":"another-session's"}}"#,
        r#"{"type":"api-request","sessionId":"s","version":"9.9.11","id":"q1","params":{"model":"m"},"tail":[]}"#,
        r#"{"type":"api-request-shape","sessionId":"s","version":"9.9.12","shapeHash":"h"}"#,
    ]
    .iter()
    .enumerate()
    {
        harvest_claude(&mut s, line.as_bytes(), end + i as i64);
    }
    assert_eq!(s.title, "mine");
    assert!(s.title_is_custom());
    assert_eq!(s.agent_name, "fixer");
    assert_eq!(s.continued_in, "next");
    assert_eq!(s.model, want.model);
    assert_eq!(s.version, "9.9.9");
    assert_eq!(s.git_branch, "");
    assert_eq!(s.cwd, want.cwd);
}

#[test]
fn test_agy_parse() {
    let (p, events) = parse(&Agy, &read_fixture("agy.jsonl"));
    let want = [
        r#"1 add prompt(human) "list the files""#,
        r#"1 add think "I should list them and read the readme.""#,
        r#"1 add tool run_command(ls) pending """#,
        r#"1 add tool view_file(README) pending """#,
        r#"1 update tool run_command(ls) ok "The command exited with code 0.\nOutput:\nREADME\nmain.go""#,
        r#"1 update tool view_file(README) ok "File Path: `file:///home/user/proj/README`\nTotal Lines: 2\n1: hello\n2: world""#,
        r#"1 add say final "Two files: README and main.go.""#,
        r#"1 add prompt(system) "The following is a <SYSTEM_MESSAGE> not actually sent by the user.""#,
        r#"2 add prompt(human) "build it""#,
        r#"2 add tool run_command(go build ./...) pending """#,
        r#"2 update tool run_command(go build ./...) error "The command exited with code 1.\nOutput:\nmain.go:3: undefined: x""#,
        r#"2 add tool view_file(/etc/secret) pending """#,
        r#"2 add tool run_command(sleep 100) pending """#,
        r#"2 update tool view_file(/etc/secret) error "Encountered error in step execution: permission denied""#,
        r#"2 update tool run_command(sleep 100) background "Tool is running as a background task with task id: c-1/task-3""#,
        r#"2 add tool run_command(go vet ./...) pending """#,
        r#"2 add prompt(delivered) "[from lead@hub] also run the linter""#,
        r#"2 add system api_error "Error: response cut off""#,
        r#"3 add prompt(human) "stop, summarize""#,
        r#"2 update tool run_command(go vet ./...) interrupted """#,
        r##"3 add system compact_summary "# Summary\nThe user asked to build and vet.""##,
        r#"3 add prompt(task-notification) "Task id \"c-1/task-3\" finished with result:\nThe command exited with code 2.\nOutput:\nfail""#,
        r#"3 add prompt(peer) "hello from c-2""#,
        r#"2 update tool run_command(sleep 100) error "The command exited with code 2.\nOutput:\nfail""#,
    ];
    assert_eq!(summaries(&events), want);
    assert_eq!(p.lines, 19);
    assert_eq!(p.skipped, 1);
    let s = p.session();
    assert_eq!(s.harness, "agy");
    assert_eq!(s.cwd, "/home/user/proj");
    assert_eq!(s.model, "Gemini 3.8 Pro");
    for e in &events {
        if e.id == "step_14#0" {
            assert_eq!(
                e.system.as_ref().unwrap().data.as_ref().unwrap(),
                r#"{"error":"response cut off"}"#
            );
        }
        if e.id == "step_1_tool_1" && e.op == OP_UPDATE {
            assert_eq!(
                e.tool.as_ref().unwrap().meta.as_ref().unwrap(),
                r#"{"lines":2}"#
            );
        }
    }
}

#[test]
fn test_agy_task_canceled() {
    let input = [
        r#"{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nserve\n</USER_REQUEST>"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"make serve"}}]}"#,
        r#"{"step_index":2,"type":"GENERIC","status":"RUNNING","content":"Tool is running as a background task with task id: c-1/task-2"}"#,
        r#"{"step_index":3,"type":"SYSTEM_MESSAGE","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-04T12:00:00Z sender=c-1/task-2 priority=MESSAGE_PRIORITY_HIGH content=Task id \"c-1/task-2\" was canceled with result:\nserving on :8080\n</SYSTEM_MESSAGE>"}"#,
    ];
    let (_, events) = parse(&Agy, format!("{}\n", input.join("\n")).as_bytes());
    assert_eq!(
        summary(events.last().unwrap()),
        r#"1 update tool run_command(make serve) interrupted "serving on :8080""#
    );
}

#[test]
fn test_agy_missing_result() {
    let input = [
        r#"{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\ngo\n</USER_REQUEST>"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"pkill srv"}},{"name":"run_command","args":{"CommandLine":"ps"}}]}"#,
        r#"{"step_index":3,"type":"GENERIC","status":"DONE","content":"srv not running"}"#,
        r#"{"step_index":4,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"make"}}]}"#,
        r#"{"step_index":5,"type":"GENERIC","status":"DONE","content":"built"}"#,
    ];
    let (_, events) = parse(&Agy, format!("{}\n", input.join("\n")).as_bytes());
    let want = [
        r#"1 add prompt(human) "go""#,
        r#"1 add tool run_command(pkill srv) pending """#,
        r#"1 add tool run_command(ps) pending """#,
        r#"1 update tool run_command(ps) ok "srv not running""#,
        r#"1 add tool run_command(make) pending """#,
        r#"1 update tool run_command(pkill srv) interrupted """#,
        r#"1 update tool run_command(make) ok "built""#,
    ];
    assert_eq!(summaries(&events), want);
}

#[test]
fn test_agy_rewind() {
    let input = [
        r#"{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nfirst\n</USER_REQUEST>"}"#,
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls"}}]}"#,
        r#"{"step_index":2,"type":"GENERIC","status":"DONE","content":"a"}"#,
        r#"{"step_index":1,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\nagain\n</USER_REQUEST>"}"#,
        r#"{"step_index":2,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command","args":{"CommandLine":"ls -a"}}]}"#,
        r#"{"step_index":3,"type":"GENERIC","status":"DONE","content":"b"}"#,
    ];
    let (_, events) = parse(&Agy, format!("{}\n", input.join("\n")).as_bytes());
    let got: Vec<String> = events
        .iter()
        .map(|e| format!("{} {} turn {}", e.op, e.id, e.turn.as_ref().unwrap().id))
        .collect();
    let want = [
        "add step_0#0 turn step_0#0",
        "add step_1_tool_0 turn step_0#0",
        "update step_1_tool_0 turn step_0#0",
        "add step_1.2#0 turn step_1.2#0",
        "add step_2.2_tool_0 turn step_1.2#0",
        "update step_2.2_tool_0 turn step_1.2#0",
    ];
    assert_eq!(got, want);
    assert_eq!(latest(&events).len(), 4);
}

#[test]
fn test_agy_background_ends() {
    let msg = |step: usize, sender: &str, content: &str| -> String {
        let body = serde_json::to_string(&format!(
            "<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-04T12:00:00Z sender={sender} priority=MESSAGE_PRIORITY_HIGH content={content}\n</SYSTEM_MESSAGE>"
        ))
        .unwrap();
        format!(
            r#"{{"step_index":{step},"type":"SYSTEM_MESSAGE","source":"SYSTEM","content":{body}}}"#
        )
    };
    let running = |step: usize, task: &str| -> String {
        format!(
            r#"{{"step_index":{step},"type":"GENERIC","status":"RUNNING","content":"Tool is running as a background task with task id: {task}"}}"#
        )
    };
    let lines = [
        r#"{"step_index":0,"type":"USER_INPUT","source":"USER_EXPLICIT","content":"<USER_REQUEST>\ngo\n</USER_REQUEST>"}"#.to_string(),
        r#"{"step_index":1,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"schedule","args":{"DurationSeconds":60,"Prompt":"Check the build"}},{"name":"run_command","args":{"CommandLine":"make serve"}},{"name":"run_command","args":{"CommandLine":"make watch"}},{"name":"run_command","args":{"CommandLine":"make docs"}}]}"#.to_string(),
        running(2, "c/task-2"),
        running(3, "c/task-3"),
        running(4, "c/task-4"),
        running(5, "c/task-5"),
        msg(6, "c/task-2", "Check the build"),
        r#"{"step_index":7,"type":"PLANNER_RESPONSE","tool_calls":[{"name":"manage_task","args":{"Action":"kill","TaskId":"c/task-3"}}]}"#.to_string(),
        r#"{"step_index":8,"type":"GENERIC","status":"DONE","content":"Task \"c/task-3\" cancelled."}"#.to_string(),
        msg(9, "system", "Subagent c-2 has gone idle."),
        msg(
            10,
            "system",
            "[Notice] All your subagents and background tasks have been stopped due to server restart.",
        ),
        r#"{"step_index":11,"type":"SYSTEM_MESSAGE","source":"SYSTEM_SDK","content":"The following is a <SYSTEM_MESSAGE> not actually sent by the user.\n\n<SYSTEM_MESSAGE>\nThe following system message was injected by a PreInvocation hook:\n[from lead@hub] also lint\n</SYSTEM_MESSAGE>"}"#.to_string(),
    ];
    let (_, events) = parse(&Agy, format!("{}\n", lines.join("\n")).as_bytes());
    let want = [
        r#"1 add prompt(task-notification) "Check the build""#,
        r#"1 update tool schedule(timer: Check the build) ok "Check the build""#,
        r#"1 add tool manage_task(kill c/task-3) pending """#,
        r#"1 update tool manage_task(kill c/task-3) ok "Task \"c/task-3\" cancelled.""#,
        r#"1 update tool run_command(make serve) interrupted """#,
        r#"1 add prompt(system) "Subagent c-2 has gone idle.""#,
        r#"1 add prompt(system) "[Notice] All your subagents and background tasks have been stopped due to server restart.""#,
        r#"1 update tool run_command(make watch) interrupted """#,
        r#"1 update tool run_command(make docs) interrupted """#,
        r#"1 add prompt(delivered) "[from lead@hub] also lint""#,
    ];
    assert_eq!(summaries(&events[9..]), want);
}

#[test]
fn test_agy_response() {
    let tests = [
        (
            "a partial read: its Showing header",
            "view_file",
            r#"{"AbsolutePath":"/p/a.go"}"#,
            "File Path: `file:///p/a.go`\nTotal Lines: 40\nShowing lines 11 to 13\n11: a\n12: b\n13: c",
            r#"{"lines":3,"range":[11,13]}"#,
        ),
        (
            "a replacement: its diff block",
            "replace_file_content",
            r#"{"TargetFile":"/p/a.go"}"#,
            "The following changes were made:\n[diff_block_start]\n@@ -3,2 +3,3 @@\n a\n-b\n+c\n+d\n\n[diff_block_end]",
            r#"{"added":2,"removed":1,"ranges":[[4,5]],"patch":[{"lines":[" a","-b","+c","+d"," "],"newLines":3,"newStart":3,"oldLines":2,"oldStart":3}]}"#,
        ),
        (
            "edit_file: its replacement count",
            "edit_file",
            r#"{"Path":"/p/a.go"}"#,
            "Made 1 replacement in file:///p/a.go starting at line 7 (+2/-1 lines)",
            r#"{"added":2,"removed":1,"ranges":[[7,8]]}"#,
        ),
        (
            "write_to_file: its content",
            "write_to_file",
            r#"{"TargetFile":"/p/n.go","CodeContent":"a\nb\n","Overwrite":true}"#,
            "Created file",
            r#"{"kind":"update","lines":2}"#,
        ),
    ];
    for (name, tool, input, output, meta) in tests {
        let resp = agy_response(tool, input.as_bytes(), output);
        let s = summarize_tool(tool, input.as_bytes(), Some(&resp), "");
        assert_eq!(s.meta.as_ref().unwrap().as_str(), meta, "{name}");
    }
}

#[test]
fn test_agy_subagent() {
    let dir = temp_dir();
    let brain = dir.path().to_str().unwrap();
    let write = |rel: &str, content: &str| {
        let p = filepath::join(&[brain, rel]);
        std::fs::create_dir_all(filepath::dir(&p)).unwrap();
        std::fs::write(&p, content).unwrap();
    };
    let logs = ".system_generated/logs/transcript_full.jsonl";
    write(
        &filepath::join(&["parent", logs]),
        &([
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-10-04T12:00:00Z","content":"<USER_REQUEST>\nfirst\n</USER_REQUEST>"}"#,
            r#"{"step_index":1,"source":"USER_EXPLICIT","type":"USER_INPUT","created_at":"2026-10-04T12:01:00Z","content":"<USER_REQUEST>\nsplit the review\n</USER_REQUEST>"}"#,
            r#"{"step_index":2,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-10-04T12:01:01Z","tool_calls":[{"name":"view_file","args":{"AbsolutePath":"/p/a"}},{"name":"invoke_subagent","args":{"Subagents":[{"Role":"api"},{"Role":"db"}]}}]}"#,
            r#"{"step_index":3,"source":"MODEL","type":"GENERIC","created_at":"2026-10-04T12:01:02Z","content":"1: a"}"#,
            r#"{"step_index":4,"source":"MODEL","type":"GENERIC","created_at":"2026-10-04T12:01:03Z","content":"Started the subagents:\n{\n  \"conversationId\":  \"child-api\"\n}\n{\n  \"conversationId\":  \"child-db\"\n}"}"#,
        ]
        .join("\n")
            + "\n"),
    );
    write(
        "parent/.system_generated/subagents/child-db.json",
        r#"{"conversationId":"child-db","spawnStepIndex":4,"state":"SUBAGENT_STATE_ALIVE"}"#,
    );
    let child = filepath::join(&[brain, "child-db", logs]);
    write(
        &filepath::join(&["child-db", logs]),
        &([
            r#"{"step_index":0,"source":"SYSTEM","type":"SYSTEM_MESSAGE","created_at":"2026-10-04T12:01:03Z","content":"<SYSTEM_MESSAGE>\n[Message] timestamp=2026-10-04T12:01:03Z sender=parent priority=MESSAGE_PRIORITY_HIGH content=Review the schema.\n</SYSTEM_MESSAGE>"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","created_at":"2026-10-04T12:01:05Z","content":"Schema looks fine."}"#,
        ]
        .join("\n")
            + "\n"),
    );

    let parent_file = read_file(&filepath::join(&[brain, "parent", logs]), Some(&Agy), 50).unwrap();
    let last = parent_file.events.last().unwrap();
    assert_eq!(last.id, "step_2_tool_1");
    assert_eq!(
        last.tool.as_ref().unwrap().sub_sessions,
        vec!["child-api", "child-db"]
    );

    let f = read_file(&child, Some(&Agy), 50).unwrap();
    assert_eq!(f.session.session_id, "child-db");
    assert_eq!(
        f.session.parent_session,
        Some(ParentSession {
            session_id: "parent".to_string(),
            tool_id: "step_2_tool_1".to_string(),
        })
    );
    let want_turn = TurnRef {
        id: "step_1#0".to_string(),
        index: 2,
        derived: true,
    };
    assert_eq!(
        summaries(&f.events),
        vec![
            r#"2 add prompt(peer) "Review the schema.""#,
            r#"2 add say final "Schema looks fine.""#,
        ]
    );
    for e in &f.events {
        assert_eq!(e.agent, "child-db");
        assert_eq!(e.turn.as_ref(), Some(&want_turn));
    }

    assert_eq!(
        agy_spawn_calls(&filepath::join(&[brain, "parent"]))
            .unwrap()
            .len(),
        2
    );
    let mut f2 = std::fs::OpenOptions::new()
        .append(true)
        .open(filepath::join(&[brain, "parent", logs]))
        .unwrap();
    f2.write_all(
        concat!(
            r#"{"step_index":5,"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"invoke_subagent","args":{"Subagents":[{"Role":"ui"}]}}]}"#,
            "\n",
            r#"{"step_index":6,"source":"MODEL","type":"GENERIC","content":"{\"conversationId\": \"child-ui\"}"}"#,
            "\n"
        )
        .as_bytes(),
    )
    .unwrap();
    drop(f2);
    let calls = agy_spawn_calls(&filepath::join(&[brain, "parent"])).unwrap();
    assert_eq!(calls.get("child-ui").unwrap().id, "step_5_tool_0");

    let opts = file_options("agy", &filepath::join(&[brain, "parent", logs]));
    assert!(opts.session.parent_session.is_none() && opts.turn.is_none());
}

#[test]
fn test_kimi_parse() {
    let wire = [
        r#"{"type":"metadata","protocol_version":"1.4","created_at":1792490400000}"#,
        r#"{"type":"turn.prompt","time":1792490400000,"prompt":"older prompt shape"}"#,
        r#"{"type":"context.append_loop_event","time":1792490401000,"event":{"type":"tool.result","toolCallId":"tc-1","result":{"output":"early\n"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490402000,"event":{"type":"tool.call","toolCallId":"tc-1","name":"Bash","args":{"command":"echo early"},"display":{"kind":"command","cwd":"/home/user/w"}}}"#,
        r#"{"type":"turn.steer","time":1792490403000,"input":[{"type":"text","text":"also check b"}]}"#,
        r#"{"type":"context.append_loop_event","time":1792490404000,"event":{"type":"tool.call","toolCallId":"tc-2","name":"Read","args":{"path":"/home/user/w/b.go","line_offset":10}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490405000,"event":{"type":"tool.result","toolCallId":"tc-2","result":{"output":"10\tx\n11\ty\n"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490406000,"event":{"type":"tool.call","toolCallId":"tc-3","name":"Bash","args":{"command":"false"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490407000,"event":{"type":"tool.result","toolCallId":"tc-3","result":{"output":[{"type":"text","text":"exit 1"}],"isError":true}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490408000,"event":{"type":"content.part","part":{"type":"think","think":"   "}}}"#,
        r#"{"type":"turn.prompt","time":1792490409000,"input":[{"type":"text","text":"next"},{"type":"text","text":"line two"}]}"#,
    ]
    .join("\n")
        + "\n";
    let (p, events) = parse(&Kimi, wire.as_bytes());
    let want = [
        r#"1 add prompt(human) "older prompt shape""#,
        r#"1 add tool Bash(echo early) pending """#,
        r#"1 update tool Bash(echo early) ok "early\n""#,
        r#"1 add prompt(queued) "also check b""#,
        r#"1 add tool Read(b.go) pending """#,
        r#"1 update tool Read(b.go) ok "10\tx\n11\ty\n""#,
        r#"1 add tool Bash(false) pending """#,
        r#"1 update tool Bash(false) error "exit 1""#,
        r#"2 add prompt(human) "next\nline two""#,
    ];
    assert_eq!(summaries(&events), want);
    assert_eq!(events[5].ts, "2026-10-20T10:00:05.000Z");
    assert_eq!(
        events[5].tool.as_ref().unwrap().meta.as_ref().unwrap(),
        r#"{"lines":2,"range":[10,11]}"#
    );
    let s = p.session();
    assert_eq!(s.harness, "kimi");
    assert_eq!(s.cwd, "/home/user/w");
}

#[test]
fn test_kimi_lenient() {
    let wire = [
        r#"{"type":"turn.prompt","time":1792490400000.5,"input":"  as a string  "}"#,
        r#"{"type":"llm.request","time":1792490401000,"model":{"name":"k3"}}"#,
        r#"{"type":"context.append_loop_event","time":"soon","event":{"type":"tool.call","toolCallId":"functions.Bash:0","name":"Bash","args":{"command":"ls"},"display":"inline"}}"#,
        r#"{"type":"context.append_loop_event","time":1792490403000,"event":{"type":"tool.result","toolCallId":"functions.Bash:0","result":{"output":"a\n","isError":"no"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490404000,"event":{"type":"tool.call","toolCallId":"functions.Bash:0","name":"Bash","args":{"command":"pwd"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490405000,"event":{"type":"tool.result","toolCallId":"functions.Bash:0","result":{"output":"/w\n"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490406000,"event":{"type":"tool.call","uuid":"u-7","name":"Glob","args":{"pattern":"*.go"}}}"#,
        r#"{"type":"context.append_loop_event","time":1792490407000,"event":{"type":"tool.result","uuid":"r-1","toolCallId":"u-7","result":{"output":"a.go\n"}}}"#,
    ]
    .join("\n")
        + "\n";
    let (p, events) = parse(&Kimi, wire.as_bytes());
    let got: Vec<String> = events
        .iter()
        .map(|e| format!("{} {} {}", e.id, e.ts, summary(e)))
        .collect();
    let want = [
        r#"L0#0 2026-10-20T10:00:00.000Z 1 add prompt(human) "as a string""#,
        r#"functions.Bash:0  1 add tool Bash(ls) pending """#,
        r#"functions.Bash:0 2026-10-20T10:00:03.000Z 1 update tool Bash(ls) ok "a\n""#,
        r#"functions.Bash:0@L4 2026-10-20T10:00:04.000Z 1 add tool Bash(pwd) pending """#,
        r#"functions.Bash:0@L4 2026-10-20T10:00:05.000Z 1 update tool Bash(pwd) ok "/w\n""#,
        r#"u-7 2026-10-20T10:00:06.000Z 1 add tool Glob(*.go) pending """#,
        r#"u-7 2026-10-20T10:00:07.000Z 1 update tool Glob(*.go) ok "a.go\n""#,
    ];
    assert_eq!(got, want);
    assert_eq!(p.skipped, 0);
    assert_eq!(p.session().model, "");
}

#[test]
fn test_kimi_subagent() {
    let dir = temp_dir();
    let session = filepath::join(&[dir.path().to_str().unwrap(), "session_f0aa"]);
    let wire = filepath::join(&[&session, "agents", "agent-1", "wire.jsonl"]);
    std::fs::create_dir_all(filepath::dir(&wire)).unwrap();
    std::fs::write(
        filepath::join(&[&session, "state.json"]),
        r#"{"title":"The main conversation","workDir":"/home/user/w"}"#,
    )
    .unwrap();
    std::fs::write(
        &wire,
        "{\"type\":\"turn.prompt\",\"time\":1792490400000,\"input\":[{\"type\":\"text\",\"text\":\"review a.go\"}]}\n",
    )
    .unwrap();

    let f = read_file(&wire, None, 50).unwrap();
    let s = &f.session;
    assert_eq!(s.harness, "kimi");
    assert_eq!(s.session_id, "f0aa:agent-1");
    assert_eq!(s.parent_session.as_ref().unwrap().session_id, "f0aa");
    assert_eq!(s.cwd, "/home/user/w");
    assert_eq!(s.title, "");
    assert_eq!(f.events.len(), 1);
    assert_eq!(f.events[0].agent, "f0aa:agent-1");
    assert_eq!(f.events[0].prompt.as_ref().unwrap().origin, ORIGIN_PEER);
}

#[test]
fn test_agy_args() {
    let tests = [
        (
            "transcript.jsonl: every value is its JSON in a string",
            true,
            r#"{"CommandLine":"\"echo hi\\nls\"","WaitMsBeforeAsync":"500","AllowMultiple":"false","questions":"[{\"question\":\"Q?\"}]","toolSummary":"\"Run\""}"#,
            r#"{"AllowMultiple":false,"CommandLine":"echo hi\nls","WaitMsBeforeAsync":500,"questions":[{"question":"Q?"}],"toolSummary":"Run"}"#,
        ),
        (
            "transcript.jsonl: a value agy cut short stays as written",
            true,
            r#"{"CommandLine":"\"make bui","Cwd":"\"/p\"","toolSummary":"\"Build\""}"#,
            r#"{"CommandLine":"\"make bui","Cwd":"/p","toolSummary":"Build"}"#,
        ),
        (
            "transcript_full.jsonl: a quoted search keeps its quotes",
            false,
            r#"{"query":"\"exact phrase\""}"#,
            r#"{"query":"\"exact phrase\""}"#,
        ),
        (
            "transcript_full.jsonl: plain values stay",
            false,
            r#"{"CommandLine":"\"ls\"","WaitMsBeforeAsync":500}"#,
            r#"{"CommandLine":"\"ls\"","WaitMsBeforeAsync":500}"#,
        ),
        ("no args", true, "{}", "{}"),
        ("not an object", true, "null", "null"),
    ];
    for (name, encoded, args, want) in tests {
        let got = String::from_utf8(agy_args(args.as_bytes(), encoded)).unwrap();
        assert_eq!(got, want, "{name}");
    }

    let line = "{\"step_index\":0,\"type\":\"PLANNER_RESPONSE\",\"tool_calls\":[{\"name\":\"search_web\",\"args\":{\"query\":\"\\\"exact phrase\\\"\"}}]}\n";
    for (file, want) in [
        ("transcript.jsonl", "exact phrase"),
        ("transcript_full.jsonl", r#""exact phrase""#),
        ("", r#""exact phrase""#),
    ] {
        let events = Agy
            .new_parser(Options {
                session: Session {
                    path: filepath::join(&["/c", file]),
                    ..Session::default()
                },
                ..Options::default()
            })
            .feed(line.as_bytes());
        assert_eq!(events[0].tool.as_ref().unwrap().detail, want, "{file:?}");
    }

    let args = agy_args(
        br#"{"TargetFile":"\"/p/d.go\"","TargetContent":"\"a\\nb\"","ReplacementContent":"\"a\\nc\\nd\\ne\"","AllowMultiple":"false"}"#,
        true,
    );
    let s = summarize_tool("replace_file_content", &args, None, "");
    assert_eq!(s.target, "/p/d.go");
    assert_eq!(s.meta.as_ref().unwrap(), r#"{"added":3,"removed":1}"#);
}

#[test]
fn test_detect() {
    assert_eq!(detect(&read_fixture("agy.jsonl")).harness(), "agy");
    assert_eq!(detect(&read_fixture("claude.jsonl")).harness(), "claude");
    let kimi = std::fs::read(corpus_path(
        "kimi/interleaved/session_f0aa1fb8-41b1-5485-a887-ecd00a0bf053/agents/main/wire.jsonl",
    ))
    .unwrap();
    assert_eq!(detect(&kimi).harness(), "kimi");
}

#[test]
fn test_degenerate_inputs() {
    for r in [&Claude as &dyn Reader, &Agy, &Kimi] {
        let (p, events) = parse(r, &[]);
        assert!(events.is_empty() && p.lines == 0);
        let (p, events) = parse(r, b"null\n42\n\"str\"\n{invalid json\n[]\n");
        assert!(events.is_empty() && p.lines == 5 && p.skipped == 5);
    }
}

#[test]
fn test_parser_feed_in_pieces() {
    let data = read_fixture("claude.jsonl");
    let (_, whole) = parse(&Claude, &data);

    let mut p = Claude.new_parser(Options::default());
    let mut pieces = Vec::new();
    let mut end = 0usize;
    while end <= data.len() {
        let start = p.pos().at as usize;
        pieces.extend(p.feed(&data[start..end.min(data.len())]));
        end += 97;
    }
    let start = p.pos().at as usize;
    pieces.extend(p.feed(&data[start..]));
    assert_eq!(pieces, whole);
    let pos = p.pos();
    assert_eq!(pos.at, data.len() as i64);
    assert_eq!(pos.line, 20);
    assert_eq!(pos.seq, whole.len());
}

#[test]
fn test_resume_after() {
    let mut claude_paths = Vec::new();
    for entry in std::fs::read_dir(corpus_path("claude")).unwrap().flatten() {
        let p = entry.path();
        if let Some(s) = p.to_str()
            && s.ends_with(".jsonl")
            && !s.ends_with(".events.jsonl")
        {
            claude_paths.push(s.to_string());
        }
    }
    claude_paths.sort();
    claude_paths.push(filepath::join(&[
        env!("CARGO_MANIFEST_DIR"),
        "testdata",
        "claude.jsonl",
    ]));

    let agy_paths = vec![
        corpus_path(
            "agy/conversation/b51e7553-7a08-532d-a2c7-2b5cc6769489/.system_generated/logs/transcript_full.jsonl",
        ),
        filepath::join(&[env!("CARGO_MANIFEST_DIR"), "testdata", "agy.jsonl"]),
    ];
    let kimi_paths = vec![corpus_path(
        "kimi/interleaved/session_f0aa1fb8-41b1-5485-a887-ecd00a0bf053/agents/main/wire.jsonl",
    )];

    for p in &claude_paths {
        check_resume_after(&Claude, p);
    }
    for p in &agy_paths {
        check_resume_after(&Agy, p);
    }
    for p in &kimi_paths {
        check_resume_after(&Kimi, p);
    }

    let data = read_fixture("claude.jsonl");
    for at in [1i64, data.len() as i64, data.len() as i64 + 5] {
        assert!(resume_after(&Claude, Options::default(), &data, at).is_err());
    }
    let err = resume_after(&Claude, Options::default(), br#"{"type":"user""#, 0)
        .err()
        .expect("unfinished line must fail");
    assert!(err.to_string().contains("no end yet"), "{err}");
}

fn check_resume_after(r: &dyn Reader, path: &str) {
    let data = std::fs::read(path).unwrap();
    for requests in [false, true] {
        let mut opts = file_options(r.harness(), path);
        opts.requests = requests;
        let whole = r.new_parser(opts.clone()).feed(&data);
        let mut at = 0usize;
        let mut line = 0usize;
        while at < data.len() {
            let mut p = resume_after(r, opts.clone(), &data, at as i64).unwrap();
            let start = p.pos().at as usize;
            let got = p.feed(&data[start..]);
            let want: Vec<Event> = whole.iter().filter(|e| e.line > line).cloned().collect();
            assert_eq!(got, want, "{path}, after line {line} (requests {requests})");
            let nl = data[at..].iter().position(|&b| b == b'\n').unwrap();
            at += nl + 1;
            line += 1;
        }
    }
}

#[test]
fn test_latest() {
    let (_, events) = parse(&Claude, &read_fixture("claude.jsonl"));
    let folded = latest(&events);
    let want = [
        r#"1 add prompt(human) "run the tests""#,
        r#"1 add think "go test should do it""#,
        r#"1 add tool Bash(go test ./...) ok "ok  \tpkg/a\t0.1s\nok  \tpkg/b\t0.2s""#,
        r#"1 add tool Read(go.mod) ok "1\tmodule example.com/proj""#,
        r#"1 add say final "All tests pass.""#,
        r#"2 add prompt(command) "/review high""#,
        r#"2 add tool Bash(make lint) error "Exit code 2\nlint: 1 issue""#,
        r#"2 add prompt(task-notification) "<task-notification>background task finished</task-notification>""#,
        r#"2 add say "One lint issue.\nSee above.""#,
        r#"2 add tool Grep(TODO) pending """#,
    ];
    assert_eq!(summaries(&folded), want);
    assert_eq!(folded[2].line, 7);
    assert_eq!(folded[2].seq, 2);
    assert!(latest(&events[4..5]).is_empty());
}

fn write_temp_file(dir: &tempfile::TempDir, name: &str, content: &[u8]) -> String {
    let path = filepath::join(&[dir.path().to_str().unwrap(), name]);
    std::fs::write(&path, content).unwrap();
    path
}

#[test]
fn test_read_file() {
    let dir = temp_dir();
    let fixture = read_fixture("claude.jsonl");
    let path = write_temp_file(&dir, "00000000-0000-0000-0000-0000000000aa.jsonl", &fixture);
    let f = read_file(&path, Some(&Claude), 50).unwrap();
    assert_eq!(f.events.len(), 13);
    assert!(!f.partial);
    assert_eq!(f.session.path, path);
    assert_eq!(f.session.size, fixture.len() as i64);
    assert_eq!(f.session.session_id, "00000000-0000-0000-0000-00000000c1a0");

    let agy_path = write_temp_file(&dir, "transcript_full.jsonl", &read_fixture("agy.jsonl"));
    let f = read_file(&agy_path, None, 50).unwrap();
    assert_eq!(f.session.harness, "agy");
    assert_eq!(f.events.len(), 24);
}

#[test]
fn test_read_file_tail_window() {
    let dir = temp_dir();
    let mut b = String::from(
        "{\"type\":\"user\",\"uuid\":\"p\",\"cwd\":\"/home/user/big\",\"sessionId\":\"big\",\"timestamp\":\"2026-08-01T10:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"first\"}}\n",
    );
    let pad = "x".repeat(2000);
    for i in 0..3000 {
        b.push_str(&format!(
            "{{\"type\":\"assistant\",\"uuid\":\"a{i}\",\"timestamp\":\"2026-08-01T10:00:01Z\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"reply {i}\"}}]}},\"pad\":\"{pad}\"}}\n"
        ));
    }
    let path = write_temp_file(&dir, "big.jsonl", b.as_bytes());

    for n in [3usize, 1000] {
        let f = read_file(&path, Some(&Claude), n).unwrap();
        assert!(f.events.len() >= n && f.partial);
        let last = f.events.last().unwrap();
        assert_eq!(last.say.as_ref().unwrap().text, "reply 2999");
        assert_eq!(last.line, 3000);
        let first = &f.events[0];
        assert_eq!(
            first.say.as_ref().unwrap().text,
            format!("reply {}", first.line - 1)
        );
        assert_eq!(f.session.cwd, "/home/user/big");
        assert_eq!(f.session.session_id, "big");
        assert_eq!(f.skipped, 0);
    }

    let f = read_file(&path, Some(&Claude), 1_000_000).unwrap();
    assert_eq!(f.events.len(), 3001);
    assert!(!f.partial);
}

#[test]
fn test_read_file_partial_last_line() {
    let dir = temp_dir();
    let path = write_temp_file(
        &dir,
        "s.jsonl",
        b"{\"type\":\"user\",\"uuid\":\"u\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n{\"type\":\"assistant\",\"message\":{\"role\":\"assis",
    );
    let f = read_file(&path, Some(&Claude), 50).unwrap();
    assert_eq!(f.events.len(), 1);
    assert_eq!(f.lines, 1);
    assert_eq!(f.skipped, 0);
}

#[test]
fn test_read_file_errors() {
    let dir = temp_dir();
    let missing = filepath::join(&[dir.path().to_str().unwrap(), "none.jsonl"]);
    let err = read_file(&missing, Some(&Claude), 50).unwrap_err();
    assert!(
        agent_base::gostd::os::is_not_exist(&err),
        "missing file: {err:?}"
    );
    // The Go's texts, printed with {:#} as the port prints errors.
    assert_eq!(
        format!("{err:#}"),
        format!("open {missing}: no such file or directory")
    );

    let bad = write_temp_file(&dir, "s.jsonl", b"plain text\nnot a transcript\n");
    let err = read_file(&bad, Some(&Claude), 50).unwrap_err();
    assert!(
        err.downcast_ref::<ErrUnparseable>().is_some(),
        "text file: {err:?}"
    );
    assert_eq!(
        format!("{err:#}"),
        "unparseable transcript: none of its 2 lines is a JSON object"
    );
    assert_eq!(err.to_string(), format!("{err:#}"));

    let empty = write_temp_file(&dir, "e.jsonl", b"");
    let f = read_file(&empty, Some(&Agy), 50).unwrap();
    assert!(f.events.is_empty());
}

#[test]
fn test_read_file_subagent_turn() {
    let path = corpus_path("claude/subagent/subagents/agent-a3f9c2e17b4d5e608.jsonl");
    let turns = claude_subagent_turns(&path).expect("turns");
    assert_eq!(turns("b92d70b1-18b1-5ccf-96b7-bd5d1ce10135"), Some(1));
    assert_eq!(turns("f6c51b03-1d25-579e-8362-5a8ab18cb715"), Some(1));
    assert_eq!(turns("unknown"), None);

    let f = read_file(&path, Some(&Claude), 50).unwrap();
    for e in &f.events {
        let turn = e.turn.as_ref().unwrap();
        assert_eq!(turn.id, "b92d70b1-18b1-5ccf-96b7-bd5d1ce10135");
        assert_eq!(turn.index, 1);
        assert_eq!(e.agent, "a3f9c2e17b4d5e608");
    }
    assert!(claude_subagent_turns(&corpus_path("claude/subagent.jsonl")).is_none());
}

#[test]
fn test_claude_subagent_turns_grow() {
    let dir = temp_dir();
    let root = dir.path().to_str().unwrap();
    let parent = filepath::join(&[root, "s1.jsonl"]);
    let prompt = |id: &str, index: usize| -> String {
        format!(
            "{{\"type\":\"user\",\"uuid\":\"{id}\",\"promptId\":\"{id}\",\"turnPosition\":{{\"promptIndex\":{index}}},\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":\"go\"}}}}\n"
        )
    };
    let write_test_file = |p: &str, content: &str| {
        std::fs::create_dir_all(filepath::dir(p)).unwrap();
        std::fs::write(p, content).unwrap();
    };
    write_test_file(&parent, &prompt("p1", 1));
    let sub = filepath::join(&[root, "s1", "subagents", "agent-x.jsonl"]);
    let say = |uuid: &str, prompt_id: &str| -> String {
        format!(
            "{{\"type\":\"user\",\"uuid\":\"{uuid}\",\"promptId\":\"{prompt_id}\",\"agentId\":\"x\",\"sessionId\":\"s1\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"tool_use_id\":\"none\",\"content\":\"\"}}]}}}}\n{{\"type\":\"assistant\",\"uuid\":\"{uuid}-a\",\"agentId\":\"x\",\"message\":{{\"role\":\"assistant\",\"content\":[{{\"type\":\"text\",\"text\":\"working\"}}],\"stop_reason\":\"tool_use\"}}}}\n"
        )
    };
    let mut p = Claude.new_parser(Options {
        turns: claude_subagent_turns(&sub),
        ..Options::default()
    });
    let mut events = Vec::new();
    events.extend(p.feed(say("r1", "p1").as_bytes()));
    write_test_file(&parent, &format!("{}{}", prompt("p1", 1), prompt("p2", 2)));
    events.extend(p.feed(say("r2", "p2").as_bytes()));
    events.extend(p.feed(say("r3", "p9").as_bytes()));
    let got: Vec<String> = events
        .iter()
        .map(|e| {
            let t = e.turn.as_ref().unwrap();
            format!("{} {}", t.id, t.index)
        })
        .collect();
    assert_eq!(got, vec!["p1 1", "p2 2", "p9 2"]);

    let wf = filepath::join(&[
        root,
        "s1",
        "subagents",
        "workflows",
        "wf_1",
        "agent-w.jsonl",
    ]);
    assert_eq!(claude_subagent_turns(&wf).unwrap()("p2"), Some(2));
}

#[test]
fn test_claude_subagent_session() {
    for (path, want) in [
        ("/p/proj/s1/subagents/agent-a1.jsonl", "/p/proj/s1"),
        (
            "/p/proj/s1/subagents/workflows/wf_1/agent-w1.jsonl",
            "/p/proj/s1",
        ),
        ("/p/proj/s1.jsonl", ""),
        ("/p/proj/s1/subagents/agent-a1.meta.json", ""),
        ("/p/proj/s1/subagents/other/agent-a1.jsonl", ""),
        ("/p/proj/s1/subagents/workflows/agent-a1.jsonl", ""),
        ("/p/proj/s1/subagents/workflows/wf_1/deep/agent.jsonl", ""),
    ] {
        assert_eq!(claude_subagent_session(path), want, "{path}");
    }
}

#[test]
fn test_claude_counted_turns() {
    let lines = [
        r#"{"type":"user","uuid":"u1","promptId":"p1","message":{"role":"user","content":"first"}}"#,
        r#"{"type":"assistant","uuid":"a1","message":{"role":"assistant","content":[{"type":"text","text":"one"}],"stop_reason":"end_turn"}}"#,
        r#"{"type":"user","uuid":"u2","promptId":"p2","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<summary>done</summary>\n</task-notification>"}}"#,
        r#"{"type":"assistant","uuid":"a2","message":{"role":"assistant","content":[{"type":"text","text":"noted"}],"stop_reason":"end_turn"}}"#,
        r#"{"type":"user","uuid":"u3","promptId":"p3","message":{"role":"user","content":"<command-name>/review</command-name>"}}"#,
        r#"{"type":"assistant","uuid":"a3","message":{"role":"assistant","content":[{"type":"text","text":"reviewed"}],"stop_reason":"end_turn"}}"#,
    ];
    let (_, events) = parse(&Claude, format!("{}\n", lines.join("\n")).as_bytes());
    let got: Vec<String> = events
        .iter()
        .map(|e| {
            let t = e.turn.as_ref().unwrap();
            format!("{} {} {}", t.id, t.index, t.derived)
        })
        .collect();
    assert_eq!(
        got,
        vec![
            "p1 1 false",
            "p1 1 false",
            "p2 1 false",
            "p2 1 false",
            "p3 2 false",
            "p3 2 false",
        ]
    );
}

#[test]
fn test_claude_rewind() {
    let lines = [
        r#"{"type":"user","uuid":"u1","parentUuid":null,"promptId":"p1","message":{"role":"user","content":"first"}}"#,
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"text","text":"one"}],"stop_reason":"end_turn"}}"#,
        r#"{"type":"user","uuid":"u2","parentUuid":"a1","promptId":"p2","message":{"role":"user","content":"second"}}"#,
        r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","message":{"role":"assistant","content":[{"type":"tool_use","id":"bg1","name":"Bash","input":{"command":"make","description":"job","run_in_background":true}}],"stop_reason":"tool_use"}}"#,
        r#"{"type":"user","uuid":"r2","parentUuid":"a2","promptId":"p2","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"bg1","content":"started"}]},"toolUseResult":{"backgroundTaskId":"t1"}}"#,
        r#"{"type":"user","uuid":"i2","parentUuid":"r2","promptId":"p2","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user]"}]}}"#,
        r#"{"type":"user","uuid":"u3","parentUuid":"a1","promptId":"p3","message":{"role":"user","content":"second, edited"}}"#,
        r#"{"type":"user","uuid":"u4","parentUuid":"u3","promptId":"p4","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification>\n<tool-use-id>bg1</tool-use-id>\n<status>completed</status>\n<summary>job done</summary>\n</task-notification>"}}"#,
        r#"{"type":"user","uuid":"u5","parentUuid":"u4","promptId":"p5","message":{"role":"user","content":"third"}}"#,
    ];
    let (_, events) = parse(&Claude, format!("{}\n", lines.join("\n")).as_bytes());
    let got: Vec<String> = events
        .iter()
        .map(|e| {
            let mut s = summary(e);
            if e.abandoned {
                s.push_str(" abandoned");
            }
            s
        })
        .collect();
    let want = [
        r#"1 add prompt(human) "first""#,
        r#"1 add say final "one""#,
        r#"2 add prompt(human) "second""#,
        r#"2 add tool Bash(make) pending """#,
        r#"2 update tool Bash(make) background "started""#,
        r#"2 add system interrupt "[Request interrupted by user]""#,
        r#"2 add prompt(human) "second, edited""#,
        r#"2 update prompt(human) "second" abandoned"#,
        r#"2 update tool Bash(make) background "started" abandoned"#,
        r#"2 update system interrupt "[Request interrupted by user]" abandoned"#,
        r#"2 add prompt(task-notification) "<task-notification>\n<tool-use-id>bg1</tool-use-id>\n<status>completed</status>\n<summary>job done</summary>\n</task-notification>""#,
        r#"2 update tool Bash(make) ok "job done" abandoned"#,
        r#"3 add prompt(human) "third""#,
    ];
    assert_eq!(got, want);

    let dead: Vec<String> = latest(&events)
        .into_iter()
        .filter(|e| e.abandoned)
        .map(|e| e.id)
        .collect();
    assert_eq!(dead, vec!["u2#0", "bg1", "i2#0"]);

    let fork = [
        r#"{"type":"user","uuid":"u1","parentUuid":null,"promptId":"p1","message":{"role":"user","content":"go"}}"#,
        r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","message":{"role":"assistant","content":[{"type":"tool_use","id":"c1","name":"Bash","input":{"command":"ls"}}],"stop_reason":"tool_use"}}"#,
        r#"{"type":"assistant","uuid":"a2","parentUuid":"a1","message":{"role":"assistant","content":[{"type":"tool_use","id":"c2","name":"Bash","input":{"command":"pwd"}}],"stop_reason":"tool_use"}}"#,
        r#"{"type":"user","uuid":"r1","parentUuid":"a1","promptId":"p1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c1","content":"a"}]}}"#,
        r#"{"type":"user","uuid":"r2","parentUuid":"r1","promptId":"p1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"c2","content":"b"}]}}"#,
    ];
    let (_, fork_events) = parse(&Claude, format!("{}\n", fork.join("\n")).as_bytes());
    for e in &fork_events {
        assert!(!e.abandoned, "{} abandoned", summary(e));
    }
}

#[test]
fn test_session_id_from_path() {
    for (path, want) in [
        ("/home/user/.claude/projects/-p/0000-aa.jsonl", "0000-aa"),
        (
            "/home/user/.claude/projects/-p/0000-aa/subagents/agent-a1b2.jsonl",
            "agent-a1b2",
        ),
        (
            "/home/user/.gemini/antigravity-cli/brain/c-1/.system_generated/logs/transcript_full.jsonl",
            "c-1",
        ),
        (
            "/home/user/.gemini/antigravity-cli/brain/c-2/transcript_full.jsonl",
            "c-2",
        ),
        (
            "/home/user/.kimi-code/sessions/wd_w_1/session_f0aa/agents/main/wire.jsonl",
            "f0aa",
        ),
        (
            "/home/user/.kimi-code/sessions/wd_w_1/session_f0aa/agents/agent-1/wire.jsonl",
            "f0aa:agent-1",
        ),
        (
            "/home/user/.gemini/antigravity-cli/brain/c-3/.system_generated/logs/transcript.jsonl",
            "c-3",
        ),
        (
            "/home/user/.gemini/antigravity/brain/c-4/transcript.jsonl",
            "c-4",
        ),
        ("/var/tmp/transcript.jsonl", ""),
        ("", ""),
    ] {
        assert_eq!(session_id_from_path(path), want, "{path}");
    }
}

#[test]
fn test_real_transcripts_if_present() {
    let Ok(paths) = std::env::var("GOMUX_REAL_TRANSCRIPTS") else {
        return;
    };
    if paths.is_empty() {
        return;
    }
    for path in std::env::split_paths(&paths) {
        let Some(p) = path.to_str() else { continue };
        let data = std::fs::read(p).unwrap();
        let r = detect(&data);
        let f = read_file(p, Some(r), 1 << 30).unwrap();
        let mut kinds: HashMap<String, usize> = HashMap::new();
        for e in latest(&f.events) {
            *kinds.entry(e.kind.clone()).or_insert(0) += 1;
            if let Some(ref tool) = e.tool
                && let Some(ref res) = tool.result
            {
                *kinds.entry(res.status.clone()).or_insert(0) += 1;
            }
            if let Some(ref t) = e.turn
                && t.index == 0
            {
                *kinds.entry("turn index 0".to_string()).or_insert(0) += 1;
            }
        }
        assert_eq!(f.skipped, 0, "{p}: {} lines skipped", f.skipped);
    }
}
