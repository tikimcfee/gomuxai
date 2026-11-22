//! `agent hooks install|remove|status` on a settings file of the test's own
//! (--settings), and handle_hook's refusals. The hook itself, fed payloads on
//! stdin inside an agent's pane, runs as a subprocess in tests/.

use std::os::unix::fs::PermissionsExt;

use agent_backend::namedagents::{CLAUDE_HOOK_EVENTS, claude_hook_command};

use crate::cli::testenv::TestEnv;

/// An executable agent binary at dir/name/agent (live_others needs one that
/// exists to refuse taking over its hook).
fn fake_binary(env: &TestEnv, name: &str) -> String {
    let dir = env.dir.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let bin = dir.join("agent");
    std::fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin.to_string_lossy().into_owned()
}

fn settings_json(path: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The commands every hook entry of event runs.
fn commands(settings: &serde_json::Value, event: &str) -> Vec<String> {
    let mut out = Vec::new();
    for g in settings["hooks"][event].as_array().into_iter().flatten() {
        for h in g["hooks"].as_array().into_iter().flatten() {
            out.push(h["command"].as_str().unwrap_or("").to_string());
        }
    }
    out
}

fn status_lines(word: &str) -> String {
    CLAUDE_HOOK_EVENTS
        .iter()
        .map(|e| format!("  {e:<19} {word}\n"))
        .collect()
}

#[test]
fn hooks_install_status_remove() {
    let mut env = TestEnv::new();
    let settings = env.dir.path().join("claude/settings.json");
    std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
    let path = settings.to_string_lossy().into_owned();
    // Someone else's hook and setting, which every edit keeps.
    std::fs::write(
        &path,
        r#"{"model": "x", "hooks": {"Stop": [{"matcher": "", "hooks": [{"type": "command", "command": "other-tool --flag"}]}]}}"#,
    )
    .unwrap();

    // No binary to register.
    env.change_cfg(|c| c.agent_binary = String::new());
    let (_, res) = env.run(&["hooks", "status", "--settings", &path]);
    assert_eq!(
        format!("{:#}", res.unwrap_err()),
        "can't tell where this agent binary is, so there is nothing to register"
    );

    let bin = fake_binary(&env, "this");
    env.change_cfg(|c| c.agent_binary = bin.clone());
    let (out, res) = env.run(&["hooks", "status", "--settings", &path]);
    res.unwrap();
    assert_eq!(
        out,
        format!(
            "{path} (this binary: {bin})\n{}No hook errors logged.\n",
            status_lines("missing")
        )
    );

    let (out, res) = env.run(&["hooks", "install", "--settings", &path]);
    res.unwrap();
    let backups = env.dir.path().join("backups").to_string_lossy().into_owned();
    let want = format!(
        "Installed in {path}: {} event(s) added, 0 pointed at this binary (was: none). Backup: {backups}/settings.json.",
        CLAUDE_HOOK_EVENTS.len()
    );
    assert!(out.starts_with(&want) && out.ends_with('\n'), "install: {out:?}\nwant {want:?}...");
    assert!(
        env.take_err().contains("sessions already running pick it up when restarted, or through /hooks"),
        "install's hint"
    );
    let s = settings_json(&path);
    assert_eq!(s["model"], "x");
    let hook = claude_hook_command(&bin);
    for e in CLAUDE_HOOK_EVENTS {
        let want: Vec<String> = if *e == "Stop" {
            vec!["other-tool --flag".into(), hook.clone()]
        } else {
            vec![hook.clone()]
        };
        assert_eq!(commands(&s, e), want, "{e}'s hooks");
    }
    let (out, _) = env.run(&["hooks", "status", "--settings", &path]);
    assert_eq!(
        out,
        format!(
            "{path} (this binary: {bin})\n{}No hook errors logged.\n",
            status_lines("installed")
        )
    );

    // Nothing to do the second time.
    let (out, res) = env.run(&["hooks", "install", "--settings", &path]);
    res.unwrap();
    assert_eq!(out, format!("Already installed in {path}: every event runs {hook}.\n"));

    // Another agent binary that still exists doesn't take the hook over
    // without --force.
    let other = fake_binary(&env, "other");
    env.change_cfg(|c| c.agent_binary = other.clone());
    let (_, res) = env.run(&["hooks", "install", "--settings", &path]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.starts_with(&format!("Claude Code's hook in {path} runs {bin}, which is still there: "))
            && err.ends_with("or pass --force to point it here"),
        "install over a live binary's hook: {err}"
    );
    let (out, _) = env.run(&["hooks", "status", "--settings", &path]);
    assert_eq!(
        out,
        format!(
            "{path} (this binary: {other})\n{}Other agent binaries' entries: {hook}\nNo hook errors logged.\n",
            status_lines("another agent binary's")
        )
    );
    let (out, res) = env.run(&["hooks", "install", "--force", "--settings", &path]);
    res.unwrap();
    assert!(
        out.starts_with(&format!(
            "Installed in {path}: 0 event(s) added, {} pointed at this binary (was: {hook}). Backup: {backups}/",
            CLAUDE_HOOK_EVENTS.len()
        )),
        "install --force: {out}"
    );
    assert_eq!(commands(&settings_json(&path), "SessionEnd"), vec![claude_hook_command(&other)]);

    // Status shows the last five lines of the hook's error log.
    std::fs::create_dir_all(&env.cfg.log_dir).unwrap();
    let log = std::path::Path::new(&env.cfg.log_dir).join("hook-errors.log");
    let lines: String = (1..=7).map(|i| format!("2026-11-20T10:00:0{i}Z error {i}\n")).collect();
    std::fs::write(&log, lines).unwrap();
    let (out, _) = env.run(&["hooks", "status", "--settings", &path]);
    let tail: String = (3..=7).map(|i| format!("  2026-11-20T10:00:0{i}Z error {i}\n")).collect();
    assert!(
        out.ends_with(&format!("Last hook errors ({}):\n{tail}", log.display())),
        "status with hook errors: {out}"
    );

    let (out, res) = env.run(&["hooks", "remove", "--settings", &path]);
    res.unwrap();
    assert!(
        out.starts_with(&format!("Removed gomuxai's hook from {path}. Backup: {backups}/settings.json.")),
        "remove: {out}"
    );
    let s = settings_json(&path);
    assert_eq!(s["model"], "x");
    assert_eq!(
        s["hooks"].as_object().map(|m| m.keys().cloned().collect::<Vec<_>>()),
        Some(vec!["Stop".to_string()]),
        "remove left: {s}"
    );
    assert_eq!(commands(&s, "Stop"), vec!["other-tool --flag".to_string()]);
    let (out, res) = env.run(&["hooks", "remove", "--settings", &path]);
    res.unwrap();
    assert_eq!(out, format!("Nothing to remove from {path}.\n"));

    let (_, res) = env.run(&["hooks", "bogus", "--settings", &path]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.contains(r#"unknown action "bogus": install, remove, or status"#),
        "unknown action: {err}"
    );
}

// A settings file Claude Code couldn't read is never written.
#[test]
fn hooks_install_leaves_a_broken_file_alone() {
    let mut env = TestEnv::new();
    let bin = fake_binary(&env, "this");
    env.change_cfg(|c| c.agent_binary = bin.clone());
    let path = env.dir.path().join("settings.json").to_string_lossy().into_owned();
    std::fs::write(&path, "{not json").unwrap();
    let (_, res) = env.run(&["hooks", "install", "--settings", &path]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(
        err.starts_with(&format!("{path} isn't a JSON object, so it was left alone")),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    std::fs::write(&path, r#"{"hooks": {"Stop": {"not": "an array"}}}"#).unwrap();
    let (_, res) = env.run(&["hooks", "install", "--settings", &path]);
    let err = format!("{:#}", res.unwrap_err());
    assert_eq!(
        err,
        format!("{path}: hooks.Stop isn't an array, so the file was left alone (fix it, then run this again)")
    );
}

// A payload the hook can't use is an error for run_hook to log (it exits 0
// all the same), and a session no Claude CLI above this process ran can't
// be placed.
#[test]
fn handle_hook_refuses_bad_payloads() {
    crate::require_termd!();
    let env = TestEnv::with_termd();
    let at = agent_store::Time::now();
    let err = format!("{:#}", super::handle_hook(&env.ctx, b"{not json", at).unwrap_err());
    assert!(err.contains("decoding hook payload"), "not JSON: {err}");
    let err = format!(
        "{:#}",
        super::handle_hook(&env.ctx, br#"{"hook_event_name": "Stop"}"#, at).unwrap_err()
    );
    assert!(err.contains("a Stop payload with no session_id"), "no session_id: {err}");
    assert_eq!(env.take_out(), "", "the hook printed for a payload it refused");
}

// Review 5, finding 1: the session that fired the hook is placed by its
// session_id as Go's json.Unmarshal reads it, whatever else the payload
// holds.
#[test]
fn hook_session_id_reads_as_go() {
    for (payload, want) in [
        (&br#"{"session_id":"s1","last_assistant_message":"x \ud800"}"#[..], "s1"),
        (br#"{"Session_ID":"s2"}"#, "s2"),
        (br#"{"session_id":"s3","tool_input":{"a":1e400},"agent_id":5}"#, "s3"),
        (br#"{"session_id":"s\udc004"}"#, "s\u{fffd}4"),
        (br#"{"session_id":null}"#, ""),
        (br#"{"session_id":5}"#, ""),
        (b"{not json", ""),
    ] {
        assert_eq!(super::hook_session_id(payload), want, "{}", String::from_utf8_lossy(payload));
    }
}

// Review 5, finding 1: agent hook, fed on its stdin what Go's hook takes (a
// null where a string goes, half a surrogate pair, names in another case),
// records each session's report and says nothing.
#[test]
fn hook_records_payloads_go_takes() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd();
    for payload in [
        r#"{"hook_event_name":"PreToolUse","session_id":"s1","transcript_path":null,"agent_id":null,"tool_name":"Bash","tool_input":{"command":"agent list"}}"#,
        r#"{"hook_event_name":"SessionEnd","session_id":"s2","reason":"quit \ud800"}"#,
        r#"{"Hook_Event_Name":"Stop","Session_ID":"s3","last_assistant_message":"x \udc00"}"#,
    ] {
        env.ctx.set_stdin(std::io::Cursor::new(payload.as_bytes().to_vec()));
        let (out, res) = env.run(&["hook"]);
        res.unwrap();
        assert_eq!((out.as_str(), env.take_err().as_str()), ("", ""), "{payload}");
    }
    let reports = env.service().session_reports().unwrap();
    let got = |sid: &str| {
        reports
            .get(sid)
            .map(|r| (r.event.clone(), r.status.clone(), r.detail.clone()))
            .unwrap_or_default()
    };
    assert_eq!(got("s1"), ("PreToolUse".into(), "processing".into(), "Bash: agent list".into()));
    assert_eq!(got("s2"), ("SessionEnd".into(), "ended".into(), "quit \u{fffd}".into()));
    assert_eq!(got("s3"), ("Stop".into(), "idle".into(), "turn finished".into()));
}

// A confined agent's PreToolUse report that can't decide (here: no such
// agent) fails, which blocks the tool call; without --confined it stays
// quiet and succeeds, as hooks must for everyone else.
#[test]
fn confined_report_fails_closed() {
    crate::require_termd!();
    let mut env = TestEnv::with_termd();
    let payload = r#"{"conversationId":"c1","toolCall":{"name":"write_file","args":{}}}"#;
    let run = |env: &mut TestEnv, confined: bool| {
        env.ctx.set_stdin(std::io::Cursor::new(payload.as_bytes().to_vec()));
        let mut words = vec!["report", "--hook", "--as", "nobody", "--event", "PreToolUse"];
        if confined {
            words.push("--confined");
        }
        let (_, res) = env.run(&words);
        let _ = env.take_err();
        res
    };
    assert!(run(&mut env, false).is_ok(), "an ordinary hook stays quiet");
    let err = run(&mut env, true).unwrap_err();
    assert!(format!("{err:#}").contains("refused"), "{err:#}");
}
