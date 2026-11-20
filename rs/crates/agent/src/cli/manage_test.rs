//! Tests of `agent manage` (no Go file: new in the Rust build).
use std::time::Duration;

use agent_manage::{HostView, SessionState, View};
use agent_store::{CommandRecord, Time};

use crate::cli::logs::logs_test::write_test_file;
use crate::cli::sessions::sessions_test::{claude_transcript, setup_sessions};
use crate::cli::testenv::TestEnv;

fn run(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    res.unwrap_or_else(|e| panic!("{words:?}: {e:#}"));
    out
}

fn view(env: &mut TestEnv, words: &[&str]) -> HostView {
    let mut line = vec!["manage", "view", "--json"];
    line.extend_from_slice(words);
    let out = run(env, &line);
    let mut v: View = serde_json::from_str(&out).unwrap_or_else(|e| panic!("decoding {out:?}: {e}"));
    assert_eq!(v.hosts.len(), 1, "hosts = {:?}", v.hosts);
    v.hosts.remove(0)
}

fn assistant(id: &str, session: &str, input: i64, output: i64) -> String {
    format!(
        "{{\"type\":\"assistant\",\"sessionId\":\"{session}\",\"message\":{{\"id\":\"{id}\",\"model\":\"claude-test\",\"role\":\"assistant\",\"content\":[],\"usage\":{{\"input_tokens\":{input},\"cache_read_input_tokens\":1000,\"output_tokens\":{output}}}}}}}\n"
    )
}

/// setup_sessions (a person's running session `sess-l`, the worker agent's
/// `sess-w`), plus `old`, which nobody runs and which was last written 40
/// days ago, with two model calls and a subagent that made one.
fn setup() -> TestEnv {
    let (env, home) = setup_sessions();
    let ago = Duration::from_secs(40 * 86400);
    let old = claude_transcript(&home, "old", "/home/user/shop", "Old work", ago);
    let mut body = std::fs::read_to_string(&old).unwrap();
    body += &assistant("m1", "old", 10, 5);
    body += &assistant("m1", "old", 10, 7);
    body += &assistant("m2", "old", 20, 3);
    write_test_file(&old, &body);
    let subs = format!("{}/subagents", old.trim_end_matches(".jsonl"));
    std::fs::create_dir_all(&subs).unwrap();
    let sub = format!("{subs}/agent-s1.jsonl");
    write_test_file(
        &sub,
        &format!(
            "{{\"type\":\"user\",\"isSidechain\":true,\"agentId\":\"s1\",\"sessionId\":\"old\",\"cwd\":\"/home/user/shop\",\"message\":{{\"role\":\"user\",\"content\":\"sub\"}}}}\n{}",
            assistant("s1", "old", 100, 50).replace("\"sessionId\"", "\"isSidechain\":true,\"agentId\":\"s1\",\"sessionId\"")
        ),
    );
    for p in [&old, &sub] {
        let at = std::time::SystemTime::now() - ago;
        let f = std::fs::File::options().write(true).open(p).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(at).set_accessed(at))
            .unwrap();
    }
    env
}

