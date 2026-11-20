//! Go: internal/cli/spawn_continue_test.go.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use agent_base::types::Status;

use super::sessions::set_claude_sessions;
use super::spawn::{self, continue_choice, continue_session};
use super::spawn_test::setup_spawn_cli;
use super::testenv::{TestEnv, parse_args, temp_dir};

/// What setup_continue makes: the env, a temp home, and two project dirs.
pub struct Continue {
    pub env: TestEnv,
    pub home: String,
    pub a: String,
    pub b: String,
    _dirs: Vec<tempfile::TempDir>,
}

/// setup_cli with a temp home and two project dirs, a and b, for synthetic
/// Claude sessions, and claude agents answering with running (None: none
/// run) (Go: setupContinue).
pub fn setup_continue(running: Option<HashMap<String, Status>>) -> Continue {
    let mut env = TestEnv::setup_cli_private();
    env.forbid_bus();
    env.set_env("AGENT_NAME", "");
    let home = temp_dir("agent-cli-home-");
    env.set_env("HOME", home.path());
    env.set_env("CLAUDE_CONFIG_DIR", "");
    let root = temp_dir("agent-cli-proj-");
    let a = root.path().join("a");
    let b = root.path().join("b");
    for d in [&a, &b] {
        std::fs::create_dir_all(d).unwrap();
    }
    let running = running.unwrap_or_default();
    set_claude_sessions(
        &env.cfg.database_path,
        Arc::new(move || Ok(running.clone())),
    );
    Continue {
        env,
        home: home.path().to_string_lossy().into_owned(),
        a: a.to_string_lossy().into_owned(),
        b: b.to_string_lossy().into_owned(),
        _dirs: vec![home, root],
    }
}

/// Sets a file's modification time to ago before now.
fn age_file(path: &str, ago: Duration) {
    let at = SystemTime::now() - ago;
    let f = std::fs::File::options().write(true).open(path).unwrap();
    f.set_modified(at).unwrap();
}

