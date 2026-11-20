//! Go: internal/cli/sessions_test.go (TestResolveTranscript_SubagentByID is
//! in transcript_test.rs, beside resolve_transcript's other tests).
use std::time::{Duration, SystemTime};

use agent_store::{Agent, Time};

use super::*;
use crate::cli::logs::logs_test::{hook_transcript, run_words, write_test_file};
use crate::cli::testenv::TestEnv;

/// Sets path's modification time to ago before now.
fn set_mtime(path: &str, ago: Duration) {
    let at = SystemTime::now() - ago;
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_times(std::fs::FileTimes::new().set_modified(at).set_accessed(at))
        .unwrap();
}

/// setup_cli with a temp home holding two synthetic Claude sessions, one of
/// them the worker agent's transcript, and claude agents answering for both
/// (Go: setupSessions). It returns the env and the home.
pub(crate) fn setup_sessions() -> (TestEnv, String) {
    let mut env = TestEnv::setup_cli_private();
    env.forbid_bus();
    std::fs::create_dir_all(&env.cfg.log_dir).unwrap();
    let home = env.dir.path().join("home").to_string_lossy().into_owned();
    std::fs::create_dir_all(&home).unwrap();
    env.set_env("HOME", &home);
    env.set_env("CLAUDE_CONFIG_DIR", "");
    let projects = format!("{home}/.claude/projects/-home-user-shop");
    std::fs::create_dir_all(&projects).unwrap();
    let local = format!("{projects}/sess-l.jsonl");
    write_test_file(
        &local,
        "{\"type\":\"user\",\"sessionId\":\"sess-l\",\"cwd\":\"/home/user/shop\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n\
         {\"type\":\"ai-title\",\"aiTitle\":\"A person's own session\",\"sessionId\":\"sess-l\"}\n",
    );
    let worker = format!("{projects}/sess-w.jsonl");
    write_test_file(
        &worker,
        "{\"type\":\"user\",\"sessionId\":\"sess-w\",\"cwd\":\"/home/user/shop\",\"message\":{\"role\":\"user\",\"content\":\"go\"}}\n\
         {\"type\":\"custom-title\",\"customTitle\":\"Worker's task\",\"sessionId\":\"sess-w\"}\n",
    );
    set_mtime(&local, Duration::from_secs(3600));
    hook_transcript(&env, &worker);
    set_claude_sessions(
        &env.cfg.database_path,
        Arc::new(|| {
            Ok(HashMap::from([
                ("sess-w".to_string(), Status::Waiting),
                ("sess-l".to_string(), Status::Idle),
            ]))
        }),
    );
    (env, home)
}

