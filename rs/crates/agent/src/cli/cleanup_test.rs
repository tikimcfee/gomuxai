//! Go: internal/cli/cleanup_test.go.

use std::collections::HashMap;

use agent_backend::EndReason;
use agent_service::{AGENT_STATUS_ENDED, StatusReport};
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, Time};

use super::cleanup;
use super::root::execute_ctx;
use super::testenv::TestEnv;

/// Adds agents with no termd sessions, each name mapped to its parent (Go:
/// addTeam).
fn add_team(env: &TestEnv, parents: &[(&str, &str)]) {
    for (name, parent) in parents {
        env.store()
            .create_agent(&Agent {
                name: name.to_string(),
                session_name: format!("rpc-cli-{name}"),
                backend: "claude".into(),
                spawned_by: parent.to_string(),
                created_at: Time::now(),
                ..Default::default()
            })
            .unwrap();
    }
}

/// Which of names still have records (Go: listed).
pub fn listed(env: &TestEnv, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|n| env.store().get_agent(n).ok().flatten().is_some())
        .map(|n| n.to_string())
        .collect()
}

fn cleanup_args(names: &[&str]) -> cleanup::Args {
    cleanup::Args {
        dry_run: false,
        ended: false,
        args: names.iter().map(|s| s.to_string()).collect(),
    }
}

fn ended_args(dry_run: bool) -> cleanup::Args {
    cleanup::Args {
        dry_run,
        ended: true,
        args: Vec::new(),
    }
}

/// Fails unless out's lines start, in order, with want's (Go: checkLines).
pub fn check_lines(what: &str, out: &str, want: &[&str]) {
    let lines: Vec<&str> = out.trim_end_matches('\n').split('\n').collect();
    let ok = lines.len() == want.len() && lines.iter().zip(want).all(|(l, w)| l.starts_with(w));
    assert!(
        ok,
        "{what} printed:\n{out}\nwant lines starting:\n{}",
        want.join("\n")
    );
}

#[test]
fn test_cleanup_agents_clean_up_only_their_helpers() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    add_team(
        &env,
        &[
            ("lead", ""),
            ("lead-a", "lead"),
            ("lead-a-x", "lead-a"),
            ("lead-a-y", "lead-a-x"),
            ("lead-b", "lead"),
            ("other", ""),
            ("other-h", "other"),
        ],
    );
    env.set_env("AGENT_NAME", "lead");

    let tests: Vec<(Vec<&str>, Vec<&str>, Vec<&str>)> = vec![
        (
            vec!["other", "worker"],
            vec![
                r#"refused "other": it was spawned by a person, not by you ("lead") or your helpers"#,
                r#"refused "worker": it was spawned by a person"#,
            ],
            vec![],
        ),
        (
            vec!["lead"],
            vec![r#"refused "lead": an agent can't clean itself up"#],
            vec![],
        ),
        (
            vec!["other-h"],
            vec![
                r#"refused "other-h": it was spawned by "other", not by you ("lead") or your helpers"#,
            ],
            vec![],
        ),
        (
            // It would leave lead-a-x where only a person could reach it.
            vec!["lead-a"],
            vec![
                r#"refused "lead-a": its helpers lead-a-x would be left behind, and only a person could clean them up then; clean them up with it (agent cleanup lead-a lead-a-x)"#,
            ],
            vec![],
        ),
        (
            // A refused helper counts as left behind too.
            vec!["lead-a", "lead-a-x"],
            vec![
                r#"refused "lead-a-x": its helpers lead-a-y would be left behind"#,
                r#"refused "lead-a": its helpers lead-a-x would be left behind"#,
            ],
            vec![],
        ),
        (
            // Helpers first, whatever order they're named in.
            vec!["lead-a", "lead-a-y", "ghost", "lead-a-x"],
            vec![
                r#"cleaned up "lead-a-y": removed its record, state, queued messages, and files; its log stays (agent logs lead-a-y)"#,
                r#"cleaned up "lead-a-x": removed"#,
                r#"cleaned up "lead-a": removed"#,
                r#"failed "ghost": agent "ghost" not found on host local (5 agents there: agent list)"#,
            ],
            vec!["lead-a", "lead-a-x", "lead-a-y"],
        ),
    ];
    for (args, want, removed) in tests {
        let res = cleanup::run(&env.ctx, &cleanup_args(&args));
        let out = env.take_out();
        check_lines(&format!("cleanup {args:?}"), out.trim(), &want);
        assert!(
            res.is_err(),
            "cleanup {args:?}: no error, want one for the refusals"
        );
        let gone = listed(&env, &removed);
        assert!(gone.is_empty(), "cleanup {args:?} left {gone:?}");
    }
    let got = listed(&env, &["lead", "lead-b", "other", "other-h", "worker"]);
    assert_eq!(got.len(), 5, "still listed: {got:?}");
}

