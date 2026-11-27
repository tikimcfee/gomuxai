//! Go tests/service/allow_test.go.

use std::os::unix::fs::PermissionsExt;

use crate::common::*;
use crate::require_termd;

// TestPreToolUseAllowsSafeAgentCommands: the PreToolUse hook approves one
// plain run of the agent binary with a safe subcommand, however it's
// invoked, and prints nothing for anything else.
#[test]
fn test_pre_tool_use_allows_safe_agent_commands() {
    require_termd!();
    let mut env = setup_test_env();
    let root = tempdir();
    let rootp = root.path().display().to_string();
    let bin = root.path().join("bin").join("agent");
    std::fs::create_dir(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = bin.display().to_string();
    env.cfg_mut().agent_binary = bin.clone();
    let fake = spawn_fake(&env, "worker");

    let hook = |tool: &str, command: &str| -> Option<Vec<u8>> {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse", "session_id": fake.session_id(), "cwd": rootp,
            "tool_name": tool, "tool_input": {"command": command},
        })
        .to_string();
        let (out, err) = env
            .service
            .handle_hook("worker", "", "", payload.as_bytes());
        if let Err(e) = err {
            panic!("HandleHook {payload}: {e:#}");
        }
        out
    };

    for cmd in [
        "./bin/agent status worker -v".to_string(),
        "./bin/agent -q list".to_string(),
        format!("{bin} state get worker _result"),
    ] {
        let out = hook("Bash", &cmd).unwrap_or_default();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap_or_default();
        assert!(
            v["hookSpecificOutput"]["hookEventName"] == "PreToolUse"
                && v["hookSpecificOutput"]["permissionDecision"] == "allow",
            "{cmd}: hook output {}, want a PreToolUse allow",
            String::from_utf8_lossy(&out)
        );
        // Approving doesn't replace the status report.
        assert_eq!(env.state_value("worker", "_detail"), format!("Bash: {cmd}"));
    }
    for cmd in [
        "./bin/agent spawn x --backend claude",
        "./bin/agent list; rm -rf ~",
        "$A list",
        "ls",
    ] {
        let out = hook("Bash", cmd);
        assert!(out.is_none(), "{cmd}: hook output {out:?}, want none");
    }
    let out = hook("Read", "./bin/agent list");
    assert!(out.is_none(), "Read: hook output {out:?}, want none");
}

// The dream verbs are approved only for the agent whose dream is open, and
// only while it is (docs/DREAMING.md).
#[test]
fn test_pre_tool_use_allows_dream_verbs_for_the_open_dream_only() {
    require_termd!();
    let mut env = setup_test_env();
    let root = tempdir();
    let rootp = root.path().display().to_string();
    let bin = root.path().join("agent");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = bin.display().to_string();
    env.cfg_mut().agent_binary = bin.clone();
    let dreamer = spawn_fake(&env, "dream-1");
    let other = spawn_fake(&env, "worker");

    let allowed = |name: &str, fake: &FakeHook, command: &str| -> bool {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse", "session_id": fake.session_id(), "cwd": rootp,
            "tool_name": "Bash", "tool_input": {"command": command},
        })
        .to_string();
        let (out, err) = env.service.handle_hook(name, "", "", payload.as_bytes());
        if let Err(e) = err {
            panic!("HandleHook {payload}: {e:#}");
        }
        let v: serde_json::Value =
            serde_json::from_slice(&out.unwrap_or_default()).unwrap_or_default();
        v["hookSpecificOutput"]["permissionDecision"] == "allow"
    };
    let note = format!("{bin} dream note \"read 3 sessions\"");

    assert!(!allowed("dream-1", &dreamer, &note), "no dream yet");
    let commit = agent_store::DreamItem {
        kind: "commit".into(),
        key: "/srv/app@abcdef1234".into(),
        fingerprint: "abcdef1234".into(),
        ..Default::default()
    };
    let d = env
        .store
        .open_dream("h", &[commit], |id| format!("dream-{id}"))
        .unwrap();
    assert_eq!(d.agent, "dream-1");
    for cmd in [
        note.clone(),
        format!("{bin} dream friction accept 3 --note x"),
        format!("{bin} dream commit"),
        format!("{bin} logs worker"),
        format!("{bin} logs claude:3f3a9c --talk --grep \"cargo test\""),
        format!("{bin} index search \"held message delivery\" --budget 800"),
        format!("{bin} index show gomuxai:rs/crates/agent-dream"),
        format!("{bin} index tree gomuxai --depth 1"),
        format!("{bin} index related gomuxai:rs/crates/agent-dream/src/memory.rs"),
        format!("{bin} index stale --project gomuxai"),
        format!("{bin} dream propose skip add .next --why \"x\" --source claude:s"),
        "git -C /srv/app show abcdef1 --stat".to_string(),
    ] {
        assert!(allowed("dream-1", &dreamer, &cmd), "{cmd}: want allowed");
    }
    // The index is read only to a dream: what writes it stays a person's.
    for cmd in [
        format!("{bin} index crawl"),
        format!("{bin} index root add /srv"),
        format!("{bin} index alias gomuxai:x y"),
        format!("{bin} index summarize --project gomuxai"),
        format!("{bin} index embed"),
        format!("{bin} index skip add out"),
        format!("{bin} dream proposals accept 1"),
    ] {
        assert!(
            !allowed("dream-1", &dreamer, &cmd),
            "{cmd}: want refused for a dream"
        );
    }
    assert!(
        !allowed("worker", &other, &note),
        "another agent's dream verbs"
    );
    assert!(
        !allowed("worker", &other, "git -C /srv/app log"),
        "git in a dream's repo, for another agent"
    );
    assert!(
        !allowed("dream-1", &dreamer, "git -C /srv/other log"),
        "a repo the dream has no commits of"
    );
    assert!(
        !allowed("dream-1", &dreamer, &format!("{bin} dream run")),
        "run is a person's"
    );
    assert!(
        !allowed("dream-1", &dreamer, &format!("{bin} friction accept 3")),
        "plain friction accept stays a person's"
    );
    // A dream's agent reads, but sends and files nothing through the general
    // commands, which every other agent keeps.
    for cmd in [
        format!("{bin} send worker hi"),
        format!("{bin} friction add \"x\""),
        format!("{bin} roster note claude/m --task edit --verdict good --session claude:s x"),
    ] {
        assert!(
            !allowed("dream-1", &dreamer, &cmd),
            "{cmd}: want refused for a dream"
        );
    }
    assert!(
        allowed("worker", &other, &format!("{bin} send dream-1 hi")),
        "send for others"
    );
    env.store.commit_dream(d.id).unwrap();
    assert!(!allowed("dream-1", &dreamer, &note), "the dream is closed");
}

