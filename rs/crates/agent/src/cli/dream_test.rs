//! Tests of `agent dream` (docs/DREAMING.md; no Go file).
use std::time::Duration;

use agent_store::{DREAM_ABANDONED, DREAM_COMMITTED, DreamItem, FRICTION_OPEN, Friction, StateEntry, Time};

use crate::cli::logs::logs_test::write_test_file;
use crate::cli::sessions::sessions_test::{claude_transcript, setup_sessions};
use crate::cli::testenv::TestEnv;

fn run(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    res.unwrap_or_else(|e| panic!("{words:?}: {e:#}"));
    out
}

fn run_err(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    format!("{:#}", res.expect_err(&format!("{words:?} succeeded")))
}

fn file_friction(env: &TestEnv, uid: &str, summary: &str) -> Friction {
    let now = Time::now();
    let mut f = Friction {
        uid: uid.into(),
        host: "local".into(),
        filed_by: "user".into(),
        created_at: now,
        updated_at: now,
        kind: "bug".into(),
        summary: summary.into(),
        status: FRICTION_OPEN.into(),
        ..Default::default()
    };
    env.store().add_friction(&mut f).unwrap();
    f
}

#[test]
fn dream_gather_run_list_and_show() {
    crate::require_termd!();
    let (mut env, home) = setup_sessions();
    // A finished session with a memory directory beside it.
    let old = claude_transcript(&home, "old", "/srv/shop", "Old work", Duration::from_secs(2 * 86400));
    let memory = format!("{}/memory", std::path::Path::new(&old).parent().unwrap().display());
    std::fs::create_dir_all(&memory).unwrap();
    write_test_file(&format!("{memory}/MEMORY.md"), "- [a](a.md) — a\n");
    file_friction(&env, "local-1", "wait misses a turn");

    let out = run(&mut env, &["dream", "gather"]);
    assert!(
        out.starts_with("The next dream would review 1 session, 1 friction entry, 1 memory directory (3 new).\n"),
        "{out}"
    );
    assert!(out.contains("  claude:old  claude session in shop: Old work\n"), "{out}");
    assert!(!out.contains("sess-l") && !out.contains("sess-w"), "running sessions: {out}");
    assert!(out.contains(&format!("  {memory}  memory of shop (1 file)\n")), "{out}");
    assert!(out.contains("#1 bug +0: wait misses a turn"), "{out}");

    let out = run(&mut env, &["dream", "gather", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["quiet"], false);
    let items: Vec<DreamItem> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| serde_json::from_value(i.clone()).unwrap())
        .collect();
    assert_eq!(items.len(), 3);

    let out = run(&mut env, &["dream", "run", "--dry-run"]);
    assert!(out.starts_with("Would start a dream: The next dream would review"), "{out}");
    assert!(env.store().list_dreams(0).unwrap().is_empty(), "dry run recorded a dream");

    // As if a dream reviewed them and committed.
    let d = env.store().open_dream("local", &items, |id| format!("dream-{id}")).unwrap();
    env.store().commit_dream(d.id).unwrap();
    let out = run(&mut env, &["dream", "gather"]);
    assert_eq!(out, "Nothing for a dream: nothing new since the last committed dream.\n");

    // An open dream whose agent is gone is abandoned; then nothing is new.
    let gone = env.store().open_dream("local", &[], |id| format!("dream-{id}")).unwrap();
    let out = run(&mut env, &["dream", "run", "--dry-run"]);
    assert!(out.starts_with(&format!("Would close dream #{} as abandoned", gone.id)), "{out}");
    let out = run(&mut env, &["dream", "run"]);
    assert_eq!(
        out,
        format!(
            "Closed dream #{} as abandoned: its agent {} is gone.\nDream #3 is quiet: nothing new since the last committed dream. No agent started.\n",
            gone.id, gone.agent
        )
    );
    assert_eq!(env.store().get_dream(gone.id).unwrap().unwrap().status, DREAM_ABANDONED);

    let out = run(&mut env, &["dream", "list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert!(lines[1].starts_with("#3    quiet      -"), "{out}");
    assert!(lines[2].starts_with("#2    abandoned  dream-2"), "{out}");
    assert!(lines[3].starts_with("#1    committed  dream-1"), "{out}");
    assert!(lines[3].contains("    3  "), "{out}");

    let out = run(&mut env, &["dream", "show", "1"]);
    assert!(out.starts_with("Dream #1 (committed) on local\nAgent:    dream-1\n"), "{out}");
    assert!(out.contains("  session  claude:old  claude session in shop: Old work\n           read: agent transcript old\n"), "{out}");
    let out = run(&mut env, &["dream", "show"]);
    assert!(out.starts_with("Dream #3 (quiet)"), "the newest: {out}");
    let err = run_err(&mut env, &["dream", "show", "9"]);
    assert!(err.contains("there is no dream #9"), "{err}");

    // A vote brings the entry back.
    let mut f = env.store().get_friction(1).unwrap().unwrap();
    f.updated_at = Time::now();
    env.store().update_friction(&f).unwrap();
    let out = run(&mut env, &["dream", "gather"]);
    assert!(out.starts_with("The next dream would review 1 friction entry (1 new).\n"), "{out}");
}

#[test]
fn dream_verbs_act_on_the_callers_open_dream() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    env.ctx.set_caller_chain(|| Ok(Vec::new()));

    let err = run_err(&mut env, &["dream", "note", "hello"]);
    assert!(err.contains("agent dream note is for a dream's own agent, on its open dream: you aren't an agent"), "{err}");

    let a = file_friction(&env, "local-1", "wait --next misses a turn that ended before it began");
    let b = file_friction(&env, "local-2", "the next wait misses an early turn");
    let d = env.store().open_dream("local", &[], |id| format!("dream-{id}")).unwrap();
    env.store()
        .create_agent(&agent_store::Agent {
            name: d.agent.clone(),
            session_name: format!("rpc-cli-{}", d.agent),
            backend: "shell".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    env.store()
        .set_state(&StateEntry {
            agent_name: d.agent.clone(),
            key: "dream".into(),
            value: d.id.to_string(),
            updated_at: Time::now(),
        })
        .unwrap();

    env.set_env("AGENT_NAME", "worker");
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    let err = run_err(&mut env, &["dream", "note", "hello"]);
    assert!(err.contains("worker has no open dream"), "{err}");

    env.set_env("AGENT_NAME", &d.agent);
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
    assert_eq!(run(&mut env, &["dream", "note", "read 3 sessions"]), "Noted in dream #1's diary.\n");

    let out = run(&mut env, &["dream", "friction", "accept", &a.id.to_string(), "--note", "real"]);
    assert!(out.starts_with("Accepted friction #1"), "{out}");
    assert_eq!(env.store().get_friction(a.id).unwrap().unwrap().accepted_by, "dream-1");
    let out = run(&mut env, &["dream", "friction", "dup", &b.id.to_string(), &a.id.to_string()]);
    assert!(out.starts_with("Closed friction #2 as a duplicate of #1"), "{out}");

    let out = run(&mut env, &["dream", "limitation", "wait --next misses a turn that ended before it began"]);
    assert!(out.starts_with("Not filed: it looks like #1"), "{out}");
    let out = run(&mut env, &["dream", "limitation", "the digest can't show subagent transcripts"]);
    assert_eq!(out, "Filed friction #3 (other).\n");
    let filed = env.store().get_friction(3).unwrap().unwrap();
    assert_eq!((filed.kind.as_str(), filed.filed_by.as_str()), ("other", "dream-1"));
    assert!(filed.detail.contains("Filed by dream #1 (dream-1)."), "{}", filed.detail);

    // What the dream triaged is recorded as it left it.
    let items = env.store().dream_items(d.id).unwrap();
    let mut keys: Vec<&str> = items.iter().map(|i| i.key.as_str()).collect();
    keys.sort();
    let mut want = vec!["local-1", "local-2", filed.uid.as_str()];
    want.sort();
    assert_eq!(keys, want);

    assert_eq!(
        run(&mut env, &["dream", "commit"]),
        "Committed dream #1: its 3 items count as reviewed.\n"
    );
    let got = env.store().get_dream(d.id).unwrap().unwrap();
    assert_eq!(got.status, DREAM_COMMITTED);
    assert!(got.diary.ends_with(" read 3 sessions"), "{}", got.diary);
    let a_now = env.store().get_friction(a.id).unwrap().unwrap();
    assert!(
        env.store()
            .is_reviewed("friction", &a_now.uid, &format!("{}+{}", a_now.updated_at.utc().format_rfc3339_nano(), a_now.votes))
            .unwrap(),
        "the accepted entry is reviewed as the dream left it"
    );

    let err = run_err(&mut env, &["dream", "note", "late"]);
    assert!(err.contains("dream #1 is committed"), "{err}");
    let err = run_err(&mut env, &["dream", "abandon"]);
    assert!(err.contains("dream #1 is committed"), "{err}");
}
