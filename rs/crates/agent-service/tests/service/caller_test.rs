//! Go tests/service/caller_test.go.

use std::collections::HashMap;
use std::path::Path;

use agent_base::await_;
use agent_claudereg::{KIND_BACKGROUND, KIND_INTERACTIVE, stat_start_time};
use agent_store::{Agent, Time};

use crate::common::*;
use crate::require_termd;

/// registerCLI writes a Claude Code registry entry for the running process
/// pid, with the start time Claude Code records, so it proves live.
fn register_cli(dir: &Path, pid: i32, session_id: &str, kind: &str) {
    let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
    let start = stat_start_time(&stat)
        .unwrap_or_else(|| panic!("no start time in {:?}", String::from_utf8_lossy(&stat)));
    let entry = format!(
        r#"{{"pid":{pid},"sessionId":{},"cwd":"/w","procStart":{},"kind":{},"status":"busy","updatedAt":{}}}"#,
        serde_json::to_string(session_id).unwrap(),
        serde_json::to_string(&start).unwrap(),
        serde_json::to_string(kind).unwrap(),
        chrono::Utc::now().timestamp_millis()
    );
    std::fs::write(dir.join("sessions").join(format!("{pid}.json")), entry).unwrap();
}

struct Kill(std::process::Child, i32);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
        if self.1 != 0 {
            // SAFETY: kill(2) on a pid this test started.
            unsafe {
                libc::kill(self.1, libc::SIGKILL);
            }
        }
    }
}

// What friction #48-#50 hit: Claude Code 2.1.325 sent agent A's
// conversation to the background (/bg) into a spare host its daemon had
// started for agent B, so the session ran with B's environment (AGENT_NAME)
// and outside A's pane, under the daemon or B's CLI. The hook placed
// nothing on A, so A's long messages lost their text and its sends went
// out as B's. Its lineage says whose it is: A's transcript continued in it.
#[test]
#[cfg(target_os = "linux")] // proves registry entries by /proc start times
fn test_background_session_belongs_to_its_conversation() {
    require_termd!();
    let mut env = setup_test_env_private();
    let cfg_dir = tempdir();
    env.set_env("CLAUDE_CONFIG_DIR", cfg_dir.path());
    for d in ["sessions", "projects/-w"] {
        std::fs::create_dir_all(cfg_dir.path().join(d)).unwrap();
    }

    // Agents a and b, each a pane whose process stands in for its CLI.
    for name in ["a", "b"] {
        let session = env.session_name(name);
        env.tmux.create_session(&session, &HashMap::new()).unwrap();
        env.store
            .create_agent(&Agent {
                name: name.into(),
                session_name: session,
                backend: "claude".into(),
                created_at: Time::now(),
                ..Default::default()
            })
            .unwrap();
    }
    let (pane_a, pane_b) = (
        pane_pid(&env, &env.session_name("a")),
        pane_pid(&env, &env.session_name("b")),
    );
    register_cli(cfg_dir.path(), pane_a, "a-old", KIND_INTERACTIVE);
    register_cli(cfg_dir.path(), pane_b, "b-sess", KIND_INTERACTIVE);
    env.set_state("b", "_session_id", "b-sess");

    // The background session: a process outside a's pane (here, this
    // test's child) holding the moved session, and a hook it runs.
    let host = std::process::Command::new("sh")
        .args(["-c", "sleep 60 & wait"])
        .spawn()
        .unwrap();
    let bgp = host.id() as i32;
    let mut hook = 0;
    await_::until("the background session's hook process", || {
        let data = std::fs::read_to_string(format!("/proc/{bgp}/task/{bgp}/children"));
        hook = data
            .as_ref()
            .map(|d| d.trim().parse().unwrap_or(0))
            .unwrap_or(0);
        (format!("{data:?}"), hook != 0)
    });
    let _kill = Kill(host, hook);
    register_cli(cfg_dir.path(), bgp, "a-new", KIND_BACKGROUND);
    // A tool the background session runs, under it, then the daemon's
    // tree up to b's pane.
    let tool = 1 << 30;
    let chain = [tool, bgp, pane_b];

    // Not moved yet: the background session is nobody's, and it is never b's.
    let r = env.service.caller_agent(&chain).unwrap();
    assert_eq!(
        r,
        (String::new(), true),
        "before the move: want a session no agent owns"
    );
    let (hs, err) = env.service.find_hook_session(hook, "a-new", "");
    assert!(
        err.is_ok() && hs.cli_pid == bgp && hs.owner.is_empty(),
        "hook before the move: {hs:?} {err:?}, want CLI {bgp}, no owner"
    );

    // a's transcript records the move. (a's pane CLI may be gone: /bg
    // frees the terminal.)
    let line = r#"{"type":"continued-in","sessionId":"a-old","continuedInSessionId":"a-new"}"#;
    let old = cfg_dir
        .path()
        .join("projects")
        .join("-w")
        .join("a-old.jsonl");
    std::fs::write(&old, format!("{line}\n")).unwrap();
    env.set_state("a", "_transcript", &old.display().to_string());
    let r = env.service.caller_agent(&chain).unwrap();
    assert_eq!(r, ("a".to_string(), true), "a's moved session: want a");
    // The hook it runs is placed on a too.
    let (hs, err) = env.service.find_hook_session(hook, "a-new", "");
    assert!(
        err.is_ok() && hs.owner == "a",
        "hook: {hs:?} {err:?}, want owner a"
    );

    // b's own CLI, and a process under b's pane, are b's.
    let r = env.service.caller_agent(&[tool, pane_b]).unwrap();
    assert_eq!(r, ("b".to_string(), true), "under b's pane: want b");
    // Outside every pane, the processes don't say.
    let r = env.service.caller_agent(&[tool, 1]).unwrap();
    assert_eq!(r, (String::new(), false), "outside the panes");
    // Once a's record names the new session, it is found without the walk.
    std::fs::remove_file(&old).unwrap();
    env.set_state("a", "_session_id", "a-new");
    let r = env.service.caller_agent(&chain).unwrap();
    assert_eq!(r.0, "a", "a on its new session");
}
