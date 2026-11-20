use std::collections::BTreeMap;

use serde_json::Value;

use crate::agy::{AGY_NUMBERED_LINE, numbered_text};
use crate::kimi::KIMI_LINE;
use crate::tools::{MAX_DIFF_CELLS, replaced_lines};
use crate::{ACTION_NOISE, FileText, RawJson, ToolSummary, describe_tool, summarize_tool};

fn ts(action: &str, target: &str, detail: &str, file: Option<FileText>) -> ToolSummary {
    ToolSummary {
        action: action.to_string(),
        target: target.to_string(),
        detail: detail.to_string(),
        meta: None,
        file,
    }
}

fn same_json(got: Option<&RawJson>, want: &str) -> bool {
    let Some(got) = got else {
        return want.is_empty();
    };
    if got.is_empty() || want.is_empty() {
        return got.is_empty() && want.is_empty();
    }
    let Ok(g) = serde_json::from_str::<Value>(got.as_str()) else {
        return false;
    };
    let Ok(w) = serde_json::from_str::<Value>(want) else {
        return false;
    };
    normalize(g) == normalize(w)
}

fn normalize(v: Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut b = BTreeMap::new();
            for (k, val) in m {
                b.insert(k, normalize(val));
            }
            let mut out = serde_json::Map::new();
            for (k, val) in b {
                out.insert(k, val);
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.into_iter().map(normalize).collect()),
        other => other,
    }
}

