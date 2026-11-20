//! Go tests/service/rpc_test.go.

use agent_backend::PermissionResponse;
use agent_base::await_;
use agent_bus::{AgentRef, RESPONSE_APPROVE, RESPONSE_DENY, RespondRequest, RpcClient, RpcServer};
use agent_service::*;

use crate::common::*;
use crate::require_termd;

/// serveService serves env's service over RPC as host "h1" (every op) and
/// returns a client on a separate connection.
fn serve_service(env: &TestEnv, bus: &TestBus) -> (RpcServer, RpcClient) {
    let mut ops = vec![agent_bus::OP_KEYS];
    ops.extend_from_slice(agent_bus::DEFAULT_REMOTE_OPS);
    serve(bus, &env.service, "h1", &ops)
}

/// waitForScreen polls a remote capture until it contains want.
fn wait_for_screen(client: &RpcClient, r: &AgentRef, want: &str) {
    await_::until(&format!("a remote capture showing {want:?}"), || {
        let out = client
            .capture(&bg(), r, 50, false)
            .unwrap_or_else(|e| panic!("Capture: {e:#}"));
        let ok = out.contains(want);
        (out, ok)
    });
}

#[test]
fn test_rpc_end_to_end_through_service() {
    require_termd!();
    let mut env = setup_test_env();
    let bus = crate::require_bus!();
    env.cfg_mut().session_prefix += "rpc-";
    let fake = spawn_fake_with(
        &env,
        "worker",
        SpawnOptions {
            spawned_by: "lead".into(),
            ..Default::default()
        },
    );
    let (_server, client) = serve_service(&env, &bus);
    let r = AgentRef::new("worker", "h1");

    let agents = client.list(&bg(), "h1").unwrap();
    assert_eq!(agents.len(), 1, "List = {agents:?}");
    let a = &agents[0];
    assert!(
        a.name == "worker"
            && a.backend == "fakehooks"
            && a.session == env.session_name("worker")
            && a.spawned_by == "lead",
        "listed {a:?}"
    );

    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                event: "PermissionRequest".into(),
                detail: "permission: Bash: ls".into(),
                tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let st = client
        .status(&bg(), &r)
        .unwrap_or_else(|e| panic!("Status: {e:#}"));
    assert!(
        st.status == "waiting"
            && st.source == STATUS_SOURCE_REPORT
            && st.tool == "Bash"
            && st.detail == "permission: Bash: ls"
            && !st.updated_at.is_zero(),
        "status = {st:?}"
    );

    // send refuses while the agent waits on a prompt: its Enter would answer
    // the prompt, so a remote message can't be used to approve one.
    let err = client.send(&bg(), &r, "echo nope").unwrap_err();
    assert!(
        format!("{err:#}").contains("waiting on a prompt"),
        "send to a waiting agent: err = {err:#}, want a refusal"
    );
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_IDLE,
                ..Default::default()
            },
        )
        .unwrap();
    fake.set(|s| s.prompt_gone = true); // the dialog is answered

    // send and capture: the shell runs what was sent, and the output shows remotely.
    client
        .send(&bg(), &r, "echo rpc-send-$((40+2))")
        .unwrap_or_else(|e| panic!("Send: {e:#}"));
    wait_for_screen(&client, &r, "rpc-send-42");

    client
        .keys(&bg(), &r, "echo rpc-keys-$((50+5))", "Enter")
        .unwrap_or_else(|e| panic!("Keys: {e:#}"));
    wait_for_screen(&client, &r, "rpc-keys-55");

    // respond: the service's own checks apply, and their errors come back.
    fake.set(|s| s.prompt_gone = false);
    env.service
        .report(
            "worker",
            StatusReport {
                status: AGENT_STATUS_WAITING,
                event: "PermissionRequest".into(),
                detail: "permission: Bash: ls".into(),
                tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap();
    let err = client
        .respond(
            &bg(),
            &r,
            RespondRequest {
                response: RESPONSE_APPROVE.into(),
                expect_tool: "Write".into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains(r#"waiting on "Bash", not "Write""#),
        "approve with wrong --expect: err = {err:#}"
    );
    client
        .respond(
            &bg(),
            &r,
            RespondRequest {
                response: RESPONSE_DENY.into(),
                expect_tool: "Bash".into(),
                ..Default::default()
            },
        )
        .unwrap_or_else(|e| panic!("deny: {e:#}"));
    assert_eq!(
        env.service.status("worker").unwrap(),
        AGENT_STATUS_IDLE,
        "after remote deny"
    );
    let err = client
        .respond(
            &bg(),
            &r,
            RespondRequest {
                response: RESPONSE_APPROVE.into(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("not waiting"),
        "approve while idle: err = {err:#}"
    );
}

#[test]
fn test_rpc_service_errors() {
    require_termd!();
    let mut env = setup_test_env();
    let bus = crate::require_bus!();
    env.cfg_mut().session_prefix += "rpc-";
    spawn_fake(&env, "worker");
    let (_server, client) = serve_service(&env, &bus);

    let err = client
        .status(&bg(), &AgentRef::new("ghost", "h1"))
        .unwrap_err();
    assert!(
        format!("{err:#}").starts_with(r#"agent "ghost" not found (1 agent there"#),
        "status of unknown agent: err = {err:#}"
    );
    let err = client
        .respond(
            &bg(),
            &AgentRef::new("worker", "h1"),
            RespondRequest {
                response: "yes-please".into(),
                force: true,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        format!("{err:#}").contains(r#"invalid permission response "yes-please""#),
        "unknown response: err = {err:#}"
    );
}

#[test]
fn test_permission_response_names() {
    for resp in [
        PermissionResponse::Approve,
        PermissionResponse::ApproveAlways,
        PermissionResponse::Deny,
    ] {
        let name = permission_response_name(resp)
            .unwrap_or_else(|e| panic!("PermissionResponseName({resp:?}): {e:#}"));
        let back = parse_permission_response(&name).unwrap();
        assert_eq!(back, resp, "round trip {resp:?} -> {name:?}");
    }
    assert!(
        parse_permission_response("").is_err(),
        "empty response should be rejected"
    );
}