/// Writes Claude session id, run in cwd, last written ago before now (Go:
/// claudeTranscript).
pub fn claude_transcript(home: &str, id: &str, cwd: &str, title: &str, ago: Duration) -> String {
    let dir = format!("{home}/.claude/projects/{}", cwd.replace('/', "-"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/{id}.jsonl");
    let mut body = format!(
        r#"{{"type":"user","sessionId":"{id}","cwd":"{cwd}","message":{{"role":"user","content":"hi"}}}}"#
    ) + "\n";
    if !title.is_empty() {
        body += &format!(r#"{{"type":"custom-title","customTitle":"{title}","sessionId":"{id}"}}"#);
        body += "\n";
    }
    std::fs::write(&path, body).unwrap();
    age_file(&path, ago);
    path
}

/// A subagent of the session at parent_path (Go: claudeSubagent).
pub fn claude_subagent(parent_path: &str, cwd: &str, ago: Duration) {
    let dir = format!("{}/subagents", parent_path.trim_end_matches(".jsonl"));
    std::fs::create_dir_all(&dir).unwrap();
    let path = format!("{dir}/agent-x1.jsonl");
    std::fs::write(
        &path,
        format!(
            r#"{{"type":"user","isSidechain":true,"sessionId":"x1","cwd":"{cwd}","message":{{"role":"user","content":"sub"}}}}"#
        ) + "\n",
    )
    .unwrap();
    age_file(&path, ago);
}

const HOUR: Duration = Duration::from_secs(3600);

#[test]
fn test_spawn_continue_picks_the_most_recent_here() {
    crate::require_termd!();
    let c = setup_continue(None);
    claude_transcript(&c.home, "a-old", &c.a, "", 3 * HOUR);
    let newest = claude_transcript(&c.home, "a-new", &c.a, "Fix the parser", 2 * HOUR);
    claude_subagent(&newest, &c.a, Duration::from_secs(60)); // newer, but a subagent's
    claude_transcript(&c.home, "b-only", &c.b, "", HOUR);

    let s = continue_session(&c.env.ctx, &c.a, &["claude"]).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(s.session_id, "a-new");
    let choice = continue_choice(&s, &c.a);
    for want in [
        r#"Continuing claude session a-new ("Fix the parser"), last active "#,
        " (2h ago)",
        &format!(": the most recent in {}.", c.a),
    ] {
        assert!(choice.contains(want), "choice {choice:?} lacks {want:?}");
    }
    let s = continue_session(&c.env.ctx, &c.b, &["claude"]).unwrap();
    assert_eq!(s.session_id, "b-only");
    let empty = temp_dir("agent-cli-empty-");
    let e = empty.path().to_string_lossy().into_owned();
    let err = continue_session(&c.env.ctx, &e, &["claude"]).expect_err("nothing there");
    assert!(
        format!("{err:#}").contains(&format!("no claude session to continue in {e}")),
        "an empty dir: err = {err:#}"
    );
}

// A session still running is refused as --resume refuses it, before
// anything is spawned.
#[test]
fn test_spawn_continue_running_is_refused() {
    crate::require_termd!();
    let mut c = setup_continue(Some(HashMap::from([("a-new".to_string(), Status::Idle)])));
    claude_transcript(&c.home, "a-old", &c.a, "", 3 * HOUR);
    claude_transcript(&c.home, "a-new", &c.a, "", HOUR);
    c.env.change_cfg(|cfg| cfg.backend = "claude".into());
    // Go: t.Chdir(a) and --continue; --cwd a picks the same way.
    let args: spawn::Args = parse_args("spawn", &["t1", "--continue", "--cwd", &c.a]);
    let err = format!("{:#}", spawn::run(&c.env.ctx, &args).expect_err("refused"));
    assert!(
        err.contains("--continue picks a-new")
            && err.contains("is idle, not ended: quit the CLI running it first"),
        "err = {err}; want a-new refused as running"
    );
    assert!(
        c.env.store().get_agent("t1").unwrap().is_none(),
        "spawned after a refusal"
    );
}

// Review 5, finding 6: --resume into a directory that exists but can't be
// entered (mode 0600) fails as Go's chdir does, before termd or the agent's
// files are touched (it used to reach termd's create_session).
#[test]
fn test_spawn_resume_into_a_directory_it_cant_enter() {
    use std::os::unix::fs::PermissionsExt;
    crate::require_termd!();
    let mut c = setup_continue(None);
    claude_transcript(&c.home, "a-gone", &c.a, "", HOUR);
    c.env.change_cfg(|cfg| cfg.backend = "claude".into());
    std::fs::set_permissions(&c.a, std::fs::Permissions::from_mode(0o600)).unwrap();
    let args: spawn::Args = parse_args("spawn", &["wres", "--resume", "a-gone"]);
    let res = spawn::run(&c.env.ctx, &args);
    std::fs::set_permissions(&c.a, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = format!("{:#}", res.expect_err("unsearchable"));
    assert_eq!(
        err,
        format!(
            "entering {a}, where session a-gone ran: chdir {a}: permission denied",
            a = c.a
        )
    );
    assert!(
        c.env.store().get_agent("wres").unwrap().is_none(),
        "wres was recorded"
    );
    let dir = c.env.cfg.agent_dir("wres");
    assert!(
        !std::path::Path::new(&dir).exists(),
        "wres's files were written: {dir}"
    );
}

#[test]
fn test_spawn_continue_flag_errors() {
    crate::require_termd!();
    let mut env = setup_spawn_cli();
    env.forbid_bus();
    let args: spawn::Args = parse_args("spawn", &["t1", "--continue", "--resume", "3f34adff"]);
    let err = format!("{:#}", spawn::run(&env.ctx, &args).expect_err("refused"));
    assert!(
        err.contains("--continue and --resume don't go together"),
        "--continue --resume: err = {err}"
    );
    for b in ["catty", "shell", "aider"] {
        env.change_cfg(|c| c.backend = b.into());
        let args: spawn::Args = parse_args("spawn", &["t1", "--continue"]);
        let err = format!("{:#}", spawn::run(&env.ctx, &args).expect_err("refused"));
        assert!(
            err.contains(&format!("backend {b} keeps no sessions to continue")),
            "--continue --backend {b}: err = {err}"
        );
    }
}