#[test]
fn test_manage_view() {
    crate::require_termd!();
    let mut env = setup();
    let h = view(&mut env, &["--local"]);
    assert!(h.reachable && h.usage_pending == 0, "{h:?}");
    let by_id = |id: &str| {
        h.sessions
            .iter()
            .find(|s| s.session_id == id)
            .unwrap_or_else(|| panic!("no session {id} in {:?}", h.sessions))
    };
    let (w, l, old, sub) = (by_id("sess-w"), by_id("sess-l"), by_id("old"), by_id("agent-s1"));
    assert_eq!((w.agent.as_str(), w.state), ("worker", SessionState::Running));
    assert_eq!((l.agent.as_str(), l.state, l.stale), ("", SessionState::Running, false));
    assert_eq!((old.state, old.stale, old.title.as_str()), (SessionState::Dormant, true, "Old work"));
    assert_eq!(sub.parent_session, "old");

    // A call written twice counts once, with its newest counts; the context
    // is the last call's prompt.
    let u = old.usage.as_ref().expect("old's usage");
    assert_eq!(
        (u.tokens.calls, u.tokens.input, u.tokens.output, u.tokens.cache_read, u.context_tokens),
        (2, 30, 10, 2000, 1020)
    );
    assert_eq!(old.model, "claude-test");
    let all = old.usage_with_subagents.expect("old with its subagent");
    assert_eq!((all.calls, all.input, all.output), (3, 130, 60));
    assert!(l.usage_with_subagents.is_none());

    // The counts are cached, and a transcript that grew is read from where
    // the count stopped.
    assert_eq!(env.store().session_usage().unwrap().len(), 4);
    let mut body = std::fs::read_to_string(&old.path).unwrap();
    body += &assistant("m3", "old", 1, 1);
    write_test_file(&old.path, &body);
    let h = view(&mut env, &["--local"]);
    let old = h.sessions.iter().find(|s| s.session_id == "old").unwrap();
    assert_eq!(old.usage.as_ref().unwrap().tokens.calls, 3);

    assert_eq!(h.data.transcripts["claude"].count, 4);
    assert!(h.data.tables.contains_key("agents") && h.data.db_bytes > 0);
    assert!(h.agents.iter().any(|a| a.name == "worker"), "{:?}", h.agents);

    // For a person: a summary. (The subagent ended with its parent, and is
    // still 40 days old; `old` was just written.)
    let text = run(&mut env, &["manage", "view", "--local"]);
    assert!(text.contains("sessions: 4 (2 running, 1 dormant, 1 ended); 1 stale"), "{text}");
    assert!(text.contains("in 4 model calls"), "{text}");
}

