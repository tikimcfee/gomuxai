//! Tests for Task 2 (`bus-2.md` / `docs/FLEET.md`):
//! - `fleet`, `template`, and `focus` contract types, snake_case/omitempty JSON,
//!   RPC server validation, `RpcClient::{fleet, template}` timeouts & sanitization,
//!   and fast/recognisable errors when a host doesn't serve an op (`NoRespondersError`
//!   and `TimeoutError`).
//! - Pipe-less (`--no-stdio`, `busd.ready`, `busd.links`, `busd.err`) launch,
//!   stop-by-PID, and macOS `Info.plist` / `LOCAL_NETWORK_WARNING` logic.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_base::await_;
use agent_bus::{
    ALL_OPS, AgentInfo, AgentRef, BUS_URL_FILE, BUSD_LINKS_FILE, BUSD_READY_FILE, CaptureRequest,
    CaptureResponse, Context, DEFAULT_REMOTE_OPS, DaemonConfig, DeliverResponse, FLEET_TIMEOUT,
    FleetAgent, FleetFriction, FleetHandoff, FleetHost, FleetJob, FleetLongSession, FleetReport,
    FleetRequest, FleetView, FocusEvent, FrictionRequest, FrictionResponse, Handler, KeysRequest,
    LOCAL_NETWORK_WARNING, ListResponse, LogsRequest, LogsResponse, OP_FLEET, OP_LIST, OP_STATUS,
    OP_TEMPLATE, PeerBundle, Publisher, RespondRequest, ResumeRequest, ResumeResponse, RpcClient,
    SendRequest, SendResponse, SpawnRequest, SpawnResponse, StatusEvent, StatusRequest,
    StatusResponse, TEMPLATE_LIST, TEMPLATE_RESUME, TEMPLATE_START, Template, TemplateRequest,
    TemplateResponse, Time, connect_local, is_no_responders, is_timeout, is_unanswered,
    local_network_warning, publish_focus, serve_rpc, start_daemon_pipeless, stop_pid,
    subject_all_focus, subject_focus, subject_rpc, subscribe_focus,
};

use crate::common::{
    BusHost, TestBus, linked_pair, request, require_busd, test_tempdir, wait_interest, wait_linked,
};

struct FleetFakeHandler {
    host: String,
    slow_fleet: Duration,
}

impl FleetFakeHandler {
    fn new(host: &str) -> Arc<Self> {
        Arc::new(Self {
            host: host.to_string(),
            slow_fleet: Duration::ZERO,
        })
    }

    fn slow(host: &str, delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            host: host.to_string(),
            slow_fleet: delay,
        })
    }
}

impl Handler for FleetFakeHandler {
    fn list(&self, _ctx: &Context) -> anyhow::Result<ListResponse> {
        Ok(ListResponse {
            agents: vec![AgentInfo {
                name: format!("agent-on-{}", self.host),
                session: format!("sess-{}", self.host),
                backend: "claude".to_string(),
                status: "idle".to_string(),
                ..Default::default()
            }],
            agent_binary: "/usr/local/bin/agent".to_string(),
        })
    }

    fn status(&self, _ctx: &Context, req: StatusRequest) -> anyhow::Result<StatusResponse> {
        Ok(StatusResponse {
            status: "idle".to_string(),
            source: "rust".to_string(),
            detail: format!("checked {}", req.agent),
            ..Default::default()
        })
    }

    fn capture(&self, _ctx: &Context, _req: CaptureRequest) -> anyhow::Result<CaptureResponse> {
        Ok(CaptureResponse::default())
    }
    fn send(&self, _ctx: &Context, _req: SendRequest) -> anyhow::Result<Option<SendResponse>> {
        Ok(Some(SendResponse::default()))
    }
    fn respond(&self, _ctx: &Context, _req: RespondRequest) -> anyhow::Result<()> {
        Ok(())
    }
    fn deliver(&self, _ctx: &Context, _req: RespondRequest) -> anyhow::Result<DeliverResponse> {
        Ok(DeliverResponse::default())
    }
    fn keys(&self, _ctx: &Context, _req: KeysRequest) -> anyhow::Result<()> {
        Ok(())
    }
    fn spawn(&self, _ctx: &Context, _req: SpawnRequest) -> anyhow::Result<SpawnResponse> {
        Ok(SpawnResponse::default())
    }
    fn friction(&self, _ctx: &Context, _req: FrictionRequest) -> anyhow::Result<FrictionResponse> {
        Ok(FrictionResponse::default())
    }
    fn logs(&self, _ctx: &Context, _req: LogsRequest) -> anyhow::Result<LogsResponse> {
        Ok(LogsResponse::default())
    }
    fn resume(&self, _ctx: &Context, _req: ResumeRequest) -> anyhow::Result<ResumeResponse> {
        Ok(ResumeResponse::default())
    }

