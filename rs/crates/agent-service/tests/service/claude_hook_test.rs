//! The global hook (HandleClaudeHook) takes every payload Go's json.Unmarshal
//! takes: Claude Code runs it for every event of every session, so a payload
//! dropped here is a report, an approval or a held-message handover lost.

use std::os::unix::fs::PermissionsExt;

use agent_service::HookSession;
use agent_store::Time;

use crate::common::*;
use crate::require_termd;

// A null where a string goes, half a surrogate pair, and member names in
// another case: the Go decodes all three (a null leaves "", the half pair is
// U+FFFD, names match ignoring case), so each payload is recorded, an
// agent's safe command still approved, and its Stop still sets _result.
#[test]
fn test_handle_claude_hook_takes_what_go_takes() {
    require_termd!();
    let mut env = setup_test_env();
    let root = tempdir();
    let rootp = root.path().display().to_string();
    let bin = root.path().join("bin").join("agent");
    std::fs::create_dir(bin.parent().unwrap()).unwrap();
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    env.cfg_mut().agent_binary = bin.display().to_string();
    let fake = spawn_fake(&env, "worker");
    let sid = fake.session_id();
    let owned = HookSession {
        cli_pid: 1,
        owner: "worker".into(),
        spawn_hooked: false,
    };
    let at = Time::now();

    // Nulls on the optional strings: recorded, and the safe command approved.
    let p = serde_json::json!({
        "hook_event_name": "PreToolUse", "session_id": sid, "cwd": rootp,
        "transcript_path": null, "agent_id": null, "agent_transcript_path": null, "reason": null,
        "tool_name": "Bash", "tool_input": {"command": "./bin/agent list"},
    })
    .to_string();
    let (out, res) = env.service.handle_claude_hook(&owned, p.as_bytes(), at);
    res.unwrap_or_else(|e| panic!("{p}: {e:#}"));
    let out = out.unwrap_or_default();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap_or_default();
    assert!(
        v["hookSpecificOutput"]["permissionDecision"] == "allow",
        "PreToolUse with nulls: hook output {}, want an allow",
        String::from_utf8_lossy(&out)
    );
    let reports = env.service.session_reports().unwrap();
    assert!(
        reports
            .get(&sid)
            .is_some_and(|r| r.event == "PreToolUse" && r.status == "processing"),
        "session report for {sid}: {:?}",
        reports.get(&sid)
    );

    // Half a surrogate pair, in a session no agent owns: recorded with U+FFFD.
    let p = br#"{"hook_event_name":"SessionEnd","session_id":"s-lone","reason":"quit \ud800"}"#;
    let (out, res) = env
        .service
        .handle_claude_hook(&HookSession::default(), p, at);
    res.unwrap_or_else(|e| panic!("lone surrogate: {e:#}"));
    assert!(out.is_none(), "an unowned session's hook printed {out:?}");
    let reports = env.service.session_reports().unwrap();
    let r = reports.get("s-lone").cloned().unwrap_or_default();
    assert!(
        r.event == "SessionEnd"
            && r.status == "ended"
            && r.end_reason == "quit"
            && r.detail == "quit \u{fffd}",
        "session report for s-lone: {r:?}"
    );

    // Member names in another case, and a lone surrogate in the reply: the
    // agent's Stop is heard, its _result set.
    let p = format!(
        r#"{{"Hook_Event_Name":"Stop","Session_ID":"{sid}","last_assistant_message":"done \udc00"}}"#
    );
    let (_, res) = env.service.handle_claude_hook(&owned, p.as_bytes(), at);
    res.unwrap_or_else(|e| panic!("{p}: {e:#}"));
    assert_eq!(env.state_value("worker", "_result"), "done \u{fffd}");
    assert_eq!(env.state_value("worker", "_status"), "idle");
    let reports = env.service.session_reports().unwrap();
    assert!(
        reports.get(&sid).is_some_and(|r| r.event == "Stop"),
        "session report for {sid} after Stop: {:?}",
        reports.get(&sid)
    );
}