#[test]
fn test_manage_untrack_and_track() {
    crate::require_termd!();
    let mut env = setup();

    // A running session is refused unless forced; an unknown one is an error.
    let (_, res) = env.run(&["manage", "untrack", "sess-l"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("claude:sess-l is running") && err.contains("--force"), "{err}");
    let (_, res) = env.run(&["manage", "untrack", "nope"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("no session nope"));
    let (_, res) = env.run(&["manage", "untrack"]);
    assert!(res.is_err());

    // By what they are: --dry-run changes nothing, and the running ones stay.
    let pick = ["manage", "untrack", "--older-than", "30d", "--state", "dormant,ended"];
    let mut dry = pick.to_vec();
    dry.push("--dry-run");
    let out = run(&mut env, &dry);
    assert!(out.contains("Would untrack 2 sessions") && out.contains("claude:old"), "{out}");
    assert!(env.store().untracked_sessions().unwrap().is_empty());
    let out = run(&mut env, &pick);
    assert!(out.contains("Untracked 2 sessions"), "{out}");

    // The session and its subagent are out of the view, unless asked for.
    let h = view(&mut env, &["--local"]);
    assert_eq!(h.sessions.len(), 2, "{:?}", h.sessions);
    assert_eq!(h.data.untracked, 2);
    let h = view(&mut env, &["--local", "--untracked"]);
    assert_eq!(h.sessions.iter().filter(|s| s.untracked).count(), 2);
    assert!(std::path::Path::new(&h.sessions.iter().find(|s| s.session_id == "old").unwrap().path).exists());
    assert!(run(&mut env, &pick).contains("No sessions to untrack"));

    let (_, res) = env.run(&["manage", "track"]);
    assert!(res.is_err());
    assert!(run(&mut env, &["manage", "track", "claude:old"]).contains("Tracking 1 session again"));
    assert!(run(&mut env, &["manage", "track", "--all"]).contains("Tracking 1 session again"));
    assert_eq!(view(&mut env, &["--local"]).sessions.len(), 4);

    // By name, a session takes its subagents along.
    assert!(run(&mut env, &["manage", "untrack", "ol"]).contains("Untracked 2 sessions"));
    assert!(run(&mut env, &["manage", "untrack", "sess-l", "--force"]).contains("Untracked 1 session"));
}

#[test]
fn test_manage_backup_and_prune() {
    crate::require_termd!();
    let mut env = setup();
    let old = Time::parse_rfc3339("2026-01-01T00:00:00Z").unwrap();
    for _ in 0..3 {
        let mut r = CommandRecord {
            started_at: old,
            args: vec!["list".to_string()],
            ..CommandRecord::default()
        };
        env.store().log_command(&mut r).unwrap();
    }
    let logs = env.cfg.log_dir.clone();
    write_test_file(&format!("{logs}/departed.log"), "old output");
    write_test_file(&format!("{logs}/worker.log"), "the worker's");

    let out = run(&mut env, &["manage", "backup"]);
    assert!(out.starts_with("Backed up to ") && out.contains("/backups/gomuxai-"), "{out}");
    assert_eq!(view(&mut env, &["--local"]).data.backups.len(), 1);

    let (_, res) = env.run(&["manage", "prune"]);
    assert!(format!("{:#}", res.unwrap_err()).contains("--older-than"));

    let out = run(&mut env, &["manage", "prune", "--older-than", "30d", "--dry-run"]);
    assert!(out.contains("command_log      3") && out.contains("Would delete "), "{out}");
    assert_eq!(view(&mut env, &["--local"]).data.backups.len(), 1, "a dry run backs nothing up");

    // The departed agent's log is new, so --logs leaves it; a cutoff in the
    // future takes it, and never the recorded worker's.
    let out = run(&mut env, &["manage", "prune", "--older-than", "30d", "--logs"]);
    assert!(out.contains("Backed up to ") && out.contains("command_log      3") && out.contains("Deleted "), "{out}");
    assert!(!out.contains("log file"), "{out}");
    let h = view(&mut env, &["--local"]);
    assert_eq!((h.data.backups.len(), h.data.departed_logs), (2, 1));
    let out = run(&mut env, &["manage", "prune", "--older-than", "2999-01-01", "--logs", "--no-backup"]);
    assert!(out.contains("1 log file of departed agents"), "{out}");
    assert!(!std::path::Path::new(&format!("{logs}/departed.log")).exists());
    assert!(std::path::Path::new(&format!("{logs}/worker.log")).exists());

    // --keep deletes only this command's older backups.
    let dir = format!("{}/backups", env.ctx.data_dir());
    write_test_file(&format!("{dir}/gomuxai-before-rust-20261120.db"), "kept by hand");
    let out = run(&mut env, &["manage", "backup", "--keep", "1"]);
    assert!(out.contains("Deleted 2 older backups"), "{out}");
    assert!(std::path::Path::new(&format!("{dir}/gomuxai-before-rust-20261120.db")).exists());
}

#[test]
fn test_manage_view_of_another_host() {
    crate::require_termd!();
    let mut env = setup();
    if env.serve_as_h1().is_none() {
        return;
    }
    let h = view(&mut env, &["--host", "h1", "--sessions", "1"]);
    assert_eq!((h.host.as_str(), h.reachable), ("h1", true));
    assert_eq!(h.sessions.len(), 1, "the newest session only: {:?}", h.sessions);
    assert_eq!(env.store().list_commands(&Default::default()).unwrap().len(), 0, "serving a view logs no command");
    // Two older groups: the worker's or the person's, and `old` with its subagent.
    assert_eq!(h.sessions_omitted, 2);

}

/// A host that doesn't serve manage is an error naming the op.
#[test]
fn test_manage_view_of_a_host_without_the_op() {
    crate::require_termd!();
    let mut env = setup();
    if env.serve_as_h1_with(&[agent_bus::OP_LIST]).is_none() {
        return;
    }
    let (_, res) = env.run(&["manage", "view", "--host", "h1"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("manage"), "{err}");
}
