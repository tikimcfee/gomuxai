//! Go tests/service/prompts_test.go.

use agent_backend::{PermissionResponse, Status};
use agent_service::*;
use agent_store::{StateEntry, Time};

use crate::common::*;
use crate::require_termd;

fn read_hook(event: &str, path: &str) -> String {
    format!(
        r#"{{"hook_event_name":{},"tool_name":"Read","tool_input":{{"file_path":{}}}}}"#,
        serde_json::to_string(event).unwrap(),
        serde_json::to_string(path).unwrap()
    )
}

/// wantPrompt checks the status describes the prompt for Read of path, with
/// detail suffix (e.g. "; 2 more queued").
fn want_prompt(env: &TestEnv, name: &str, path: &str, suffix: &str) -> StatusInfo {
    let info = env.service.status_info(name).unwrap();
    let want_detail = format!("permission: Read: {path}{suffix}");
    let want_input = format!(
        r#"{{"file_path":{}}}"#,
        serde_json::to_string(path).unwrap()
    );
    assert!(
        info.status == AGENT_STATUS_WAITING
            && info.tool == "Read"
            && info.detail == want_detail
            && info.tool_input == want_input,
        "status = {}, tool {:?}, detail {:?}, input {}\nwant waiting on Read, {want_detail:?}, {want_input}",
        info.status,
        info.tool,
        info.detail,
        info.tool_input
    );
    info
}

fn pending_keys(env: &TestEnv, name: &str) -> usize {
    env.store
        .list_state(name)
        .unwrap()
        .iter()
        .filter(|e| e.key.starts_with("_prompt."))
        .count()
}

// Repro (real claude): four parallel Reads raise four PermissionRequests at
// once, and Claude shows them one at a time ("1 of 4"). Status named the
// last one raised (/etc/group) while the screen asked about the first, and
// once that was answered, its PostToolUse left status waiting with no
// detail or tool, so approve --expect couldn't match it.
#[test]
fn test_status_queued_prompts_describe_the_one_on_screen() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    let fake = spawn_fake(&env, "worker");
    fake.set(|s| {
        s.native_found = true;
        s.native = Status::Waiting;
    });

    for p in ["/a", "/b", "/c", "/d"] {
        hook_as(&env, &fake, "worker", &read_hook("PermissionRequest", p));
    }
    want_prompt(&env, "worker", "/a", "; 3 more queued");

    // approve answers the prompt on screen, and the next one shows while
    // the approved call runs.
    let info = env
        .service
        .respond_permission(
            "worker",
            PermissionResponse::Approve,
            PermissionOptions {
                expect_tool: "Read".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("approve: {e:#}"));
    assert_eq!(
        info.detail, "permission: Read: /a; 3 more queued",
        "approve answered the wrong prompt"
    );
    want_prompt(&env, "worker", "/b", "; 2 more queued");
    hook_as(&env, &fake, "worker", &read_hook("PostToolUse", "/a"));
    want_prompt(&env, "worker", "/b", "; 2 more queued");

    // A person answers on screen: the call's PostToolUse ends its prompt.
    hook_as(&env, &fake, "worker", &read_hook("PostToolUse", "/b"));
    want_prompt(&env, "worker", "/c", "; 1 more queued");

    // Deny's Escape cancels the rest of the batch.
    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions::default(),
        )
        .unwrap();
    assert_eq!(
        pending_keys(&env, "worker"),
        0,
        "prompts still pending after deny"
    );
}