// A coordinator that wasn't spawned through agent still owns its helpers.
#[test]
fn test_cleanup_unlisted_caller_owns_its_helpers() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    add_team(&env, &[("coord-a", "coord")]);
    env.set_env("AGENT_NAME", "coord");
    let res = cleanup::run(&env.ctx, &cleanup_args(&["coord-a"]));
    let out = env.take_out();
    assert!(
        res.is_ok() && out.starts_with(r#"cleaned up "coord-a""#),
        "cleanup = {out:?}, {res:?}"
    );
}

// A person can clean up anything, and hears which helpers remain.
#[test]
fn test_cleanup_person_gets_a_note_on_remaining_helpers() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();
    add_team(&env, &[("worker-a", "worker"), ("worker-b", "worker")]);
    let res = cleanup::run(&env.ctx, &cleanup_args(&["worker", "worker-b"]));
    let out = env.take_out();
    res.unwrap_or_else(|e| panic!("cleanup: {e:#}\n{out}"));
    let want = r#"cleaned up "worker-b": removed its record, state, queued messages, and files; its log stays (agent logs worker-b)
cleaned up "worker": killed session rpc-cli-worker; removed its record, state, queued messages, and files; its log stays (agent logs worker)
  note: its helpers worker-a remain (agent cleanup worker-a)
"#;
    assert_eq!(out, want);
    assert_eq!(
        listed(&env, &["worker", "worker-a", "worker-b"]),
        vec!["worker-a"]
    );
}

/// Adds an agent record, with a running termd session (a shell) if live. An
/// agent with no session resolves to ended (gone); a live aider agent, read
/// off its screen, is running (Go: addRecord).
pub fn add_record(env: &TestEnv, mut a: Agent, live: bool) {
    if a.session_name.is_empty() {
        a.session_name = format!("rpc-cli-{}", a.name);
    }
    if a.backend.is_empty() {
        a.backend = "claude".into();
    }
    a.created_at = Time::now();
    if live {
        env.termd_ops()
            .create_session(&a.session_name, &HashMap::new())
            .unwrap();
    }
    env.store().create_agent(&a).unwrap();
}

fn agent(name: &str, by: &str, backend: &str) -> Agent {
    Agent {
        name: name.into(),
        spawned_by: by.into(),
        backend: backend.into(),
        ..Default::default()
    }
}

/// Holds n messages for name, as send does for a busy agent (Go:
/// holdMessages).
pub fn hold_messages(env: &TestEnv, name: &str, n: usize) {
    for i in 0..n {
        env.store()
            .create_message(&mut Message {
                sender_id: "someone".into(),
                receiver_id: name.into(),
                content: format!("message {i}"),
                status: MESSAGE_STATUS_PENDING,
                created_at: Time::now(),
                ..Default::default()
            })
            .unwrap();
    }
}

/// Counts the messages held for name (Go: heldFor).
pub fn held_for(env: &TestEnv, name: &str) -> usize {
    env.store().get_pending_messages(name, 100).unwrap().len()
}

