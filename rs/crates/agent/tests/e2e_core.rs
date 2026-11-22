//! The built binary through the core loop, end to end, with the `shell`
//! backend: one scenario per test (tests/e2e/harness.rs says how each runs
//! isolated from the live system).

#[path = "e2e/harness.rs"]
mod harness;

use harness::{Host, wait_for};

/// Spawns a shell agent named name and waits for its shell to answer.
fn spawn_shell(h: &Host, name: &str) {
    let o = h.run(&["spawn", name, "--backend", "shell"]).ok("spawn");
    assert_eq!(
        o.stdout,
        format!(
            "Spawned agent {name} (session agent-{name}, backend shell).\nIt has no task yet: agent send {name} \"...\"\nIts status is unknown so far: agent wait {name} --for idle --timeout 1m waits until it's ready.\n"
        )
    );
    h.type_line(name, "echo ready-$((40+2))");
    h.wait_line(name, "ready-42");
}

// The termd the binary drives is the test's private one: the session it
// makes is in the private socket dir, in the test's namespace.
#[test]
fn e2e_session_new_list_kill_in_the_private_termd() {
    require_termd!();
    let h = Host::new("hub");
    let o = h.run(&["session", "new", "dev-shell"]).ok("session new");
    assert_eq!(o.stdout, "Created session dev-shell.\n");
    let sock = h.termd_dir().join(format!("termd-{}.sock", h.ns));
    assert!(sock.exists(), "no socket at {}", sock.display());
    let o = h.run(&["session", "list"]).ok("session list");
    let rows: Vec<Vec<&str>> = o
        .stdout
        .lines()
        .map(|l| l.split_whitespace().collect())
        .collect();
    assert_eq!(rows[0], ["NAME", "AGENT", "ATTACHED"], "{}", o.stdout);
    assert!(rows.contains(&vec!["dev-shell", "-", "no"]), "{}", o.stdout);

    let o = h.run(&["session", "kill", "dev-shell"]).ok("session kill");
    assert_eq!(o.stdout, "Killed session dev-shell.\n");
    let o = h.run(&["session", "kill", "dev-shell"]);
    assert_eq!(o.code, 1);
    assert_eq!(
        o.stderr,
        "Error: no session \"dev-shell\" (agent session list shows them)\n"
    );
}