// Repro (real claude): a background subagent's Bash prompt ("Bash command ·
// from the general-purpose agent") was up when the main turn's Stop fired,
// which overwrote the report: status said waiting with no detail.
#[test]
fn test_status_subagent_prompt_outlives_main_turn() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    let fake = spawn_fake(&env, "worker");
    fake.set(|s| {
        s.native_found = true;
        s.native = Status::Waiting;
    });

    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"PermissionRequest","agent_id":"a1","agent_type":"general-purpose","tool_name":"Bash","tool_input":{"command":"rmdir sub1"}}"#,
    );
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","last_assistant_message":"started","background_tasks":[{"id":"a1","type":"subagent","status":"running"}]}"#,
    );
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"UserPromptSubmit"}"#,
    ); // the main agent's own prompts only
    let info = env.service.status_info("worker").unwrap();
    assert!(
        info.tool == "Bash"
            && info.detail == "permission: Bash: rmdir sub1 (from the general-purpose agent)",
        "status: tool {:?}, detail {:?}, want the subagent's Bash prompt",
        info.tool,
        info.detail
    );

    // Denying a subagent's call ends only that call; the turn goes on.
    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions {
                expect_tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        env.state_value("worker", STATE_KEY_STATUS),
        "processing",
        "recorded after denying a subagent's call"
    );
    assert_eq!(
        pending_keys(&env, "worker"),
        0,
        "prompts still pending after deny"
    );
}

// Repro (real claude): deny, then send at once. Claude's session registry still
// said waiting for about half a second, so send refused ("waiting on a
// prompt"). A deny that left nothing pending or on screen is believed over
// a native waiting, briefly; a dialog on screen still stops the send.
#[test]
fn test_send_right_after_deny() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    let fake = spawn_fake(&env, "worker");
    fake.set(|s| {
        s.native_found = true;
        s.native = Status::Waiting;
    });

    hook_as(&env, &fake, "worker", &read_hook("PermissionRequest", "/a"));
    env.service
        .respond_permission(
            "worker",
            PermissionResponse::Deny,
            PermissionOptions::default(),
        )
        .unwrap();
    // Native still says waiting, and the dialog is still on screen: type
    // nothing (--now refuses), hold the message.
    assert!(
        env.service
            .send(
                &bg(),
                "worker",
                "hello",
                SendOptions {
                    now: true,
                    ..Default::default()
                }
            )
            .is_err(),
        "typed while a dialog was on screen"
    );
    let res = env
        .service
        .send(&bg(), "worker", "hello", SendOptions::default())
        .unwrap();
    assert!(res.held, "send at a dialog: {res:?}; want held");
    fake.set(|s| s.prompt_gone = true);
    let res = env
        .service
        .send(
            &bg(),
            "worker",
            "do it without reading /a",
            SendOptions::default(),
        )
        .unwrap();
    assert!(
        res.status == AGENT_STATUS_IDLE && res.typed == 2,
        "send right after deny: {res:?}; want idle, typed with the held one"
    );

    // A deny long ago doesn't outweigh native.
    let old = Time(chrono::Local::now().fixed_offset() - chrono::Duration::minutes(1));
    env.store
        .set_states(&[
            StateEntry {
                agent_name: "worker".into(),
                key: STATE_KEY_STATUS.into(),
                value: "idle".into(),
                updated_at: old,
            },
            StateEntry {
                agent_name: "worker".into(),
                key: STATE_KEY_EVENT.into(),
                value: "denied".into(),
                updated_at: old,
            },
        ])
        .unwrap();
    assert_eq!(
        env.service.status("worker").unwrap(),
        AGENT_STATUS_WAITING,
        "a minute after deny: want native's waiting"
    );
}

// A turn that ends with nothing running can't leave a prompt up, so
// prompts nobody answered through agent (Esc pressed by a person) go.
#[test]
fn test_status_stop_ends_pending_prompts() {
    require_termd!();
    let mut env = setup_test_env();
    env.cfg_mut().agent_binary = "/opt/x/agent".into();
    let fake = spawn_fake(&env, "worker");

    hook_as(&env, &fake, "worker", &read_hook("PermissionRequest", "/a"));
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"PermissionRequest","agent_id":"a1","tool_name":"Bash","tool_input":{"command":"ls"}}"#,
    );
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"UserPromptSubmit"}"#,
    );
    assert_eq!(
        pending_keys(&env, "worker"),
        1,
        "after the main agent took a prompt: want only the subagent's"
    );
    hook_as(
        &env,
        &fake,
        "worker",
        r#"{"hook_event_name":"Stop","last_assistant_message":"done"}"#,
    );
    assert_eq!(pending_keys(&env, "worker"), 0, "after Stop");
}