// A person's cleanup --ended: ended agents go (helpers first; an adopted
// one's session stays running), running ones and an ended agent's running
// helper stay, and --dry-run before it lists the same and changes nothing.
#[test]
fn test_cleanup_ended_person() {
    crate::require_termd!();
    let env = TestEnv::setup_cli();

    add_record(&env, agent("gone", "", ""), false);
    add_record(&env, agent("gone-h", "gone", ""), false);
    add_record(&env, agent("lead", "", ""), false);
    add_record(&env, agent("lead-run", "lead", "aider"), true);
    add_record(&env, agent("runner", "", "aider"), true);
    let user_sess = format!("rpc-cli-user-ended-{}", std::process::id());
    add_record(
        &env,
        Agent {
            name: "adopted".into(),
            session_name: user_sess.clone(),
            adopted: true,
            ..Default::default()
        },
        true,
    );
    env.service()
        .report(
            "adopted",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                end_reason: EndReason::from("quit"),
                detail: "prompt_input_exit".into(),
                ..Default::default()
            },
        )
        .unwrap();
    hold_messages(&env, "gone", 2);
    // The worker setup_cli adds is waiting, not ended: it stays.

    // Helpers first, then by name.
    let adopted_fate = format!(
        "  adopted: its session {user_sess} stays running, and only the record goes; no held messages"
    );
    let want = [
        r#"would clean up "gone-h": ended (gone): its session rpc-cli-gone-h is gone (a reboot, or g3d-termd was stopped); agent resume gone-h continues its session"#,
        "  its session rpc-cli-gone-h is already gone; no held messages",
        r#"would clean up "adopted": ended ("#,
        &adopted_fate,
        r#"would clean up "gone": ended (gone): its session rpc-cli-gone is gone"#,
        "  its session rpc-cli-gone is already gone; deletes 2 messages held for it",
        r#"would clean up "lead": ended (gone)"#,
        "  its session rpc-cli-lead is already gone; no held messages",
        "  note: its helpers lead-run haven't ended, so they stay (agent cleanup lead-run)",
        "Would clean up 4 ended agents; nothing changed (dry run).",
    ];
    let res = cleanup::run(&env.ctx, &ended_args(true));
    let out = env.take_out();
    res.unwrap_or_else(|e| panic!("dry run: {e:#}\n{out}"));
    check_lines("cleanup --ended --dry-run", &out, &want);
    let all = ["gone", "gone-h", "lead", "lead-run", "runner", "adopted"];
    assert_eq!(
        listed(&env, &all).len(),
        all.len(),
        "dry run removed agents"
    );
    assert_eq!(held_for(&env, "gone"), 2, "dry run deleted held messages");

    let res = cleanup::run(&env.ctx, &ended_args(false));
    let out = env.take_out();
    res.unwrap_or_else(|e| panic!("cleanup --ended: {e:#}\n{out}"));
    let adopted_line =
        format!(r#"cleaned up "adopted": left session {user_sess} running; removed"#);
    check_lines(
        "cleanup --ended",
        &out,
        &[
            r#"cleaned up "gone-h": removed its record, state, queued messages, and files; its log stays (agent logs gone-h)"#,
            &adopted_line,
            r#"cleaned up "gone": removed"#,
            r#"cleaned up "lead": removed"#,
            "  note: its helpers lead-run haven't ended, so they stay (agent cleanup lead-run)",
            "Cleaned up 4 ended agents.",
        ],
    );
    assert_eq!(listed(&env, &all), vec!["lead-run", "runner"]);
    assert_eq!(held_for(&env, "gone"), 0, "held messages left for gone");
    for s in [user_sess.as_str(), "rpc-cli-lead-run", "rpc-cli-runner"] {
        assert!(
            env.termd_ops().session_exists(s).unwrap_or(false),
            "session {s} was killed"
        );
    }

    // Nothing ended is left.
    let res = cleanup::run(&env.ctx, &ended_args(false));
    let out = env.take_out();
    assert!(
        res.is_ok() && out == "No ended agents to clean up here.\n",
        "second cleanup --ended = {out:?}, {res:?}"
    );
}