#[test]
fn test_summarize_tool() {
    const CWD: &str = "/home/user/proj/";
    struct Case {
        name: &'static str,
        tool: &'static str,
        input: &'static str,
        response: &'static str,
        want: ToolSummary,
        want_meta: &'static str,
    }
    let tests = [
        Case {
            name: "read: a partial slice has a range; the target is relativized",
            tool: "Read",
            input: r#"{"file_path":"/home/user/proj/a/b.js"}"#,
            response: r#"{"type":"text","file":{"numLines":42,"startLine":40,"totalLines":100}}"#,
            want: ts("read", "a/b.js", "", None),
            want_meta: r#"{"lines":42,"range":[40,81]}"#,
        },
        Case {
            name: "read: a whole file has no range",
            tool: "Read",
            input: r#"{"file_path":"/home/user/proj/x.js"}"#,
            response: r#"{"file":{"numLines":100,"startLine":1,"totalLines":100}}"#,
            want: ts("read", "x.js", "", None),
            want_meta: r#"{"lines":100}"#,
        },
        Case {
            name: "edit: structuredPatch gives the counts, the added runs and the hunks",
            tool: "Edit",
            input: r#"{"file_path":"/home/user/proj/a.js"}"#,
            response: r#"{"structuredPatch":[{"newStart":10,"lines":[" a","+b","+c"," d","-e","+f"]}]}"#,
            want: ts("edit", "a.js", "", None),
            want_meta: r#"{"added":3,"removed":1,"ranges":[[11,12],[14,14]],"patch":[{"newStart":10,"lines":[" a","+b","+c"," d","-e","+f"]}]}"#,
        },
        Case {
            name: "edit: the patch wins over the input",
            tool: "Edit",
            input: r#"{"file_path":"/p/f.js","old_string":"a","new_string":"b"}"#,
            response: r#"{"structuredPatch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}]}"#,
            want: ts("edit", "/p/f.js", "", None),
            want_meta: r#"{"added":1,"removed":1,"ranges":[[1,1]],"patch":[{"oldStart":1,"oldLines":1,"newStart":1,"newLines":1,"lines":["-a","+b"]}]}"#,
        },
        Case {
            name: "edit: no patch and no replacement in the input: no meta",
            tool: "Edit",
            input: r#"{"file_path":"/p/f.js"}"#,
            response: r#"{}"#,
            want: ts("edit", "/p/f.js", "", None),
            want_meta: "",
        },
        Case {
            name: "MultiEdit is an edit",
            tool: "MultiEdit",
            input: r#"{"file_path":"/home/user/proj/m.js"}"#,
            response: r#"{"structuredPatch":[]}"#,
            want: ts("edit", "m.js", "", None),
            want_meta: r#"{"added":0,"removed":0,"patch":[]}"#,
        },
        Case {
            name: "NotebookEdit is an edit of its notebook_path",
            tool: "NotebookEdit",
            input: r#"{"notebook_path":"/home/user/proj/n.ipynb"}"#,
            response: r#"{"structuredPatch":[]}"#,
            want: ts("edit", "n.ipynb", "", None),
            want_meta: r#"{"added":0,"removed":0,"patch":[]}"#,
        },
        Case {
            name: "write: kind and lines",
            tool: "Write",
            input: r#"{"file_path":"/home/user/proj/w.js"}"#,
            response: r#"{"type":"create","content":"a\nb\nc"}"#,
            want: ts(
                "write",
                "w.js",
                "",
                Some(FileText {
                    text: "a\nb\nc".to_string(),
                    start: 1,
                    total: 3,
                }),
            ),
            want_meta: r#"{"kind":"create","lines":3}"#,
        },
        Case {
            name: "bash: the command; its output's lines, a final newline not counted",
            tool: "Bash",
            input: r#"{"command":"ls -la"}"#,
            response: r#"{"stdout":"one\ntwo\n","interrupted":false}"#,
            want: ts("bash", "", "ls -la", None),
            want_meta: r#"{"lines":2}"#,
        },
        Case {
            name: "grep: the pattern; its path is no target",
            tool: "Grep",
            input: r#"{"path":"/home/user/proj/src","pattern":"TODO"}"#,
            response: r#"{"content":"src/a.js:1:TODO"}"#,
            want: ts("grep", "", "TODO", None),
            want_meta: "",
        },
        Case {
            name: "glob: the pattern",
            tool: "Glob",
            input: r#"{"pattern":"**/*.js"}"#,
            response: "",
            want: ts("glob", "", "**/*.js", None),
            want_meta: "",
        },
        Case {
            name: "task: subagent_type first; tools, tokens and ms",
            tool: "Task",
            input: r#"{"subagent_type":"Explore","description":"find x"}"#,
            response: r#"{"totalToolUseCount":7,"totalTokens":1234,"totalDurationMs":4500,"content":"done"}"#,
            want: ts("task", "", "Explore", None),
            want_meta: r#"{"tools":7,"tokens":1234,"ms":4500}"#,
        },
        Case {
            name: "Agent is a task",
            tool: "Agent",
            input: r#"{"description":"d"}"#,
            response: r#"{}"#,
            want: ts("task", "", "d", None),
            want_meta: "",
        },
        Case {
            name: "ask: the questions, one per line",
            tool: "AskUserQuestion",
            input: r#"{"questions":[{"question":"A?"},{"question":"B?"}]}"#,
            response: r#""chose A""#,
            want: ts("ask", "", "A?\nB?", None),
            want_meta: "",
        },
        Case {
            name: "fetch: the url",
            tool: "WebFetch",
            input: r#"{"url":"http://x"}"#,
            response: r#"{"result":"summary"}"#,
            want: ts("fetch", "", "http://x", None),
            want_meta: "",
        },
        Case {
            name: "search: the query",
            tool: "WebSearch",
            input: r#"{"query":"q"}"#,
            response: r#"{"content":"hits"}"#,
            want: ts("search", "", "q", None),
            want_meta: "",
        },
        Case {
            name: "Kimi TodoList is noise",
            tool: "TodoList",
            input: r#"{}"#,
            response: r#"{}"#,
            want: ts(ACTION_NOISE, "", "", None),
            want_meta: "",
        },
        Case {
            name: "Kimi EnterPlanMode is noise",
            tool: "EnterPlanMode",
            input: r#"{}"#,
            response: r#"{}"#,
            want: ts(ACTION_NOISE, "", "", None),
            want_meta: "",
        },
        Case {
            name: "Kimi AgentSwarm is a task: its description",
            tool: "AgentSwarm",
            input: r#"{"description":"review each file"}"#,
            response: r#"{"output":"done"}"#,
            want: ts("task", "", "review each file", None),
            want_meta: "",
        },
        Case {
            name: "Kimi FetchURL is a fetch",
            tool: "FetchURL",
            input: r#"{"url":"http://x"}"#,
            response: r#"{"output":"page text"}"#,
            want: ts("fetch", "", "http://x", None),
            want_meta: "",
        },
        Case {
            name: "Skill: the skill's name",
            tool: "Skill",
            input: r#"{"skill":"pdf"}"#,
            response: "",
            want: ts("skill", "", "pdf", None),
            want_meta: "",
        },
        Case {
            name: "agy view_file is a read of its AbsolutePath",
            tool: "view_file",
            input: r#"{"AbsolutePath":"/home/user/proj/src/a.js"}"#,
            response: r#"{"file":{"numLines":2,"startLine":5,"totalLines":9}}"#,
            want: ts("read", "src/a.js", "", None),
            want_meta: r#"{"lines":2,"range":[5,6]}"#,
        },
        Case {
            name: "agy read_file is a read of its Path",
            tool: "read_file",
            input: r#"{"Path":"/home/user/proj/b.md"}"#,
            response: "",
            want: ts("read", "b.md", "", None),
            want_meta: "",
        },
        Case {
            name: "agy edit_file: a pure deletion has no range",
            tool: "edit_file",
            input: r#"{"Path":"/home/user/proj/c.js"}"#,
            response: r#"{"replacement":{"startLine":10,"added":0,"removed":2}}"#,
            want: ts("edit", "c.js", "", None),
            want_meta: r#"{"added":0,"removed":2}"#,
        },
        Case {
            name: "agy replace_file_content is an edit of its TargetFile",
            tool: "replace_file_content",
            input: r#"{"TargetFile":"/home/user/proj/d.js"}"#,
            response: r#"{}"#,
            want: ts("edit", "d.js", "", None),
            want_meta: "",
        },
        Case {
            name: "agy write_file is a write",
            tool: "write_file",
            input: r#"{"Path":"/home/user/proj/e.txt"}"#,
            response: r#"{"type":"update","content":"a\nb"}"#,
            want: ts("write", "e.txt", "", None),
            want_meta: r#"{"kind":"update","lines":2}"#,
        },
        Case {
            name: "agy run_command is a bash of its CommandLine",
            tool: "run_command",
            input: r#"{"CommandLine":"make"}"#,
            response: r#"{"stdout":"ok\n","output":"ok\n"}"#,
            want: ts("bash", "", "make", None),
            want_meta: r#"{"lines":1}"#,
        },
        Case {
            name: "agy manage_task: its action and task",
            tool: "manage_task",
            input: r#"{"Action":"kill","TaskId":"c/task-3"}"#,
            response: r#"{"output":"cancelled"}"#,
            want: ts("task", "", "kill c/task-3", None),
            want_meta: "",
        },
        Case {
            name: "agy invoke_subagent: a single top-level spec",
            tool: "invoke_subagent",
            input: r#"{"Role":"R","Prompt":"p"}"#,
            response: "",
            want: ts("task", "", "R", None),
            want_meta: "",
        },
        Case {
            name: "agy search_web is a search",
            tool: "search_web",
            input: r#"{"query":"q"}"#,
            response: r#"{"output":"hits"}"#,
            want: ts("search", "", "q", None),
            want_meta: "",
        },
        Case {
            name: "agy read_url_content is a fetch of its Url",
            tool: "read_url_content",
            input: r#"{"Url":"http://x"}"#,
            response: "",
            want: ts("fetch", "", "http://x", None),
            want_meta: "",
        },
        Case {
            name: "TodoWrite is noise",
            tool: "TodoWrite",
            input: r#"{"todos":[]}"#,
            response: r#"{}"#,
            want: ts(ACTION_NOISE, "", "", None),
            want_meta: "",
        },
        Case {
            name: "ToolSearch is noise, with its query",
            tool: "ToolSearch",
            input: r#"{"query":"select:Read"}"#,
            response: r#"{}"#,
            want: ts(ACTION_NOISE, "", "select:Read", None),
            want_meta: "",
        },
        Case {
            name: "unknown tool: its name lowercased; the first known input key, command before name",
            tool: "SomeNewTool",
            input: r#"{"name":"nope","command":"do-it"}"#,
            response: r#"{"output":"out"}"#,
            want: ts("somenewtool", "", "do-it", None),
            want_meta: "",
        },
        Case {
            name: "out of cwd: the target stays absolute",
            tool: "Read",
            input: r#"{"file_path":"/var/tmp/scratch.txt"}"#,
            response: r#"{"file":{"numLines":1}}"#,
            want: ts("read", "/var/tmp/scratch.txt", "", None),
            want_meta: r#"{"lines":1}"#,
        },
        Case {
            name: "MultiEdit without a patch: every edit's lines, from the input",
            tool: "MultiEdit",
            input: r#"{"file_path":"/home/user/proj/m.go","edits":[
				{"old_string":"foo\nbar\nbaz","new_string":"foo\nBAR\nbaz"},
				{"old_string":"x := 1\n","new_string":"x := 1\ny := 2\nz := 3\n"},
				{"old_string":"a\ngone\nb","new_string":"a\nb"}]}"#,
            response: "",
            want: ts("edit", "m.go", "", None),
            want_meta: r#"{"added":3,"removed":2}"#,
        },
        Case {
            name: "edit from the input: a replacement within one line changes it",
            tool: "Edit",
            input: r#"{"file_path":"/home/user/proj/a.go","old_string":"foo","new_string":"bar"}"#,
            response: "",
            want: ts("edit", "a.go", "", None),
            want_meta: r#"{"added":1,"removed":1}"#,
        },
        Case {
            name: "edit from the input: a function appended after a closing brace",
            tool: "Edit",
            input: r#"{"file_path":"/home/user/proj/a.go","old_string":"func a() {\n\treturn 1\n}","new_string":"func a() {\n\treturn 1\n}\n\nfunc b() {}"}"#,
            response: "",
            want: ts("edit", "a.go", "", None),
            want_meta: r#"{"added":2,"removed":0}"#,
        },
        Case {
            name: "edit from the input: lines deleted after a kept one",
            tool: "Edit",
            input: r#"{"file_path":"/home/user/proj/a.go","old_string":"x\ndead1\ndead2","new_string":"x"}"#,
            response: "",
            want: ts("edit", "a.go", "", None),
            want_meta: r#"{"added":0,"removed":2}"#,
        },
        Case {
            name: "edit from the input: an empty side is no lines",
            tool: "Edit",
            input: r#"{"file_path":"/home/user/proj/a.go","old_string":"","new_string":"a\nb"}"#,
            response: "",
            want: ts("edit", "a.go", "", None),
            want_meta: r#"{"added":2,"removed":0}"#,
        },
        Case {
            name: "MultiEdit whose edits aren't a list: the top-level replacement",
            tool: "MultiEdit",
            input: r#"{"file_path":"/p/m.go","edits":null,"old_string":"a","new_string":"b"}"#,
            response: "",
            want: ts("edit", "/p/m.go", "", None),
            want_meta: r#"{"added":1,"removed":1}"#,
        },
        Case {
            name: "edit from the input: replace_all can't be counted",
            tool: "Edit",
            input: r#"{"file_path":"/home/user/proj/a.go","old_string":"a","new_string":"b","replace_all":true}"#,
            response: "",
            want: ts("edit", "a.go", "", None),
            want_meta: "",
        },
        Case {
            name: "MultiEdit from the input: one replace_all leaves the whole call uncounted",
            tool: "MultiEdit",
            input: r#"{"file_path":"/p/m.go","edits":[{"old_string":"a","new_string":"b"},{"old_string":"c","new_string":"d","replace_all":true}]}"#,
            response: "",
            want: ts("edit", "/p/m.go", "", None),
            want_meta: "",
        },
        Case {
            name: "agy replace_file_content from the input",
            tool: "replace_file_content",
            input: r#"{"TargetFile":"/home/user/proj/d.go","TargetContent":"a\nb\n","ReplacementContent":"a\nc\nd\n","AllowMultiple":false}"#,
            response: "",
            want: ts("edit", "d.go", "", None),
            want_meta: r#"{"added":2,"removed":1}"#,
        },
        Case {
            name: "NotebookEdit: an inserted cell's lines",
            tool: "NotebookEdit",
            input: r#"{"notebook_path":"/home/user/proj/n.ipynb","edit_mode":"insert","new_source":"import os\nprint(1)"}"#,
            response: r#"{"new_source":"import os\nprint(1)","edit_mode":"insert"}"#,
            want: ts("edit", "n.ipynb", "", None),
            want_meta: r#"{"added":2,"removed":0}"#,
        },
        Case {
            name: "NotebookEdit: a replaced cell can't be counted from the input",
            tool: "NotebookEdit",
            input: r#"{"notebook_path":"/home/user/proj/n.ipynb","edit_mode":"replace","new_source":"x"}"#,
            response: "",
            want: ts("edit", "n.ipynb", "", None),
            want_meta: "",
        },
        Case {
            name: "write while running: its lines from the input",
            tool: "Write",
            input: r#"{"file_path":"/home/user/proj/w.go","content":"package w\n\nfunc W() {}\n"}"#,
            response: "",
            want: ts("write", "w.go", "", None),
            want_meta: r#"{"lines":3}"#,
        },
        Case {
            name: "agy write_to_file: its CodeContent",
            tool: "write_to_file",
            input: r##"{"TargetFile":"/home/user/proj/w.md","CodeContent":"# W\n"}"##,
            response: "",
            want: ts("write", "w.md", "", None),
            want_meta: r#"{"lines":1}"#,
        },
        Case {
            name: "Kimi file tools name the file path",
            tool: "Read",
            input: r#"{"path":"/home/user/proj/k.rs"}"#,
            response: "",
            want: ts("read", "k.rs", "", None),
            want_meta: "",
        },
        Case {
            name: "Kimi Bash output counts its lines",
            tool: "Bash",
            input: r#"{"command":"ls"}"#,
            response: r#"{"output":"a\nb\nc\n"}"#,
            want: ts("bash", "", "ls", None),
            want_meta: r#"{"lines":3}"#,
        },
        Case {
            name: "bash while running: no meta",
            tool: "Bash",
            input: r#"{"command":"sleep 9"}"#,
            response: "",
            want: ts("bash", "", "sleep 9", None),
            want_meta: "",
        },
        Case {
            name: "bash interrupted",
            tool: "Bash",
            input: r#"{"command":"sleep 9"}"#,
            response: r#"{"stdout":"","interrupted":true}"#,
            want: ts("bash", "", "sleep 9", None),
            want_meta: r#"{"lines":0,"interrupted":true}"#,
        },
        Case {
            name: "read of an image: its size",
            tool: "Read",
            input: r#"{"file_path":"/home/user/proj/i.png"}"#,
            response: r#"{"type":"image","file":{"originalSize":2048}}"#,
            want: ts("read", "i.png", "", None),
            want_meta: r#"{"bytes":2048}"#,
        },
        Case {
            name: "agy invoke_subagent: its Subagents' roles",
            tool: "invoke_subagent",
            input: r#"{"Subagents":[{"Role":"reviewer"},{"TypeName":"tester"}]}"#,
            response: "",
            want: ts("task", "", "reviewer\ntester", None),
            want_meta: "",
        },
        Case {
            name: "agy invoke_subagent whose Subagents isn't a list: its own spec",
            tool: "invoke_subagent",
            input: r#"{"Subagents":null,"Role":"R"}"#,
            response: "",
            want: ts("task", "", "R", None),
            want_meta: "",
        },
        Case {
            name: "agy send_message: the recipient and the whole message",
            tool: "send_message",
            input: r#"{"Recipient":"main","Message":"done:\n3 files"}"#,
            response: "",
            want: ts("send", "", "→ main\ndone:\n3 files", None),
            want_meta: "",
        },
        Case {
            name: "agy manage_subagents and schedule are labeled",
            tool: "schedule",
            input: r#"{"Prompt":"check the build"}"#,
            response: "",
            want: ts("task", "", "timer: check the build", None),
            want_meta: "",
        },
        Case {
            name: "Workflow without a name is just labeled",
            tool: "Workflow",
            input: r#"{"script":"…"}"#,
            response: "",
            want: ts("task", "", "workflow", None),
            want_meta: "",
        },
        Case {
            name: "agy ask_question: its questions",
            tool: "ask_question",
            input: r#"{"questions":[{"question":"Which?","options":["a","b"]}]}"#,
            response: "",
            want: ts("ask", "", "Which?", None),
            want_meta: "",
        },
        Case {
            name: "unknown agy tool: agy's own keys",
            tool: "grep_search",
            input: r#"{"Query":"TODO","SearchPath":"/home/user/proj"}"#,
            response: "",
            want: ts("grep_search", "", "TODO", None),
            want_meta: "",
        },
        Case {
            name: "unknown tool: a path under cwd is relativized",
            tool: "mcp__fs__stat",
            input: r#"{"path":"/home/user/proj/go.mod"}"#,
            response: "",
            want: ts("mcp__fs__stat", "", "go.mod", None),
            want_meta: "",
        },
        Case {
            name: "a detail is whole, its quotes kept",
            tool: "search_web",
            input: r#"{"query":"\"exact phrase\""}"#,
            response: "",
            want: ts("search", "", r#""exact phrase""#, None),
            want_meta: "",
        },
        Case {
            name: "input that isn't an object",
            tool: "Bash",
            input: "not json",
            response: "",
            want: ts("bash", "", "", None),
            want_meta: "",
        },
    ];

    for tt in tests {
        let resp = if tt.response.is_empty() {
            None
        } else {
            Some(tt.response.as_bytes())
        };
        let mut got = summarize_tool(tt.tool, tt.input.as_bytes(), resp, CWD);
        let meta = got.meta.take();
        assert_eq!(got, tt.want, "{}: summary mismatch", tt.name);
        assert!(
            same_json(meta.as_ref(), tt.want_meta),
            "{}: meta = {:?}, want {}",
            tt.name,
            meta,
            tt.want_meta
        );
    }
}

