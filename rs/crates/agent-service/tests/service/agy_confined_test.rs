//! A dream's agy agent is confined: its PreToolUse hook allows only what its
//! dream may do, and denies every other call (confine.rs). Other agy agents
//! are left to agy's own settings, as before.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use agent_backend::namedagents::{self, AgyConfig};
use agent_service::*;

use crate::common::*;
use crate::require_termd;

fn spawn_agy(env: &mut TestEnv, name: &str) {
    let o = Arc::new(AgyOverrides {
        name: "fakeagyconfined".into(),
        launch_command_with_hooks: Some(Box::new(|_, _| "true".to_string())),
        append_prompt_flag: Some(Box::new(|_, (cmd, _)| cmd)),
        ..Default::default()
    });
    env.backends.register_factory("fakeagyconfined", move |_| {
        Box::new(AgyFake::new(
            namedagents::new_agy_backend_with_config(AgyConfig::default()),
            &o,
        ))
    });
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: name.into(),
                backend: "fakeagyconfined".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));
}

/// The decision a PreToolUse for `tool` with `args` gets: "allow", "deny",
/// or "" for no output.
fn decision(env: &TestEnv, name: &str, tool: &str, args: serde_json::Value) -> String {
    let payload = serde_json::json!({
        "conversationId": "c1",
        "toolCall": {"name": tool, "args": args},
    })
    .to_string();
    let (out, err) = env
        .service
        .handle_hook(name, "", "PreToolUse", payload.as_bytes());
    if let Err(e) = err {
        panic!("hook {payload}: {e:#}");
    }
    match out {
        None => String::new(),
        Some(b) => {
            let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
            v["decision"].as_str().unwrap_or_default().to_string()
        }
    }
}

/// agy's own encoding: each arg value is JSON text inside a string.
fn enc(s: &str) -> serde_json::Value {
    serde_json::Value::String(serde_json::to_string(s).unwrap())
}

#[test]
fn test_a_dreams_agy_agent_is_confined() {
    require_termd!();
    let mut env = setup_test_env();
    // The command checker resolves the binary, so it must exist.
    let root = tempdir();
    let bin = root.path().join("agent");
    std::fs::write(&bin, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let bin = bin.display().to_string();
    env.cfg_mut().agent_binary = bin.clone();
    spawn_agy(&mut env, "dream-1");
    spawn_agy(&mut env, "helper");

    // Before its dream opens it is an ordinary agy agent: left to agy.
    assert_eq!(
        decision(
            &env,
            "dream-1",
            "write_file",
            serde_json::json!({"Path": enc("/tmp/x")})
        ),
        ""
    );

    let d = env
        .store
        .open_dream("h", &[], |id| format!("dream-{id}"))
        .unwrap();
    assert_eq!(d.agent, "dream-1");
    env.set_state(
        "dream-1",
        STATE_KEY_DREAM_READ,
        r#"["/srv/app","/data/memory"]"#,
    );

    let run = |cmd: &str| serde_json::json!({"CommandLine": enc(cmd)});
    let read = |p: &str| serde_json::json!({"Path": enc(p)});
    for (tool, args, want) in [
        (
            "run_command",
            run(&format!("{bin} dream note \"read 2 sessions\"")),
            "allow",
        ),
        ("run_command", run(&format!("{bin} logs helper")), "allow"),
        (
            "run_command",
            run(&format!("{bin} index search \"dream memory\"")),
            "allow",
        ),
        ("run_command", run(&format!("{bin} index crawl")), "deny"),
        ("run_command", run(&format!("{bin} send helper hi")), "deny"),
        ("run_command", run("rm -rf /srv/app"), "deny"),
        (
            "run_command",
            run(&format!("{bin} dream note x; rm -rf /")),
            "deny",
        ),
        ("read_file", read("/srv/app/src/main.rs"), "allow"),
        ("view_file", read("/data/memory/MEMORY.md"), "allow"),
        ("read_file", read("/etc/passwd"), "deny"),
        ("read_file", read("/srv/app/../../etc/passwd"), "deny"),
        ("write_file", read("/srv/app/x"), "deny"),
        ("edit_file", read("/data/memory/a.md"), "deny"),
        ("search_web", serde_json::json!({"query": enc("x")}), "deny"),
        ("invoke_subagent", serde_json::json!({}), "deny"),
        ("call_mcp_tool", serde_json::json!({}), "deny"),
    ] {
        assert_eq!(
            decision(&env, "dream-1", tool, args.clone()),
            want,
            "{tool} {args}"
        );
    }

    // Another agy agent is untouched by the dream.
    assert_eq!(
        decision(&env, "helper", "write_file", read("/srv/app/x")),
        ""
    );

    // Once the dream closes, it is an ordinary agent again.
    env.store.commit_dream(d.id).unwrap();
    assert_eq!(
        decision(&env, "dream-1", "write_file", read("/srv/app/x")),
        ""
    );
}

/// Spawn writes the agent's given state keys before its CLI starts, so the
/// first hook already sees them: dream #10's critic was refused its own brief
/// when its read roots were written after spawn returned.
#[test]
fn test_spawn_writes_state_before_the_cli_starts() {
    require_termd!();
    let env = setup_test_env();
    let seen: Arc<std::sync::Mutex<Option<String>>> = Arc::default();
    let (store, at_launch) = (env.store.clone(), seen.clone());
    let o = Arc::new(AgyOverrides {
        name: "fakeagystate".into(),
        launch_command_with_hooks: Some(Box::new(|_, _| "true".to_string())),
        // Called as the launch command is built (it has a model).
        with_model: Some(Box::new(move |_, (cmd, _)| {
            *at_launch.lock().unwrap() = store
                .get_state("early", STATE_KEY_DREAM_READ)
                .ok()
                .flatten()
                .map(|e| e.value);
            cmd
        })),
        append_prompt_flag: Some(Box::new(|_, (cmd, _)| cmd)),
        ..Default::default()
    });
    env.backends.register_factory("fakeagystate", move |_| {
        Box::new(AgyFake::new(
            namedagents::new_agy_backend_with_config(AgyConfig::default()),
            &o,
        ))
    });
    env.service
        .spawn(
            &bg(),
            SpawnOptions {
                name: "early".into(),
                backend: "fakeagystate".into(),
                model: "m".into(),
                state: vec![(STATE_KEY_DREAM_READ.into(), r#"["/data/dreams/10"]"#.into())],
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));
    assert_eq!(
        seen.lock().unwrap().as_deref(),
        Some(r#"["/data/dreams/10"]"#),
        "the read roots were there when the CLI's command was built"
    );
}
