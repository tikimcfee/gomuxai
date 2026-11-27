//! Tests of `agent dream act` (docs/DREAMING-ACT.md, "The general actor").
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use agent_store::{FRICTION_OPEN, Friction, Time};

use crate::cli::testenv::{TestEnv, temp_dir};

fn run(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    res.unwrap_or_else(|e| panic!("{words:?}: {e:#}\n{out}"));
    out
}

fn run_err(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    format!("{:#}", res.expect_err(&format!("{words:?} succeeded")))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A repo with one commit on main, its identity set.
fn repo(dir: &Path) -> String {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.email", "t@example.invalid"]);
    git(dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("README.md"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "first"]);
    dir.to_string_lossy().into_owned()
}

fn file_friction(env: &TestEnv, summary: &str) -> Friction {
    let mut f = Friction {
        uid: "local-abc123".into(),
        host: "local".into(),
        filed_by: "user".into(),
        created_at: Time::now(),
        kind: "bug".into(),
        summary: summary.into(),
        detail: "it trips; my password is hunter2x".into(),
        status: FRICTION_OPEN.into(),
        ..Default::default()
    };
    env.store().add_friction(&mut f).unwrap();
    f
}

/// An env whose act runs live under a temp dir, run checks with sh, and
/// record the agents they would spawn.
fn act_env() -> (
    TestEnv,
    tempfile::TempDir,
    Arc<Mutex<Vec<agent_service::SpawnOptions>>>,
) {
    let mut env = TestEnv::setup_cli_private();
    env.forbid_bus();
    let tmp = temp_dir("dream-act-");
    env.set_env("AGENT_ACT_DIR", tmp.path().join("act"));
    env.set_env("AGENT_ACT_RUNNER", "sh");
    let spawns = Arc::new(Mutex::new(Vec::new()));
    env.ctx.dream_spawns = Some(Arc::clone(&spawns));
    (env, tmp, spawns)
}

fn be(env: &mut TestEnv, agent: &str) {
    env.set_env("AGENT_NAME", agent);
    env.ctx.set_caller_chain(|| Ok(Vec::new()));
}

#[test]
fn a_run_works_within_its_scope() {
    crate::require_termd!();
    let (mut env, tmp, spawns) = act_env();
    let app = repo(&tmp.path().join("app"));
    run(&mut env, &["dream", "scope", "add", &app]);
    let f = file_friction(&env, "the empty scope message is unclear");

    // What a person must give.
    let err = run_err(
        &mut env,
        &[
            "dream",
            "act",
            "start",
            &f.id.to_string(),
            "--scope",
            "huge",
        ],
    );
    assert!(err.contains("--scope is one of"), "{err}");
    let err = run_err(
        &mut env,
        &["dream", "act", "start", &f.id.to_string(), "--scope", "fix"],
    );
    assert!(err.contains("--repo"), "{err}");

    let out = run(
        &mut env,
        &[
            "dream",
            "act",
            "start",
            &f.id.to_string(),
            "--scope",
            "commit",
            "--repo",
            &app,
            "--check",
            "test -f ok.txt",
            "--source",
            "claude:s1",
        ],
    );
    assert!(
        out.starts_with(
            "Started act run #1 (scope commit) on friction:local-abc123: agent act-1 in "
        ),
        "{out}"
    );
    let r = env.store().get_act_run(1).unwrap().unwrap();
    assert_eq!(r.sources, ["friction:local-abc123", "claude:s1"]);
    assert!(
        r.branch.starts_with("act/1-the-empty-scope"),
        "{}",
        r.branch
    );
    let wt = r.worktree.clone();
    assert!(Path::new(&wt).join("README.md").exists());
    let item = std::fs::read_to_string(tmp.path().join("act/1/item.md")).unwrap();
    assert!(
        item.contains("Scope: commit") && !item.contains("hunter2x"),
        "redacted: {item}"
    );
    let sp = spawns.lock().unwrap();
    assert_eq!(sp.len(), 1);
    assert_eq!(sp[0].permission_mode, "dontAsk");
    assert!(
        sp[0].allow_tools.contains(&format!("Edit(/{wt}/**)")),
        "{:?}",
        sp[0].allow_tools
    );
    drop(sp);
    let err = run_err(
        &mut env,
        &["dream", "act", "start", &f.id.to_string(), "--scope", "ask"],
    );
    assert!(err.contains("one at a time"), "{err}");

    be(&mut env, "act-1");
    run(
        &mut env,
        &["dream", "act", "note", "the message is in dream.rs"],
    );
    let err = run_err(&mut env, &["dream", "act", "land"]);
    assert!(err.contains("has scope commit; land needs branch"), "{err}");
    let out = run(&mut env, &["dream", "act", "check"]);
    assert!(out.contains("The check failed"), "{out}");
    std::fs::write(Path::new(&wt).join("ok.txt"), "ok\n").unwrap();
    let err = run_err(&mut env, &["dream", "act", "done", "fixed"]);
    assert!(err.contains("uncommitted changes"), "{err}");
    let out = run(&mut env, &["dream", "act", "commit", "add ok.txt"]);
    assert!(out.starts_with("Committed "), "{out}");
    let msg = git(Path::new(&wt), &["log", "-1", "--format=%B"]);
    assert!(
        msg.contains("Act-Run: 1") && msg.contains("Sources: friction:local-abc123, claude:s1"),
        "{msg}"
    );
    assert!(run(&mut env, &["dream", "act", "check"]).contains("The check passed"));
    let err = run_err(&mut env, &["dream", "act", "commit", "again"]);
    assert!(err.contains("nothing to commit"), "{err}");

    // Upkeep: a merged, clean worktree goes; an unmerged one stays.
    git(
        Path::new(&app),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "done-work",
            &tmp.path().join("wt-done").to_string_lossy(),
        ],
    );
    git(
        Path::new(&app),
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "open-work",
            &tmp.path().join("wt-open").to_string_lossy(),
        ],
    );
    std::fs::write(tmp.path().join("wt-open/new.txt"), "x\n").unwrap();
    git(&tmp.path().join("wt-open"), &["add", "."]);
    git(
        &tmp.path().join("wt-open"),
        &["commit", "-q", "-m", "unmerged"],
    );
    let done = tmp.path().join("wt-done").to_string_lossy().into_owned();
    let open = tmp.path().join("wt-open").to_string_lossy().into_owned();
    let out = run(&mut env, &["dream", "act", "remove-worktree", &done]);
    assert!(out.starts_with("Removed "), "{out}");
    let err = run_err(&mut env, &["dream", "act", "remove-worktree", &open]);
    assert!(err.contains("isn't merged"), "{err}");
    let err = run_err(&mut env, &["dream", "act", "remove-worktree", &app]);
    assert!(err.contains("main checkout is never touched"), "{err}");
    let err = run_err(&mut env, &["dream", "act", "remove-worktree", &wt]);
    assert!(err.contains("own worktree"), "{err}");
    // Branches: merged goes; unmerged, checked out, main and the run's stay.
    run(
        &mut env,
        &["dream", "act", "delete-branch", &app, "done-work"],
    );
    assert!(
        run_err(
            &mut env,
            &["dream", "act", "delete-branch", &app, "open-work"]
        )
        .contains("checked out")
    );
    assert!(
        run_err(&mut env, &["dream", "act", "delete-branch", &app, "main"])
            .contains("never deleted")
    );
    assert!(
        run_err(
            &mut env,
            &["dream", "act", "delete-branch", &app, &r.branch]
        )
        .contains("run #1's branch")
    );
    // An idle worktree's branch comes up to date with main.
    std::fs::write(tmp.path().join("app/main2.txt"), "m\n").unwrap();
    git(Path::new(&app), &["add", "."]);
    git(Path::new(&app), &["commit", "-q", "-m", "main moves"]);
    let out = run(&mut env, &["dream", "act", "rebase", &open]);
    assert!(out.starts_with("Rebased "), "{out}");
    assert!(Path::new(&open).join("main2.txt").exists());
    assert!(run_err(&mut env, &["dream", "act", "cleanup-agent", "act-1"]).contains("own agent"));

    let out = run(&mut env, &["dream", "act", "done", "fixed; check passes"]);
    assert!(out.contains("is done"), "{out}");
    let r = env.store().get_act_run(1).unwrap().unwrap();
    assert_eq!(r.status, "done");
    for want in [
        "note: the message is in dream.rs",
        "check failed",
        "check ok at",
        "commit ",
        "removed worktree",
        "deleted branch done-work",
        "rebased ",
    ] {
        assert!(r.log.contains(want), "log lacks {want:?}:\n{}", r.log);
    }
    assert!(run_err(&mut env, &["dream", "act", "note", "after"]).contains("no open run"));
    // main was never touched.
    assert_eq!(
        git(Path::new(&app), &["log", "-1", "--format=%s", "main"]),
        "main moves"
    );
}