/// Writes Claude session id, run in cwd, last written ago before now (Go:
/// claudeTranscript, in spawn_continue_test.go).
pub(crate) fn claude_transcript(home: &str, id: &str, cwd: &str, title: &str, ago: Duration) -> String {
    let dir = format!("{home}/.claude/projects/{}", cwd.replace('/', "-"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/{id}.jsonl");
    let mut body = format!(
        "{{\"type\":\"user\",\"sessionId\":\"{id}\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"hi\"}}}}\n"
    );
    if !title.is_empty() {
        body += &format!("{{\"type\":\"custom-title\",\"customTitle\":\"{title}\",\"sessionId\":\"{id}\"}}\n");
    }
    write_test_file(&path, &body);
    set_mtime(&path, ago);
    path
}

/// Writes a subagent of the Claude session at parent_path (Go:
/// claudeSubagent).
pub(crate) fn claude_subagent(parent_path: &str, cwd: &str, ago: Duration) {
    let dir = format!("{}/subagents", parent_path.trim_end_matches(".jsonl"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/agent-x1.jsonl");
    write_test_file(
        &path,
        &format!(
            "{{\"type\":\"user\",\"isSidechain\":true,\"sessionId\":\"x1\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"content\":\"sub\"}}}}\n"
        ),
    );
    set_mtime(&path, ago);
}

/// Runs agent sessions with extra words, and decodes its --json.
fn run_sessions_json(env: &mut TestEnv, words: &[&str]) -> Vec<Session> {
    let mut line = vec!["sessions", "--json"];
    line.extend_from_slice(words);
    let (res, out, _) = run_words(env, &line);
    res.unwrap_or_else(|e| panic!("{line:?}: {e:#}"));
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("decoding {out:?}: {e}"))
}

fn session_ids(sessions: &[Session]) -> Vec<String> {
    sessions.iter().map(|s| s.session_id.clone()).collect()
}

#[test]
fn test_sessions() {
    crate::require_termd!();
    let (mut env, _home) = setup_sessions();
    let got = run_sessions_json(&mut env, &[]);
    assert_eq!(got.len(), 2, "sessions = {got:?}, want the worker's and the person's");
    let (w, l) = (&got[0], &got[1]);
    let g = w.workgroup.as_ref();
    assert!(
        w.session_id == "sess-w"
            && g.is_some_and(|g| g.name == "worker"
                && g.store == filepath::dir(&env.cfg.database_path)
                && g.host == "local")
            && w.status == "waiting"
            && w.title == "Worker's task"
            && w.project == "shop",
        "worker's session = {w:?}"
    );
    assert!(
        l.session_id == "sess-l" && l.workgroup.is_none() && l.status == "idle" && l.title == "A person's own session",
        "person's session = {l:?}"
    );

    assert_eq!(session_ids(&run_sessions_json(&mut env, &["--local"])), ["sess-l"], "--local");
    assert_eq!(session_ids(&run_sessions_json(&mut env, &["--workgroup"])), ["sess-w"], "--workgroup");
    assert!(run_sessions_json(&mut env, &["--harness", "agy"]).is_empty(), "--harness agy");
}

#[test]
fn test_sessions_human() {
    crate::require_termd!();
    let (mut env, _home) = setup_sessions();
    let (res, out, _) = run_words(&mut env, &["sessions"]);
    res.unwrap();
    for want in [
        "Looked in: claude ~/.claude/projects (default)\n",
        "claude   shop     waiting  worker     Worker's task\n",
        "claude   shop     idle     -          A person's own session\n",
        "2 sessions, 1 in a workgroup.\n",
    ] {
        assert!(out.contains(want), "output lacks {want:?}:\n{out}");
    }
}

#[test]
fn test_sessions_errors() {
    crate::require_termd!();
    let (mut env, _home) = setup_sessions();
    let (res, _, _) = run_words(&mut env, &["sessions", "--workgroup", "--local"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("use one"), "--workgroup --local: {err}");
    let (res, _, _) = run_words(&mut env, &["sessions", "--harness", "codex"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("claude, agy, kimi"), "--harness codex: {err}");
}

/// A live agent whose backend keeps no transcript is listed under its
/// backend's name, and --harness takes that name.
#[test]
fn test_sessions_backend_without_transcript() {
    crate::require_termd!();
    let (mut env, _home) = setup_sessions();
    env.store()
        .create_agent(&Agent {
            name: "aid".into(),
            session_name: "rpc-cli-aid".into(),
            backend: "aider".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    let got = run_sessions_json(&mut env, &["--harness", "aider"]);
    assert!(
        got.len() == 1
            && got[0].harness == "aider"
            && got[0].path.is_empty()
            && got[0].workgroup.as_ref().is_some_and(|w| w.name == "aid"),
        "--harness aider = {got:?}, want aid with no transcript"
    );
}

/// --dirs prints where sessions looks: as the table's opening line, or each
/// dir as JSON with whether it exists.
#[test]
fn test_sessions_dirs() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    let missing = format!("{home}/no-such-kimi");
    let mut cfg = env.cfg.clone();
    cfg.kimi_sessions_dir = Some(missing.clone());
    env.ctx.base_cfg = Some(cfg);
    let (res, out, _) = run_words(&mut env, &["sessions", "--dirs"]);
    res.unwrap();
    assert!(
        out.starts_with("Looked in: claude ~/.claude/projects (default)") && out.contains("(config, missing)"),
        "--dirs = {out:?}"
    );
    let (res, out, _) = run_words(&mut env, &["sessions", "--dirs", "--json"]);
    res.unwrap();
    let got: Vec<SessionDir> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("decoding {out:?}: {e}"));
    let want = vec![
        SessionDir {
            dir: Dir {
                harness: "claude".into(),
                path: format!("{home}/.claude/projects"),
                origin: "default".into(),
            },
            exists: true,
        },
        SessionDir {
            dir: Dir {
                harness: "kimi".into(),
                path: missing,
                origin: "config".into(),
            },
            exists: false,
        },
    ];
    assert_eq!(got, want, "--dirs --json");
    assert!(
        out.contains("\"harness\": \"claude\",\n    \"path\""),
        "--dirs --json keeps Go's field order:\n{out}"
    );
}

/// Adds n Claude sessions run in cwd to setup_sessions' home, sess-000 the
/// newest, all older than sess-w and sess-l (Go: manySessions).
fn many_sessions(home: &str, cwd: &str, n: u64) {
    for i in 0..n {
        claude_transcript(home, &format!("sess-{i:03}"), cwd, "", Duration::from_secs(2 * 3600 + i * 60));
    }
}

/// The table shows the newest 25 and a hint says how many are left out; --all
/// and --limit change that; --json prints every one unless --limit says
/// otherwise.
#[test]
fn test_sessions_default_limit() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    many_sessions(&home, "/home/user/shop", 30);

    let (res, out, hints) = run_words(&mut env, &["sessions"]);
    res.unwrap();
    assert!(
        out.contains("25 sessions, 1 in a workgroup.") && !out.contains("sess-023") && out.contains("(sess-022)"),
        "default table, want sess-w, sess-l and sess-000..022:\n{out}"
    );
    assert_eq!(
        hints,
        "hint: showing the newest 25 sessions and their subagents; 7 older left out: --all lists every one, --limit N the newest N\n"
    );

    let (res, out, hints) = run_words(&mut env, &["sessions", "--all"]);
    res.unwrap();
    assert!(
        out.contains("32 sessions, 1 in a workgroup.") && hints.is_empty(),
        "--all: hints {hints:?}, table:\n{out}"
    );

    let (res, out, hints) = run_words(&mut env, &["sessions", "--json"]);
    res.unwrap();
    let got: Vec<Session> = serde_json::from_str(&out).unwrap();
    assert!(got.len() == 32 && hints.is_empty(), "--json printed {} sessions, hints {hints:?}", got.len());
    let (res, out, hints) = run_words(&mut env, &["sessions", "--json", "--limit", "3"]);
    res.unwrap();
    let got: Vec<Session> = serde_json::from_str(&out).unwrap();
    assert_eq!(session_ids(&got), ["sess-w", "sess-l", "sess-000"], "--json --limit 3");
    assert!(hints.contains("29 older left out"), "--json --limit 3: hint {hints:?}");
}

/// --since, --project and the limit combine with each other and with the
/// filters there were, a subagent going with its session.
#[test]
fn test_sessions_filters() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    let notes = claude_transcript(&home, "notes", "/home/user/notes", "Notes", Duration::from_secs(3 * 3600));
    claude_subagent(&notes, "/home/user/notes", Duration::from_secs(3 * 3600 + 60));
    claude_transcript(&home, "deep", "/home/user/shop/web", "", Duration::from_secs(4 * 3600));

    let date = (Local::now() - chrono::Duration::minutes(200))
        .format("%Y-%m-%dT%H:%M:%S")
        .to_string();
    let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        ("since 2h", vec!["--since", "2h"], vec!["sess-w", "sess-l"]),
        ("since 30m", vec!["--since", "30m"], vec!["sess-w"]),
        ("since a date", vec!["--since", &date], vec!["sess-w", "sess-l", "notes", "agent-x1"]),
        ("project name", vec!["--project", "NOT"], vec!["notes", "agent-x1"]),
        ("project dir", vec!["--project", "/home/user/shop"], vec!["sess-w", "sess-l", "deep"]),
        (
            "project dir, local",
            vec!["--project", "/home/user/shop", "--local"],
            vec!["sess-l", "deep"],
        ),
        (
            "project, since",
            vec!["--project", "/home/user/shop/", "--since", "2h"],
            vec!["sess-w", "sess-l"],
        ),
        (
            "limit with a subagent",
            vec!["--since", "1d", "--limit", "3"],
            vec!["sess-w", "sess-l", "notes", "agent-x1"],
        ),
        ("workgroup, project", vec!["--workgroup", "--project", "notes"], vec![]),
    ];
    for (name, words, want) in cases {
        assert_eq!(session_ids(&run_sessions_json(&mut env, &words)), want, "{name}");
    }
}

/// A --project with a slash, or . or ~, is a directory, made absolute.
#[test]
fn test_sessions_project_dir() {
    crate::require_termd!();
    let (env, home) = setup_sessions();
    let wd = std::env::current_dir().unwrap().to_string_lossy().into_owned();
    for (arg, want) in [
        (".", wd.clone()),
        ("~", home.clone()),
        ("~/src/app", format!("{home}/src/app")),
        ("sub/dir", format!("{wd}/sub/dir")),
        ("/abs/path/", "/abs/path".to_string()),
        ("gomuxai", "gomuxai".to_string()),
        ("My Project 2", "My Project 2".to_string()),
    ] {
        let args = Args {
            all: false,
            dirs: false,
            harness: String::new(),
            json: false,
            limit: 0,
            local: false,
            project: arg.to_string(),
            since: String::new(),
            workgroup: false,
            args: Vec::new(),
        };
        let q = sessions_query(&env.ctx, &args, Local::now());
        assert!(
            q.as_ref().is_ok_and(|q| q.project == want),
            "--project {arg} = {:?}, want {want:?}",
            q.map(|q| q.project)
        );
    }
}

#[test]
fn test_sessions_limit_errors() {
    crate::require_termd!();
    let (mut env, _home) = setup_sessions();
    for (words, want) in [
        (&["sessions", "--all", "--limit", "5"][..], "--all and --limit don't go together"),
        (&["sessions", "--limit", "0"], "give 1 or more"),
        (&["sessions", "--since", "yesterday"], "--since yesterday: give a duration"),
    ] {
        let (res, _, _) = run_words(&mut env, words);
        let err = res.err().map(|e| format!("{e:#}"));
        assert!(err.as_deref().is_some_and(|e| e.contains(want)), "{words:?}: {err:?}");
    }
}

#[test]
fn test_parse_since() {
    crate::cli::testenv::isolate(); // TZ=UTC
    let now = Local.with_ymd_and_hms(2026, 11, 8, 15, 0, 0).unwrap();
    let local = |y, mo, d, h, mi, s| Local.with_ymd_and_hms(y, mo, d, h, mi, s).unwrap().with_timezone(&Utc);
    let nowu = now.with_timezone(&Utc);
    for (input, want) in [
        ("90m", nowu - chrono::Duration::minutes(90)),
        ("2h", nowu - chrono::Duration::hours(2)),
        ("1h30m", nowu - chrono::Duration::minutes(90)),
        ("3d", add_days(now, -3).with_timezone(&Utc)),
        ("1w", add_days(now, -7).with_timezone(&Utc)),
        ("2026-11-01", local(2026, 11, 1, 0, 0, 0)),
        ("2026-11-01 13:30", local(2026, 11, 1, 13, 30, 0)),
        ("2026-11-01T13:30:05", local(2026, 11, 1, 13, 30, 5)),
        ("2026-11-01T13:30:00Z", Utc.with_ymd_and_hms(2026, 11, 1, 13, 30, 0).unwrap()),
    ] {
        let got = parse_since(input, now);
        assert!(
            got.as_ref().is_ok_and(|g| *g == want),
            "parse_since({input:?}) = {got:?}, want {want}"
        );
    }
    for input in ["", "d", "3x", "-2h", "3dw", "11/01/2026"] {
        assert!(parse_since(input, now).is_err(), "parse_since({input:?}) took it");
    }
}

/// The table, as Go's tabwriter lays it out.
#[test]
fn test_tab_align() {
    let rows: Vec<Vec<String>> = [["WHEN", "H", "TITLE"], ["2026-10-25 13:35", "claude", "Fix it"]]
        .iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect();
    assert_eq!(
        tab_align(&rows, 2),
        "WHEN              H       TITLE\n2026-10-25 13:35  claude  Fix it\n"
    );
}