#[test]
fn test_replaced_lines_too_large() {
    let big = "a\n".repeat(3000);
    let other = "b\n".repeat(3000);
    let mut budget = MAX_DIFF_CELLS;
    assert!(
        replaced_lines(&big, &other, &mut budget).is_none(),
        "replaced_lines compared 3000x3000 lines, want it to give up"
    );
    assert_eq!(
        replaced_lines(&format!("{big}x"), &format!("{big}y"), &mut budget),
        Some((1, 1))
    );

    let edit = serde_json::json!({
        "old_string": "a\n".repeat(500),
        "new_string": "b\n".repeat(500),
    });
    let edits: Vec<Value> = (0..20).map(|_| edit.clone()).collect();
    let input = serde_json::to_vec(&serde_json::json!({
        "file_path": "/p/m.go",
        "edits": edits,
    }))
    .unwrap();
    let s = summarize_tool("MultiEdit", &input, None, "");
    assert!(
        s.meta.is_none(),
        "20 large edits: meta = {:?}, want none",
        s.meta
    );

    let input1 = serde_json::to_vec(&serde_json::json!({
        "file_path": "/p/m.go",
        "edits": [edit],
    }))
    .unwrap();
    let s1 = summarize_tool("MultiEdit", &input1, None, "");
    assert!(
        same_json(s1.meta.as_ref(), r#"{"added":500,"removed":500}"#),
        "one large edit: meta = {:?}, want +500 -500",
        s1.meta
    );
}

#[test]
fn test_describe_tool() {
    let input = br#"{"file_path":"/home/user/proj/a.go","old_string":"a","new_string":"b"}"#;
    let got = describe_tool("Edit", input, "/home/user/proj");
    assert_eq!(got, ts("edit", "a.go", "", None));
}

#[test]
fn test_summarize_tool_file() {
    struct Case {
        name: &'static str,
        tool: &'static str,
        input: &'static str,
        response: &'static str,
        want: Option<FileText>,
    }
    let tests = [
        Case {
            name: "read: the lines it read, from where they start",
            tool: "Read",
            input: r#"{"file_path":"/p/a.go","offset":5,"limit":2}"#,
            response: r#"{"type":"text","file":{"filePath":"/p/a.go","content":"five\nsix\n","numLines":2,"startLine":5,"totalLines":9}}"#,
            want: Some(FileText {
                text: "five\nsix\n".to_string(),
                start: 5,
                total: 9,
            }),
        },
        Case {
            name: "read of an image: no text",
            tool: "Read",
            input: r#"{"file_path":"/p/a.png"}"#,
            response: r#"{"type":"image","file":{"base64":"iVBO","type":"image/png","originalSize":4}}"#,
            want: None,
        },
        Case {
            name: "edit: the file before it, with the replacement the result made",
            tool: "Edit",
            input: r#"{"file_path":"/p/a.go","old_string":"x","new_string":"input's"}"#,
            response: r#"{"oldString":"one","newString":"1","originalFile":"one two one\n","replaceAll":false}"#,
            want: Some(FileText {
                text: "1 two one\n".to_string(),
                start: 1,
                total: 1,
            }),
        },
        Case {
            name: "edit: replace_all replaces every match",
            tool: "Edit",
            input: r#"{"file_path":"/p/a.go","old_string":"one","new_string":"1","replace_all":true}"#,
            response: r#"{"oldString":"one","newString":"1","originalFile":"one two one\n","replaceAll":true}"#,
            want: Some(FileText {
                text: "1 two 1\n".to_string(),
                start: 1,
                total: 1,
            }),
        },
        Case {
            name: "multiedit: the edits in order over originalFileContents",
            tool: "MultiEdit",
            input: r#"{"file_path":"/p/a.go","edits":[{"old_string":"a","new_string":"b"},{"old_string":"b\n","new_string":"c\nd\n"}]}"#,
            response: r#"{"filePath":"/p/a.go","originalFileContents":"a\n"}"#,
            want: Some(FileText {
                text: "c\nd\n".to_string(),
                start: 1,
                total: 2,
            }),
        },
        Case {
            name: "edit: a result without the file before it gives none",
            tool: "Edit",
            input: r#"{"file_path":"/p/a.go","old_string":"one","new_string":"1"}"#,
            response: r#"{"oldString":"one","newString":"1","originalFile":null}"#,
            want: None,
        },
        Case {
            name: "edit: a replacement that doesn't match gives none",
            tool: "Edit",
            input: r#"{"file_path":"/p/a.go","old_string":"three","new_string":"3"}"#,
            response: r#"{"oldString":"three","newString":"3","originalFile":"one two\n"}"#,
            want: None,
        },
        Case {
            name: "write while running: none (the file isn't written yet)",
            tool: "Write",
            input: r#"{"file_path":"/p/a.go","content":"x"}"#,
            response: "",
            want: None,
        },
    ];
    for tt in tests {
        let resp = if tt.response.is_empty() {
            None
        } else {
            Some(tt.response.as_bytes())
        };
        let got = summarize_tool(tt.tool, tt.input.as_bytes(), resp, "/p").file;
        assert_eq!(got, tt.want, "{}", tt.name);
    }
}

#[test]
fn test_numbered_text() {
    let tests = [
        (
            "agy: header skipped, numbers off",
            "File Path: `file:///p/a.go`\nShowing lines 3 to 4\nThe following code...\n3: x := 1\n4: \nThe above content...",
            3usize,
            2usize,
            Some("x := 1\n\n"),
        ),
        (
            "kimi: right-aligned numbers",
            "     9\tfoo\n    10\t\tbar",
            9,
            2,
            Some("foo\n\tbar\n"),
        ),
        (
            "a line missing (truncated output)",
            "1: a\n3: c",
            1,
            3,
            None,
        ),
        ("fewer lines than read", "1: a\n2: b", 1, 3, None),
        ("nothing numbered", "no lines here", 1, 1, None),
    ];
    for (name, output, start, n, want) in tests {
        let re = if output.contains('\t') {
            &*KIMI_LINE
        } else {
            &*AGY_NUMBERED_LINE
        };
        let got = numbered_text(output, re, start, n);
        assert_eq!(got.as_deref(), want, "{name}");
    }
}