// spawn, list, send, keys, key, capture (and --raw), status (-v, --json),
// report, wait, logs, cleanup.
#[test]
fn e2e_shell_agent_core_loop() {
    require_termd!();
    let h = Host::new("hub");
    spawn_shell(&h, "w");

    let o = h.run(&["list"]).ok("list");
    assert!(
        o.stdout
            .starts_with("w: unknown (shell)\n  session: agent-w\n  created: "),
        "{}",
        o.stdout
    );

    // A person's message is tagged as theirs; the shell runs what follows.
    let o = h.run(&["send", "w", "; echo sent-$((40+2))"]).ok("send");
    assert_eq!(o.stdout, "Sent to w (its status is unknown).\n");
    let screen = h.wait_line("w", "sent-42");
    assert!(
        screen.contains("[from a person on hub] ; echo sent-$((40+2))"),
        "{screen}"
    );

    let o = h
        .run(&["keys", "agent-w", "echo keys-$((40+2))"])
        .ok("keys");
    assert_eq!(o.stdout, "Typed into session agent-w and pressed Enter.\n");
    h.wait_line("w", "keys-42");
    // A line typed without Enter, then Ctrl-C drops it: it never runs.
    h.run(&["keys", "agent-w", "--no-enter", "echo never-$((40+2))"]);
    let o = h.run(&["key", "agent-w", "C-c"]).ok("key");
    assert_eq!(o.stdout, "Pressed C-c in session agent-w.\n");
    h.type_line("w", "echo after-$((40+2))");
    let screen = h.wait_line("w", "after-42");
    assert!(!screen.lines().any(|l| l.trim() == "never-42"), "{screen}");

    let o = h
        .run(&["capture", "w", "--lines", "50", "--raw"])
        .ok("capture --raw");
    assert!(o.stdout.contains("after-42"), "{}", o.stdout);

    let o = h.run(&["status", "w", "-v"]).ok("status -v");
    assert_eq!(
        o.stdout,
        "w: unknown\n  means:  nothing reliable says yet (a CLI still starting, or a harness with no status of its own)\n  source: scrape\n"
    );
    assert_eq!(
        o.stderr,
        "hint: its backend doesn't report status and its screen matched no known prompt: agent capture w shows the screen\n"
    );

    let o = h
        .run(&["report", "processing", "running tests", "--as", "w"])
        .ok("report");
    assert_eq!(o.stdout, "Reported w processing (running tests).\n");
    let o = h.run(&["status", "w", "--json"]).ok("status --json");
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        (
            &v["agent"],
            &v["host"],
            &v["status"],
            &v["source"],
            &v["event"],
            &v["detail"]
        ),
        (
            &"w".into(),
            &"hub".into(),
            &"processing".into(),
            &"report".into(),
            &"report".into(),
            &"running tests".into()
        ),
        "{}",
        o.stdout
    );

    // wait: met at once; a timeout exits 2.
    let o = h.run(&["report", "idle", "--as", "w"]).ok("report idle");
    assert_eq!(o.stdout, "Reported w idle.\n");
    let o = h
        .run(&["wait", "w", "--for", "idle", "--timeout", "10s"])
        .ok("wait");
    assert_eq!(o.stdout, "w: idle\n");
    let o = h.run(&["wait", "w", "--for", "waiting", "--timeout", "1s"]);
    assert_eq!(o.code, 2, "{o:?}");
    assert_eq!(
        o.stderr,
        "Error: timed out after 1s waiting for w to be waiting (last status: idle)\n"
    );

    // logs: a shell agent keeps no transcript, so its terminal log.
    let o = h.run(&["logs", "w", "--lines", "20"]).ok("logs");
    assert!(o.stdout.contains("after-42"), "{}", o.stdout);
    assert_eq!(
        o.stderr,
        "hint: printing w's terminal log: no transcript recorded for it\n"
    );
    let o = h.run(&["transcript", "w"]);
    assert_eq!(o.code, 1);
    assert_eq!(
        o.stderr,
        "Error: agent w has no transcript yet: a claude or agy agent's hooks name it at its first prompt, and other backends keep none (agent logs w --terminal prints its terminal log)\n"
    );

    // Not idle, so cleanup kills the session after snapshotting its screen.
    // (An idle shell agent gets the shell's exit typed first, and if the pane
    // is gone before the snapshot, no log stays: the Go's Cleanup does the
    // same.)
    h.run(&["report", "processing", "--as", "w"]).ok("report");
    let o = h.run(&["cleanup", "w"]).ok("cleanup");
    assert_eq!(
        o.stdout,
        "cleaned up \"w\": killed session agent-w; removed its record, state, queued messages, and files; its log stays (agent logs w)\n"
    );
    let o = h.run(&["session", "list"]).ok("session list");
    assert!(!o.stdout.contains("agent-w"), "{}", o.stdout);
    // The log outlives the agent.
    let o = h
        .run(&["logs", "w", "--lines", "50"])
        .ok("logs after cleanup");
    assert!(o.stdout.contains("after-42"), "{}", o.stdout);
}

