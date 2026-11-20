use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use agent_base::gostd::filepath;
use chrono::{DateTime, TimeDelta, TimeZone, Utc};

use crate::claude::{
    CLAUDE_HEAD, CLAUDE_TAIL, CLAUDE_TAIL_CHUNK, CLAUDE_TITLE_SLACK, ReaderAt, each_line_backward,
    read_claude,
};
use crate::sessions::{Liveness, SubState};
use crate::{
    Agent, Dir, HARNESS_AGY, HARNESS_CLAUDE, HARNESS_KIMI, Live, NATIVE_GONE, NATIVE_WINS, Native,
    ORIGIN_CONFIG, ORIGIN_DEFAULT, Overrides, ParentSession, REPORT_DENIED, REPORT_STANDS, Report,
    STATUS_ENDED, Session, Workgroup, describe_dirs, probe, reconcile_native, resolve,
    resolve_dirs, scan,
};

pub(crate) fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .tempdir_in("/var/tmp")
        .expect("tempdir in /var/tmp")
}

pub(crate) fn write(path: &str, content: &str) {
    std::fs::create_dir_all(filepath::dir(path)).unwrap();
    std::fs::write(path, content).unwrap();
}

pub(crate) fn touch(path: &str, at: DateTime<Utc>) {
    let c_path = CString::new(path).unwrap();
    let ts = libc::timespec {
        tv_sec: at.timestamp(),
        tv_nsec: i64::from(at.timestamp_subsec_nanos()),
    };
    let times = [ts, ts];
    // SAFETY: `c_path` is a valid NUL-terminated path and `times` is a 2-element timespec array.
    let rc = unsafe { libc::utimensat(libc::AT_FDCWD, c_path.as_ptr(), times.as_ptr(), 0) };
    assert_eq!(rc, 0, "utimensat({path}) failed");
}

pub(crate) fn lines(ls: &[&str]) -> String {
    let mut out = ls.join("\n");
    out.push('\n');
    out
}

#[test]
fn test_resolve_dirs() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    for d in [".claude/projects", ".gemini/antigravity-cli/brain"] {
        std::fs::create_dir_all(filepath::join(&[home, d])).unwrap();
    }

    let got1 = resolve_dirs(&Overrides::default(), home, "");
    assert_eq!(
        got1,
        vec![
            Dir {
                harness: HARNESS_CLAUDE.to_string(),
                path: filepath::join(&[home, ".claude/projects"]),
                origin: ORIGIN_DEFAULT.to_string(),
            },
            Dir {
                harness: HARNESS_AGY.to_string(),
                path: filepath::join(&[home, ".gemini/antigravity-cli/brain"]),
                origin: ORIGIN_DEFAULT.to_string(),
            },
        ]
    );

    let got2 = resolve_dirs(
        &Overrides {
            agy: Some(String::new()),
            ..Overrides::default()
        },
        home,
        &filepath::join(&[home, "missing"]),
    );
    assert!(got2.is_empty());

    let got3 = resolve_dirs(
        &Overrides {
            claude: Some(String::new()),
            agy: Some("~/elsewhere".to_string()),
            kimi: Some("/k".to_string()),
        },
        home,
        "",
    );
    assert_eq!(
        got3,
        vec![
            Dir {
                harness: HARNESS_AGY.to_string(),
                path: filepath::join(&[home, "elsewhere"]),
                origin: ORIGIN_CONFIG.to_string(),
            },
            Dir {
                harness: HARNESS_KIMI.to_string(),
                path: "/k".to_string(),
                origin: ORIGIN_CONFIG.to_string(),
            },
        ]
    );

    assert!(resolve_dirs(&Overrides::default(), "", "").is_empty());

    let desc = describe_dirs(
        &[
            Dir {
                harness: HARNESS_CLAUDE.to_string(),
                path: filepath::join(&[home, ".claude/projects"]),
                origin: ORIGIN_DEFAULT.to_string(),
            },
            Dir {
                harness: HARNESS_KIMI.to_string(),
                path: filepath::join(&[home, "k"]),
                origin: ORIGIN_CONFIG.to_string(),
            },
        ],
        home,
    );
    assert_eq!(
        desc,
        "claude ~/.claude/projects (default); kimi ~/k (config, missing)"
    );
    assert!(describe_dirs(&[], home).contains("claude_projects_dir"));
}

#[test]
fn test_each_line_backward() {
    let data = format!("a\n{}\n\nccc\n{}\nlast", "b".repeat(40), "d".repeat(7)).into_bytes();
    let mut want: Vec<String> = Vec::new();
    let mut want_at: Vec<i64> = Vec::new();
    let mut off = 0i64;
    for l in data.split(|&b| b == b'\n') {
        if !l.is_empty() {
            want.push(String::from_utf8(l.to_vec()).unwrap());
            want_at.push(off);
        }
        off += l.len() as i64 + 1;
    }
    want.reverse();
    want_at.reverse();

    for chunk in [1i64, 3, 5, 16, 1000] {
        let mut got = Vec::new();
        let mut got_at = Vec::new();
        each_line_backward(
            data.as_slice(),
            data.len() as i64,
            chunk,
            1 << 20,
            |line, at| {
                got.push(String::from_utf8(line.to_vec()).unwrap());
                got_at.push(at);
                true
            },
        );
        assert_eq!(got, want, "chunk {chunk}");
        assert_eq!(got_at, want_at, "chunk {chunk} offsets");
    }

    let mut n = 0;
    each_line_backward(data.as_slice(), data.len() as i64, 4, 1 << 20, |_, _| {
        n += 1;
        n < 2
    });
    assert_eq!(n, 2);

    for chunk in [1i64, 3, 1000] {
        let mut got = Vec::new();
        each_line_backward(data.as_slice(), data.len() as i64, chunk, 15, |line, _| {
            got.push(String::from_utf8(line.to_vec()).unwrap());
            true
        });
        assert_eq!(got, vec!["last", "ddddddd"], "chunk {chunk}, limit 15");
    }
}

