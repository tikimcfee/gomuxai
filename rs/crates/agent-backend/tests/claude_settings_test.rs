//! Go tests/backend/claude_settings_test.go. Every settings file here is in
//! a temp dir: nothing reads or writes the real ~/.claude.

mod common;

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use agent_backend::namedagents::{self, HookState};
use common::*;
use serde_json::Value;

fn read_json(path: &str) -> serde_json::Map<String, Value> {
    let data = std::fs::read_to_string(path).unwrap();
    serde_json::from_str(&data).unwrap_or_else(|e| panic!("{path}: {e}\n{data}"))
}

/// Lists, per event, the hook commands that are an agent binary's.
fn our_commands(settings: &serde_json::Map<String, Value>) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for (event, v) in hooks {
        for g in v.as_array().unwrap() {
            for h in g["hooks"].as_array().unwrap() {
                let c = h["command"].as_str().unwrap();
                if c.ends_with(" hook") {
                    out.entry(event.clone()).or_default().push(c.to_string());
                }
            }
        }
    }
    out
}

#[test]
fn test_install_claude_hooks() {
    let dir = temp_dir();
    let path = path_in(&dir, "settings.json");
    let backups = path_in(&dir, "backups");
    // A person's settings, with a hook of their own on Stop and an old
    // gomuxai binary's entry on PreToolUse.
    let orig = r#"{"model":"opus","env":{"A":"1"},"hooks":{
		"Stop":[{"matcher":"","hooks":[{"type":"command","command":"notify-send done"}]}],
		"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"'/old/bin/agent' hook","timeout":10}]}]}}"#;
    std::fs::write(&path, orig).unwrap();
    let bin = "/home/user/.local/bin/agent";
    let r = namedagents::install_claude_hooks(&path, bin, &backups, false).unwrap();
    assert!(
        r.events.get("PreToolUse") == Some(&HookState::Stale)
            && r.events.get("Stop").copied().unwrap_or_default() == HookState::Missing
            && r.changed
            && !r.backup.is_empty(),
        "report {r:?}"
    );
    assert_eq!(std::fs::read_to_string(&r.backup).unwrap(), orig);
    let got = read_json(&path);
    assert_eq!(got["model"], "opus");
    assert_eq!(got["env"]["A"], "1");
    let want = format!("'{bin}' hook");
    let ours = our_commands(&got);
    for event in namedagents::CLAUDE_HOOK_EVENTS {
        assert_eq!(ours.get(*event), Some(&vec![want.clone()]), "{event}");
    }
    // The person's own hook stays, beside ours.
    let stop = got["hooks"]["Stop"].as_array().unwrap();
    assert!(
        stop.len() == 2 && stop[0]["hooks"][0]["command"] == "notify-send done",
        "Stop = {stop:?}"
    );

    // Written as Claude Code writes it: two-space indents, keys sorted.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.starts_with("{\n  \"env\": {\n    \"A\": \"1\"\n  },\n  \"hooks\": {\n"),
        "{text}"
    );
    assert!(text.ends_with("  \"model\": \"opus\"\n}\n"), "{text}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    // Again: nothing to change, nothing written.
    let before = std::fs::read(&path).unwrap();
    let r = namedagents::install_claude_hooks(&path, bin, &backups, false).unwrap();
    assert!(!r.changed && r.backup.is_empty(), "second install: {r:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a second install rewrote the file"
    );
    let st = namedagents::claude_hooks_status(&path, bin).unwrap();
    for event in namedagents::CLAUDE_HOOK_EVENTS {
        assert_eq!(
            st.events.get(*event),
            Some(&HookState::Current),
            "status {event}"
        );
    }

    // Remove takes out only ours, and the events left empty.
    namedagents::remove_claude_hooks(&path, bin, &backups).unwrap();
    let got = read_json(&path);
    assert!(
        our_commands(&got).is_empty(),
        "left {:?}",
        our_commands(&got)
    );
    let hooks = got["hooks"].as_object().unwrap();
    assert!(
        hooks.len() == 1 && hooks["Stop"].as_array().unwrap().len() == 1,
        "after remove: {hooks:?}"
    );
}

#[test]
fn test_install_claude_hooks_refuses_a_broken_file() {
    let dir = temp_dir();
    let path = path_in(&dir, "settings.json");
    std::fs::write(&path, r#"{"model": "opus","#).unwrap();
    let err = namedagents::install_claude_hooks(&path, "/bin/agent", &path_in(&dir, "b"), false)
        .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!(
            "{path} isn't a JSON object, so it was left alone (fix it, then run this again): unexpected EOF"
        )
    );
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        r#"{"model": "opus","#,
        "the file was changed"
    );
}

#[test]
fn test_install_claude_hooks_new_file_and_symlink() {
    let dir = temp_dir();
    // No file yet.
    let path = path_in(&dir, "settings.json");
    namedagents::install_claude_hooks(&path, "/bin/agent", &path_in(&dir, "b"), false).unwrap();
    assert_eq!(
        our_commands(&read_json(&path)).len(),
        namedagents::CLAUDE_HOOK_EVENTS.len()
    );
    // A new file has no backup.
    assert!(!dir.path().join("b").exists());
    // A settings file kept elsewhere and linked in stays linked.
    let real = path_in(&dir, "dotfiles.json");
    std::fs::write(&real, "{}").unwrap();
    let link = path_in(&dir, "linked.json");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    namedagents::install_claude_hooks(&link, "/bin/agent", &path_in(&dir, "b"), false).unwrap();
    let fi = std::fs::symlink_metadata(&link).unwrap();
    assert!(fi.file_type().is_symlink(), "the link was replaced");
    assert_eq!(
        our_commands(&read_json(&real)).len(),
        namedagents::CLAUDE_HOOK_EVENTS.len()
    );
}