// queue holds a message until a watcher delivers it; watch starts, says
// what it watches, delivers, and stops cleanly on SIGTERM; deliver then has
// nothing held.
#[test]
fn e2e_queue_watch_and_deliver() {
    require_termd!();
    let h = Host::new("hub");
    spawn_shell(&h, "w");
    h.run(&["report", "idle", "--as", "w"]).ok("report");

    let o = h
        .run(&["queue", "w", "; echo queued-$((40+2))"])
        .ok("queue");
    assert_eq!(o.stdout, "Queued for w from user (it is idle).\n");
    assert_eq!(
        o.stderr,
        "hint: no watcher is running, so it won't be delivered until one starts: agent watch (or agent send w now)\n"
    );
    let o = h.run(&["status", "w", "-v"]).ok("status -v");
    assert!(
        o.stdout
            .contains("  queued: 1 message, delivered by agent watch when it's idle\n"),
        "{}",
        o.stdout
    );

    let mut watch = h.start("watch", &["watch", "--interval", "200ms"]);
    wait_for("the watcher's banner", || {
        let s = watch.stdout();
        let ok = !s.is_empty();
        (s, ok)
    });
    assert_eq!(
        watch.stdout(),
        "Watching 1 agent on host hub every 200ms; 1 message queued. Ctrl-C stops.\n"
    );
    h.wait_line("w", "queued-42");
    wait_for("the watcher to log the delivery", || {
        let s = watch.stderr();
        let ok = s.contains("message delivered successfully agent=w");
        (s, ok)
    });
    // With a watcher running, queue doesn't say none is.
    let o = h
        .run(&["queue", "w", "; echo again-$((40+2))"])
        .ok("queue with a watcher");
    assert_eq!(o.stdout, "Queued for w from user (it is idle).\n");
    assert_eq!(o.stderr, "", "a watcher is running: no hint");
    h.wait_line("w", "again-42");

    let code = watch.stop(libc::SIGTERM);
    assert_eq!(code, 0, "watch on SIGTERM; stderr:\n{}", watch.stderr());
    assert!(
        watch.stdout().ends_with("\nWatcher stopped\n"),
        "{}",
        watch.stdout()
    );
    assert!(
        watch
            .stderr()
            .contains("INFO watcher stopping: stop signal received"),
        "{}",
        watch.stderr()
    );

    // deliver types what is held for an agent at its prompt now.
    h.run(&["queue", "w", "; echo delivered-$((40+2))"])
        .ok("queue");
    let o = h.run(&["deliver", "w"]).ok("deliver");
    assert_eq!(o.stdout, "Typed 1 message held for w.\n");
    h.wait_line("w", "delivered-42");
    let o = h.run(&["deliver", "w"]).ok("deliver again");
    assert_eq!(o.stdout, "Nothing is held for w.\n");
}

// state set/get/list/clear, statuses, version.
#[test]
fn e2e_state_statuses_version() {
    require_termd!();
    let h = Host::new("hub");
    spawn_shell(&h, "w");

    let o = h
        .run(&["state", "set", "w", "progress", "3/5 files"])
        .ok("state set");
    assert_eq!(o.stdout, "Set w.progress = \"3/5 files\".\n");
    assert_eq!(
        h.run(&["state", "get", "w", "progress"]).stdout,
        "3/5 files\n"
    );
    let o = h
        .run(&["state", "get", "w", "progress", "--json"])
        .ok("state get --json");
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        (&v["agent"], &v["key"], &v["value"]),
        (&"w".into(), &"progress".into(), &"3/5 files".into())
    );
    let o = h
        .run(&["state", "set", "w", "long", &"x".repeat(40)])
        .ok("state set long");
    assert_eq!(o.code, 0);
    let o = h.run(&["state", "list", "w"]).ok("state list");
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert_eq!(
        lines[0].split_whitespace().collect::<Vec<_>>(),
        ["KEY", "VALUE", "UPDATED"]
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("progress             3/5 files ")),
        "{}",
        o.stdout
    );
    // A long value is cut to fit its column.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with(&format!("long                 {}... ", "x".repeat(27)))),
        "{}",
        o.stdout
    );
    let o = h
        .run(&["state", "list", "w", "--json"])
        .ok("state list --json");
    let v: Vec<serde_json::Value> = serde_json::from_str(&o.stdout).unwrap();
    assert!(
        v.iter()
            .any(|e| e["key"] == "long" && e["value"] == "x".repeat(40)),
        "{}",
        o.stdout
    );

    let o = h.run(&["state", "clear", "w"]).ok("state clear");
    assert!(
        o.stdout.starts_with("Cleared 2 keys for agent w"),
        "{}",
        o.stdout
    );
    let o = h.run(&["state", "get", "w", "progress"]);
    assert_eq!((o.code, o.stdout.as_str()), (1, ""), "{o:?}");

    let o = h.run(&["statuses"]).ok("statuses");
    assert!(o.stdout.starts_with("Statuses:\n  idle "), "{}", o.stdout);
    let o = h.run(&["version"]).ok("version");
    assert!(o.stdout.starts_with("agent "), "{}", o.stdout);
}