// An act run's agent gets its verbs (each checks the run's scope itself),
// its reading commands and git that reads its worktree; nothing that
// writes outside a verb, and nothing once the run has ended.
#[test]
fn test_pre_tool_use_allows_act_verbs_for_the_open_run_only() {
    require_termd!();
    let mut env = setup_test_env();
    let root = tempdir();
    let rootp = root.path().display().to_string();
    let bin = root.path().join("agent");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = bin.display().to_string();
    env.cfg_mut().agent_binary = bin.clone();
    let actor = spawn_fake(&env, "act-1");
    let other = spawn_fake(&env, "worker");
    let allowed = |name: &str, fake: &FakeHook, command: &str| -> bool {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse", "session_id": fake.session_id(), "cwd": rootp,
            "tool_name": "Bash", "tool_input": {"command": command},
        })
        .to_string();
        let (out, err) = env.service.handle_hook(name, "", "", payload.as_bytes());
        if let Err(e) = err {
            panic!("HandleHook {payload}: {e:#}");
        }
        let v: serde_json::Value =
            serde_json::from_slice(&out.unwrap_or_default()).unwrap_or_default();
        v["hookSpecificOutput"]["permissionDecision"] == "allow"
    };
    let wt = format!("{rootp}/act/1/app");
    let run = env
        .store
        .open_act_run(
            &agent_store::ActRun {
                item: "friction:x".into(),
                summary: "s".into(),
                scope: "commit".into(),
                worktree: wt.clone(),
                ..Default::default()
            },
            |id| format!("act-{id}"),
        )
        .unwrap();
    assert_eq!(run.agent, "act-1");
    for cmd in [
        format!("{bin} dream act commit \"the fix\""),
        format!("{bin} dream act remove-worktree /x/wt"),
        format!("{bin} dream act done \"ok\""),
        format!("{bin} logs claude:s --talk"),
        format!("{bin} index search \"x\""),
        format!("git -C {wt} diff"),
        format!("git -C {wt} log --oneline -3"),
    ] {
        assert!(allowed("act-1", &actor, &cmd), "{cmd}: want allowed");
    }
    for cmd in [
        format!("git -C {wt} commit -m x"),
        format!("git -C {wt} push"),
        format!("git -C {wt} diff --output=/tmp/x"),
        format!("{bin} send worker hi"),
        format!("{bin} dream note x"),
        format!("{bin} dream act start 1 --scope branch"),
        format!("{bin} cleanup worker"),
        "rm -rf /".to_string(),
    ] {
        assert!(
            !allowed("act-1", &actor, &cmd),
            "{cmd}: want refused for an act run"
        );
    }
    assert!(
        !allowed("worker", &other, &format!("{bin} dream act commit \"x\"")),
        "another agent's act verbs"
    );
    env.store.end_act_run(run.id, "done", "ok").unwrap();
    assert!(
        !allowed("act-1", &actor, &format!("{bin} dream act note x")),
        "the run has ended"
    );
}
