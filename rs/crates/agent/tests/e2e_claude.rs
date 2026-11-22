//! The built binary with a Claude agent, end to end: hooks install, spawn,
//! and Claude Code's hook (`agent hook`) fed real payload shapes on stdin,
//! run where Claude Code runs it, under the CLI in the agent's pane (a
//! stand-in: tests/e2e/harness.rs's FAKE_CLAUDE). The reports drive status,
//! approve, deny, held messages, wait --next, logs, transcript, sessions,
//! and resume.

#[path = "e2e/harness.rs"]
mod harness;

use std::os::unix::fs::PermissionsExt;

use harness::{Host, claude_transcript, wait_for};

/// Installs the hook in the stand-in Claude settings, checking what it says.
fn install_hooks(h: &Host) {
    let settings = h.claude_dir().join("settings.json");
    let o = h.run(&["hooks", "install"]).ok("hooks install");
    assert_eq!(
        o.stdout,
        format!(
            "Installed in {}: 14 event(s) added, 0 pointed at this binary (was: none). Backup: none\n",
            settings.display()
        )
    );
    assert_eq!(
        o.stderr,
        "hint: sessions already running pick it up when restarted, or through /hooks\n"
    );
}

/// Spawns Claude agent name and waits for its stand-in CLI to register its
/// session; returns the session id.
fn spawn_claude(h: &Host, name: &str) -> String {
    let o = h.run(&["spawn", name, "--backend", "claude"]).ok("spawn");
    assert!(
        o.stdout.starts_with(&format!(
            "Spawned agent {name} (session agent-{name}, backend claude).\n"
        )),
        "{}",
        o.stdout
    );
    let sid = h.state(name, "_session_id");
    assert!(!sid.is_empty(), "no _session_id after spawn");
    wait_for_registry(h, &sid);
    sid
}

/// Waits for the stand-in CLI running session sid to register it.
fn wait_for_registry(h: &Host, sid: &str) {
    let dir = h.claude_dir().join("sessions");
    wait_for(&format!("session {sid} in the registry"), || {
        let all: Vec<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                    .collect()
            })
            .unwrap_or_default();
        let ok = all
            .iter()
            .any(|e| e.contains(&format!("\"sessionId\":\"{sid}\"")));
        (all.join(""), ok)
    });
}

/// Runs the hook for event in agent's pane, checking it exits 0 with
/// nothing on stderr; returns its stdout.
fn hook(h: &Host, agent: &str, payload: serde_json::Value) -> String {
    let o = h.hook_in_pane(agent, &payload.to_string());
    assert_eq!(
        (o.code, o.stderr.as_str()),
        (0, ""),
        "hook {payload}: {o:?}"
    );
    o.stdout
}

fn status(h: &Host, name: &str) -> String {
    h.run(&["status", name]).ok("status").stdout
}

/// Writes a transcript for session sid where Claude Code keeps them
/// ($CLAUDE_CONFIG_DIR/projects/<project>/<sid>.jsonl).
fn write_transcript(h: &Host, sid: &str) -> String {
    let cwd = h.work().to_string_lossy().into_owned();
    let project = cwd.replace(['/', '.', '_'], "-");
    let dir = h.claude_dir().join("projects").join(project);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::write(
        &path,
        claude_transcript(sid, &cwd, "list the files", "Found 3 files."),
    )
    .unwrap();
    path.to_string_lossy().into_owned()
}

/// Draws Claude's permission dialog in agent's pane (its footer last, as
/// approve and deny look for it before pressing a key).
fn draw_permission_dialog(h: &Host, agent: &str, tool: &str) {
    h.type_line(
        agent,
        &format!(
            "PS1=; printf '\\n {tool}\\n\\n Do you want to proceed?\\n \\342\\235\\257 1. Yes\\n   2. No\\n\\n Esc to cancel\\n'"
        ),
    );
    h.wait_line(agent, "Esc to cancel");
}

