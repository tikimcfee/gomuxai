//! `agent state get` and `agent state list` as the CLI runs them (set and
//! clear, and get's missing-key errors, are in output.rs's
//! state_errors_and_clear).

use agent_store::{Agent, Time};

use crate::cli::testenv::TestEnv;

#[test]
fn state_get_and_list() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd();
    env.store()
        .create_agent(&Agent {
            name: "worker".into(),
            session_name: "rpc-cli-worker".into(),
            backend: "shell".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();

    let (out, res) = env.run(&["state", "list", "worker"]);
    res.unwrap();
    assert_eq!(
        out,
        "No state for agent worker yet (agent state set worker <key> <value> adds some).\n"
    );
    let (out, res) = env.run(&["state", "list", "worker", "--json"]);
    res.unwrap();
    assert_eq!(out, "[]\n");

    let long = "first line of notes\nand  a second, past thirty characters";
    for (k, v) in [("progress", "3/5 files"), ("notes", long)] {
        let (_, res) = env.run(&["state", "set", "worker", k, v]);
        res.unwrap();
    }

    let (out, res) = env.run(&["state", "get", "worker", "progress"]);
    res.unwrap();
    assert_eq!(out, "3/5 files\n");
    let (out, res) = env.run(&["state", "get", "worker", "notes"]);
    res.unwrap();
    assert_eq!(out, format!("{long}\n"), "get prints the value whole");
    let (out, res) = env.run(&["state", "get", "worker", "progress", "--json"]);
    res.unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["agent"], "worker");
    assert_eq!(v["key"], "progress");
    assert_eq!(v["value"], "3/5 files");
    assert!(v["updated_at"].as_str().is_some_and(|t| t.ends_with('Z')), "{out}");

    // Sorted by key, values on one line and cut to 30 characters.
    let (out, res) = env.run(&["state", "list", "worker"]);
    res.unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert_eq!(lines[0], format!("{:<20} {:<30} {}", "KEY", "VALUE", "UPDATED"));
    assert_eq!(lines[1], "-".repeat(72));
    assert!(
        lines[2].starts_with(&format!("{:<20} {:<30} ", "notes", "first line of notes and a s..."))
            && lines[2].ends_with('Z'),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].starts_with(&format!("{:<20} {:<30} ", "progress", "3/5 files")),
        "{}",
        lines[3]
    );
    let (out, res) = env.run(&["state", "list", "worker", "--json"]);
    res.unwrap();
    let v: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!((v[0]["key"].as_str(), v[0]["value"].as_str()), (Some("notes"), Some(long)));
    assert_eq!(v[1]["agent"], "worker");

    // State belongs to an agent record on this host.
    let (_, res) = env.run(&["state", "list", "ghost"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.starts_with("state belongs to an agent record: "), "{err}");
    let (_, res) = env.run(&["state", "get", "worker@h2", "progress"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.starts_with("state is kept per host, and worker@h2's is on \"h2\""),
        "{err}"
    );
}
