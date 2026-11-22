//! Port of `tests/natsbus/rpc_test.go`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_bus::{
    ALL_OPS, AgentRef, CaptureRequest, Context, DEFAULT_REMOTE_OPS, FRICTION_RESOLVE,
    FrictionEntry, FrictionRef, FrictionRequest, KeysRequest, LogsRequest, OP_CAPTURE, OP_FRICTION,
    OP_KEYS, OP_RESPOND, OP_SEND, OP_SPAWN, OP_STATUS, RESPONSE_DENY, RPCResponse, RemoteError,
    RespondRequest, ResumeRequest, ResumeResponse, RpcClient, RpcServer, SendRequest, SpawnRequest,
    SpawnResponse, StatusRequest, serve_rpc, subject_rpc,
};

use crate::common::{FakeRpc, LAZY_DELAY, RecordedReq, TestBus};

fn worker() -> AgentRef {
    AgentRef::new("worker", "h1")
}

fn serve_fake(
    ops: &[&str],
    timeout: Duration,
) -> Option<(TestBus, Arc<FakeRpc>, RpcClient, RpcServer)> {
    let bus = TestBus::start()?;
    let fake = FakeRpc::new();
    let srv = serve_rpc(&bus.conn, "h1", fake.clone(), ops).expect("ServeRPC");
    let client = RpcClient::new(bus.connect_another(), timeout);
    Some((bus, fake, client, srv))
}