// adopt a person's session, then cleanup leaves it running.
#[test]
fn e2e_adopt_and_cleanup_leaves_the_session() {
    require_termd!();
    let h = Host::new("hub");
    h.run(&["session", "new", "dev-shell"]).ok("session new");
    let o = h
        .run(&["adopt", "mine", "--session", "dev-shell"])
        .ok("adopt");
    assert_eq!(
        o.stdout,
        format!(
            "Adopted agent mine (session dev-shell)\n  Backend: shell\n  Cwd:     {}\n\nNext: agent status mine -v  |  agent send mine \"...\"  |  agent cleanup mine (leaves dev-shell running)\n",
            h.work().display()
        )
    );
    let o = h.run(&["list"]).ok("list");
    assert!(
        o.stdout
            .starts_with("mine: unknown (shell, adopted)\n  session: dev-shell\n"),
        "{}",
        o.stdout
    );
    let o = h.run(&["cleanup", "mine"]).ok("cleanup");
    assert_eq!(
        o.stdout,
        "cleaned up \"mine\": left session dev-shell running; removed its record, state, queued messages, and files; its log stays (agent logs mine)\n"
    );
    let o = h.run(&["session", "list"]).ok("session list");
    assert!(o.stdout.contains("dev-shell "), "{}", o.stdout);
    h.run(&["session", "kill", "dev-shell"]).ok("session kill");
}

// cleanup --ended (--dry-run first), export and export --json.
#[test]
fn e2e_cleanup_ended_and_export() {
    require_termd!();
    let h = Host::new("hub");
    spawn_shell(&h, "w");
    spawn_shell(&h, "done");

    let o = h.run(&["export", "--json"]).ok("export --json");
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    assert_eq!(
        (&v["type"], &v["host"]),
        (&"snapshot".into(), &"hub".into()),
        "{}",
        o.stdout
    );
    let names: Vec<&str> = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2, "{}", o.stdout);
    assert!(
        names.contains(&"w") && names.contains(&"done"),
        "{}",
        o.stdout
    );
    let w = v["agents"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "w")
        .unwrap();
    assert_eq!(
        (&w["backend"], &w["session"], &w["namespace"], &w["live"]),
        (
            &"shell".into(),
            &"agent-w".into(),
            &serde_json::Value::from(h.ns.as_str()),
            &true.into()
        )
    );
    let o = h.run(&["export"]).ok("export");
    assert!(
        o.stdout.starts_with(&format!(
            "Store {} (this binary's): events up to ",
            h.data_dir().display()
        )),
        "{}",
        o.stdout
    );

    h.run(&["report", "ended", "--as", "done"])
        .ok("report ended");
    let o = h.run(&["status", "done"]).ok("status");
    assert!(o.stdout.starts_with("done: ended (quit)\n"), "{}", o.stdout);
    let o = h
        .run(&["cleanup", "--ended", "--dry-run"])
        .ok("cleanup --ended --dry-run");
    let lines: Vec<&str> = o.stdout.lines().collect();
    assert!(
        lines[0].starts_with("would clean up \"done\": ended (quit) "),
        "{}",
        o.stdout
    );
    assert_eq!(
        lines[1], "  kills its session agent-done; no held messages",
        "{}",
        o.stdout
    );
    assert_eq!(
        lines[2], "Would clean up 1 ended agent; nothing changed (dry run).",
        "{}",
        o.stdout
    );
    let o = h.run(&["cleanup", "--ended"]).ok("cleanup --ended");
    assert_eq!(
        o.stdout,
        "cleaned up \"done\": killed session agent-done; removed its record, state, queued messages, and files; its log stays (agent logs done)\nCleaned up 1 ended agent.\n"
    );
    let o = h.run(&["list"]).ok("list");
    assert!(
        o.stdout.starts_with("w: ") && !o.stdout.contains("done"),
        "{}",
        o.stdout
    );
}

