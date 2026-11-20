//! `agent deliver` (review 5, finding 7).

use std::os::unix::fs::PermissionsExt;

use agent_store::{Agent, Time};

use crate::cli::testenv::TestEnv;

// An adopted agent has no directory of its own: deliver --wait makes it for
// its lock, where the Go (and this, before) failed with "opening deliverer
// lock: open .../deliver.lock: no such file or directory". A name no agent
// has still fails, and leaves nothing behind.
#[test]
fn deliver_wait_for_an_adopted_agent() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd();
    env.create_live_agent(&Agent {
        name: "ad1".into(),
        session_name: "rpc-cli-ad1".into(),
        backend: "claude".into(),
        adopted: true,
        created_at: Time::now(),
        ..Default::default()
    });
    let dir = std::path::PathBuf::from(env.cfg.agent_dir("ad1"));
    assert!(!dir.exists(), "adopt made {}", dir.display());

    let (out, res) = env.run(&["deliver", "--wait", "ad1"]);
    res.unwrap_or_else(|e| panic!("deliver --wait ad1: {e:#}"));
    assert_eq!(out, "", "nothing was held");
    let mode = std::fs::metadata(&dir).expect("ad1's directory").permissions().mode();
    assert_eq!(mode & 0o777, 0o700, "{} mode {mode:o}", dir.display());

    let (_, res) = env.run(&["deliver", "--wait", "nosuch"]);
    let err = format!("{:#}", res.expect_err("no such agent"));
    assert!(err.starts_with("opening deliverer lock: open "), "err = {err}");
    assert!(!std::path::Path::new(&env.cfg.agent_dir("nosuch")).exists(), "made a directory for nosuch");
}