#[test]
fn an_ask_run_changes_nothing_and_a_branch_run_lands() {
    crate::require_termd!();
    let (mut env, tmp, _spawns) = act_env();
    let app = repo(&tmp.path().join("app"));
    run(&mut env, &["dream", "scope", "add", &app]);
    let f = file_friction(&env, "a question");
    run(
        &mut env,
        &["dream", "act", "start", &f.id.to_string(), "--scope", "ask"],
    );
    be(&mut env, "act-1");
    assert!(run_err(&mut env, &["dream", "act", "check"]).contains("needs fix"));
    assert!(
        run_err(&mut env, &["dream", "act", "start", "1", "--scope", "ask"])
            .contains("a person starts")
    );
    run(
        &mut env,
        &["dream", "act", "ask", "may I change the hook output?"],
    );
    let r = env.store().get_act_run(1).unwrap().unwrap();
    assert_eq!(
        (r.status.as_str(), r.report.as_str()),
        ("asked", "may I change the hook output?")
    );

    env.set_env("AGENT_NAME", "");
    let out = run(&mut env, &["dream", "act", "list", "--all"]);
    assert!(
        out.contains("asked: may I change the hook output?"),
        "{out}"
    );
    run(
        &mut env,
        &[
            "dream",
            "act",
            "start",
            &f.id.to_string(),
            "--scope",
            "branch",
            "--repo",
            &app,
            "--check",
            "true",
        ],
    );
    let wt = env.store().get_act_run(2).unwrap().unwrap().worktree;
    be(&mut env, "act-2");
    assert!(run_err(&mut env, &["dream", "act", "land"]).contains("no commit of its own"));
    std::fs::write(Path::new(&wt).join("fix.txt"), "f\n").unwrap();
    run(&mut env, &["dream", "act", "commit", "the fix"]);
    assert!(run_err(&mut env, &["dream", "act", "land"]).contains("passing check at HEAD"));
    run(&mut env, &["dream", "act", "check"]);
    let out = run(&mut env, &["dream", "act", "land"]);
    assert!(out.contains("on act/landed"), "{out}");
    let head = git(Path::new(&wt), &["rev-parse", "HEAD"]);
    assert_eq!(git(Path::new(&app), &["rev-parse", "act/landed"]), head);
    assert_ne!(
        git(Path::new(&app), &["rev-parse", "main"]),
        head,
        "main stays where it was"
    );
    // A person stops a run; its agent can't.
    assert!(run_err(&mut env, &["dream", "act", "stop", "2"]).contains("give-up"));
    env.set_env("AGENT_NAME", "");
    run(&mut env, &["dream", "act", "stop", "2", "--note", "enough"]);
    assert_eq!(
        env.store().get_act_run(2).unwrap().unwrap().status,
        "abandoned"
    );
}