// friction add (exit 3 on a similar entry), +1, list, show; task; history
// records it all.
#[test]
fn e2e_friction_task_history() {
    require_termd!();
    let h = Host::new("hub");
    let o = h
        .run(&[
            "friction",
            "add",
            "logs can't show just one turn",
            "--kind",
            "missing",
        ])
        .ok("friction add");
    assert_eq!(o.stdout, "Filed friction #1 (missing).\n");
    let o = h.run(&[
        "friction",
        "add",
        "logs cannot show just one turn",
        "--kind",
        "missing",
    ]);
    assert_eq!(o.code, 3, "{o:?}");
    assert_eq!(
        o.stdout,
        "Similar entries:\n  #1 missing +0 logs can't show just one turn\n"
    );
    assert_eq!(
        o.stderr,
        "Error: not filed: it looks like #1; if it's the same, agent friction +1 1 \"your note\"; if not, re-run with --new\n"
    );
    let o = h
        .run(&["friction", "+1", "1", "hit it again"])
        .ok("friction +1");
    assert!(
        o.stdout
            .starts_with("Voted on friction #1 (missing, now +1)"),
        "{}",
        o.stdout
    );
    let o = h.run(&["friction", "list"]).ok("friction list");
    assert_eq!(
        o.stdout,
        "ID KIND    VOTES  AGE BY   SUMMARY\n#1 missing    +1  now user logs can't show just one turn\n"
    );
    let o = h.run(&["friction", "show", "1"]).ok("friction show");
    assert!(
        o.stdout.starts_with(
            "#1 missing, open, +1: logs can't show just one turn\nFiled by user on hub, "
        ),
        "{}",
        o.stdout
    );
    assert!(o.stdout.contains(": hit it again\n"), "{}", o.stdout);

    let o = h
        .run(&["task", "set", "review", "Review auth"])
        .ok("task set");
    assert_eq!(o.stdout, "Saved task review (1 line, inline).\n");
    assert_eq!(h.run(&["task", "get", "review"]).stdout, "Review auth\n");

    let o = h.run(&["history", "--failed"]).ok("history --failed");
    assert!(
        o.stdout.contains("  3 friction add 'logs cannot show just one turn' --kind missing  # not filed: it looks like #1"),
        "{}",
        o.stdout
    );
    let o = h.run(&["history", "--json"]).ok("history --json");
    let recs: Vec<serde_json::Value> = serde_json::from_str(&o.stdout).unwrap();
    let args: Vec<String> = recs
        .iter()
        .map(|r| r["args"][0].as_str().unwrap().to_string())
        .collect();
    assert!(
        args.iter().filter(|a| *a == "friction").count() >= 5,
        "{args:?}"
    );
    assert!(args.contains(&"task".to_string()), "{args:?}");
}