fn claude_fixture(home: &str) -> String {
    let projects = filepath::join(&[home, ".claude", "projects"]);
    let main = filepath::join(&[&projects, "-slug-not-the-project", "s-main.jsonl"]);
    write(
        &main,
        &lines(&[
            r#"{"type":"permission-mode","permissionMode":"default","sessionId":"s-main"}"#,
            r#"{"type":"user","sessionId":"s-main","cwd":"/home/user/src/shop","version":"2.1.300","gitBranch":"main","message":{"role":"user","content":"make the cart work"}}"#,
            r#"{"type":"ai-title","aiTitle":"Cart fixes","sessionId":"s-main"}"#,
            r#"{"type":"assistant","sessionId":"s-main","cwd":"/home/user/src/shop","version":"2.1.300","gitBranch":"main","message":{"role":"assistant","model":"claude-old","content":[]}}"#,
            r#"{"type":"custom-title","customTitle":"Cart (renamed)","sessionId":"s-main"}"#,
            r#"{"type":"ai-title","aiTitle":"Cart fixes, part two","sessionId":"s-main"}"#,
            r#"{"type":"custom-title","customTitle":"Cart (renamed again)","sessionId":"s-main"}"#,
            r#"{"type":"assistant","sessionId":"s-main","cwd":"/home/user/src/shop/wt","version":"2.1.310","gitBranch":"feature","message":{"role":"assistant","model":"claude-new","content":[]}}"#,
            r#"{"type":"assistant","sessionId":"s-main","cwd":"/home/user/src/shop","version":"2.1.310","gitBranch":"feature","message":{"role":"assistant","model":"<synthetic>","content":[]}}"#,
            r#"{"type":"continued-in","continuedInSessionId":"s-next","sessionId":"s-main","timestamp":"2026-10-20T10:00:00Z"}"#,
        ]),
    );
    let sub = filepath::join(&[
        &projects,
        "-slug-not-the-project",
        "s-main",
        "subagents",
        "agent-x1.jsonl",
    ]);
    write(
        &sub,
        &lines(&[
            r#"{"type":"user","sessionId":"s-main","agentId":"x1","isSidechain":true,"cwd":"/home/user/src/shop","message":{"role":"user","content":"look around"}}"#,
        ]),
    );
    write(
        &format!("{}.meta.json", sub.strip_suffix(".jsonl").unwrap()),
        r#"{"agentType":"Explore","description":"Look around the cart code","toolUseId":"toolu_01","finished":true}"#,
    );
    let sub2 = filepath::join(&[
        &projects,
        "-slug-not-the-project",
        "s-main",
        "subagents",
        "agent-x2.jsonl",
    ]);
    write(
        &sub2,
        &lines(&[
            r#"{"type":"user","sessionId":"s-main","agentId":"x2","isSidechain":true,"cwd":"/home/user/src/shop","message":{"role":"user","content":"keep going"}}"#,
        ]),
    );
    write(
        &format!("{}.meta.json", sub2.strip_suffix(".jsonl").unwrap()),
        r#"{"agentType":"general-purpose","description":"Keep going","toolUseId":"toolu_02","finished":false}"#,
    );
    write(
        &filepath::join(&[&projects, "-other", "s-ai.jsonl"]),
        &lines(&[
            r#"{"type":"user","sessionId":"s-ai","cwd":"/home/user/notes","message":{"role":"user","content":"hi"}}"#,
            r#"{"type":"ai-title","aiTitle":"First","sessionId":"s-ai"}"#,
            r#"{"type":"ai-title","aiTitle":"Second","sessionId":"s-ai"}"#,
        ]),
    );
    write(
        &filepath::join(&[&projects, "s-top.jsonl"]),
        &lines(&[
            r#"{"type":"user","sessionId":"s-top","cwd":"/home/user","message":{"role":"user","content":"x"}}"#,
        ]),
    );
    write(&filepath::join(&[&projects, "-other", "s-empty.jsonl"]), "");
    projects
}

fn by_id(sessions: Vec<Session>) -> HashMap<String, Session> {
    sessions
        .into_iter()
        .map(|s| (s.session_id.clone(), s))
        .collect()
}

#[test]
fn test_scan_claude() {
    let tmp = temp_dir();
    let projects = claude_fixture(tmp.path().to_str().unwrap());
    let got = by_id(scan(&[Dir {
        harness: HARNESS_CLAUDE.to_string(),
        path: projects,
        origin: ORIGIN_DEFAULT.to_string(),
    }]));
    assert!(!got.contains_key("s-empty") && got.len() == 5);
    let m = &got["s-main"];
    assert_eq!(m.harness, HARNESS_CLAUDE);
    assert_eq!(m.cwd, "/home/user/src/shop");
    assert_eq!(m.project, "shop");
    assert_eq!(m.title, "Cart (renamed again)");
    assert_eq!(m.model, "claude-new");
    assert_eq!(m.version, "2.1.310");
    assert_eq!(m.git_branch, "feature");
    assert_eq!(m.continued_in, "s-next");
    assert!(m.size > 0 && m.mtime.is_some() && m.parent_session.is_none() && m.workgroup.is_none());
    let s_ai = &got["s-ai"];
    assert_eq!(s_ai.title, "Second");
    assert_eq!(s_ai.project, "notes");
    let x1 = &got["agent-x1"];
    assert_eq!(
        x1.parent_session,
        Some(ParentSession {
            session_id: "s-main".to_string(),
            tool_id: "toolu_01".to_string(),
        })
    );
    assert_eq!(x1.title, "Look around the cart code");
    assert_eq!(x1.state, SubState::Finished);
    assert_eq!(got["agent-x2"].state, SubState::Running);
}

