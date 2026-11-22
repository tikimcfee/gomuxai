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

fn last_line(out: &str) -> &str {
    out.lines().last().unwrap_or("")
}

#[test]
fn roster_set_list_show_remove() {
    let mut env = env();
    let out = must(&mut env, &["roster"]);
    assert_eq!(
        out,
        "Roster on local: harnesses and models this host can run\nIt has no entries yet.\n\n\
         Next: agent roster set HARNESS/MODEL --tasks KINDS --speed fast|medium|slow --cost low|medium|high adds an entry.\n"
    );

    // sh is on every PATH; the other harness is on none.
    let out = must(
        &mut env,
        &[
            "roster", "set", "sh/fast-model@low", "--speed", "fast", "--cost", "low", "--tasks",
            "edit,test", "--good-at", "small edits",
        ],
    );
    assert_eq!(
        out,
        "Added sh/fast-model@low.\nNext: agent roster show sh/fast-model@low shows it as pick sees it.\n"
    );
    let out = must(&mut env, &["roster", "set", "no-such-cli-x7/m"]);
    assert!(out.contains("--tasks KINDS says which it suits"), "{out}");
    must(&mut env, &["roster", "set", "no-such-cli-x7/m", "--tasks", "review"]);
    let out = must(&mut env, &["roster", "set", "sh/fast-model@low", "--avoid", "debugging"]);
    assert!(out.starts_with("Updated sh/fast-model@low.\n"), "{out}");

    let out = must(&mut env, &["roster", "list"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Roster on local: harnesses and models this host can run");
    assert!(lines[1].starts_with("ENTRY") && lines[1].contains("AVAILABLE"), "{out}");
    assert!(
        lines[2].starts_with("no-such-cli-x7/m")
            && lines[2].contains("off: no-such-cli-x7 not on PATH")
            && lines[2].ends_with("no notes"),
        "{out}"
    );
    assert!(
        lines[3].starts_with("sh/fast-model@low") && lines[3].contains("available") && lines[3].contains("edit,test"),
        "{out}"
    );
    assert!(last_line(&out).starts_with("Next: agent roster pick --task KIND"), "{out}");
    assert_eq!(must(&mut env, &["roster"]), out);

    let out = must(&mut env, &["roster", "disable", "sh/fast-model@low"]);
    assert!(out.starts_with("Disabled sh/fast-model@low: it stays listed"), "{out}");
    let out = must(&mut env, &["roster"]);
    assert!(out.contains("off: disabled"), "{out}");
    assert!(last_line(&out).starts_with("Next: no entry is available"), "{out}");
    let out = must(&mut env, &["roster", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let h = &v["hosts"][0];
    assert_eq!(h["host"], "local");
    assert_eq!(h["entries"][1]["key"], "sh/fast-model@low");
    assert_eq!(h["entries"][1]["available"], false);
    assert_eq!(h["entries"][1]["unavailable"], "disabled");
    assert_eq!(h["entries"][1]["avoid"], "debugging");
    assert_eq!(h["entries"][1]["good_at"], "small edits");
    assert_eq!(v["hosts"].as_array().unwrap().len(), 1);
    must(&mut env, &["roster", "enable", "sh/fast-model@low"]);

    let out = must(&mut env, &["roster", "show", "sh/fast-model@low"]);
    assert!(
        out.starts_with("Roster entry sh/fast-model@low on local\n  availability: available\n"),
        "{out}"
    );
    assert!(out.contains("It has no notes yet."), "{out}");
    assert!(last_line(&out).starts_with("Next: agent roster note sh/fast-model@low"), "{out}");

    let e = fails(&mut env, &["roster", "set", "sh/m", "--speed", "quick"]);
    assert!(e.contains("\"quick\" isn't a speed: use fast, medium, slow"), "{e}");
    let e = fails(&mut env, &["roster", "set", "sh/m", "--tasks", "edit,refactor"]);
    assert!(e.contains("\"refactor\" isn't a task kind"), "{e}");
    let e = fails(&mut env, &["roster", "disable", "x/y"]);
    assert!(e.contains("no roster entry x/y on this host") && e.contains("agent roster set x/y"), "{e}");
    let e = fails(&mut env, &["roster", "show", "x/y"]);
    assert!(e.contains("no roster entry or note for x/y"), "{e}");

    let out = must(&mut env, &["roster", "remove", "no-such-cli-x7/m"]);
    assert!(out.starts_with("Removed no-such-cli-x7/m; its notes stay"), "{out}");
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
    assert_eq!(
        out,
        "Noted #1 on sh/m: good on edit, from claude:3f3a9c00-1111.\nNext: agent roster show sh/m lists its notes.\n"
    );

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
    // Only a dream's own agent writes as a dream, through agent dream roster note.
    let e = fails(
        &mut env,
        &[
            "roster", "note", "sh/m", "--task", "edit", "--verdict", "good", "--session",
            "claude:3f3a", "--by", "dream-7", "x",
        ],
    );
    assert!(e.contains("agent dream roster note"), "{e}");

    // A note on a harness and model with no entry is kept, and listed.
    let out = must(
        &mut env,
        &[
            "roster", "note", "kimi/k2", "--task", "chat", "--verdict", "mixed", "--session",
            "claude:3f3a9c00-1111", "slow to answer",
        ],
    );
    assert!(out.contains("kimi/k2 has no roster entry, so pick won't offer it."), "{out}");
    let out = must(
        &mut env,
        &[
            "roster", "note", "sh/m", "--task", "edit", "--verdict", "bad", "--session", "claude:3f3a",
            "--supersedes", "1", "it missed a caller after all",
        ],
    );
    assert!(out.contains("(supersedes #1, which stops counting)"), "{out}");

    let out = must(&mut env, &["roster"]);
    assert!(
        out.contains("Notes on harnesses and models with no entry (pick doesn't offer them):\n  kimi/k2  1 mixed from 1 session\n"),
        "{out}"
    );
    let out = must(&mut env, &["roster", "show", "sh/m"]);
    assert!(out.contains("  notes:        1 bad from 1 session (superseded notes don't count)\n"), "{out}");
    assert!(
        out.contains("#1 ") && out.contains("good on edit  by lead  from claude:3f3a9c00-1111  (superseded by #3)"),
        "{out}"
    );
    assert!(out.contains("      renamed it cleanly\n      why: small and precise\n"), "{out}");
    assert!(out.contains("supersedes #1"), "{out}");
    let v: serde_json::Value = serde_json::from_str(&must(&mut env, &["roster", "show", "sh/m", "--json"])).unwrap();
    assert_eq!(v["notes"][0]["superseded_by"], 3);
    assert_eq!(v["entry"]["notes"]["bad"]["notes"], 1);
    assert_eq!(v["entry"]["notes"]["sessions"], 1);
    let out = must(&mut env, &["roster", "show", "kimi/k2"]);
    assert!(out.starts_with("Roster entry kimi/k2 on local: none, only notes"), "{out}");
    assert!(last_line(&out).contains("agent roster set kimi/k2 --tasks KINDS adds it"), "{out}");
}

#[test]
fn roster_pick() {
    let mut env = env();
    let e = fails(&mut env, &["roster", "pick"]);
    assert!(e.contains("--task is required") && e.contains("long-horizon") && e.contains("--saved"), "{e}");
    let e = fails(&mut env, &["roster", "pick", "--task", "edit"]);
    assert!(e.contains("no entries to pick from"), "{e}");

    must(&mut env, &["roster", "set", "sh/small", "--tasks", "edit", "--speed", "fast", "--cost", "low"]);
    must(&mut env, &["roster", "set", "sh/", "--tasks", "edit", "--speed", "slow", "--cost", "high"]);
    must(&mut env, &["roster", "set", "sh/mid", "--tasks", "feature", "--speed", "medium"]);
    must(&mut env, &["roster", "set", "sh/other", "--tasks", "review"]);
    must(&mut env, &["roster", "set", "no-such-cli-x7/m", "--tasks", "edit"]);

    let out = must(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "fast"]);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Picked for edit: sh/small on local");
    assert_eq!(lines[1], "  agent spawn NAME --backend sh --model small");
    assert_eq!(lines[2], "  why: tasks include edit; no notes on edit; fast, low cost");
    assert_eq!(lines[4], "Runners-up:");
    // The default model spawns without --model.
    assert_eq!(lines[5], "  2. sh/ on local: agent spawn NAME --backend sh");
    assert_eq!(
        last_line(&out),
        "Next: run the spawn line with your agent's name for NAME; agent roster show sh/small lists its notes; add --save NAME to keep this command."
    );

    let out = must(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "strong", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["task"], "edit");
    assert_eq!(v["prefer"], "strong");
    assert_eq!(v["pick"]["key"], "sh/");
    assert_eq!(v["pick"]["host"], "local");
    assert_eq!(v["pick"]["argv"], serde_json::json!(["agent", "spawn", "NAME", "--backend", "sh"]));
    assert_eq!(v["runners_up"].as_array().unwrap().len(), 2);
    assert!(v.get("saved").is_none());

    let e = fails(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "best"]);
    assert!(e.contains("\"best\" isn't a preference: use fast, cheap, strong"), "{e}");
    let out = must(&mut env, &["roster", "pick", "--task", "kernel"]);
    assert!(out.contains("Nothing on record suits kernel"), "{out}");
}

#[test]
fn roster_saved_picks() {
    let mut env = env();
    must(&mut env, &["roster", "set", "sh/small", "--tasks", "edit", "--speed", "fast", "--cost", "low"]);
    must(&mut env, &["roster", "set", "sh/big", "--tasks", "edit", "--speed", "slow", "--cost", "high"]);

    let out = must(&mut env, &["roster", "picks"]);
    assert!(out.contains("There are none yet.") && last_line(&out).contains("--save NAME"), "{out}");

    let out = must(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "fast", "--save", "quick-edit"]);
    assert!(
        out.contains("Saved as quick-edit@1 (a new saved pick): agent roster pick --saved quick-edit prints it again.\n"),
        "{out}"
    );
    assert!(!last_line(&out).contains("--save"), "{out}");
    let out = must(&mut env, &["roster", "pick", "--task", "edit", "--prefer", "fast", "--save", "quick-edit"]);
    assert!(out.contains("Not saved: quick-edit@1 already has this command."), "{out}");
    let out = must(
        &mut env,
        &["roster", "pick", "--task", "edit", "--prefer", "strong", "--save", "quick-edit", "--json"],
    );
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["saved"]["outcome"], "new_version");
    assert_eq!(v["saved"]["pick"]["version"], 2);

    let out = must(&mut env, &["roster", "picks"]);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("Saved picks on local: startup commands kept with pick --save"), "{out}");
    assert!(lines[1].starts_with("NAME") && lines[1].contains("COMMAND"), "{out}");
    assert!(
        lines[2].starts_with("quick-edit@2") && lines[2].contains("sh/big") && lines[2].ends_with("agent spawn NAME --backend sh --model big"),
        "{out}"
    );
    let out = must(&mut env, &["roster", "picks", "quick-edit"]);
    assert!(out.starts_with("Saved pick quick-edit on local: every version, newest first\n"), "{out}");
    assert!(out.contains("@2 ") && out.contains("@1 "), "{out}");
    let v: serde_json::Value = serde_json::from_str(&must(&mut env, &["roster", "picks", "--json"])).unwrap();
    assert_eq!(v["picks"][0]["name"], "quick-edit");
    let e = fails(&mut env, &["roster", "picks", "nope"]);
    assert!(e.contains("no saved pick nope"), "{e}");

    // Replayed exactly, whatever the roster says now.
    must(&mut env, &["roster", "disable", "sh/small"]);
    let out = must(&mut env, &["roster", "pick", "--saved", "quick-edit@1"]);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("Saved pick quick-edit@1: for edit, saved by "), "{out}");
    assert_eq!(lines[1], "  agent spawn NAME --backend sh --model small");
    assert!(out.contains("Its entry sh/small on local now: off: disabled.\n"), "{out}");
    assert!(last_line(&out).contains("agent roster pick --task edit picks again"), "{out}");
    let out = must(&mut env, &["roster", "pick", "--saved", "quick-edit"]);
    assert!(out.contains("--model big") && out.contains("now: available."), "{out}");
    let v: serde_json::Value =
        serde_json::from_str(&must(&mut env, &["roster", "pick", "--saved", "quick-edit@1", "--json"])).unwrap();
    assert_eq!(v["available"], false);
    assert_eq!(v["status"], "off: disabled");

    let e = fails(&mut env, &["roster", "pick", "--saved", "quick-edit@9"]);
    assert!(e.contains("has no version 9: it has 1 to 2"), "{e}");
    let e = fails(&mut env, &["roster", "pick", "--saved", "quick-edit", "--task", "edit"]);
    assert!(e.contains("saved") && e.contains("task"), "{e}");
    let e = fails(&mut env, &["roster", "pick", "--task", "edit", "--save", "bad name"]);
    assert!(e.contains("can't name a saved pick"), "{e}");
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