// attach hands the terminal to g3d, so it refuses what it can't open: an
// agent whose session is gone, and (needing ssh, so a terminal) an agent on
// another host when stdin isn't a terminal.
#[test]
fn e2e_attach_refusals() {
    require_termd!();
    let h = Host::new("hub");
    spawn_shell(&h, "w");
    h.run(&["session", "kill", "agent-w"]).ok("session kill");
    let o = h.run(&["attach", "w"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(
        o.stderr.starts_with("Error: w has ended")
            && o.stderr.ends_with(
                ": its session agent-w is gone, so there is nothing to attach to (agent logs w shows its last output)\n"
            ),
        "{}",
        o.stderr
    );
    let o = h.run(&["attach", "w@laptop"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(
        o.stderr,
        "Error: attach w@laptop opens its session in this terminal, over ssh, so it needs one: run it in a terminal (agent capture w@laptop shows its screen from here)\n"
    );
}

// skill install writes the skill where the stand-in home's harnesses read
// it (claude is on PATH; agy is neither on PATH nor configured), and Claude
// Code's hook with it; show prints the text.
#[test]
fn e2e_skill_install_under_the_temp_home() {
    require_termd!();
    let h = Host::new("hub");
    let o = h.run(&["skill", "install"]).ok("skill install");
    let skill = h.claude_dir().join("skills/agent/SKILL.md");
    let settings = h.claude_dir().join("settings.json");
    assert_eq!(
        o.stdout,
        format!(
            "Installed the agent skill for claude: {}\n  its hook in {}: 14 event(s) added, 0 pointed at this binary\n",
            skill.display(),
            settings.display()
        )
    );
    assert_eq!(
        o.stderr,
        "hint: sessions already running pick it up when restarted\n"
    );
    let text = std::fs::read_to_string(&skill).unwrap();
    assert!(text.starts_with("---\nname: agent\n"), "{text}");
    assert!(
        text.contains(&h.bin.display().to_string()),
        "the skill names this binary: {text}"
    );
    let s = std::fs::read_to_string(&settings).unwrap();
    assert!(
        s.contains(&format!("{} hook", h.bin.display()))
            || s.contains(&h.bin.display().to_string()),
        "{s}"
    );
    let o = h.run(&["hooks", "status"]).ok("hooks status");
    assert!(!o.stdout.contains("missing"), "{}", o.stdout);
    assert!(
        !h.root.path().join("home/.gemini").exists(),
        "agy isn't here: nothing written for it"
    );

    let o = h.run(&["skill", "install"]).ok("skill install again");
    assert!(
        o.stdout
            .starts_with("Already installed the agent skill for claude: "),
        "{}",
        o.stdout
    );
    let o = h
        .run(&["skill", "install", "--harness", "agy"])
        .ok("skill install --harness agy");
    let agy = h
        .root
        .path()
        .join("home/.gemini/config/skills/agent/SKILL.md");
    assert_eq!(
        o.stdout,
        format!("Installed the agent skill for agy: {}\n", agy.display())
    );
    let o = h.run(&["skill", "show"]).ok("skill show");
    assert!(
        o.stdout.contains("agent help") && !o.stdout.starts_with("---"),
        "{}",
        o.stdout
    );
}

// agent run from an agent's pane: the job is that agent's, and fleet lists
// it while it runs. The `agent` in the pane reaches the test's termd through
// the pane's own G3D_TERMD_SOCKET (agent_termd's guard).
#[test]
fn e2e_run_in_a_pane_shows_in_fleet() {
    require_termd!();
    let h = Host::new("hub");
    spawn_shell(&h, "w");
    let gate = h.work().join("gate");
    let script = h.work().join("slow.sh");
    std::fs::write(
        &script,
        format!(
            "while [ ! -e '{}' ]; do sleep 0.1; done\necho slow-done-$((40+2))\n",
            gate.display()
        ),
    )
    .unwrap();
    h.type_line(
        "w",
        &format!(
            "'{}' run --no-protect '{}' x",
            h.bin.display(),
            script.display()
        ),
    );
    let fleet = wait_for("the job in fleet", || {
        let o = h.run(&["fleet"]);
        let ok = o.stdout.contains("  jobs:\n");
        (format!("{o:?}"), ok)
    });
    let o = h.run(&["fleet"]).ok("fleet");
    eprintln!("agent fleet with a job running:\n{}", o.stdout);
    assert!(
        o.stdout.contains("  jobs:\n    #1 ") && o.stdout.contains("(by w"),
        "{fleet}\n{}",
        o.stdout
    );
    std::fs::write(&gate, "").unwrap();
    h.wait_line("w", "slow-done-42");
    // The job prints its last line a moment before its runner records the
    // end.
    let listed = wait_for("the job's end recorded", || {
        let o = h.run(&["run", "--list", "--all"]).ok("run --list --all");
        let row: Vec<String> = o
            .stdout
            .lines()
            .nth(1)
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect();
        let ok = row.len() > 6 && row[0] == "1" && row[4] == "0" && row[5] == "w";
        (o.stdout, ok)
    });
    assert!(listed.contains("/slow.sh x\n"), "{listed}");
    let o = h.run(&["fleet"]).ok("fleet");
    assert!(!o.stdout.contains("jobs:"), "{}", o.stdout);
}