#[test]
fn test_rpc_all_ops_round_trip() {
    let Some((_bus, fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };
    let ctx = Context::background();
    let w = worker();

    let agents = client.list(&ctx, "h1").unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].name, "worker");
    assert_eq!(agents[0].session, "agent-worker");

    let listed = client.list_host(&ctx, "h1").unwrap();
    assert_eq!(listed.agent_binary, "/opt/gomux/agent");
    assert_eq!(listed.agents.len(), 1);

    let st = client.status(&ctx, &w).unwrap();
    assert_eq!(st.status, "waiting");
    assert_eq!(st.tool, "Bash");
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Status(StatusRequest {
            agent: "worker".to_string()
        }))
    );

    let out = client.capture(&ctx, &w, 20, true).unwrap();
    assert_eq!(out, "screen of worker");
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Capture(CaptureRequest {
            agent: "worker".to_string(),
            lines: 20,
            raw: true
        }))
    );

    client.send(&ctx, &w, "hello there").unwrap();
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Send(SendRequest {
            agent: "worker".to_string(),
            message: "hello there".to_string(),
            now: false
        }))
    );

    client
        .respond(
            &ctx,
            &w,
            RespondRequest {
                agent: "ignored".to_string(),
                response: RESPONSE_DENY.to_string(),
                force: false,
                expect_tool: "Bash".to_string(),
            },
        )
        .unwrap();
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Respond(RespondRequest {
            agent: "worker".to_string(),
            response: "deny".to_string(),
            force: false,
            expect_tool: "Bash".to_string(),
        }))
    );

    client.keys(&ctx, &w, "ls", "Enter").unwrap();
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Keys(KeysRequest {
            agent: "worker".to_string(),
            text: "ls".to_string(),
            key: "Enter".to_string(),
        }))
    );

    let spawned = client
        .spawn(
            &ctx,
            &w,
            SpawnRequest {
                agent: "ignored".to_string(),
                backend: "claude".to_string(),
                cwd: "/srv/project".to_string(),
                task_file: "tasks/build.md".to_string(),
                task_content: "do the thing".to_string(),
                spawned_by: "lead@laptop".to_string(),
                model: String::new(),
                effort: "high".to_string(),
            },
        )
        .unwrap();
    let want_req = SpawnRequest {
        agent: "worker".to_string(),
        backend: "claude".to_string(),
        cwd: "/srv/project".to_string(),
        task_file: "tasks/build.md".to_string(),
        task_content: "do the thing".to_string(),
        spawned_by: "lead@laptop".to_string(),
        model: String::new(),
        effort: "high".to_string(),
    };
    assert_eq!(fake.last(), Some(RecordedReq::Spawn(want_req)));
    assert_eq!(
        spawned,
        SpawnResponse {
            name: "worker".to_string(),
            session: "agent-worker".to_string(),
            backend: "claude".to_string(),
            spawned_by: "lead@laptop".to_string(),
            cwd: "/srv/project".to_string(),
            task_file: "tasks/build.md".to_string(),
            status: "idle".to_string(),
            ..Default::default()
        }
    );

    let logs = client
        .logs(
            &ctx,
            &w,
            LogsRequest {
                agent: "ignored".to_string(),
                lines: 5,
                full: true,
                json: false,
                terminal: false,
            },
        )
        .unwrap();
    assert_eq!(logs.output, "12:04:31 say    ]0;evilhi\n");
    assert_eq!(logs.hint, "printing [2Jterminal log");
    assert_eq!(logs.truncated, 3);
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Logs(LogsRequest {
            agent: "worker".to_string(),
            lines: 5,
            full: true,
            json: false,
            terminal: false,
        }))
    );

    let resumed = client
        .resume(
            &ctx,
            &w,
            ResumeRequest {
                agent: "ignored".to_string(),
                resumed_by: "lead@laptop".to_string(),
            },
        )
        .unwrap();
    assert_eq!(
        resumed,
        ResumeResponse {
            name: "worker".to_string(),
            session: "agent-worker".to_string(),
            backend: "claude".to_string(),
            session_id: "5f0c".to_string(),
            cwd: "/srv/project".to_string(),
            held: 2,
            status: "idle".to_string(),
            ..Default::default()
        }
    );
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Resume(ResumeRequest {
            agent: "worker".to_string(),
            resumed_by: "lead@laptop".to_string(),
        }))
    );

    let fr = client
        .friction(
            &ctx,
            "h1",
            FrictionRequest {
                action: FRICTION_RESOLVE.to_string(),
                id: 34,
                status: "resolved".to_string(),
                from: Some(FrictionEntry {
                    host: "laptop".to_string(),
                    id: 4,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(fr.resolved);
    assert_eq!(fr.entries.len(), 1);
    assert_eq!(
        fr.entries[0].links[0],
        FrictionRef {
            host: "laptop".to_string(),
            id: 4,
            uid: "laptop-4".to_string(),
        }
    );
    assert_eq!(fr.missing, vec![35]);
    assert!(!fr.entries[0].summary.contains('\x1b'));
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Friction(FRICTION_RESOLVE.to_string()))
    );
}

#[test]
fn test_rpc_spawned_by_needs_a_host() {
    let Some((_bus, fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };
    let ctx = Context::background();
    let w = worker();

    let err = client
        .spawn(
            &ctx,
            &w,
            SpawnRequest {
                backend: "claude".to_string(),
                spawned_by: "lead".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("names no host"), "{err}");
    assert_eq!(fake.last(), None);

    for by in ["", "lead@laptop"] {
        client
            .spawn(
                &ctx,
                &w,
                SpawnRequest {
                    backend: "claude".to_string(),
                    spawned_by: by.to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
        match fake.last() {
            Some(RecordedReq::Spawn(got)) => assert_eq!(got.spawned_by, by),
            other => panic!("unexpected recorded req: {other:?}"),
        }
    }
}

#[test]
fn test_rpc_logs_and_resume_requests_checked() {
    let Some((_bus, fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };
    let ctx = Context::background();
    let w = worker();

    let err = client.logs(&ctx, &w, LogsRequest::default()).unwrap_err();
    assert!(err.to_string().contains("at least 1"), "{err}");

    let err = client
        .resume(
            &ctx,
            &w,
            ResumeRequest {
                agent: String::new(),
                resumed_by: "lead".to_string(),
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("names no host"), "{err}");
    assert_eq!(fake.last(), None);
}

#[test]
fn test_rpc_spawn_uses_the_longer_timeout() {
    let Some((_bus, fake, client, _srv)) = serve_fake(ALL_OPS, Duration::from_millis(50)) else {
        return;
    };
    let lazy = AgentRef::new("lazy", "h1");

    let err = client.status(&Context::background(), &lazy).unwrap_err();
    assert!(
        err.to_string()
            .contains("host \"h1\" did not answer status within 50ms"),
        "err = {err}"
    );

    let start = Instant::now();
    let resp = client
        .spawn(
            &Context::background(),
            &lazy,
            SpawnRequest {
                backend: "claude".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(resp.name, "lazy");
    assert_eq!(resp.status, "idle");
    assert!(start.elapsed() >= LAZY_DELAY);
    assert_eq!(
        fake.last(),
        Some(RecordedReq::Spawn(SpawnRequest {
            agent: "lazy".to_string(),
            backend: "claude".to_string(),
            ..Default::default()
        }))
    );

    // Caller's own deadline still bounds it.
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_millis(50));
    assert!(
        client
            .spawn(
                &ctx,
                &lazy,
                SpawnRequest {
                    backend: "claude".to_string(),
                    ..Default::default()
                },
            )
            .is_err(),
        "Spawn outlived its context's deadline"
    );
}

#[test]
fn test_rpc_remote_error_comes_back_unchanged() {
    let Some((_bus, _fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };

    let err = client
        .status(&Context::background(), &AgentRef::new("ghost", "h1"))
        .unwrap_err();
    let remote = err
        .downcast_ref::<RemoteError>()
        .expect("expected RemoteError");
    assert_eq!(err.to_string(), "agent \"ghost\" not found");
    assert_eq!(remote.host, "h1");
}

#[test]
fn test_rpc_panic_becomes_error_reply() {
    let Some((_bus, _fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };

    let err = client
        .send(&Context::background(), &AgentRef::new("boom", "h1"), "x")
        .unwrap_err();
    assert!(err.to_string().contains("internal error"), "err = {err}");

    // Server survives.
    client
        .status(&Context::background(), &worker())
        .expect("Status after panic");
}

#[test]
fn test_rpc_malformed_requests() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let fake = FakeRpc::new();
    let _srv = serve_rpc(&bus.conn, "h1", fake.clone(), ALL_OPS).unwrap();
    let other = bus.connect_another();

    let cases = [
        (OP_STATUS, "{not json", "malformed status request"),
        (OP_CAPTURE, r#"{"agent": 7}"#, "malformed capture request"),
        (OP_SEND, "{}", "invalid agent name"),
        (OP_RESPOND, r#"{"agent":"../x"}"#, "invalid agent name"),
        (OP_KEYS, r#"{"agent":"worker"}"#, "neither text nor key"),
        (OP_STATUS, "", "invalid agent name"),
        (OP_SPAWN, r#"{"agent":"worker"}"#, "names no backend"),
        (
            OP_SPAWN,
            r#"{"agent":"worker","backend":""}"#,
            "names no backend",
        ),
        (
            OP_SPAWN,
            r#"{"agent":"worker","backend":"claude","spawned_by":"a b"}"#,
            "spawned_by",
        ),
        (
            OP_SPAWN,
            r#"{"agent":"worker","backend":"claude","spawned_by":"x@bad host"}"#,
            "spawned_by",
        ),
        (
            OP_SPAWN,
            r#"{"agent":"worker","backend":"claude","spawned_by":"lead@"}"#,
            "spawned_by",
        ),
        (
            OP_SPAWN,
            r#"{"agent":"a b","backend":"claude"}"#,
            "invalid agent name",
        ),
        (
            OP_SPAWN,
            r#"{"agent":"x@h1","backend":"claude"}"#,
            "invalid agent name",
        ),
        (OP_SPAWN, r#"{"backend":"claude"}"#, "invalid agent name"),
        (
            OP_SPAWN,
            r#"{"agent":"worker","backend":7}"#,
            "malformed spawn request",
        ),
        (OP_SPAWN, "", "invalid agent name"),
        (
            OP_FRICTION,
            r#"{"action":"delete","id":3}"#,
            "unknown friction action",
        ),
        (OP_FRICTION, r#"{"action":"get"}"#, "names no ids"),
        (
            OP_FRICTION,
            r#"{"action":"resolve","status":"resolved"}"#,
            "names no entry",
        ),
        (
            OP_FRICTION,
            r#"{"action":"link","id":3}"#,
            "names no entry it comes from",
        ),
        (
            OP_FRICTION,
            r#"{"action":"resolve","id":3,"status":"resolved"}"#,
            "names no entry it comes from",
        ),
        (
            OP_FRICTION,
            r#"{"action":"resolve","id":3,"status":"resolved","from":{"host":"h2"}}"#,
            "names no entry it comes from",
        ),
        (
            OP_FRICTION,
            r#"{"action":"link","id":3,"from":{"host":"bad host","id":4}}"#,
            "invalid host name",
        ),
        (
            OP_FRICTION,
            r#"{"action":"resolve","id":3,"status":"duplicate"}"#,
            "isn't resolved or wontfix",
        ),
        (
            OP_FRICTION,
            r#"{"action":"get","ids":"x"}"#,
            "malformed friction request",
        ),
    ];

    for (op, body, want) in cases {
        let msg = other
            .request(
                &subject_rpc("h1", op),
                body.as_bytes().to_vec(),
                Duration::from_secs(2),
            )
            .unwrap_or_else(|e| panic!("{op} {body:?}: request failed: {e}"));
        let resp: RPCResponse = serde_json::from_slice(&msg.data)
            .unwrap_or_else(|e| panic!("{op} {body:?}: not RPCResponse: {e}"));
        assert!(
            resp.error.contains(want),
            "{op} {body:?}: error = {:?}, want {want:?}",
            resp.error
        );
    }
    assert_eq!(fake.last(), None);
}

#[test]
fn test_rpc_only_allowed_ops_served() {
    let Some((_bus, _fake, client, _srv)) = serve_fake(DEFAULT_REMOTE_OPS, Duration::ZERO) else {
        return;
    };
    let ctx = Context::background();
    let w = worker();

    let err = client.keys(&ctx, &w, "ls", "").unwrap_err();
    assert!(
        err.to_string().contains("no bus answering for host \"h1\""),
        "{err}"
    );
    let err = client
        .spawn(
            &ctx,
            &w,
            SpawnRequest {
                backend: "claude".to_string(),
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(
        err.to_string().contains("no bus answering for host \"h1\""),
        "{err}"
    );
}

#[test]
fn test_rpc_no_bus_for_host() {
    let Some((_bus, _fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };
    let err = client
        .status(&Context::background(), &AgentRef::new("worker", "h2"))
        .unwrap_err();
    assert!(
        err.to_string().contains("no bus answering for host \"h2\""),
        "{err}"
    );
}

#[test]
fn test_rpc_timeout() {
    let Some((_bus, fake, client, srv)) = serve_fake(ALL_OPS, Duration::from_millis(200)) else {
        return;
    };
    let start = Instant::now();
    let err = client
        .status(&Context::background(), &AgentRef::new("slow", "h1"))
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("host \"h1\" did not answer status within 200ms"),
        "err = {err}"
    );
    assert!(start.elapsed() <= Duration::from_secs(2));
    fake.wait_slow_started();
    srv.stop(); // cancels stuck handler and waits for it
}

#[test]
fn test_rpc_stop_unsubscribes() {
    let Some((_bus, _fake, client, srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };
    srv.stop();
    srv.stop(); // idempotent

    let err = client
        .status(&Context::background(), &worker())
        .unwrap_err();
    assert!(err.to_string().contains("no bus answering"), "err = {err}");
}

#[test]
fn test_rpc_client_needs_host() {
    let Some((_bus, _fake, client, _srv)) = serve_fake(ALL_OPS, Duration::ZERO) else {
        return;
    };
    assert!(
        client
            .status(&Context::background(), &AgentRef::local("worker"))
            .is_err()
    );
    assert!(client.list(&Context::background(), "a.b").is_err());
}

#[test]
fn test_serve_rpc_rejects_bad_config() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    assert!(serve_rpc(&bus.conn, "h.1", FakeRpc::new(), ALL_OPS).is_err());
    assert!(serve_rpc(&bus.conn, "h1", FakeRpc::new(), &["shell"]).is_err());
}