// Across hosts: the hub reads its own roster and the leaf's over the bus,
// and lists "old" (whose bus serves only list) and "gone" (no bus) as not
// read, with the fix, never as an error.
#[test]
fn roster_across_hosts() {
    crate::require_termd!();
    let Some(mut h) = crate::cli::fleet::fleet_test::TwoHosts::new(false) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    let (_, _, res) = h.leaf.run(&[
        "roster", "set", "sh/leafy", "--tasks", "edit", "--speed", "fast", "--cost", "low",
    ]);
    res.unwrap();
    must(&mut h.hub, &["roster", "set", "sh/hubby", "--tasks", "review"]);

    let out = must(&mut h.hub, &["roster"]);
    eprintln!("agent roster (on hub):\n{out}");
    for want in [
        "Roster on hub: harnesses and models this host can run\n",
        "Roster on leaf: harnesses and models leaf can run\n",
        "Roster on old: not read: its bus doesn't answer roster",
        "  fix: on old: agent bus restart --remote-ops <its list>,roster",
        "Roster on gone: not read: ",
    ] {
        assert!(out.contains(want), "want {want:?} in:\n{out}");
    }
    assert!(out.starts_with("Roster on hub:"), "this host first:\n{out}");
    let leaf_part = &out[out.find("Roster on leaf").unwrap()..];
    assert!(leaf_part.contains("sh/leafy") && leaf_part.contains("available"), "{out}");
    assert_eq!(h.hub.take_err(), "", "no hint with a bus running");

    let v: serde_json::Value = serde_json::from_str(&must(&mut h.hub, &["roster", "--json"])).unwrap();
    let mut hosts: Vec<(String, bool)> = v["hosts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| (x["host"].as_str().unwrap().to_string(), x.get("error").is_none()))
        .collect();
    hosts.sort();
    let want: Vec<(String, bool)> =
        [("gone", false), ("hub", true), ("leaf", true), ("old", false)]
            .iter()
            .map(|(h, r)| (h.to_string(), *r))
            .collect();
    assert_eq!(hosts, want);

    // pick ranks every read host's entries; the leaf's spawns there.
    let out = must(&mut h.hub, &["roster", "pick", "--task", "edit"]);
    eprintln!("agent roster pick (on hub):\n{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "Picked for edit: sh/leafy on leaf");
    assert_eq!(lines[1], "  agent spawn NAME@leaf --backend sh --model leafy");
    assert!(
        out.contains("Not read, so not in the running:\n") && out.contains("\n  old: its bus doesn't answer roster"),
        "{out}"
    );
    assert!(last_line(&out).contains("agent roster show sh/leafy on leaf lists its notes"), "{out}");
    let v: serde_json::Value =
        serde_json::from_str(&must(&mut h.hub, &["roster", "pick", "--task", "edit", "--json"])).unwrap();
    assert_eq!(v["pick"]["host"], "leaf");
    assert_eq!(v["unread"].as_array().unwrap().len(), 2);

    // spawn --pick: the same pick, spawned on the leaf as NAME@leaf; the
    // leaf's bus doesn't serve spawn (the default ops leave it out), and
    // spawn says so plainly after saying what it picked.
    let (out, res) = h.hub.run(&["spawn", "w", "--pick", "edit"]);
    let e = format!("{:#}", res.expect_err("the leaf doesn't serve spawn"));
    assert!(
        out.starts_with("Picked sh/leafy on leaf for edit: tasks include edit"),
        "{out}"
    );
    assert!(
        e.starts_with("the pick runs on leaf, so leaf was asked to spawn w and didn't: a host starts agents for others only if its bus's --remote-ops lists spawn")
            && e.contains("agent spawn w@hub --pick edit picks from this host only"),
        "{e}"
    );

    // --host: one host, through the bus; a linked one that doesn't serve
    // roster is a row; one nobody knows is an error.
    let out = must(&mut h.hub, &["roster", "--host", "LEAF"]);
    assert!(out.starts_with("Roster on leaf:") && !out.contains("hub"), "{out}");
    let out = must(&mut h.hub, &["roster", "--host", "old"]);
    assert!(out.starts_with("Roster on old: not read:") && out.contains("fix: on old"), "{out}");
    let e = fails(&mut h.hub, &["roster", "--host", "typo"]);
    assert!(e.contains("no bus answering for host \"typo\""), "{e}");
    let out = must(&mut h.hub, &["roster", "pick", "--task", "review", "--host", "hub"]);
    assert!(out.starts_with("Picked for review: sh/hubby on hub\n  agent spawn NAME --backend sh"), "{out}");

    // A saved pick from the leaf is checked on the leaf.
    must(&mut h.hub, &["roster", "pick", "--task", "edit", "--save", "leafy"]);
    let out = must(&mut h.hub, &["roster", "pick", "--saved", "leafy"]);
    assert!(out.contains("  agent spawn NAME@leaf --backend sh --model leafy\n"), "{out}");
    assert!(out.contains("Its entry sh/leafy on leaf now: available."), "{out}");
    let (_, _, res) = h.leaf.run(&["roster", "disable", "sh/leafy"]);
    res.unwrap();
    let out = must(&mut h.hub, &["roster", "pick", "--saved", "leafy"]);
    assert!(out.contains("Its entry sh/leafy on leaf now: off: disabled."), "{out}");
}

// No bus: this host's roster, each linked host not read, and a hint.
#[test]
fn roster_without_bus() {
    let mut env = env();
    env.no_bus();
    crate::cli::remote_test::trust_peers(&env, &["hub"]);
    must(&mut env, &["roster", "set", "sh/m", "--tasks", "edit"]);
    let out = must(&mut env, &["roster"]);
    assert!(
        out.contains("Roster on hub: not read: no agent bus runs here\n  fix: here: agent bus start\n"),
        "{out}"
    );
    assert!(env.take_err().contains("no agent bus runs here, so this is this host only, without hub"));
    let out = must(&mut env, &["roster", "pick", "--task", "edit"]);
    assert!(out.starts_with("Picked for edit: sh/m on local"), "{out}");
}

// Plan limits from this host's llm-exp snapshot ($HOME/.cache/llm-exp/...):
// near the limit in the list and ranked down, at it off; show says limits
// it doesn't know.
#[test]
fn roster_plan_limits() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd_private();
    let home = env.dir.path().join("home");
    let bin = env.dir.path().join("bin");
    std::fs::create_dir_all(home.join(".cache/llm-exp")).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    for cli in ["claude", "agy", "aider"] {
        let p = bin.join(cli);
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let now = chrono::Utc::now();
    let reset = (now + chrono::TimeDelta::hours(30)).to_rfc3339();
    std::fs::write(
        home.join(".cache/llm-exp/usage-snapshot.json"),
        format!(
            r#"{{"captured_at": "{}",
                "claude": {{"session": {{"pct": 10}}, "week_all": {{"pct": 50, "reset_at": "{reset}"}},
                           "week_model": {{"pct": 93, "reset_at": "{reset}"}}, "model_name": "Fable"}},
                "agy": {{"bars": [{{"label": "gemini week", "pct": 100, "reset_at": "{reset}"}}]}}}}"#,
            now.to_rfc3339()
        ),
    )
    .unwrap();
    env.set_env("HOME", &home);
    env.set_env("PATH", &bin);
    let when = agent_roster::limits::when(&reset, now);

    for (key, tasks) in [
        ("claude/claude-fable-5-2", "edit"),
        ("claude/claude-opus-5-5", "edit"),
        ("agy/gemini", "edit"),
        ("aider/x", "review"),
    ] {
        must(&mut env, &["roster", "set", key, "--tasks", tasks]);
    }
    let out = must(&mut env, &["roster"]);
    eprintln!("{out}");
    let line = |k: &str| out.lines().find(|l| l.starts_with(k)).unwrap().to_string();
    assert!(line("claude/claude-fable-5-2").contains(&format!("near limit: week fable 93%, resets {when}")), "{out}");
    assert!(line("claude/claude-opus-5-5").contains("  available  "), "{out}");
    assert!(line("agy/gemini").contains(&format!("off: at limit until {when}")), "{out}");
    assert!(!out.contains("limits not known"), "the list doesn't say it:\n{out}");

    let out = must(&mut env, &["roster", "show", "aider/x"]);
    assert!(out.contains("  limits:       limits not known\n"), "{out}");
    let out = must(&mut env, &["roster", "show", "claude/claude-opus-5-5"]);
    assert!(out.contains(&format!("  limits:       week 50%, resets {when}\n")), "{out}");

    let out = must(&mut env, &["roster", "pick", "--task", "edit"]);
    assert!(out.starts_with("Picked for edit: claude/claude-opus-5-5 on local\n"), "{out}");
    // Near its limit, fable falls from a tie with opus to one with aider/x
    // (which doesn't list edit), and sorts after it by name.
    assert!(out.contains("3. claude/claude-fable-5-2 on local"), "{out}");
    assert!(!out.contains("agy/gemini"), "at its limit, not picked:\n{out}");

    let v: serde_json::Value = serde_json::from_str(&must(&mut env, &["roster", "--json"])).unwrap();
    let h = &v["hosts"][0];
    assert!(h["limits_as_of"].as_str().is_some_and(|s| !s.is_empty()), "{h}");
    let fable = h["entries"].as_array().unwrap().iter().find(|e| e["key"] == "claude/claude-fable-5-2").unwrap();
    assert_eq!(fable["near_limit"], true);
    assert_eq!(fable["limit"]["pct"], 93);
    assert_eq!(fable["limit"]["window"], "week fable");
}
