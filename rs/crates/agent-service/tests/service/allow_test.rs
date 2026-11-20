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