#[test]
fn test_scan_claude_workflows() {
    let tmp = temp_dir();
    let projects = filepath::join(&[tmp.path().to_str().unwrap(), "projects"]);
    let session = filepath::join(&[&projects, "-proj", "s1"]);
    write(
        &format!("{session}.jsonl"),
        &lines(&[
            r#"{"type":"user","sessionId":"s1","cwd":"/home/user/src/app","message":{"role":"user","content":"run the workflow"}}"#,
        ]),
    );
    let make_agent = |rel: &str, id: &str, text: &str| -> String {
        let path = filepath::join(&[&session, "subagents", rel, &format!("agent-{id}.jsonl")]);
        write(
            &path,
            &lines(&[&format!(
                r#"{{"type":"user","sessionId":"s1","agentId":"{id}","isSidechain":true,"cwd":"/home/user/src/app","message":{{"role":"user","content":"{text}"}}}}"#
            )]),
        );
        path
    };
    let w1 = make_agent("workflows/wf_1", "w1", "gate");
    write(
        &format!("{}.meta.json", w1.strip_suffix(".jsonl").unwrap()),
        r#"{"agentType":"workflow-subagent","description":"gate:apple","workflowPhase":"Gate"}"#,
    );
    make_agent("workflows/wf_2", "w2", "render");
    let top = make_agent("", "dup", "top");
    make_agent("workflows/wf_1", "dup", "run");
    write(
        &filepath::join(&[
            &session,
            "subagents",
            "workflows",
            "wf_1",
            "agent-empty.jsonl",
        ]),
        "",
    );
    write(
        &filepath::join(&[&session, "subagents", "workflows", "wf_1", "notes.txt"]),
        "not a transcript",
    );

    let sessions = scan(&[Dir {
        harness: HARNESS_CLAUDE.to_string(),
        path: projects,
        origin: ORIGIN_DEFAULT.to_string(),
    }]);
    assert_eq!(sessions.len(), 4);
    let got = by_id(sessions);
    assert_eq!(got.len(), 4);
    for id in ["agent-w1", "agent-w2", "agent-dup"] {
        assert_eq!(got[id].parent_session.as_ref().unwrap().session_id, "s1");
    }
    assert_eq!(got["agent-w1"].title, "gate:apple");
    assert_eq!(got["agent-w1"].cwd, "/home/user/src/app");
    assert_eq!(got["agent-dup"].path, top);

    let probed = probe(&w1).expect("probe w1");
    assert_eq!(probed.session_id, "agent-w1");
    assert_eq!(probed.parent_session.as_ref().unwrap().session_id, "s1");
    assert_eq!(probed.title, "gate:apple");
}

struct CountingReader<'a> {
    data: &'a [u8],
    n: Cell<i64>,
}

impl ReaderAt for CountingReader<'_> {
    fn read_at_some(&self, buf: &mut [u8], offset: u64) -> std::io::Result<usize> {
        let got = self.data.read_at_some(buf, offset)?;
        self.n.set(self.n.get() + got as i64);
        Ok(got)
    }
}

fn filler_line() -> String {
    format!(
        r#"{{"type":"attachment","sessionId":"s","attachment":{{"content":"{}"}}}}"#,
        "x".repeat(4000)
    )
}

fn fillers(n: usize) -> Vec<String> {
    let f = filler_line();
    (0..n).map(|_| f.clone()).collect()
}

fn read_counted(data: &str, subagent: bool) -> (agent_transcript::Session, i64) {
    let r = CountingReader {
        data: data.as_bytes(),
        n: Cell::new(0),
    };
    let s = read_claude(&r, data.len() as i64, subagent);
    (s, r.n.get())
}