// hooks install and status; the hook reports a session's prompts, a
// permission prompt (approved, then another denied), a message held while
// the turn runs and handed over by the Stop hook, wait --next, _result,
// deliver, logs, transcript, sessions.
#[test]
fn e2e_claude_hooks_drive_status_and_prompts() {
    require_termd!();
    let h = Host::new("hub");
    // spawn refuses a Claude agent until the hook is installed.
    let o = h.run(&["spawn", "c", "--backend", "claude"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(o.stderr.contains("agent hooks install"), "{}", o.stderr);
    install_hooks(&h);
    let o = h.run(&["hooks", "status"]).ok("hooks status");
    let mut want = format!(
        "{} (this binary: {})\n",
        h.claude_dir().join("settings.json").display(),
        h.bin.display()
    );
    for e in [
        "SessionStart",
        "UserPromptSubmit",
        "PreToolUse",
        "PermissionRequest",
        "PostToolUse",
        "PostToolUseFailure",
        "PostToolBatch",
        "PreCompact",
        "Notification",
        "SubagentStart",
        "SubagentStop",
        "Stop",
        "StopFailure",
        "SessionEnd",
    ] {
        want += &format!("  {e:<19} installed\n");
    }
    want += "No hook errors logged.\n";
    assert_eq!(o.stdout, want);
    let o = h.run(&["hooks", "install"]).ok("hooks install again");
    assert!(
        o.stdout.starts_with("Already installed in "),
        "{}",
        o.stdout
    );

    let sid = spawn_claude(&h, "c");
    let cwd = h.work().to_string_lossy().into_owned();
    let transcript = write_transcript(&h, &sid);
    let base = |event: &str| serde_json::json!({"hook_event_name": event, "session_id": sid, "cwd": cwd, "transcript_path": transcript});

    let mut p = base("SessionStart");
    p["source"] = "startup".into();
    hook(&h, "c", p);
    assert_eq!(status(&h, "c"), "c: idle\n");

    let mut p = base("UserPromptSubmit");
    p["prompt"] = "list the files".into();
    hook(&h, "c", p);
    assert_eq!(status(&h, "c"), "c: processing\n");
    assert_eq!(h.state("c", "_transcript"), transcript);

    // A permission prompt: waiting, with the call; approve answers it.
    let mut p = base("PermissionRequest");
    p["tool_name"] = "Bash".into();
    p["tool_input"] = serde_json::json!({"command": "ls"});
    hook(&h, "c", p);
    let o = h.run(&["status", "c", "-v"]).ok("status -v");
    assert!(
        o.stdout.starts_with("c: waiting\n")
            && o.stdout
                .contains("  source: report\n  event:  PermissionRequest (")
            && o.stdout.ends_with(
                "  detail: permission: Bash: ls\n  tool:   Bash\n  input:  command: ls\n"
            ),
        "{}",
        o.stdout
    );
    assert_eq!(h.state("c", "_status"), "waiting");
    assert_eq!(h.state("c", "_tool"), "Bash");
    // Not that prompt: refused.
    let o = h.run(&["approve", "c", "--expect", "Write"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert_eq!(
        o.stderr,
        "Error: agent c is waiting on \"Bash\", not \"Write\": check the new prompt with agent status c -v before answering it\n"
    );
    draw_permission_dialog(&h, "c", "Bash command");
    let o = h.run(&["approve", "c", "--expect", "Bash"]).ok("approve");
    assert_eq!(
        o.stdout,
        "Approved c's Bash prompt (permission: Bash: ls).\n"
    );
    let o = h.run(&["status", "c", "-v"]).ok("status -v");
    assert!(
        o.stdout.starts_with("c: processing\n") && o.stdout.contains("  event:  approved ("),
        "{}",
        o.stdout
    );
    h.run(&["key", "agent-c", "C-c"]).ok("key C-c");

    // Another, denied: Claude's turn ends, and deny says how to go on.
    let mut p = base("PermissionRequest");
    p["tool_name"] = "Write".into();
    p["tool_input"] = serde_json::json!({"file_path": "/var/tmp/x.txt", "content": "x"});
    hook(&h, "c", p);
    assert_eq!(h.state("c", "_tool"), "Write");
    draw_permission_dialog(&h, "c", "Create file");
    let o = h.run(&["deny", "c"]).ok("deny");
    assert_eq!(
        o.stdout,
        "Denied c's Write prompt (permission: Write: /var/tmp/x.txt).\n"
    );
    assert_eq!(
        o.stderr,
        "hint: denying cancels Claude's turn: tell it what to do instead with agent send c \"...\"\n"
    );
    let o = h.run(&["status", "c", "-v"]).ok("status -v");
    assert!(
        o.stdout.starts_with("c: idle\n") && o.stdout.contains("  event:  denied ("),
        "{}",
        o.stdout
    );
    h.run(&["key", "agent-c", "C-c"]).ok("key C-c");

    // A new turn; a message sent meanwhile is held, and the Stop hook hands
    // it over.
    let mut p = base("UserPromptSubmit");
    p["prompt"] = "go on".into();
    hook(&h, "c", p);
    let o = h.run(&["send", "c", "also check the docs"]).ok("send");
    assert_eq!(
        o.stdout,
        "Held for c until its next tool batch or turn ends (it was processing; #1).\n"
    );
    let o = h.run(&["status", "c", "-v"]).ok("status -v");
    assert!(o.stdout.contains("  held:   1 message"), "{}", o.stdout);
    let mut wait = h.start(
        "wait",
        &[
            "wait",
            "c",
            "--for",
            "idle,waiting",
            "--next",
            "--timeout",
            "60s",
        ],
    );

    let mut p = base("Stop");
    p["last_assistant_message"] = "Found 3 files.".into();
    let out = hook(&h, "c", p);
    let v: serde_json::Value = serde_json::from_str(&out).expect("the Stop hook prints JSON");
    assert_eq!(
        v,
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": "Stop",
            "additionalContext": "A message arrived while you were working:\n\n[from a person on hub] also check the docs",
        }}),
        "the Stop hook's output: {out}"
    );
    // Claude goes on with the message: the turn isn't over.
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(
        wait.child.try_wait().unwrap().is_none(),
        "wait --next returned mid-turn: {}",
        wait.stdout()
    );

    let mut p = base("Stop");
    p["last_assistant_message"] = "Checked the docs too.".into();
    let out = hook(&h, "c", p);
    assert_eq!(out, "", "nothing more held");
    assert_eq!(
        wait.wait(std::time::Duration::from_secs(30)),
        0,
        "{}",
        wait.stderr()
    );
    assert_eq!(wait.stdout(), "c: idle\n");
    assert_eq!(h.state("c", "_result"), "Checked the docs too.");
    let o = h.run(&["deliver", "c"]).ok("deliver");
    assert_eq!(o.stdout, "Nothing is held for c.\n");

    // The transcript the hooks named: logs, transcript, sessions read it.
    let o = h.run(&["logs", "c"]).ok("logs");
    assert_eq!(
        o.stdout,
        "10:00:00 prompt list the files\n10:00:01 say    Found 3 files.\n"
    );
    let o = h.run(&["transcript", "c"]).ok("transcript");
    let lines: Vec<serde_json::Value> = o
        .stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect();
    assert!(lines.len() >= 3, "{}", o.stdout);
    assert_eq!(
        (
            &lines[0]["type"],
            &lines[0]["session_id"],
            &lines[0]["harness"],
            &lines[0]["title"]
        ),
        (
            &"session".into(),
            &serde_json::Value::from(sid.as_str()),
            &"claude".into(),
            &"List the files".into()
        ),
        "{}",
        o.stdout
    );
    assert_eq!(lines[0]["workgroup"]["name"], "c", "{}", o.stdout);
    assert_eq!(
        (&lines[1]["kind"], &lines[1]["prompt"]["text"]),
        (&"prompt".into(), &"list the files".into())
    );
    assert_eq!(
        (&lines[2]["kind"], &lines[2]["say"]["text"]),
        (&"say".into(), &"Found 3 files.".into())
    );
    let o = h.run(&["sessions", "--json"]).ok("sessions --json");
    let v: serde_json::Value = serde_json::from_str(&o.stdout).unwrap();
    let all = v.as_array().cloned().unwrap_or_default();
    let s = all
        .iter()
        .find(|s| s.to_string().contains(&sid))
        .unwrap_or_else(|| panic!("no session {sid}: {}", o.stdout));
    assert_eq!(s["workgroup"]["name"], "c", "{s}");
    let o = h.run(&["sessions"]).ok("sessions");
    assert!(o.stdout.contains("List the files"), "{}", o.stdout);
}

// SessionEnd ends the agent (quit); resume starts its CLI again on its
// last session.
#[test]
fn e2e_claude_session_end_and_resume() {
    require_termd!();
    let h = Host::new("hub");
    install_hooks(&h);
    let sid = spawn_claude(&h, "c");
    let cwd = h.work().to_string_lossy().into_owned();
    let transcript = write_transcript(&h, &sid);
    let base = |event: &str| serde_json::json!({"hook_event_name": event, "session_id": sid, "cwd": cwd, "transcript_path": transcript});
    hook(&h, "c", base("UserPromptSubmit"));
    let mut p = base("Stop");
    p["last_assistant_message"] = "Found 3 files.".into();
    hook(&h, "c", p);
    assert_eq!(status(&h, "c"), "c: idle\n");

    let mut p = base("SessionEnd");
    p["reason"] = "prompt_input_exit".into();
    hook(&h, "c", p);
    h.type_line("c", "exit");
    let o = h.run(&["status", "c", "-v"]).ok("status -v");
    assert!(
        o.stdout.starts_with("c: ended (quit)\n")
            && o.stdout.contains("  detail: prompt_input_exit\n"),
        "{}",
        o.stdout
    );
    let o = h.run(&["list"]).ok("list");
    assert!(
        o.stdout.starts_with("c: ended (quit) (claude)\n"),
        "{}",
        o.stdout
    );

    let o = h.run(&["resume", "c"]).ok("resume");
    assert_eq!(
        o.stdout,
        format!(
            "Resumed agent c (session agent-c, backend claude) on session {sid}, in {cwd}.\nIts status is unknown so far: agent wait c --for idle --timeout 1m waits until it's ready.\n"
        )
    );
    let launch = std::fs::read_to_string(h.data_dir().join("agents/c/launch.sh")).unwrap();
    assert!(launch.contains(&format!("--resume '{sid}'")), "{launch}");
    // The CLI runs that session again, and its hooks report it.
    wait_for(&format!("{sid} resumed in the registry"), || {
        let n = std::fs::read_dir(h.claude_dir().join("sessions"))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains(&sid)))
                    .count()
            })
            .unwrap_or(0);
        (format!("{n} entries"), n >= 2)
    });
    let mut p = base("SessionStart");
    p["source"] = "resume".into();
    hook(&h, "c", p);
    assert_eq!(status(&h, "c"), "c: idle\n");
}