// Inside an agent, cleanup --ended picks only the caller's own ended helpers
// and theirs, never the caller itself, and skips an ended helper whose own
// helpers are still running, since nothing would tie them to the caller once
// it's gone.
#[test]
fn test_cleanup_ended_inside_agent() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli_private();
    env.set_env("AGENT_NAME", "boss");

    add_record(&env, agent("boss", "", ""), false); // ended itself, say on a stale record
    add_record(&env, agent("boss-a", "boss", ""), false);
    add_record(&env, agent("boss-a-x", "boss-a", ""), false);
    add_record(&env, agent("boss-b", "boss", ""), false);
    add_record(&env, agent("boss-b-r", "boss-b", "aider"), true);
    add_record(&env, agent("boss-c", "boss", "aider"), true);
    add_record(&env, agent("stranger", "", ""), false);
    add_record(&env, agent("stranger-h", "stranger", ""), false);

    let skips = [
        r#"skipped "boss-b" (ended): its helpers boss-b-r haven't ended, and only a person could clean them up once it's gone"#,
        r#"skipped "boss" (ended): that's you, and an agent can't clean itself up"#,
    ];
    let res = cleanup::run(&env.ctx, &ended_args(true));
    let out = env.take_out();
    res.unwrap_or_else(|e| panic!("dry run: {e:#}\n{out}"));
    let mut want = skips.to_vec();
    want.extend([
        r#"would clean up "boss-a-x": ended (gone)"#,
        "  its session rpc-cli-boss-a-x is already gone; no held messages",
        r#"would clean up "boss-a": ended (gone)"#,
        "  its session rpc-cli-boss-a is already gone; no held messages",
        r#"Would clean up 2 ended agents; nothing changed (dry run). Skipped 2. Left 2 more ended agents here that others spawned, not you ("boss") or your helpers."#,
    ]);
    check_lines("cleanup --ended --dry-run inside boss", &out, &want);

    let res = cleanup::run(&env.ctx, &ended_args(false));
    let out = env.take_out();
    res.unwrap_or_else(|e| panic!("cleanup --ended: {e:#}\n{out}"));
    let mut want = skips.to_vec();
    want.extend([
        r#"cleaned up "boss-a-x": removed"#,
        r#"cleaned up "boss-a": removed"#,
        r#"Cleaned up 2 ended agents. Skipped 2. Left 2 more ended agents here that others spawned, not you ("boss") or your helpers."#,
    ]);
    check_lines("cleanup --ended inside boss", &out, &want);
    assert_eq!(
        listed(
            &env,
            &[
                "boss",
                "boss-a",
                "boss-a-x",
                "boss-b",
                "boss-b-r",
                "boss-c",
                "stranger",
                "stranger-h"
            ]
        ),
        vec![
            "boss",
            "boss-b",
            "boss-b-r",
            "boss-c",
            "stranger",
            "stranger-h"
        ]
    );
}

// --ended takes no names, and --dry-run goes only with --ended.
#[test]
fn test_cleanup_ended_args() {
    for (argv, want) in [
        (vec!["--ended", "x"], "--ended picks the agents itself"),
        (vec!["--dry-run", "x"], "--dry-run goes with --ended"),
        (vec![], "requires at least 1 arg"),
    ] {
        let mut env = TestEnv::new();
        let mut line = vec!["cleanup".to_string()];
        line.extend(argv.iter().map(|s| s.to_string()));
        let err = execute_ctx(&mut env.ctx, &line).expect_err("an argument error");
        assert!(
            format!("{err:#}").contains(want),
            "{argv:?}: err = {err:#}, want {want:?}"
        );
    }
}