    fn fleet(&self, ctx: &Context, req: FleetRequest) -> anyhow::Result<FleetHost> {
        if !self.slow_fleet.is_zero() {
            let deadline = Instant::now() + self.slow_fleet;
            while Instant::now() < deadline {
                if ctx.err().is_some() {
                    anyhow::bail!("canceled");
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let mut by_kind = BTreeMap::new();
        by_kind.insert("\x1b[31mbug\x1b[0m".to_string(), 2);

        let report = req.report.then(|| {
            let mut by_status = BTreeMap::new();
            by_status.insert("\x1b]0;pwn\x07idle".to_string(), 1);
            FleetReport {
                agents_by_status: by_status,
                long_sessions: vec![FleetLongSession {
                    agent: "\x1b[2Jlead".to_string(),
                    session_id: "s-1".to_string(),
                    backend: "claude".to_string(),
                    started_at: Time::date_utc(2026, 11, 15, 0, 0, 0, 0),
                    compactions: 4,
                }],
                ..Default::default()
            }
        });

        Ok(FleetHost {
            host: self.host.clone(),
            build: "\x1b]52;c;aGVsbG8=\x070.2.0".to_string(),
            reachable: true,
            error: String::new(),
            agents: vec![FleetAgent {
                name: "lead".to_string(),
                backend: "claude".to_string(),
                model: "claude-opus-4-6".to_string(),
                status: "waiting".to_string(),
                detail: "\x1b[31mpermission: Bash\x1b[0m".to_string(),
                since: Time::date_utc(2026, 11, 20, 10, 0, 0, 0),
                held: 2,
                session_id: "sid-1".to_string(),
                session_name: "\x1b]0;evil\x07lead".to_string(),
                cwd: "/srv/project".to_string(),
                flags: vec!["\x1b[2Jstuck: waiting 14m".to_string()],
                children: vec![FleetAgent {
                    name: "helper".to_string(),
                    spawned_by: format!("lead@{}", self.host),
                    detail: "\x1b[?1049hworking".to_string(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            focus: Some(FocusEvent {
                host: self.host.clone(),
                agent: "lead".to_string(),
                session: "\x1b[32magent-lead\x1b[0m".to_string(),
                at: Time::date_utc(2026, 11, 20, 10, 5, 0, 0),
                by: "\x1b]2;title\x07user".to_string(),
            }),
            jobs: vec![FleetJob {
                id: 7,
                script: "\x1b[1mnightly\x1b[0m".to_string(),
                by: "lead".to_string(),
                started_at: Time::date_utc(2026, 11, 20, 10, 2, 0, 0),
                unit: "agent-job-7.scope".to_string(),
                memory_max: "24G".to_string(),
            }],
            handoffs: vec![FleetHandoff {
                agent: "lead".to_string(),
                path: "\x1b[4mdocs/lanes/lead.md\x1b[0m".to_string(),
                doc_at: Time::date_utc(2026, 11, 19, 12, 0, 0, 0),
                commit_at: Time::date_utc(2026, 11, 20, 9, 0, 0, 0),
            }],
            friction: Some(FleetFriction { open: 2, by_kind }),
            report,
        })
    }

    fn template(&self, _ctx: &Context, req: TemplateRequest) -> anyhow::Result<TemplateResponse> {
        match req.action.as_str() {
            TEMPLATE_LIST => Ok(TemplateResponse {
                templates: vec![Template {
                    name: "builder".to_string(),
                    backend: "claude".to_string(),
                    model: "claude-opus-4-6".to_string(),
                    cwd: "/srv/project".to_string(),
                    brief: "\x1b]0;evil\x07/srv/project/docs/lanes/builder.md".to_string(),
                    problem: "\x1b[33mmissing brief\x1b[0m".to_string(),
                }],
                agent: None,
            }),
            TEMPLATE_START | TEMPLATE_RESUME => Ok(TemplateResponse {
                templates: Vec::new(),
                agent: Some(AgentInfo {
                    name: req.template,
                    session: "agent-builder".to_string(),
                    backend: "claude".to_string(),
                    spawned_by: req.from,
                    status: "idle".to_string(),
                    detail: "\x1b[31mstarted\x1b[0m".to_string(),
                    ..Default::default()
                }),
            }),
            other => anyhow::bail!("unexpected action {other}"),
        }
    }
}

#[test]
fn test_fleet_and_template_contract_json_and_ops() {
    assert!(ALL_OPS.contains(&OP_FLEET));
    assert!(ALL_OPS.contains(&OP_TEMPLATE));
    assert!(DEFAULT_REMOTE_OPS.contains(&OP_FLEET));
    assert!(DEFAULT_REMOTE_OPS.contains(&OP_TEMPLATE));
    assert_eq!(subject_focus("hub"), "gomux.hub.focus");
    assert_eq!(subject_all_focus(), "gomux.*.focus");
    assert_eq!(subject_rpc("hub", OP_FLEET), "gomux.hub.rpc.fleet");
    assert_eq!(subject_rpc("hub", OP_TEMPLATE), "gomux.hub.rpc.template");

    // Empty optional fields must be omitted when serialized.
    let req_json = serde_json::to_string(&FleetRequest::default()).unwrap();
    assert_eq!(req_json, "{}");

    let minimal_host = FleetHost {
        host: "hub".to_string(),
        reachable: true,
        ..Default::default()
    };
    let host_json = serde_json::to_string(&minimal_host).unwrap();
    assert_eq!(host_json, r#"{"host":"hub","reachable":true}"#);

    let unreachable = FleetHost {
        host: "laptop".to_string(),
        reachable: false,
        error: "timeout".to_string(),
        ..Default::default()
    };
    let unreachable_json = serde_json::to_string(&unreachable).unwrap();
    assert_eq!(
        unreachable_json,
        r#"{"host":"laptop","reachable":false,"error":"timeout"}"#
    );

    let view = FleetView {
        at: Time::date_utc(2026, 11, 20, 12, 0, 0, 0),
        hosts: vec![minimal_host, unreachable],
    };
    let view_json = serde_json::to_string(&view).unwrap();
    let decoded_view: FleetView = serde_json::from_str(&view_json).unwrap();
    assert_eq!(decoded_view, view);

    let empty_tmpl_resp = serde_json::to_string(&TemplateResponse::default()).unwrap();
    assert_eq!(empty_tmpl_resp, "{}");

    let clear_focus = FocusEvent {
        host: "hub".to_string(),
        agent: String::new(),
        ..Default::default()
    };
    let focus_json = serde_json::to_string(&clear_focus).unwrap();
    assert_eq!(focus_json, r#"{"host":"hub","agent":""}"#);
}

#[test]
fn test_fleet_and_template_round_trip_sanitization_and_validation() {
    let Some(pair) = linked_pair(None, None) else {
        return;
    };
    let _hub_srv = serve_rpc(&pair.hub_conn, "hub", FleetFakeHandler::new("hub"), ALL_OPS).unwrap();
    let _leaf_srv = serve_rpc(
        &pair.leaf_conn,
        "leaf",
        FleetFakeHandler::new("leaf"),
        ALL_OPS,
    )
    .unwrap();
    // Confirm allowed ops (`OP_LIST`) have already propagated across the link before asserting `fleet` is blocked.
    request(&pair.hub_conn, "leaf", OP_LIST).unwrap();
    request(&pair.leaf_conn, "hub", OP_LIST).unwrap();
    // Wait for inter-server leafnode subscription interest (`LS+`) to cross both ways.
    request(&pair.hub_conn, "leaf", OP_FLEET).unwrap();
    request(&pair.leaf_conn, "hub", OP_FLEET).unwrap();

    let hub_client = RpcClient::new(pair.hub_conn.clone(), Duration::ZERO);
    let leaf_client = RpcClient::new(pair.leaf_conn.clone(), Duration::ZERO);
    let ctx = Context::background();

    // Both fleet and template are in DEFAULT_REMOTE_OPS, so they cross hub <-> leaf both ways,
    // and remote terminal escapes are sanitized.
    let got_leaf = hub_client
        .fleet(&ctx, "leaf", FleetRequest { report: true })
        .expect("hub -> leaf fleet");
    let sterm = agent_base::util::sanitize_terminal;
    assert_eq!(got_leaf.host, "leaf");
    assert!(!got_leaf.build.contains(['\x1b', '\x07']));
    assert_eq!(got_leaf.build, sterm("\x1b]52;c;aGVsbG8=\x070.2.0"));
    assert!(got_leaf.reachable);
    assert_eq!(
        got_leaf.agents[0].detail,
        sterm("\x1b[31mpermission: Bash\x1b[0m")
    );
    assert_eq!(
        got_leaf.agents[0].session_name,
        sterm("\x1b]0;evil\x07lead")
    );
    assert_eq!(
        got_leaf.agents[0].flags,
        vec![sterm("\x1b[2Jstuck: waiting 14m")]
    );
    assert_eq!(
        got_leaf.agents[0].children[0].detail,
        sterm("\x1b[?1049hworking")
    );
    assert_eq!(
        got_leaf.focus.as_ref().unwrap().session,
        sterm("\x1b[32magent-lead\x1b[0m")
    );
    assert_eq!(
        got_leaf.focus.as_ref().unwrap().by,
        sterm("\x1b]2;title\x07user")
    );
    assert_eq!(got_leaf.jobs[0].script, sterm("\x1b[1mnightly\x1b[0m"));
    assert_eq!(
        got_leaf.handoffs[0].path,
        sterm("\x1b[4mdocs/lanes/lead.md\x1b[0m")
    );
    assert_eq!(
        got_leaf
            .friction
            .as_ref()
            .unwrap()
            .by_kind
            .get(&sterm("\x1b[31mbug\x1b[0m")),
        Some(&2)
    );
    let rep = got_leaf.report.as_ref().unwrap();
    assert_eq!(
        rep.agents_by_status.get(&sterm("\x1b]0;pwn\x07idle")),
        Some(&1)
    );
    assert_eq!(rep.long_sessions[0].agent, sterm("\x1b[2Jlead"));

    let got_hub = leaf_client
        .fleet(&ctx, "hub", FleetRequest { report: false })
        .expect("leaf -> hub fleet");
    assert_eq!(got_hub.host, "hub");
    assert!(got_hub.report.is_none());

    // Template list / start / resume across the link.
    let list_resp = hub_client
        .template(
            &ctx,
            "leaf",
            TemplateRequest {
                action: TEMPLATE_LIST.to_string(),
                ..Default::default()
            },
        )
        .expect("template list");
    assert_eq!(list_resp.templates.len(), 1);
    assert!(!list_resp.templates[0].brief.contains(['\x1b', '\x07']));
    assert!(!list_resp.templates[0].problem.contains(['\x1b', '\x07']));
    assert_eq!(
        list_resp.templates[0].problem,
        sterm("\x1b[33mmissing brief\x1b[0m")
    );
    assert_eq!(
        list_resp.templates[0].brief,
        sterm("\x1b]0;evil\x07/srv/project/docs/lanes/builder.md")
    );

    let start_resp = leaf_client
        .template(
            &ctx,
            "hub",
            TemplateRequest {
                action: TEMPLATE_START.to_string(),
                template: "builder".to_string(),
                from: "lead@leaf".to_string(),
            },
        )
        .expect("template start");
    let started = start_resp.agent.expect("started agent");
    assert_eq!(started.name, "builder");
    assert_eq!(started.spawned_by, "lead@leaf");
    assert_eq!(started.detail, sterm("\x1b[31mstarted\x1b[0m"));

    // Person starting a template has empty `from`, which is valid.
    let resume_resp = hub_client
        .template(
            &ctx,
            "leaf",
            TemplateRequest {
                action: TEMPLATE_RESUME.to_string(),
                template: "builder".to_string(),
                from: String::new(),
            },
        )
        .expect("template resume");
    assert_eq!(resume_resp.agent.unwrap().name, "builder");

    // Validation errors on bad template requests.
    for bad in [
        TemplateRequest {
            action: "delete".to_string(),
            ..Default::default()
        },
        TemplateRequest {
            action: TEMPLATE_START.to_string(),
            template: String::new(),
            from: "lead@hub".to_string(),
        },
        TemplateRequest {
            action: TEMPLATE_START.to_string(),
            template: "../bad".to_string(),
            from: "lead@hub".to_string(),
        },
        TemplateRequest {
            action: TEMPLATE_START.to_string(),
            template: "builder".to_string(),
            from: "bare-name-no-host".to_string(),
        },
    ] {
        assert!(
            hub_client.template(&ctx, "leaf", bad.clone()).is_err(),
            "bad template request accepted: {bad:?}"
        );
    }
}

#[test]
fn test_fleet_unanswered_when_unsubscribed_or_denied_by_remote_ops() {
    // 1. Nothing subscribes to fleet (e.g. a host that only serves old ops): fails fast
    //    with NoRespondersError so caller can fall back to `list`.
    let Some(bus) = TestBus::start() else {
        return;
    };
    let old_ops = [OP_LIST, OP_STATUS];
    let _srv = serve_rpc(
        &bus.conn,
        &bus.host.name,
        FleetFakeHandler::new(&bus.host.name),
        &old_ops,
    )
    .unwrap();
    let client = RpcClient::new(bus.connect_another(), Duration::ZERO);
    let t0 = Instant::now();
    let err = client
        .fleet(
            &Context::background(),
            &bus.host.name,
            FleetRequest::default(),
        )
        .unwrap_err();
    assert!(
        t0.elapsed() < Duration::from_millis(500),
        "unsubscribed fleet took {:?}, want immediate fail",
        t0.elapsed()
    );
    assert!(
        is_no_responders(&err),
        "want NoRespondersError, got {err:?}"
    );
    assert!(is_unanswered(&err));
    // Fallback to `list` works right away.
    let fallback = client
        .list_host(&Context::background(), &bus.host.name)
        .unwrap();
    assert_eq!(fallback.agents.len(), 1);

    // 2. Linked pair where `--remote-ops` on BOTH sides lacks `fleet` (`list,status` only),
    //    even though both sides subscribe ALL_OPS locally.
    let allowed = Some(vec![OP_LIST.to_string(), OP_STATUS.to_string()]);
    let Some(pair) = linked_pair(allowed.clone(), allowed) else {
        return;
    };
    let _hub_srv = serve_rpc(&pair.hub_conn, "hub", FleetFakeHandler::new("hub"), ALL_OPS).unwrap();
    let _leaf_srv = serve_rpc(
        &pair.leaf_conn,
        "leaf",
        FleetFakeHandler::new("leaf"),
        ALL_OPS,
    )
    .unwrap();
    // Confirm allowed ops (`OP_LIST`) have already propagated across the link before asserting `fleet` is blocked.
    request(&pair.hub_conn, "leaf", OP_LIST).unwrap();
    request(&pair.leaf_conn, "hub", OP_LIST).unwrap();

    // Hub -> Leaf: leaf's DenyImports suppresses interest, so hub gets NoRespondersError immediately.
    let hub_client = RpcClient::new(pair.hub_conn.clone(), Duration::ZERO);
    let t1 = Instant::now();
    let err_leaf = hub_client
        .fleet(&Context::background(), "leaf", FleetRequest::default())
        .unwrap_err();
    assert!(
        t1.elapsed() < Duration::from_millis(500),
        "denied leaf fleet took {:?}",
        t1.elapsed()
    );
    assert!(is_no_responders(&err_leaf), "got {err_leaf:?}");
    assert!(is_unanswered(&err_leaf));
    assert_eq!(
        hub_client
            .list_host(&Context::background(), "leaf")
            .unwrap()
            .agents
            .len(),
        1
    );

    // Leaf -> Hub: hub's peerPermissions denies publish of gomux.hub.rpc.fleet, so
    // nats-server suppresses interest or drops the request — either NoRespondersError
    // or TimeoutError, both matched by `is_unanswered`!
    let leaf_client = RpcClient::new(pair.leaf_conn.clone(), Duration::from_millis(150));
    let err_hub = leaf_client
        .fleet(&Context::background(), "hub", FleetRequest::default())
        .unwrap_err();
    assert!(
        is_no_responders(&err_hub) || is_timeout(&err_hub),
        "want NoRespondersError or TimeoutError, got {err_hub:?}"
    );
    assert!(is_unanswered(&err_hub));
    assert_eq!(
        leaf_client
            .list_host(&Context::background(), "hub")
            .unwrap()
            .agents
            .len(),
        1
    );
}

#[test]
fn test_fleet_uses_3s_default_timeout() {
    assert_eq!(FLEET_TIMEOUT, Duration::from_secs(3));
    let Some(bus) = TestBus::start() else {
        return;
    };
    let srv = serve_rpc(
        &bus.conn,
        &bus.host.name,
        FleetFakeHandler::slow(&bus.host.name, Duration::from_secs(6)),
        ALL_OPS,
    )
    .unwrap();
    // Default RpcClient has 10s general timeout, but `fleet` must time out at 3s.
    let client = RpcClient::new(bus.connect_another(), Duration::ZERO);
    let start = Instant::now();
    let err = client
        .fleet(
            &Context::background(),
            &bus.host.name,
            FleetRequest::default(),
        )
        .unwrap_err();
    let elapsed = start.elapsed();
    assert!(is_timeout(&err), "want TimeoutError, got {err:?}");
    assert!(
        elapsed >= Duration::from_millis(2800) && elapsed < Duration::from_millis(4500),
        "fleet default timeout took {elapsed:?}, want ~3s (not 10s)"
    );
    srv.stop();
}

#[test]
fn test_publisher_publish_focus_and_host_filtered_subscription() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let mut sub_h = subscribe_focus(&bus.conn, Some(&bus.host.name)).unwrap();
    let pub_dir = bus.host.dir().to_path_buf();
    let pub_host = bus.host.name.clone();
    let p = Publisher::new(move || connect_local(&pub_dir, &pub_host));

    // Invalid host or invalid non-empty agent rejected before publishing.
    assert!(
        publish_focus(
            &bus.conn,
            &FocusEvent {
                host: "bad.host".to_string(),
                agent: "a".to_string(),
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        publish_focus(
            &bus.conn,
            &FocusEvent {
                host: bus.host.name.clone(),
                agent: "bad.agent".to_string(),
                ..Default::default()
            }
        )
        .is_err()
    );

    p.publish_focus(&FocusEvent {
        host: bus.host.name.clone(),
        agent: "helper".to_string(),
        session: "agent-helper".to_string(),
        at: Time::date_utc(2026, 11, 20, 12, 0, 0, 0),
        by: "user".to_string(),
    })
    .unwrap();
    p.close().unwrap();

    let ev = sub_h
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("missing focus event");
    assert_eq!(ev.host, bus.host.name);
    assert_eq!(ev.agent, "helper");
}

// ---------------------------------------------------------------------------
// Pipe-less daemon launch, stop-by-PID, and macOS Info.plist warning
// ---------------------------------------------------------------------------

#[test]
fn test_daemon_pipeless_lifecycle_links_errors_and_stop_pid() {
    let Some(bin) = require_busd() else {
        return;
    };

    let hub = BusHost::new("hub");
    let leaf = BusHost::new("leaf");
    hub.trust(leaf.id.public_bundle(""));

    // 1. Startup error in pipe-less mode surfaces the exact identity mismatch message via busd.err.
    let bad_err = match start_daemon_pipeless(DaemonConfig {
        dir: hub.dir().to_path_buf(),
        host: "wronghost".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(bin.clone()),
    }) {
        Ok(_) => panic!("expected mismatched host to fail"),
        Err(e) => e,
    };
    assert!(
        bad_err.to_string().contains("for host \"hub\""),
        "unexpected pipeless error: {bad_err}"
    );

    // 2. Start hub and leaf in pipe-less mode (--no-stdio, busd.ready, busd.links).
    let hub_d = start_daemon_pipeless(DaemonConfig {
        dir: hub.dir().to_path_buf(),
        host: "hub".to_string(),
        client_port: -1,
        leaf_listen: "127.0.0.1:-1".to_string(),
        remote_ops: None,
        busd_path: Some(bin.clone()),
    })
    .expect("starting pipeless hub");
    assert!(!hub_d.disclaimed());
    assert!(hub.dir().join(BUSD_READY_FILE).exists());
    assert!(hub.dir().join(BUSD_LINKS_FILE).exists());
    assert_eq!(hub_d.linked_peers(), 0);

    leaf.trust(
        hub.id
            .public_bundle(&format!("tls://{}", hub_d.leaf_addr())),
    );
    let leaf_d = start_daemon_pipeless(DaemonConfig {
        dir: leaf.dir().to_path_buf(),
        host: "leaf".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(bin),
    })
    .expect("starting pipeless leaf");

    // Link count updates via busd.links!
    wait_linked(&hub_d, 1);
    wait_linked(&leaf_d, 1);

    // RPC & focus events work end-to-end across the pipe-less buses.
    let hub_conn = hub.connect();
    let leaf_conn = leaf.connect();
    let _srv = serve_rpc(&leaf_conn, "leaf", FleetFakeHandler::new("leaf"), ALL_OPS).unwrap();
    let mut focus_sub = subscribe_focus(&hub_conn, Some("leaf")).unwrap();
    request(&hub_conn, "leaf", OP_FLEET).unwrap();
    wait_interest(&hub_conn, &leaf_conn, "leaf");

    let got_fleet = RpcClient::new(hub_conn.clone(), Duration::ZERO)
        .fleet(&Context::background(), "leaf", FleetRequest::default())
        .expect("fleet over pipeless link");
    assert_eq!(got_fleet.host, "leaf");

    publish_focus(
        &leaf_conn,
        &FocusEvent {
            host: "leaf".to_string(),
            agent: "lead".to_string(),
            at: Time::now(),
            ..Default::default()
        },
    )
    .unwrap();
    leaf_conn.flush().unwrap();
    let fev = focus_sub
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("focus event over pipeless link");
    assert_eq!(fev.agent, "lead");
    drop(focus_sub);
    leaf_conn.close();
    hub_conn.close();

    // 3. Test stopping leaf directly by PID via `stop_pid`, and hub via `Daemon::shutdown`.
    let leaf_pid = leaf_d.pid().expect("leaf pid");
    stop_pid(leaf.dir(), leaf_pid);
    for name in [BUS_URL_FILE, BUSD_READY_FILE, BUSD_LINKS_FILE] {
        assert!(
            !leaf.dir().join(name).exists(),
            "leaf {name} still exists after stop_pid"
        );
    }
    leaf_d.shutdown(); // idempotent after stop_pid

    hub_d.shutdown();
    for name in [BUS_URL_FILE, BUSD_READY_FILE, BUSD_LINKS_FILE] {
        assert!(
            !hub.dir().join(name).exists(),
            "hub {name} still exists after shutdown"
        );
    }
}

#[test]
fn test_macos_local_network_warning_conditions() {
    let hub_peer = PeerBundle {
        host: "hub".to_string(),
        nkey: "UDZ7HRETTSGNRVHUYNVXT7RCD".to_string(),
        cert_sha: "ab".repeat(32),
        url: "tls://192.0.2.10:7422".to_string(),
        aka: Vec::new(),
    };
    let leaf_peer = PeerBundle {
        url: String::new(),
        ..hub_peer.clone()
    };

    // Binary without Info.plist, inside g3d-termd, not disclaimed, dialing a hub -> warns with Go's exact text.
    assert_eq!(
        local_network_warning(false, true, false, "", std::slice::from_ref(&hub_peer)),
        Some(LOCAL_NETWORK_WARNING)
    );
    assert!(LOCAL_NETWORK_WARNING.contains("make macos"));
    assert!(LOCAL_NETWORK_WARNING.contains("no route to host"));

    // Binary WITH Info.plist -> no warning.
    assert_eq!(
        local_network_warning(true, true, false, "", std::slice::from_ref(&hub_peer)),
        None
    );
    // Not inside g3d-termd -> no warning.
    assert_eq!(
        local_network_warning(false, false, false, "", std::slice::from_ref(&hub_peer)),
        None
    );
    // Already disclaimed -> no warning.
    assert_eq!(
        local_network_warning(false, true, true, "", std::slice::from_ref(&hub_peer)),
        None
    );
    // Acting as hub (--listen set) -> no warning.
    assert_eq!(
        local_network_warning(
            false,
            true,
            false,
            "0.0.0.0:7422",
            std::slice::from_ref(&hub_peer)
        ),
        None
    );
    // No peer has a URL to dial -> no warning.
    assert_eq!(
        local_network_warning(false, true, false, "", &[leaf_peer]),
        None
    );
}

#[test]
fn test_subscribe_flush_round_trip_prevents_no_responders() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let conn_a = bus.conn.clone();
    let conn_b = bus.connect_another();
    let client_b = RpcClient::new(conn_b, Duration::from_millis(500));
    let ctx = Context::background();

    // Tight loop: serve_rpc (which flushes) -> immediate request on another connection ->
    // stop (which flushes UNSUB) -> immediate request must fail fast with NoRespondersError.
    for i in 0..200 {
        let srv = serve_rpc(
            &conn_a,
            &bus.host.name,
            FleetFakeHandler::new(&bus.host.name),
            &[OP_STATUS],
        )
        .unwrap();
        client_b
            .status(&ctx, &AgentRef::new("w", &bus.host.name))
            .unwrap_or_else(|e| {
                panic!("iteration {i}: immediate request after serve_rpc failed: {e}")
            });
        srv.stop();
        let err = client_b
            .status(&ctx, &AgentRef::new("w", &bus.host.name))
            .unwrap_err();
        assert!(
            is_no_responders(&err),
            "iteration {i}: after srv.stop(), expected immediate NoRespondersError, got {err:?}"
        );
    }
}

#[test]
fn test_rpc_null_data_reply_decodes_as_default() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let host = bus.host.name.clone();
    let _sub = bus
        .conn
        .subscribe_callback(&subject_rpc(&host, "*"), |m| {
            if m.subject.ends_with(".keys") {
                let _ = m.respond(b"{}".to_vec());
            } else if m.subject.ends_with(".logs") {
                // Omitted `data` (not `"data": null`) must still fail `call`.
                let _ = m.respond(b"{}".to_vec());
            } else {
                let _ = m.respond(br#"{"data":null}"#.to_vec());
            }
        })
        .unwrap();
    bus.conn.flush().unwrap();

    let client = RpcClient::new(bus.connect_another(), Duration::from_secs(2));
    let ctx = Context::background();
    let ref_ = AgentRef::new("w", &host);

    // `"data": null` decodes to the zero/default struct, matching Go's json.RawMessage.
    assert_eq!(client.capture(&ctx, &ref_, 10, false).unwrap(), "");
    assert_eq!(
        client.status(&ctx, &ref_).unwrap(),
        StatusResponse::default()
    );
    assert_eq!(
        client.list_host(&ctx, &host).unwrap(),
        ListResponse::default()
    );
    assert_eq!(
        client.fleet(&ctx, &host, FleetRequest::default()).unwrap(),
        FleetHost::default()
    );
    assert_eq!(
        client
            .template(
                &ctx,
                &host,
                TemplateRequest {
                    action: TEMPLATE_LIST.to_string(),
                    ..Default::default()
                }
            )
            .unwrap(),
        TemplateResponse::default()
    );

    // Omitted `"data"` (`{}`) still fails with `"empty logs reply"`.
    let err = client
        .logs(
            &ctx,
            &ref_,
            LogsRequest {
                lines: 5,
                ..Default::default()
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("empty logs reply"), "got {err}");
}

#[test]
fn test_daemon_notices_dead_busd_and_keeps_stderr() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = test_tempdir();
    let fake_busd = tmp.path().join("fake-busd.sh");
    std::fs::write(
        &fake_busd,
        "#!/bin/sh\n\
         echo '{\"client_url\":\"tls://127.0.0.1:1\",\"leaf_addr\":\"\"}' \n\
         echo 'panic: simulated busd runtime crash' >&2\n\
         exit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_busd, std::fs::Permissions::from_mode(0o755)).unwrap();

    let d = agent_bus::start_daemon(DaemonConfig {
        dir: tmp.path().to_path_buf(),
        host: "solo".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(fake_busd),
    })
    .unwrap();

    await_::within(Duration::from_secs(2), "fake busd crash noticed", || {
        let alive = d.is_alive();
        (format!("alive={alive}"), !alive)
    });

    let err = d.check_alive().unwrap_err().to_string();
    assert!(
        err.contains("bus exited with status 2"),
        "missing exit status in check_alive: {err}"
    );
    assert!(
        err.contains("panic: simulated busd runtime crash"),
        "missing stderr output in check_alive: {err}"
    );
    assert!(
        d.stderr_output()
            .contains("panic: simulated busd runtime crash"),
        "stderr_output = {:?}",
        d.stderr_output()
    );

    // Also verify SIGKILL on a real running agent-busd is immediately noticed.
    if let Some(bin) = require_busd() {
        let h = BusHost::new("live");
        let real_d = agent_bus::start_daemon(DaemonConfig {
            dir: h.dir().to_path_buf(),
            host: "live".to_string(),
            client_port: -1,
            leaf_listen: String::new(),
            remote_ops: None,
            busd_path: Some(bin),
        })
        .unwrap();
        assert!(real_d.is_alive());
        assert!(real_d.check_alive().is_ok());
        let pid = real_d.pid().expect("real busd pid");
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
        await_::within(Duration::from_secs(2), "real busd SIGKILL noticed", || {
            let alive = real_d.is_alive();
            (format!("alive={alive}"), !alive)
        });
        let kill_err = real_d.check_alive().unwrap_err().to_string();
        assert!(
            kill_err.contains("bus killed by signal 9"),
            "unexpected kill status: {kill_err}"
        );
    }
}

#[test]
fn test_daemon_exits_on_parent_death_in_pipeless_and_piped_modes() {
    let Some(bin) = require_busd() else {
        return;
    };

    // 1. Piped mode: closing stdin EOF causes `agent-busd` to shut down cleanly and remove `bus.url`.
    let h1 = BusHost::new("pipedeof");
    let mut piped_child = Command::new(&bin)
        .arg("--dir")
        .arg(h1.dir())
        .arg("--host")
        .arg("pipedeof")
        .arg("--client-port")
        .arg("-1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning piped agent-busd");
    let piped_stdin = piped_child.stdin.take().unwrap();
    let mut piped_stdout = BufReader::new(piped_child.stdout.take().unwrap());
    let mut ready_line = String::new();
    piped_stdout.read_line(&mut ready_line).unwrap();
    assert!(h1.dir().join(BUS_URL_FILE).exists());

    // Close stdin without sending SIGTERM -> busd must exit and remove bus.url.
    drop(piped_stdin);
    await_::within(
        Duration::from_secs(3),
        "piped busd to exit on stdin EOF",
        || match piped_child.try_wait() {
            Ok(Some(st)) => (format!("exited: {st}"), st.success()),
            Ok(None) => ("still running".to_string(), false),
            Err(e) => (format!("err: {e}"), false),
        },
    );
    assert!(!h1.dir().join(BUS_URL_FILE).exists());

    // 2. Pipe-less (`--no-stdio`) mode: SIGKILLing the parent process causes `agent-busd`
    //    to detect `getppid() != parent` and shut down cleanly, removing `bus.url`.
    let h2 = BusHost::new("realorphan");
    let mut sh_parent = Command::new("sh")
        .arg("-c")
        .arg("\"$1\" --dir \"$2\" --host realorphan --client-port -1 --no-stdio >/dev/null 2>&1 & sleep 60")
        .arg("sh")
        .arg(&bin)
        .arg(h2.dir())
        .spawn()
        .expect("spawning sh parent");

    await_::within(Duration::from_secs(5), "realorphan busd.ready", || {
        let ok = h2.dir().join(BUSD_READY_FILE).exists();
        (format!("exists={ok}"), ok)
    });
    let ready_bytes = std::fs::read(h2.dir().join(BUSD_READY_FILE)).unwrap();
    let ready_json: serde_json::Value = serde_json::from_slice(&ready_bytes).unwrap();
    let busd_pid = ready_json["pid"].as_i64().unwrap() as i32;
    assert!(busd_pid > 0);

    // SIGKILL the parent shell (simulating OOM kill / SIGKILL of `agent bus run`).
    let _ = sh_parent.kill();
    let _ = sh_parent.wait();

    await_::within(
        Duration::from_secs(3),
        "orphan agent-busd to exit and remove bus.url",
        || {
            let url_gone = !h2.dir().join(BUS_URL_FILE).exists();
            let proc_gone = unsafe { libc::kill(busd_pid, 0) } != 0;
            (
                format!("url_gone={url_gone}, proc_gone={proc_gone}"),
                url_gone && proc_gone,
            )
        },
    );
}

#[test]
fn test_publisher_flush_names_focus_vs_status_events() {
    let Some(_bin) = require_busd() else {
        return;
    };

    // 1. After `publish_focus`, a failing `flush` says "flushing focus events",
    //    never "flushing status events".
    let h_focus = BusHost::new("pubfocus");
    let daemon_focus = h_focus.start("", None);
    let dir_focus = h_focus.dir().to_path_buf();
    let pub_focus = Publisher::new(move || connect_local(&dir_focus, "pubfocus"));
    pub_focus
        .publish_focus(&FocusEvent {
            host: "pubfocus".into(),
            agent: "helper".into(),
            ..Default::default()
        })
        .expect("publish_focus on live daemon");
    daemon_focus.shutdown();
    let focus_err = pub_focus.flush(Duration::from_millis(200)).unwrap_err();
    let focus_msg = format!("{focus_err:#}");
    assert!(
        focus_msg.contains("flushing focus events:"),
        "expected 'flushing focus events:', got: {focus_msg}"
    );
    assert!(!focus_msg.contains("status events"));

    // 2. After `publish_status`, a failing `flush` still says "flushing status events".
    let h_status = BusHost::new("pubstatus");
    let daemon_status = h_status.start("", None);
    let dir_status = h_status.dir().to_path_buf();
    let pub_status = Publisher::new(move || connect_local(&dir_status, "pubstatus"));
    pub_status
        .publish_status(&StatusEvent {
            host: "pubstatus".into(),
            agent: "helper".into(),
            status: "idle".into(),
            ..Default::default()
        })
        .expect("publish_status on live daemon");
    daemon_status.shutdown();
    let status_err = pub_status.flush(Duration::from_millis(200)).unwrap_err();
    let status_msg = format!("{status_err:#}");
    assert!(
        status_msg.contains("flushing status events:"),
        "expected 'flushing status events:', got: {status_msg}"
    );

    // 3. Wire `Template` omits empty `problem` and includes non-empty `problem`.
    let clean_tmpl = Template {
        name: "builder".into(),
        backend: "claude".into(),
        model: String::new(),
        cwd: "/srv/proj".into(),
        brief: String::new(),
        problem: String::new(),
    };
    let clean_json = serde_json::to_string(&clean_tmpl).unwrap();
    assert!(
        !clean_json.contains("problem"),
        "empty problem must be omitted: {clean_json}"
    );
}