// The hook never disturbs a session: on a payload it can't use, or from a
// process no Claude Code runs, it exits 0, says why on stderr, and logs it
// (0600) for agent hooks status. hooks remove takes the entries out.
#[test]
fn e2e_hook_bad_payloads_exit_zero_and_are_logged() {
    require_termd!();
    let h = Host::new("hub");
    install_hooks(&h);
    let log = h.data_dir().join("logs/hook-errors.log");

    let o = h.run_with(&["hook"], &[], b"{this is not json");
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{o:?}");
    assert!(
        o.stderr.starts_with("agent hook: decoding hook payload: "),
        "{}",
        o.stderr
    );
    let o = h.run_with(&["hook"], &[], br#"{"hook_event_name":"Stop"}"#);
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{o:?}");
    assert_eq!(o.stderr, "agent hook: a Stop payload with no session_id\n");
    // A session no agent owns: its report is recorded, and nothing printed.
    let o = h.run_with(
        &["hook"],
        &[],
        br#"{"hook_event_name":"Stop","session_id":"0000aaaa-0000-0000-0000-000000000001"}"#,
    );
    assert_eq!((o.code, o.stdout.as_str()), (0, ""), "{o:?}");

    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", log.display());
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(logged.lines().count() >= 2, "{logged}");
    assert!(
        logged.contains("a Stop payload with no session_id"),
        "{logged}"
    );

    let o = h.run(&["hooks", "status"]).ok("hooks status");
    assert!(
        o.stdout
            .contains(&format!("Last hook errors ({}):\n  ", log.display())),
        "{}",
        o.stdout
    );
    assert!(
        o.stdout.contains("a Stop payload with no session_id"),
        "{}",
        o.stdout
    );

    let settings = h.claude_dir().join("settings.json");
    let o = h.run(&["hooks", "remove"]).ok("hooks remove");
    assert!(
        o.stdout.starts_with(&format!(
            "Removed gomuxai's hook from {}. Backup: ",
            settings.display()
        )),
        "{}",
        o.stdout
    );
    let o = h.run(&["hooks", "remove"]).ok("hooks remove again");
    assert_eq!(
        o.stdout,
        format!("Nothing to remove from {}.\n", settings.display())
    );
    let o = h.run(&["hooks", "bogus"]);
    assert_eq!(o.code, 1, "{o:?}");
    assert!(
        o.stderr
            .contains("unknown action \"bogus\": install, remove, or status"),
        "{}",
        o.stderr
    );
}