#[test]
fn test_read_claude_bounded() {
    let head = r#"{"type":"user","sessionId":"s","cwd":"/home/user/big","message":{"role":"user","content":"go"}}"#;
    let meta = r#"{"type":"assistant","sessionId":"s","version":"2.1.310","gitBranch":"","message":{"role":"assistant","model":"m","content":[]}}"#;
    let limit = CLAUDE_HEAD + CLAUDE_TAIL + CLAUDE_TAIL_CHUNK;

    let mut no_title = vec![head.to_string()];
    no_title.extend(fillers(5000));
    no_title.push(meta.to_string());

    let mut sub = vec![head.to_string(), meta.to_string()];
    sub.extend(fillers(5000));
    sub.push(meta.to_string());

    let big_line = format!(
        "{head}\n{meta}\n{{\"type\":\"attachment\",\"x\":\"{}\"}}\n",
        "y".repeat(20 << 20)
    );

    for (name, data, is_sub) in [
        ("no title", format!("{}\n", no_title.join("\n")), false),
        ("subagent", format!("{}\n", sub.join("\n")), true),
        ("one big line", big_line, false),
    ] {
        let (s, n) = read_counted(&data, is_sub);
        assert!(
            n <= limit,
            "{name}: read {n} of {}, want <= {limit}",
            data.len()
        );
        assert_eq!(s.cwd, "/home/user/big", "{name}");
    }

    let mut titled = vec![
        head.to_string(),
        r#"{"type":"ai-title","aiTitle":"Old","sessionId":"s"}"#.to_string(),
    ];
    titled.extend(fillers(5000));
    titled.push(r#"{"type":"ai-title","aiTitle":"New","sessionId":"s"}"#.to_string());
    titled.push(meta.to_string());
    let (s, n) = read_counted(&format!("{}\n", titled.join("\n")), false);
    assert_eq!(s.title, "New");
    assert!(n <= CLAUDE_HEAD + CLAUDE_TITLE_SLACK + 2 * CLAUDE_TAIL_CHUNK);
}

#[test]
fn test_read_claude_title_window() {
    let user = r#"{"type":"user","sessionId":"s","cwd":"/home/user/big","message":{"role":"user","content":"go"}}"#;
    let mut ls = vec![
        user.to_string(),
        r#"{"type":"custom-title","customTitle":"Named by hand","sessionId":"s"}"#.to_string(),
    ];
    ls.extend(fillers(10));
    ls.push(r#"{"type":"ai-title","aiTitle":"Named by the model","sessionId":"s"}"#.to_string());
    ls.extend(fillers(100));
    let (s, _) = read_counted(&format!("{}\n", ls.join("\n")), false);
    assert_eq!(s.title, "Named by hand");

    let mut ls2 = vec![
        user.to_string(),
        r#"{"type":"ai-title","aiTitle":"Ancient","sessionId":"s"}"#.to_string(),
    ];
    ls2.extend(fillers(400));
    let (s2, _) = read_counted(&format!("{}\n", ls2.join("\n")), false);
    assert_eq!(s2.title, "");
}

fn walk_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for p in paths {
        if p.is_dir() {
            walk_files(&p, out);
        } else {
            out.push(p);
        }
    }
}

#[test]
fn test_probe_matches_corpus() {
    let corpus = filepath::join(&[
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "agent-transcript",
        "testdata",
        "corpus",
    ]);
    let mut all = Vec::new();
    walk_files(Path::new(&corpus), &mut all);
    let cases: Vec<PathBuf> = all
        .into_iter()
        .filter(|p| {
            p.to_str().is_some_and(|s| {
                s.ends_with(".events.jsonl") && !s.ends_with(".requests.events.jsonl")
            })
        })
        .collect();
    assert!(!cases.is_empty(), "no corpus cases at {corpus}");

    for c in cases {
        let data = std::fs::read(&c).unwrap();
        let nl = data.iter().position(|&b| b == b'\n').unwrap();
        let want: agent_transcript::Session = serde_json::from_slice(&data[..nl]).unwrap();
        let s = probe(&filepath::join(&[&corpus, &want.path])).expect("probe corpus transcript");
        let mut got = agent_transcript::Session {
            session_id: s.session_id.clone(),
            harness: s.harness.clone(),
            path: want.path.clone(),
            size: s.size,
            cwd: s.cwd.clone(),
            project: s.project.clone(),
            title: s.title.clone(),
            agent_name: s.agent_name.clone(),
            model: s.model.clone(),
            version: s.version.clone(),
            git_branch: s.git_branch.clone(),
            parent_session: s.parent_session.clone(),
            continued_in: s.continued_in.clone(),
            ..Default::default()
        };
        if s.harness == HARNESS_CLAUDE {
            if s.parent_session.is_none() {
                got.session_id = want.session_id.clone();
            } else if let Some(ref wp) = want.parent_session
                && let Some(ref mut gp) = got.parent_session
            {
                gp.session_id = wp.session_id.clone();
            }
        }
        if s.harness == HARNESS_AGY {
            got.cwd = want.cwd.clone();
            got.project = want.project.clone();
            got.model = want.model.clone();
            if let (Some(gp), Some(wp)) = (&mut got.parent_session, &want.parent_session) {
                gp.tool_id = wp.tool_id.clone();
            }
        }
        assert_eq!(got, want, "{}", want.path);
    }
}

#[test]
fn test_read_claude_no_git_branch() {
    let data = lines(&[
        r#"{"type":"user","sessionId":"s","cwd":"/home/user/plain","version":"2.1.300","gitBranch":"","message":{"role":"user","content":"go"}}"#,
        r#"{"type":"custom-title","customTitle":"First name","sessionId":"s"}"#,
        r#"{"type":"ai-title","aiTitle":"AI","sessionId":"s"}"#,
        r#"{"type":"custom-title","customTitle":"Second name","sessionId":"s"}"#,
        r#"{"type":"assistant","sessionId":"s","version":"2.1.310","gitBranch":"","message":{"role":"assistant","model":"m","content":[]}}"#,
    ]);
    let (s, _) = read_counted(&data, false);
    assert_eq!(s.title, "Second name");
    assert_eq!(s.version, "2.1.310");
    assert_eq!(s.git_branch, "");
    assert_eq!(s.model, "m");
}

#[test]
fn test_read_claude_api_request_keeps_branch() {
    let data = lines(&[
        r#"{"type":"user","sessionId":"s","cwd":"/home/user/repo","version":"2.1.310","gitBranch":"feature","message":{"role":"user","content":"go"}}"#,
        r#"{"type":"assistant","sessionId":"s","version":"2.1.310","gitBranch":"feature","message":{"role":"assistant","model":"m","content":[]}}"#,
        r#"{"type":"api-request-shape","sessionId":"s","version":"2.1.311","shapeHash":"h"}"#,
        r#"{"type":"api-request","sessionId":"s","version":"2.1.311","id":"q1","params":{"model":"m"},"tail":[]}"#,
    ]);
    let (s, _) = read_counted(&data, false);
    assert_eq!(s.version, "2.1.310");
    assert_eq!(s.git_branch, "feature");
}

fn agy_fixture(home: &str) -> (String, String) {
    let agy = filepath::join(&[home, ".gemini", "antigravity-cli"]);
    let brain = filepath::join(&[&agy, "brain"]);
    let step = r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-20T10:00:00Z","content":"hello"}"#;
    write(
        &filepath::join(&[
            &brain,
            "conv-a",
            ".system_generated",
            "logs",
            "transcript_full.jsonl",
        ]),
        &lines(&[step]),
    );
    write(
        &filepath::join(&[
            &brain,
            "conv-a",
            ".system_generated",
            "logs",
            "transcript.jsonl",
        ]),
        &lines(&[step]),
    );
    write(
        &filepath::join(&[
            &brain,
            "conv-a",
            ".system_generated",
            "subagents",
            "conv-sub.json",
        ]),
        r#"{"conversationId":"conv-sub","subagentDescriptor":{"typeName":"x","role":"Test Researcher"},"state":"SUBAGENT_STATE_ALIVE","spawnStepIndex":4,"workspaceUris":["file:///home/user/src/game"]}"#,
    );
    write(
        &filepath::join(&[
            &brain,
            "conv-sub",
            ".system_generated",
            "logs",
            "transcript_full.jsonl",
        ]),
        &lines(&[step]),
    );
    write(
        &filepath::join(&[
            &brain,
            "conv-old",
            ".system_generated",
            "logs",
            "transcript_full.jsonl",
        ]),
        &lines(&[step]),
    );
    std::fs::create_dir_all(filepath::join(&[&brain, "conv-nosteps"])).unwrap();
    write(
        &filepath::join(&[&agy, "history.jsonl"]),
        &lines(&[
            r#"{"display":"/model","timestamp":1,"workspace":"/home/user/src/game","type":"slash_command"}"#,
            r#"{"display":"hello","timestamp":2,"workspace":"/home/user/src/game","conversationId":"conv-a"}"#,
            r#"{"display":"later","timestamp":3,"workspace":"/home/user/src/elsewhere","conversationId":"conv-a"}"#,
            "not json",
        ]),
    );
    write(
        &filepath::join(&[&agy, "annotations", "conv-a.pbtxt"]),
        "title:\"Game \\\"engine\\\" review\"\n",
    );
    let lock = filepath::join(&[&agy, "presence", "conv-a.lock"]);
    write(&lock, "");
    write(&filepath::join(&[&agy, "presence", "conv-old.lock"]), "");
    (brain, lock)
}

fn hold_lock(path: &str) -> std::fs::File {
    let f = std::fs::File::open(path).unwrap();
    // SAFETY: `f` is a valid open file descriptor.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "flock LOCK_EX failed");
    f
}

#[test]
fn test_scan_agy() {
    let tmp = temp_dir();
    let (brain, lock) = agy_fixture(tmp.path().to_str().unwrap());
    let _held = hold_lock(&lock);
    let got = by_id(scan(&[Dir {
        harness: HARNESS_AGY.to_string(),
        path: brain,
        origin: ORIGIN_DEFAULT.to_string(),
    }]));
    assert_eq!(got.len(), 3);
    let a = &got["conv-a"];
    assert!(a.path.ends_with("transcript_full.jsonl"));
    assert_eq!(a.cwd, "/home/user/src/game");
    assert_eq!(a.project, "game");
    assert_eq!(a.title, r#"Game "engine" review"#);
    assert_eq!(a.live, Liveness::Yes);
    assert!(a.parent_session.is_none());

    let sub = &got["conv-sub"];
    assert_eq!(sub.parent_session.as_ref().unwrap().session_id, "conv-a");
    assert_eq!(sub.title, "Test Researcher");
    assert_eq!(sub.cwd, "/home/user/src/game");
    assert_eq!(sub.live, Liveness::Yes);

    let old = &got["conv-old"];
    assert_eq!(old.live, Liveness::No);
    assert_eq!(old.cwd, "");
}

#[test]
fn test_scan_kimi() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    let kimi = filepath::join(&[home, ".kimi-code"]);
    let sess = filepath::join(&[&kimi, "sessions", "wd_shop_abc123", "session_k-1"]);
    write(
        &filepath::join(&[&sess, "state.json"]),
        r#"{"title":"Kimi cart work","workDir":"/home/user/src/shop"}"#,
    );
    write(
        &filepath::join(&[&sess, "agents", "main", "wire.jsonl"]),
        &lines(&[
            r#"{"type":"metadata","protocol_version":"1.4","created_at":1}"#,
            r#"{"type":"llm.request","time":2,"provider":"kimi","model":"k-old"}"#,
            r#"{"type":"turn.prompt","input":[{"type":"text","text":"go"}]}"#,
            r#"{"type":"llm.request","time":3,"provider":"kimi","model":"k-new"}"#,
        ]),
    );
    write(
        &filepath::join(&[&sess, "agents", "agent-1", "wire.jsonl"]),
        &lines(&[r#"{"type":"turn.prompt","input":[{"type":"text","text":"sub"}]}"#]),
    );
    let other = filepath::join(&[&kimi, "sessions", "wd_notes_def456", "session_k-2"]);
    write(
        &filepath::join(&[&other, "agents", "main", "wire.jsonl"]),
        &lines(&[r#"{"type":"turn.prompt","input":[{"type":"text","text":"x"}]}"#]),
    );
    write(
        &filepath::join(&[&kimi, "session_index.jsonl"]),
        &lines(&[&format!(
            r#"{{"sessionId":"session_k-2","sessionDir":"{other}","workDir":"/home/user/notes"}}"#
        )]),
    );

    let got = by_id(scan(&[Dir {
        harness: HARNESS_KIMI.to_string(),
        path: filepath::join(&[&kimi, "sessions"]),
        origin: ORIGIN_DEFAULT.to_string(),
    }]));
    assert_eq!(got.len(), 3);
    let k1 = &got["k-1"];
    assert_eq!(k1.title, "Kimi cart work");
    assert_eq!(k1.project, "shop");
    assert_eq!(k1.model, "k-new");
    assert!(k1.parent_session.is_none());

    let k1_sub = &got["k-1:agent-1"];
    assert_eq!(k1_sub.parent_session.as_ref().unwrap().session_id, "k-1");
    assert_eq!(k1_sub.project, "shop");
    assert_eq!(got["k-2"].cwd, "/home/user/notes");
}

fn statuses(sessions: &[Session]) -> HashMap<String, String> {
    sessions
        .iter()
        .map(|s| (s.session_id.clone(), s.status.clone()))
        .collect()
}

#[test]
fn test_resolve_local() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    let projects = claude_fixture(home);
    let (brain, lock) = agy_fixture(home);
    let _held = hold_lock(&lock);
    let now = Utc::now();
    let k = filepath::join(&[home, ".kimi-code", "sessions", "wd_a_1"]);
    write(
        &filepath::join(&[&k, "session_fresh", "agents", "main", "wire.jsonl"]),
        &lines(&[r#"{"type":"turn.prompt"}"#]),
    );
    let stale_wire = filepath::join(&[&k, "session_stale", "agents", "main", "wire.jsonl"]);
    write(&stale_wire, &lines(&[r#"{"type":"turn.prompt"}"#]));
    touch(&stale_wire, now - TimeDelta::hours(1));

    let dirs = [
        Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_DEFAULT.to_string(),
        },
        Dir {
            harness: HARNESS_AGY.to_string(),
            path: brain,
            origin: ORIGIN_DEFAULT.to_string(),
        },
        Dir {
            harness: HARNESS_KIMI.to_string(),
            path: filepath::dir(&k),
            origin: ORIGIN_DEFAULT.to_string(),
        },
    ];

    let mut claude_live = HashMap::new();
    claude_live.insert("s-main".to_string(), "processing".to_string());
    let got = statuses(&resolve(
        scan(&dirs),
        Vec::new(),
        Live {
            claude: Some(claude_live),
            reports: HashMap::new(),
            now: Some(now),
        },
    ));
    for (id, want) in [
        ("s-main", "processing"),
        ("s-ai", "ended"),
        ("s-top", "ended"),
        ("agent-x1", "ended"),
        ("agent-x2", "unknown"),
        ("conv-a", "unknown"),
        ("conv-sub", "unknown"),
        ("conv-old", "ended"),
        ("fresh", "unknown"),
        ("stale", "ended"),
    ] {
        assert_eq!(got.get(id).map(String::as_str), Some(want), "{id}");
    }

    let got_no_reg = statuses(&resolve(
        scan(&dirs),
        Vec::new(),
        Live {
            claude: None,
            reports: HashMap::new(),
            now: Some(now),
        },
    ));
    assert_eq!(got_no_reg["s-main"], "unknown");
    assert_eq!(got_no_reg["s-ai"], "unknown");
    assert_eq!(got_no_reg["agent-x2"], "unknown");

    let got_empty_reg = statuses(&resolve(
        scan(&dirs),
        Vec::new(),
        Live {
            claude: Some(HashMap::new()),
            reports: HashMap::new(),
            now: Some(now),
        },
    ));
    assert_eq!(got_empty_reg["agent-x2"], "ended");
}

#[test]
fn test_resolve_workgroup() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    let projects = claude_fixture(home);
    let store = filepath::join(&[home, "bin", ".gomuxai"]);
    let outside = filepath::join(&[home, "elsewhere", "projects", "-x", "s-out.jsonl"]);
    write(
        &outside,
        &lines(&[
            r#"{"type":"user","sessionId":"s-out","cwd":"/home/user/x","message":{"role":"user","content":"x"}}"#,
        ]),
    );
    let old = filepath::join(&[&projects, "-other", "s-ai.jsonl"]);
    let current = filepath::join(&[&projects, "-slug-not-the-project", "s-main.jsonl"]);
    let top = filepath::join(&[&projects, "s-top.jsonl"]);
    let now = Utc.timestamp_opt(1_790_000_000, 0).unwrap();
    touch(&old, now - TimeDelta::hours(2));
    touch(&top, now - TimeDelta::minutes(30));
    touch(&current, now);
    touch(&outside, now);
    let ended = now - TimeDelta::minutes(10);

    let agents = vec![
        Agent {
            name: "lead".to_string(),
            store: store.clone(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            created_at: Some(now - TimeDelta::hours(3)),
            ended_at: Some(ended),
            status: "idle".to_string(),
            transcript: top.clone(),
            history: vec![old.clone(), top.clone(), current.clone()],
            ..Agent::default()
        },
        Agent {
            name: "lead".to_string(),
            store: store.clone(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now - TimeDelta::hours(1)),
            status: "waiting".to_string(),
            status_at: Some(now - TimeDelta::minutes(1)),
            session_id: "s-main".to_string(),
            transcript: current.clone(),
            history: vec![old, top, current],
            ..Agent::default()
        },
        Agent {
            name: "lead-t".to_string(),
            parent: "lead".to_string(),
            store: store.clone(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now),
            status: "idle".to_string(),
            transcript: outside,
            ..Agent::default()
        },
        Agent {
            name: "fresh".to_string(),
            parent: "lead".to_string(),
            store: store.clone(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now),
            session_id: "s-new".to_string(),
            ..Agent::default()
        },
    ];
    let dirs = [Dir {
        harness: HARNESS_CLAUDE.to_string(),
        path: projects,
        origin: ORIGIN_DEFAULT.to_string(),
    }];
    let mut claude_live = HashMap::new();
    claude_live.insert("s-main".to_string(), "waiting".to_string());
    let sessions = resolve(
        scan(&dirs),
        agents.clone(),
        Live {
            claude: Some(claude_live),
            reports: HashMap::new(),
            now: Some(now),
        },
    );
    let got = by_id(sessions);

    let m = &got["s-main"];
    assert_eq!(
        m.workgroup,
        Some(Workgroup {
            name: "lead".to_string(),
            parent: String::new(),
            store,
            host: "box".to_string(),
        })
    );
    assert_eq!(m.status, "waiting");
    assert_eq!(m.status_at, Some(now - TimeDelta::minutes(1)));

    let x2 = &got["agent-x2"];
    assert_eq!(x2.workgroup.as_ref().unwrap().name, "lead");
    assert_eq!(x2.status, "unknown");

    let s_top = &got["s-top"];
    assert_eq!(s_top.workgroup.as_ref().unwrap().name, "lead");
    assert_eq!(s_top.status, "ended");
    assert_eq!(s_top.status_at, Some(ended));

    let s_ai = &got["s-ai"];
    assert!(s_ai.workgroup.is_some());
    assert_eq!(s_ai.status, "ended");

    let s_out = &got["s-out"];
    assert_eq!(s_out.workgroup.as_ref().unwrap().name, "lead-t");
    assert_eq!(s_out.workgroup.as_ref().unwrap().parent, "lead");
    assert_eq!(s_out.project, "x");
    assert_eq!(s_out.status, "ended");

    let s_new = &got["s-new"];
    assert_eq!(s_new.workgroup.as_ref().unwrap().name, "fresh");
    assert_eq!(s_new.path, "");
    assert_eq!(s_new.harness, "claude");
    assert_eq!(s_new.status, "unknown");
    assert!(got["agent-x1"].workgroup.is_some());

    let mut claude_idle = HashMap::new();
    claude_idle.insert("s-main".to_string(), "idle".to_string());
    let sessions2 = resolve(
        scan(&dirs),
        agents.clone(),
        Live {
            claude: Some(claude_idle),
            reports: HashMap::new(),
            now: Some(now),
        },
    );
    assert_eq!(by_id(sessions2)["s-main"].status, "idle");

    let sessions3 = resolve(
        scan(&dirs),
        agents,
        Live {
            claude: None,
            reports: HashMap::new(),
            now: Some(now),
        },
    );
    for i in 1..sessions3.len() {
        if let (Some(cur), Some(prev)) = (sessions3[i].mtime, sessions3[i - 1].mtime) {
            assert!(cur <= prev, "not newest first at {i}");
        }
    }
    assert_eq!(by_id(sessions3)["s-out"].status, "idle");
}

#[test]
fn test_reconcile_native() {
    type DeniedFn = fn() -> bool;
    type Case = (bool, bool, &'static str, &'static str, DeniedFn, i32);
    let never: DeniedFn = || false;
    let always: DeniedFn = || true;
    let cases: &[Case] = &[
        (true, true, "idle", "processing", never, NATIVE_WINS),
        (true, true, "waiting", "idle", always, REPORT_DENIED),
        (true, true, "waiting", "idle", never, NATIVE_WINS),
        (true, true, "unknown", "idle", never, REPORT_STANDS),
        (true, false, "", "idle", never, NATIVE_GONE),
        (true, false, "", "ended", never, REPORT_STANDS),
        (true, false, "", "", never, REPORT_STANDS),
        (false, false, "", "idle", never, REPORT_STANDS),
    ];
    for &(queried, listed, status, report, denied, want) in cases {
        let n = Native {
            queried,
            listed,
            status: status.to_string(),
            pane_err: None,
            err: None,
        };
        assert_eq!(reconcile_native(&n, report, denied), want);
    }
}

#[test]
fn test_resolve_native_rule() {
    let tmp = temp_dir();
    let projects = claude_fixture(tmp.path().to_str().unwrap());
    let now = Utc::now();
    let current = filepath::join(&[&projects, "-slug-not-the-project", "s-main.jsonl"]);
    let mut agent = Agent {
        name: "lead".to_string(),
        store: "/s".to_string(),
        host: "box".to_string(),
        backend: "claude".to_string(),
        live: true,
        created_at: Some(now - TimeDelta::hours(1)),
        status: "idle".to_string(),
        event: "denied".to_string(),
        status_at: Some(now - TimeDelta::seconds(1)),
        session_id: "s-main".to_string(),
        transcript: current,
        ..Agent::default()
    };
    let check_status = |a: &Agent, native: &str| -> String {
        let mut claude_live = HashMap::new();
        claude_live.insert("s-main".to_string(), native.to_string());
        let got = by_id(resolve(
            scan(&[Dir {
                harness: HARNESS_CLAUDE.to_string(),
                path: projects.clone(),
                origin: ORIGIN_DEFAULT.to_string(),
            }]),
            vec![a.clone()],
            Live {
                claude: Some(claude_live),
                reports: HashMap::new(),
                now: Some(now),
            },
        ));
        got["s-main"].status.clone()
    };
    assert_eq!(check_status(&agent, "waiting"), "idle");
    agent.status_at = Some(now - TimeDelta::minutes(1));
    assert_eq!(check_status(&agent, "waiting"), "waiting");
    assert_eq!(check_status(&agent, "unknown"), "idle");
}

#[test]
fn test_resolve_history_outside_dirs() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    let now = Utc::now();
    let old = filepath::join(&[home, "elsewhere", "-x", "s-old.jsonl"]);
    write(
        &old,
        &lines(&[
            r#"{"type":"user","sessionId":"s-old","cwd":"/home/user/x","message":{"role":"user","content":"x"}}"#,
        ]),
    );
    touch(&old, now - TimeDelta::hours(2));
    let agents = vec![
        Agent {
            name: "lead".to_string(),
            parent: "first".to_string(),
            store: "/s".to_string(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            created_at: Some(now - TimeDelta::hours(3)),
            ended_at: Some(now - TimeDelta::minutes(90)),
            history: vec![old.clone()],
            ..Agent::default()
        },
        Agent {
            name: "lead".to_string(),
            parent: "second".to_string(),
            store: "/s".to_string(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now - TimeDelta::hours(1)),
            history: vec![old],
            ..Agent::default()
        },
    ];
    let got = by_id(resolve(
        Vec::new(),
        agents,
        Live {
            now: Some(now),
            ..Live::default()
        },
    ));
    let s = &got["s-old"];
    assert_eq!(s.workgroup.as_ref().unwrap().parent, "first");
    assert_eq!(s.status, "ended");
}

#[test]
fn test_resolve_symlinks() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    let real = claude_fixture(&filepath::join(&[home, "real"]));
    let link = filepath::join(&[home, "link"]);
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let store_link = filepath::join(&[home, "store-link"]);
    std::os::unix::fs::symlink(home, &store_link).unwrap();

    let now = Utc::now();
    let current = filepath::join(&[&real, "-slug-not-the-project", "s-main.jsonl"]);
    let agents = vec![
        Agent {
            name: "lead".to_string(),
            store: home.to_string(),
            host: "box".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now),
            status: "idle".to_string(),
            transcript: current.clone(),
            ..Agent::default()
        },
        Agent {
            name: "lead".to_string(),
            store: store_link,
            host: "box".to_string(),
            backend: "claude".to_string(),
            live: true,
            created_at: Some(now),
            status: "idle".to_string(),
            transcript: current,
            ..Agent::default()
        },
    ];
    let sessions = resolve(
        scan(&[Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: link,
            origin: ORIGIN_DEFAULT.to_string(),
        }]),
        agents,
        Live {
            now: Some(now),
            ..Live::default()
        },
    );
    let matches: Vec<&Session> = sessions
        .iter()
        .filter(|s| s.session_id == "s-main")
        .collect();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].workgroup.as_ref().unwrap().name, "lead");
}

#[test]
fn test_resolve_nested_subagents() {
    let now = Utc::now();
    let sessions = vec![
        Session {
            session_id: "c".to_string(),
            harness: HARNESS_AGY.to_string(),
            path: "/b/c".to_string(),
            mtime: Some(now),
            parent_session: Some(ParentSession {
                session_id: "b".to_string(),
                tool_id: String::new(),
            }),
            ..Session::default()
        },
        Session {
            session_id: "b".to_string(),
            harness: HARNESS_AGY.to_string(),
            path: "/b/b".to_string(),
            mtime: Some(now),
            parent_session: Some(ParentSession {
                session_id: "a".to_string(),
                tool_id: String::new(),
            }),
            ..Session::default()
        },
        Session {
            session_id: "a".to_string(),
            harness: HARNESS_AGY.to_string(),
            path: "/b/a".to_string(),
            mtime: Some(now),
            live: Liveness::No,
            ..Session::default()
        },
    ];
    let agents = vec![Agent {
        name: "lead".to_string(),
        store: "/s".to_string(),
        host: "box".to_string(),
        backend: "agy".to_string(),
        live: true,
        created_at: Some(now),
        status: "idle".to_string(),
        transcript: "/b/a".to_string(),
        ..Agent::default()
    }];
    let got = by_id(resolve(
        sessions,
        agents,
        Live {
            now: Some(now),
            ..Live::default()
        },
    ));
    for id in ["a", "b", "c"] {
        let s = &got[id];
        assert_eq!(s.workgroup.as_ref().unwrap().name, "lead", "{id}");
        assert_eq!(s.status, "ended", "{id}");
    }
}

#[test]
fn test_resolve_adopted_session_joins_by_id() {
    let tmp = temp_dir();
    let projects = claude_fixture(tmp.path().to_str().unwrap());
    let now = Utc::now();
    let agent = Agent {
        name: "lead".to_string(),
        store: "/s".to_string(),
        host: "box".to_string(),
        backend: "claude".to_string(),
        live: true,
        created_at: Some(now - TimeDelta::hours(1)),
        status: "idle".to_string(),
        event: "adopted".to_string(),
        status_at: Some(now),
        session_id: "s-main".to_string(),
        ..Agent::default()
    };
    let mut claude_live = HashMap::new();
    claude_live.insert("s-main".to_string(), "processing".to_string());
    let got = by_id(resolve(
        scan(&[Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_DEFAULT.to_string(),
        }]),
        vec![agent],
        Live {
            claude: Some(claude_live),
            reports: HashMap::new(),
            now: Some(now),
        },
    ));
    let s = &got["s-main"];
    assert_eq!(s.workgroup.as_ref().unwrap().name, "lead");
    assert_eq!(s.status, "processing");
}

#[test]
fn test_resolve_hook_reports() {
    let tmp = temp_dir();
    let projects = claude_fixture(tmp.path().to_str().unwrap());
    let now = Utc::now();
    let mut reports = HashMap::new();
    reports.insert(
        "s-main".to_string(),
        Report {
            status: STATUS_ENDED.to_string(),
            end_reason: "quit".to_string(),
            at: Some(now),
        },
    );
    let got = by_id(resolve(
        scan(&[Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_DEFAULT.to_string(),
        }]),
        Vec::new(),
        Live {
            claude: Some(HashMap::new()),
            reports,
            now: Some(now),
        },
    ));
    let s_main = &got["s-main"];
    assert_eq!(s_main.status, STATUS_ENDED);
    assert_eq!(s_main.end_reason, "quit");
    let s_top = &got["s-top"];
    assert_eq!(s_top.status, STATUS_ENDED);
    assert_eq!(s_top.end_reason, "gone");
}

/// The review's D1: a file or folder whose name isn't UTF-8 is listed, as
/// the Go lists it (its name as Go writes it, each bad byte a U+FFFD), and
/// read.
#[test]
fn test_scan_names_not_utf8() {
    use std::os::unix::ffi::OsStrExt;
    let tmp = temp_dir();
    let projects = tmp.path().join(".claude/projects");
    let record = |cwd: &str| {
        format!(
            "{{\"type\":\"user\",\"uuid\":\"n1\",\"sessionId\":\"x\",\"timestamp\":\"2026-10-20T09:00:00.000Z\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}},\"version\":\"2.1.0\",\"gitBranch\":\"main\"}}\n"
        )
    };
    let other = projects.join("-home-user-other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(
        other.join(std::ffi::OsStr::from_bytes(b"\xff\xfe.jsonl")),
        record("/home/user/other"),
    )
    .unwrap();
    let odd = projects.join(std::ffi::OsStr::from_bytes(b"-home-user-\xe2\x82"));
    std::fs::create_dir_all(odd.join("s-x/subagents")).unwrap();
    std::fs::write(odd.join("s-x.jsonl"), record("/home/user/odd")).unwrap();
    std::fs::write(
        odd.join("s-x/subagents/agent-a1.jsonl"),
        record("/home/user/odd"),
    )
    .unwrap();

    let got = by_id(scan(&[Dir {
        harness: HARNESS_CLAUDE.to_string(),
        path: projects.to_str().unwrap().to_string(),
        origin: ORIGIN_CONFIG.to_string(),
    }]));
    let r = char::REPLACEMENT_CHARACTER;
    let s = &got[&format!("{r}{r}")];
    assert_eq!(
        s.path,
        format!(
            "{}/-home-user-other/{r}{r}.jsonl",
            projects.to_str().unwrap()
        )
    );
    assert_eq!(
        (
            s.cwd.as_str(),
            s.project.as_str(),
            s.version.as_str(),
            s.git_branch.as_str()
        ),
        ("/home/user/other", "other", "2.1.0", "main")
    );
    let s = &got["s-x"];
    assert_eq!(
        s.path,
        format!("{}/-home-user-{r}{r}/s-x.jsonl", projects.to_str().unwrap())
    );
    assert_eq!(s.cwd, "/home/user/odd");
    let sub = &got["agent-a1"];
    assert_eq!(sub.cwd, "/home/user/odd");
    assert_eq!(sub.parent_session.as_ref().unwrap().session_id, "s-x");
}

/// The review's C3 and T1: a line's vertical tabs are trimmed as Go's
/// `bytes.TrimSpace` trims them, an index line a form feed ends doesn't
/// decode (JSON whitespace is space, tab, CR and LF), and a title with a
/// lone surrogate escape is read, the escape a U+FFFD.
#[test]
fn test_scan_whitespace_and_surrogates_as_go() {
    let tmp = temp_dir();
    let home = tmp.path().to_str().unwrap();
    let projects = filepath::join(&[home, ".claude/projects"]);
    write(
        &filepath::join(&[&projects, "-home-user-vt", "s-v.jsonl"]),
        "{\"type\":\"user\",\"uuid\":\"v1\",\"sessionId\":\"s-v\",\"cwd\":\"/home/user/vt\",\"version\":\"2.1.0\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n\
         \x0b{\"type\":\"custom-title\",\"customTitle\":\"VT\",\"sessionId\":\"s-v\"}\x0b\n",
    );
    write(
        &filepath::join(&[&projects, "-home-user-e", "s-e.jsonl"]),
        "{\"type\":\"user\",\"uuid\":\"e1\",\"sessionId\":\"s-e\",\"cwd\":\"/home/user/e\",\"version\":\"2.1.0\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n\
         {\"type\":\"custom-title\",\"customTitle\":\"Emoji \\ud83d\",\"sessionId\":\"s-e\"}\n",
    );
    let kimi = filepath::join(&[home, ".kimi-code"]);
    write(
        &filepath::join(&[
            &kimi,
            "sessions",
            "wd",
            "session_k1",
            "agents",
            "main",
            "wire.jsonl",
        ]),
        &lines(&[r#"{"type":"turn.prompt","input":[{"type":"text","text":"x"}]}"#]),
    );
    write(
        &filepath::join(&[&kimi, "session_index.jsonl"]),
        "{\"sessionId\":\"k1\",\"workDir\":\"/home/user/kimi1\"}\n{\"sessionId\":\"k1\",\"workDir\":\"/home/user/ff\"}\x0c\n",
    );
    let got = by_id(scan(&[
        Dir {
            harness: HARNESS_CLAUDE.to_string(),
            path: projects,
            origin: ORIGIN_CONFIG.to_string(),
        },
        Dir {
            harness: HARNESS_KIMI.to_string(),
            path: filepath::join(&[&kimi, "sessions"]),
            origin: ORIGIN_CONFIG.to_string(),
        },
    ]));
    assert_eq!(got["s-v"].title, "VT");
    assert_eq!(
        got["s-e"].title,
        format!("Emoji {}", char::REPLACEMENT_CHARACTER)
    );
    assert_eq!(got["k1"].cwd, "/home/user/kimi1");
}

#[test]
fn test_session_json_format() {
    let s = Session {
        session_id: "s1".to_string(),
        harness: HARNESS_CLAUDE.to_string(),
        path: "/p/s1.jsonl".to_string(),
        size: 123,
        mtime: Some(
            Utc.with_ymd_and_hms(2026, 11, 8, 12, 20, 0).unwrap() + TimeDelta::milliseconds(500),
        ),
        cwd: "/p".to_string(),
        project: "p".to_string(),
        workgroup: None,
        status: STATUS_ENDED.to_string(),
        end_reason: "gone".to_string(),
        ..Session::default()
    };
    let json = serde_json::to_string(&s).unwrap();
    assert_eq!(
        json,
        r#"{"session_id":"s1","harness":"claude","path":"/p/s1.jsonl","size":123,"mtime":"2026-11-08T12:20:00.5Z","cwd":"/p","project":"p","workgroup":null,"status":"ended","end_reason":"gone"}"#
    );
}
