//! agent spawn --pick and --saved (no Go file).
use super::spawn_test::setup_spawn_cli;
use super::testenv::{TestEnv, temp_dir};

fn must(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    if let Err(e) = res {
        panic!("agent {}: {e:#}", words.join(" "));
    }
    out
}

fn fails(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    match res {
        Ok(()) => panic!("agent {} succeeded", words.join(" ")),
        Err(e) => format!("{e:#}"),
    }
}

/// setup_spawn_cli with a custom backend named "cat" that runs cat, so a
/// roster entry cat/ is available (cat is on PATH) and spawns no real CLI.
fn setup() -> TestEnv {
    let env = setup_spawn_cli();
    env.forbid_bus();
    let dir = temp_dir("agent-cli-cat-");
    let path = dir.path().join("cat.json");
    std::fs::write(&path, r#"{"name": "cat", "launch_command": "cat"}"#).unwrap();
    env.ctx
        .backends()
        .load_from_file(&path.to_string_lossy())
        .unwrap();
    env
}

#[test]
fn pick_flag_help_names_the_kinds() {
    let cmd = super::spawn::def().clap_command();
    let help = cmd
        .get_arguments()
        .find(|a| a.get_long() == Some("pick"))
        .and_then(|a| a.get_help())
        .map(|h| h.to_string())
        .unwrap();
    assert!(
        help.contains(&format!("({})", agent_roster::TASK_KINDS.join(", "))),
        "{help}"
    );
}

// What --pick and --saved don't go with is refused before the roster is
// read; so is a kind that isn't one.
#[test]
fn spawn_pick_refusals() {
    let mut env = TestEnv::new();
    for (words, want) in [
        (
            vec!["spawn", "x", "--pick", "edit", "--model", "m"],
            "--pick chooses the backend, model and effort, so it doesn't go with --model",
        ),
        (
            vec!["spawn", "x", "--pick", "edit", "--effort", "high"],
            "doesn't go with --effort",
        ),
        (
            vec!["spawn", "x", "--saved", "qe", "--backend", "claude"],
            "--saved chooses the backend, model and effort, so it doesn't go with --backend",
        ),
        (
            vec!["spawn", "x", "--pick", "edit", "--saved", "qe"],
            "--pick and --saved don't go together",
        ),
        (
            vec!["spawn", "x", "--pick", "nope"],
            "--pick nope: \"nope\" isn't a task kind",
        ),
        (
            vec!["spawn", "x", "--saved", "bad name"],
            "can't name a saved pick",
        ),
    ] {
        let e = fails(&mut env, &words);
        assert!(e.contains(want), "{words:?}: {e}");
    }
    let e = fails(&mut env, &["spawn", "x", "--pick", "edit"]);
    assert!(
        e.contains("--pick edit: nothing to spawn: the roster has no entries to pick from")
            && e.contains("Add one with agent roster set HARNESS/MODEL[@EFFORT] --tasks edit"),
        "{e}"
    );
}

// --pick spawns the roster's top entry for the kind and says why; --saved
// spawns a kept pick while its entry is available; with nothing available,
// the refusal lists what roster list shows.
#[test]
fn spawn_pick_and_saved() {
    crate::require_termd!();
    let mut env = setup();
    must(
        &mut env,
        &[
            "roster", "set", "cat/", "--tasks", "edit", "--speed", "fast",
        ],
    );
    must(
        &mut env,
        &["roster", "set", "no-such-cli-x7/m", "--tasks", "edit"],
    );

    let out = must(&mut env, &["spawn", "p1", "--pick", "edit"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(
        lines[0], "Picked cat/ for edit: tasks include edit; no notes on edit; fast",
        "{out}"
    );
    assert!(
        lines[1].starts_with("Spawned agent p1 (session ") && lines[1].contains("backend cat)"),
        "{out}"
    );
    assert_eq!(
        env.store().get_agent("p1").unwrap().expect("p1").backend,
        "cat"
    );

    // A kind nothing lists is a guess, and says so.
    let out = must(&mut env, &["spawn", "p2", "--pick", "kernel"]);
    assert!(
        out.lines()
            .nth(1)
            .is_some_and(|l| l.starts_with("Nothing on record suits kernel")),
        "{out}"
    );

    must(
        &mut env,
        &["roster", "pick", "--task", "edit", "--save", "qe"],
    );
    let out = must(&mut env, &["spawn", "p3", "--saved", "qe"]);
    assert!(
        out.starts_with("Spawning saved pick qe@1: cat/, picked for edit (tasks include edit; no notes on edit; fast); its entry is available.\nSpawned agent p3 "),
        "{out}"
    );
    // A saved pick is spawned on its own host only.
    let e = fails(&mut env, &["spawn", "p4@hub", "--saved", "qe"]);
    assert!(
        e.contains("saved pick qe@1 runs cat/ on local, but the agent is named for hub"),
        "{e}"
    );

    must(&mut env, &["roster", "disable", "cat/"]);
    let e = fails(&mut env, &["spawn", "p4", "--saved", "qe"]);
    assert!(
        e.contains("saved pick qe@1 runs cat/ on local, which is off: disabled now: agent roster pick --task edit picks again"),
        "{e}"
    );
    let e = fails(&mut env, &["spawn", "p4", "--pick", "edit"]);
    assert!(
        e.contains("--pick edit: nothing to spawn: no roster entry is available")
            && e.contains("\nagent roster list shows:\n  cat/ on local: off: disabled\n  no-such-cli-x7/m on local: off: no-such-cli-x7 not on PATH\n")
            && e.contains("agent roster enable HARNESS/MODEL"),
        "{e}"
    );
    assert!(env.store().get_agent("p4").unwrap().is_none());
}