/// Hooks Claude Code couldn't read are an error, not something to edit
/// around; numbers come back as written; a group left empty stays an array.
#[test]
fn test_install_claude_hooks_odd_files() {
    let dir = temp_dir();
    let d = dir.path().to_str().unwrap();
    for (bad, msg) in [
        (r#"{"hooks":{"Stop":[1]}}"#, "hooks.Stop[0] isn't an object"),
        (r#"{"hooks":{"Stop":"nope"}}"#, "hooks.Stop isn't an array"),
        (r#"{"hooks":[]}"#, "hooks isn't an object"),
        (
            r#"{"hooks":{"Stop":[{"hooks":[7]}]}}"#,
            "hooks.Stop[0].hooks[0] isn't an object",
        ),
    ] {
        let path = path_in(&dir, "bad.json");
        std::fs::write(&path, bad).unwrap();
        let err = namedagents::install_claude_hooks(&path, "/bin/agent", d, false).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            format!("{path}: {msg}, so the file was left alone (fix it, then run this again)")
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            bad,
            "{bad} was changed"
        );
    }
    let path = path_in(&dir, "settings.json");
    std::fs::write(
        &path,
        r#"{"big":12345678901234567890123,"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"'/gone/agent' hook"}]}]}}"#,
    )
    .unwrap();
    namedagents::install_claude_hooks(&path, "/bin/agent", d, false).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        text.contains("12345678901234567890123"),
        "number rewritten: {text}"
    );
    namedagents::remove_claude_hooks(&path, "/bin/agent", d).unwrap();
}

/// One hook serves one binary: install won't take over another binary's
/// entry while that binary exists, unless forced; a binary that is gone is
/// replaced freely.
#[test]
fn test_install_claude_hooks_another_binary() {
    let dir = temp_dir();
    let d = dir.path().to_str().unwrap();
    let other = path_in(&dir, "installed/agent");
    std::fs::create_dir_all(dir.path().join("installed")).unwrap();
    std::fs::write(&other, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = path_in(&dir, "settings.json");
    namedagents::install_claude_hooks(&path, &other, d, false).unwrap();
    let dev = path_in(&dir, "dev/agent");
    let err = namedagents::install_claude_hooks(&path, &dev, d, false).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains(&other) && msg.contains("--force"), "err {msg}");
    let r = namedagents::install_claude_hooks(&path, &dev, d, true).unwrap();
    assert!(r.changed && r.others.len() == 1, "forced: {r:?}");
    let _ = std::fs::remove_file(&dev); // the binary the hook runs is gone: re-pointing it is free
    namedagents::install_claude_hooks(&path, &other, d, false)
        .expect("re-pointing a gone binary's entry");
}

#[test]
fn test_hook_state_words_and_sorted_events() {
    assert_eq!(
        namedagents::hook_state_word(HookState::Current),
        "installed"
    );
    assert_eq!(
        namedagents::hook_state_word(HookState::Stale),
        "another agent binary's"
    );
    assert_eq!(namedagents::hook_state_word(HookState::Missing), "missing");
    assert_eq!(
        namedagents::claude_hook_command("/opt/my tools/agent"),
        "'/opt/my tools/agent' hook"
    );
    let mut r = namedagents::HooksReport::default();
    r.events.insert("Zeta".to_string(), HookState::Current);
    r.events.insert("Stop".to_string(), HookState::Current);
    r.events.insert("Alpha".to_string(), HookState::Stale);
    let events = r.sorted_events();
    assert_eq!(events.len(), namedagents::CLAUDE_HOOK_EVENTS.len() + 2);
    assert_eq!(&events[events.len() - 2..], ["Alpha", "Zeta"]);
}

/// Deeply nested settings files return Go's "exceeded max depth" error
/// without overflowing the stack, leaving the file untouched.
#[test]
fn test_install_claude_hooks_rejects_excessive_nesting() {
    let dir = temp_dir();
    let path = path_in(&dir, "settings.json");
    let backups = path_in(&dir, "backups");
    let deep = "{\"a\":".repeat(50_000);
    std::fs::write(&path, &deep).unwrap();

    let path_c = path.clone();
    let backups_c = backups.clone();
    std::thread::Builder::new()
        .stack_size(1 << 20)
        .spawn(move || {
            let want = format!(
                "{path_c} isn't a JSON object, so it was left alone (fix it, then run this again): exceeded max depth"
            );
            let err = namedagents::install_claude_hooks(&path_c, "/bin/agent", &backups_c, false).unwrap_err();
            assert_eq!(format!("{err:#}"), want);
            let err = namedagents::claude_hooks_status(&path_c, "/bin/agent").unwrap_err();
            assert_eq!(format!("{err:#}"), want);
            let err = namedagents::remove_claude_hooks(&path_c, "/bin/agent", &backups_c).unwrap_err();
            assert_eq!(format!("{err:#}"), want);
        })
        .unwrap()
        .join()
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        deep,
        "settings.json was modified"
    );
    assert!(
        !std::path::Path::new(&backups).exists(),
        "backup written on rejected file"
    );
}
