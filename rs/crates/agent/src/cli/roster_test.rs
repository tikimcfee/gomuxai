//! agent roster (no Go file).
use crate::cli::testenv::TestEnv;

/// A CLI whose sessions are under its temp dir, with one Claude session,
/// 3f3a9c00-1111.
fn env() -> TestEnv {
    let mut env = TestEnv::new();
    let projects = env.dir.path().join("projects");
    let dir = projects.join("-work");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("3f3a9c00-1111.jsonl"),
        "{\"type\":\"user\",\"sessionId\":\"3f3a9c00-1111\",\"cwd\":\"/work\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();
    let mut cfg = env.cfg.clone();
    cfg.claude_projects_dir = Some(projects.to_string_lossy().into_owned());
    cfg.agy_brain_dir = Some(String::new());
    cfg.kimi_sessions_dir = Some(String::new());
    env.cfg = cfg.clone();
    env.ctx.base_cfg = Some(cfg.clone());
    env.ctx.set_cfg(cfg);
    env
}

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

#[test]
fn roster_set_list_show_remove() {
    let mut env = env();
    let out = must(&mut env, &["roster"]);
    assert_eq!(out, "The roster on local is empty.\n");
    assert!(env.take_err().contains("agent roster set"));

    // sh is on every PATH; the other harness is on none.
    let out = must(
        &mut env,
        &[
            "roster", "set", "sh/fast-model@low", "--speed", "fast", "--cost", "low", "--tasks",
            "edit,test", "--good-at", "small edits",
        ],
    );
    assert_eq!(out, "Added sh/fast-model@low.\n");
    must(&mut env, &["roster", "set", "no-such-cli-x7/m", "--tasks", "review"]);
    let out = must(&mut env, &["roster", "set", "sh/fast-model@low", "--avoid", "debugging"]);
    assert_eq!(out, "Updated sh/fast-model@low.\n");

    let out = must(&mut env, &["roster", "list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("ENTRY") && lines[0].contains("AVAILABLE"), "{out}");
    assert!(lines[1].starts_with("no-such-cli-x7/m") && lines[1].contains("no: no-such-cli-x7 isn't on PATH"), "{out}");
    assert!(lines[2].starts_with("sh/fast-model@low") && lines[2].contains("yes") && lines[2].contains("edit,test"), "{out}");
    assert_eq!(must(&mut env, &["roster"]), out);

    let out = must(&mut env, &["roster", "disable", "sh/fast-model@low"]);
    assert_eq!(out, "Disabled sh/fast-model@low.\n");
    let out = must(&mut env, &["roster", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["host"], "local");
    assert_eq!(v["entries"][1]["key"], "sh/fast-model@low");
    assert_eq!(v["entries"][1]["available"], false);
    assert_eq!(v["entries"][1]["unavailable"], "disabled");
    assert_eq!(v["entries"][1]["avoid"], "debugging");
    assert_eq!(v["entries"][1]["good_at"], "small edits");
    must(&mut env, &["roster", "enable", "sh/fast-model@low"]);

    let out = must(&mut env, &["roster", "show", "sh/fast-model@low"]);
    assert!(out.starts_with("sh/fast-model@low on local\n  available: yes\n"), "{out}");
    assert!(out.contains("No notes yet"), "{out}");

    let e = fails(&mut env, &["roster", "set", "sh/m", "--speed", "quick"]);
    assert!(e.contains("\"quick\" isn't a speed: use fast, medium, slow"), "{e}");
    let e = fails(&mut env, &["roster", "set", "sh/m", "--tasks", "edit,refactor"]);
    assert!(e.contains("\"refactor\" isn't a task kind"), "{e}");
    let e = fails(&mut env, &["roster", "disable", "x/y"]);
    assert!(e.contains("no roster entry x/y on this host") && e.contains("agent roster set x/y"), "{e}");
    let e = fails(&mut env, &["roster", "show", "x/y"]);
    assert!(e.contains("no roster entry or note for x/y"), "{e}");

    let out = must(&mut env, &["roster", "remove", "no-such-cli-x7/m"]);
    assert_eq!(out, "Removed no-such-cli-x7/m; its notes stay.\n");
    assert!(fails(&mut env, &["roster", "remove", "no-such-cli-x7/m"]).contains("no roster entry"));
}

#[test]
fn roster_note_checks_the_session() {
    let mut env = env();
    must(&mut env, &["roster", "set", "sh/m", "--tasks", "edit"]);

    let out = must(
        &mut env,
        &[
            "roster", "note", "sh/m", "--task", "edit", "--verdict", "good", "--session",
            "claude:3f3a", "--by", "lead", "renamed it cleanly", "--why", "small and precise",
        ],
    );
    assert_eq!(out, "Noted #1 on sh/m: good on edit, from claude:3f3a9c00-1111.\n");

    let e = fails(
        &mut env,
        &["roster", "note", "sh/m", "--task", "edit", "--verdict", "good", "--session", "claude:beef", "x"],
    );
    assert!(e.contains("no claude session on this host is beef") && e.contains("agent sessions --harness claude"), "{e}");
    let e = fails(&mut env, &["roster", "note", "sh/m", "--task", "edit", "--verdict", "good", "x"]);
    assert!(e.contains("--session is required") && e.contains("agent sessions"), "{e}");
    let e = fails(
        &mut env,
        &["roster", "note", "sh/m", "--task", "fix", "--verdict", "good", "--session", "claude:3f3a", "x"],
    );
    assert!(e.contains("\"fix\" isn't a task kind: use edit, bugfix"), "{e}");

    // A note on a harness and model with no entry is kept, and listed.
    must(
        &mut env,
        &[
            "roster", "note", "kimi/k2", "--task", "chat", "--verdict", "mixed", "--session",
            "claude:3f3a9c00-1111", "slow to answer",
        ],
    );
    assert!(env.take_err().contains("kimi/k2 has no roster entry"));
    must(
        &mut env,
        &[
            "roster", "note", "sh/m", "--task", "edit", "--verdict", "bad", "--session", "claude:3f3a",
            "--supersedes", "1", "it missed a caller after all",
        ],
    );

    let out = must(&mut env, &["roster"]);
    assert!(out.contains("Notes on harnesses and models with no entry:\n  kimi/k2  1 mixed\n"), "{out}");
    let out = must(&mut env, &["roster", "show", "sh/m"]);
    assert!(out.contains("  notes:     1 bad\n"), "{out}");
    assert!(out.contains("#1 ") && out.contains("by lead  from claude:3f3a9c00-1111  (superseded by #3)"), "{out}");
    assert!(out.contains("      renamed it cleanly\n      why: small and precise\n"), "{out}");
    assert!(out.contains("supersedes #1"), "{out}");
    let v: serde_json::Value = serde_json::from_str(&must(&mut env, &["roster", "show", "sh/m", "--json"])).unwrap();
    assert_eq!(v["notes"][0]["superseded_by"], 3);
    assert_eq!(v["entry"]["notes"]["bad"]["notes"], 1);
}

#[test]
fn roster_pick() {
    let mut env = env();
    let e = fails(&mut env, &["roster", "pick"]);
    assert!(e.contains("--task is required") && e.contains("long-horizon"), "{e}");
    let e = fails(&mut env, &["roster", "pick", "--task", "edit"]);
    assert!(e.contains("no entries to pick from"), "{e}");

    must(&mut env, &["roster", "set", "sh/small", "--tasks", "edit", "--speed", "fast", "--cost", "low"]);
    must(&mut env, &["roster", "set", "sh/", "--tasks", "edit", "--speed", "slow", "--cost", "high"]);
    must(&mut env, &["roster", "set", "sh/mid", "--tasks", "feature", "--speed", "medium"]);
    must(&mut env, &["roster", "set", "sh/other", "--tasks", "review"]);
    must(&mut env, &["roster", "set", "no-such-cli-x7/m", "--tasks", "edit"]);

    let out = must(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "fast"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "agent spawn NAME --backend sh --model small");
    assert!(lines[1].starts_with("  sh/small: suits edit; no notes on edit; fast, low cost"), "{out}");
    assert_eq!(lines[3], "Runners-up:");
    // The default model spawns without --model.
    assert_eq!(lines[4], "  agent spawn NAME --backend sh");
    assert_eq!(lines.len(), 8, "{out}");

    let out = must(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "strong", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["task"], "edit");
    assert_eq!(v["prefer"], "strong");
    assert_eq!(v["pick"]["key"], "sh/");
    assert_eq!(v["pick"]["host"], "local");
    assert_eq!(v["runners_up"].as_array().unwrap().len(), 2);

    let e = fails(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "best"]);
    assert!(e.contains("\"best\" isn't a preference: use fast, cheap, strong"), "{e}");
    must(&mut env, &["roster", "pick", "--task", "kernel"]);
    assert!(env.take_err().contains("nothing on record suits kernel"));
}

#[test]
fn task_flag_help_names_the_kinds() {
    let want = format!("Task kind: {}", agent_roster::TASK_KINDS.join(", "));
    for def in [super::roster_pick_def(), super::roster_note_def()] {
        let cmd = def.clap_command();
        let help = cmd
            .get_arguments()
            .find(|a| a.get_long() == Some("task"))
            .and_then(|a| a.get_help())
            .map(|h| h.to_string())
            .unwrap();
        assert_eq!(help, want, "{}", def.name());
    }
}
