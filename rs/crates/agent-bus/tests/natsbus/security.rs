//! Port of `tests/natsbus/security_test.go`.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use agent_base::await_;
use agent_bus::{
    ALL_OPS, AgentInfo, AgentRef, CaptureRequest, CaptureResponse, Conn, Context, Daemon,
    DaemonConfig, DeliverResponse, FRICTION_GET, FocusEvent, FrictionEntry, FrictionRef,
    FrictionRequest, FrictionResponse, Handler, KeysRequest, ListResponse, LogsRequest,
    LogsResponse, MAX_IN_FLIGHT, Msg, OP_CAPTURE, OP_KEYS, OP_STATUS, RespondRequest,
    ResumeRequest, ResumeResponse, RpcClient, SendRequest, SendResponse, SpawnRequest,
    SpawnResponse, StatusEvent, StatusRequest, StatusResponse, Time, connect_local,
    decode_focus_event, decode_status_event, inbox_prefix, publish_focus, publish_status,
    serve_rpc, start_daemon, subject_focus, subject_local, subject_rpc, subject_status,
    subscribe_focus, validate_agent_binary, wait,
};

use crate::common::{
    BusHost, StatusSource, TestBus, publish, request, require_busd, serve_secret, wait_interest,
    wait_linked, wait_opts,
};

fn known_bug(what: &str) -> bool {
    if std::env::var("GOMUXAI_KNOWN_BUGS").as_deref() != Ok("1") {
        eprintln!("skipping known bug (set GOMUXAI_KNOWN_BUGS=1 to run): {what}");
        return true;
    }
    false
}

struct HubCluster {
    _hub: BusHost,
    _hub_daemon: Daemon,
    _leaves: Vec<(BusHost, Daemon)>,
    hub_conn: Conn,
    peers: HashMap<String, Conn>,
}

fn hub_and_peers(names: &[&str]) -> Option<HubCluster> {
    require_busd()?;
    let hub = BusHost::new("hub");
    let mut leaf_hosts = Vec::with_capacity(names.len());
    for &n in names {
        let l = BusHost::new(n);
        hub.trust(l.id.public_bundle(""));
        leaf_hosts.push(l);
    }
    let hub_d = hub.start("127.0.0.1:-1", None);
    let hub_url = format!("tls://{}", hub_d.leaf_addr());
    let mut leaves = Vec::with_capacity(leaf_hosts.len());
    for l in leaf_hosts {
        l.trust(hub.id.public_bundle(&hub_url));
        let d = l.start("", None);
        leaves.push((l, d));
    }
    wait_linked(&hub_d, names.len());
    let mut peers = HashMap::new();
    for (l, _) in &leaves {
        peers.insert(l.name.clone(), l.connect());
    }
    let hub_conn = hub.connect();
    Some(HubCluster {
        _hub: hub,
        _hub_daemon: hub_d,
        _leaves: leaves,
        hub_conn,
        peers,
    })
}

#[test]
fn test_security_peer_cant_read_other_hosts_replies() {
    let Some(cluster) = hub_and_peers(&["alpha", "mallory"]) else {
        return;
    };
    let alpha = cluster.peers.get("alpha").unwrap();
    let mallory = cluster.peers.get("mallory").unwrap();

    let mut spy = mallory.subscribe("_INBOX.>").unwrap();
    mallory.flush().unwrap();
    wait_interest(mallory, &cluster.hub_conn, "hub");
    wait_interest(mallory, alpha, "alpha");

    let _s1 = serve_secret(&cluster.hub_conn, "hub", "hub-capture-secret");
    let _s2 = serve_secret(alpha, "alpha", "alpha-local-secret");
    assert_eq!(
        request(alpha, "hub", OP_CAPTURE).unwrap(),
        "hub-capture-secret"
    );
    assert_eq!(
        request(alpha, "alpha", OP_CAPTURE).unwrap(),
        "alpha-local-secret"
    );
    assert_eq!(
        request(&cluster.hub_conn, "alpha", OP_CAPTURE).unwrap(),
        "alpha-local-secret"
    );

    cluster
        .hub_conn
        .publish(
            &format!("{}.end", inbox_prefix("mallory")),
            b"hub-end".to_vec(),
        )
        .unwrap();
    cluster.hub_conn.flush().unwrap();
    alpha
        .publish(
            &format!("{}.end", inbox_prefix("mallory")),
            b"alpha-end".to_vec(),
        )
        .unwrap();
    alpha.flush().unwrap();

    let mut ends = HashSet::new();
    let mallory_prefix = format!("{}.", inbox_prefix("mallory"));
    while !ends.contains("hub-end") || !ends.contains("alpha-end") {
        let msg = spy
            .next_msg(await_::TIMEOUT)
            .unwrap_or_else(|e| panic!("waiting for mallory replies: {e}; saw {ends:?}"));
        if msg.subject.starts_with(&mallory_prefix) {
            ends.insert(String::from_utf8_lossy(&msg.data).into_owned());
            continue;
        }
        panic!(
            "mallory read a reply meant for someone else on {}: {:?}",
            msg.subject,
            String::from_utf8_lossy(&msg.data)
        );
    }

    assert_eq!(
        request(mallory, "alpha", OP_STATUS).unwrap(),
        "alpha-local-secret"
    );
}

#[test]
fn test_security_peer_cant_forge_replies_to_others() {
    if known_bug("peers may publish to any _INBOX subject") {
        return;
    }
    let Some(cluster) = hub_and_peers(&["alpha", "mallory"]) else {
        return;
    };
    let alpha = cluster.peers.get("alpha").unwrap();
    let mallory = cluster.peers.get("mallory").unwrap().clone();

    let (tx, rx) = mpsc::channel::<String>();
    let _sub = cluster
        .hub_conn
        .subscribe_callback(&subject_rpc("hub", OP_STATUS), move |m| {
            if let Some(ref reply) = m.reply {
                let _ = tx.send(reply.clone());
            }
            // Intentional delay so a forged reply arrives first if allowed.
            std::thread::sleep(Duration::from_millis(500));
            let _ = m.respond(b"real".to_vec());
        })
        .unwrap();
    cluster.hub_conn.flush().unwrap();
    wait_interest(&cluster.hub_conn, alpha, "alpha");

    std::thread::spawn(move || {
        if let Ok(reply) = rx.recv() {
            let _ = mallory.publish(&reply, b"forged".to_vec());
            let _ = mallory.flush();
        }
    });

    let msg = alpha
        .request(
            &subject_rpc("hub", OP_STATUS),
            Vec::new(),
            Duration::from_secs(2),
        )
        .unwrap();
    assert_eq!(msg.data, b"real");
}

#[test]
fn test_security_peer_cant_reach_denied_ops_or_local_subjects() {
    let Some(cluster) = hub_and_peers(&["alpha", "mallory"]) else {
        return;
    };
    let alpha = cluster.peers.get("alpha").unwrap();
    let mallory = cluster.peers.get("mallory").unwrap();

    let (hit_tx, hit_rx) = mpsc::channel::<String>();
    let mut guards = Vec::new();
    for (nc, subjects) in [
        (
            &cluster.hub_conn,
            [subject_rpc("hub", OP_KEYS), subject_local("hub", "bus")],
        ),
        (
            alpha,
            [subject_rpc("alpha", OP_KEYS), subject_local("alpha", "bus")],
        ),
    ] {
        for s in subjects {
            let tx = hit_tx.clone();
            let sub = nc
                .subscribe_callback(&s, move |m| {
                    let _ = tx.send(format!(
                        "{} <- {}",
                        m.subject,
                        String::from_utf8_lossy(&m.data)
                    ));
                })
                .unwrap();
            guards.push(sub);
        }
        nc.flush().unwrap();
    }
    wait_interest(&cluster.hub_conn, mallory, "mallory");
    wait_interest(alpha, mallory, "mallory");

    for s in [
        "gomux.hub.rpc.keys",
        "gomux.alpha.rpc.keys",
        "gomux.alpha.rpc.*",
        "gomux.*.rpc.keys",
        "gomux.alpha.rpc.>",
        "gomux.>",
        ">",
        "gomux.hub.local.bus",
        "gomux.alpha.local.bus",
        "gomux.alpha.local.*",
        "gomux.*.local.bus",
    ] {
        mallory.publish(s, s.as_bytes().to_vec()).unwrap();
    }
    mallory.flush().unwrap();
    wait_interest(&cluster.hub_conn, mallory, "mallory");
    wait_interest(alpha, mallory, "mallory");

    let hits: Vec<String> = hit_rx.try_iter().collect();
    assert!(hits.is_empty(), "mallory reached {hits:?}");
}

#[test]
fn test_security_only_one_hub() {
    let Some(bin) = require_busd() else {
        return;
    };
    let hub = BusHost::new("hub");
    let alpha = BusHost::new("alpha");
    let mallory = BusHost::new("mallory");
    hub.trust(alpha.id.public_bundle(""));
    mallory.trust(alpha.id.public_bundle(""));
    let hub_d = hub.start("127.0.0.1:-1", None);
    let mal_d = mallory.start("127.0.0.1:-1", None);
    alpha.trust(
        hub.id
            .public_bundle(&format!("tls://{}", hub_d.leaf_addr())),
    );
    alpha.trust(
        mallory
            .id
            .public_bundle(&format!("tls://{}", mal_d.leaf_addr())),
    );

    let res = start_daemon(DaemonConfig {
        dir: alpha.dir().to_path_buf(),
        host: "alpha".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(bin),
    });
    let err = match res {
        Ok(_d) => panic!("alpha started with two peers advertising hub URLs; v1 supports one hub"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("hub"), "unexpected error: {err}");
}

#[test]
fn test_security_no_system_account() {
    let Some(cluster) = hub_and_peers(&["alpha"]) else {
        return;
    };
    let alpha = cluster.peers.get("alpha").unwrap();
    for (name, nc) in [("hub", &cluster.hub_conn), ("alpha", alpha)] {
        for subj in [
            "$SYS.REQ.SERVER.PING.CONNZ",
            "$SYS.REQ.ACCOUNT.PING.CONNZ",
            "$SYS.REQ.USER.INFO",
        ] {
            if let Ok(msg) = nc.request(subj, Vec::new(), Duration::from_millis(300)) {
                panic!("{name}: {subj} answered ({} bytes)", msg.data.len());
            }
        }
    }
}

#[test]
fn test_security_leaf_port_speaks_tls_first() {
    if require_busd().is_none() {
        return;
    }
    let hub = BusHost::new("hub");
    let d = hub.start("127.0.0.1:-1", None);

    let stream =
        TcpStream::connect_timeout(&d.leaf_addr().parse().unwrap(), Duration::from_secs(1))
            .unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut line = String::new();
    let _ = BufReader::new(stream).read_line(&mut line);
    assert!(
        !line.starts_with("INFO"),
        "leaf port sent plaintext before TLS: {line}"
    );
}

#[test]
fn test_security_event_body_must_match_subject() {
    let bad = Msg::new(
        subject_status("mallory", "worker"),
        br#"{"host":"hub","agent":"builder","status":"idle"}"#.to_vec(),
    );
    assert!(decode_status_event(&bad).is_err());

    let good = Msg::new(
        subject_status("mallory", "worker"),
        br#"{"host":"mallory","agent":"worker","status":"idle"}"#.to_vec(),
    );
    decode_status_event(&good).expect("genuine event rejected");
}

#[test]
fn test_security_forged_event_cant_arm_next() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let pub_conn = bus.connect_another();
    let src = StatusSource::new("idle");
    src.set_on_first(move || {
        std::thread::spawn(move || {
            publish(&pub_conn, "worker", "processing"); // forged
        });
    });

    let mut opts = wait_opts(&src, Duration::from_millis(100), &["idle"]);
    opts.next = true;
    let (ctx, _) = Context::with_timeout(&Context::background(), Duration::from_secs(1));
    assert!(
        wait(&ctx, Some(&bus.conn), opts).is_err(),
        "--next wait released by a forged event"
    );
}

#[test]
fn test_security_local_clients_verify_the_bus() {
    let h = BusHost::new("solo");

    let ln = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = ln.local_addr().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let Ok((mut c, _)) = ln.accept() else {
            return;
        };
        let _ = c.write_all(
            b"INFO {\"server_id\":\"x\",\"version\":\"2.12.5\",\"proto\":1,\"headers\":true,\"max_payload\":1048576,\"nonce\":\"abc\",\"auth_required\":true}\r\n",
        );
        let mut r = BufReader::new(c.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line.starts_with("PING") {
                let _ = c.write_all(b"PONG\r\n");
            }
            let _ = tx.send(line);
        }
    });

    std::fs::write(h.dir().join("bus.url"), format!("nats://{addr}")).unwrap();

    if let Ok(nc) = connect_local(h.dir(), "solo") {
        let _ = publish_status(
            &nc,
            &StatusEvent {
                host: "solo".to_string(),
                agent: "worker".to_string(),
                status: "waiting".to_string(),
                detail: "secret detail".to_string(),
                ..Default::default()
            },
        );
        let _ = nc.flush();
        drop(rx);
        panic!("connected to a server that isn't this host's bus");
    }
}

struct EvilHandler {
    text: String,
}

impl Handler for EvilHandler {
    fn list(&self, _ctx: &Context) -> anyhow::Result<ListResponse> {
        let t = &self.text;
        Ok(ListResponse {
            agents: vec![AgentInfo {
                name: t.clone(),
                detail: t.clone(),
                cwd: t.clone(),
                session_id: t.clone(),
                spawned_by: t.clone(),
                end_reason: t.clone(),
                ..Default::default()
            }],
            agent_binary: t.clone(),
        })
    }

    fn status(&self, _ctx: &Context, _req: StatusRequest) -> anyhow::Result<StatusResponse> {
        let t = &self.text;
        Ok(StatusResponse {
            status: "idle".to_string(),
            end_reason: t.clone(),
            detail: t.clone(),
            model: t.clone(),
            tool_input: r#"{"command":"ok\u001b]52;c;pwned\u0007\u001b[2Jls"}"#.to_string(),
            ..Default::default()
        })
    }

    fn capture(&self, _ctx: &Context, _req: CaptureRequest) -> anyhow::Result<CaptureResponse> {
        Ok(CaptureResponse {
            output: self.text.clone(),
        })
    }

    fn send(&self, _ctx: &Context, req: SendRequest) -> anyhow::Result<Option<SendResponse>> {
        if req.message == "reply" {
            let t = &self.text;
            return Ok(Some(SendResponse {
                status: t.clone(),
                detail: t.clone(),
                held: true,
                held_count: 1,
                held_until: t.clone(),
                typed: 0,
                input_blocked: t.clone(),
                unchecked: t.clone(),
            }));
        }
        anyhow::bail!("{}", self.text)
    }

    fn respond(&self, _ctx: &Context, _req: RespondRequest) -> anyhow::Result<()> {
        Ok(())
    }

    fn deliver(&self, _ctx: &Context, _req: RespondRequest) -> anyhow::Result<DeliverResponse> {
        Ok(DeliverResponse {
            typed: 0,
            held: 1,
            status: self.text.clone(),
        })
    }

    fn keys(&self, _ctx: &Context, _req: KeysRequest) -> anyhow::Result<()> {
        Ok(())
    }

    fn spawn(&self, _ctx: &Context, _req: SpawnRequest) -> anyhow::Result<SpawnResponse> {
        let t = &self.text;
        Ok(SpawnResponse {
            name: t.clone(),
            session: t.clone(),
            backend: t.clone(),
            spawned_by: t.clone(),
            cwd: t.clone(),
            task_file: t.clone(),
            model: t.clone(),
            status: t.clone(),
            detail: t.clone(),
            end_reason: t.clone(),
        })
    }

    fn logs(&self, _ctx: &Context, _req: LogsRequest) -> anyhow::Result<LogsResponse> {
        Ok(LogsResponse {
            output: self.text.clone(),
            hint: self.text.clone(),
            truncated: 0,
        })
    }

    fn resume(&self, _ctx: &Context, _req: ResumeRequest) -> anyhow::Result<ResumeResponse> {
        let t = &self.text;
        Ok(ResumeResponse {
            name: t.clone(),
            session: t.clone(),
            backend: t.clone(),
            session_id: t.clone(),
            cwd: t.clone(),
            status: t.clone(),
            end_reason: t.clone(),
            detail: t.clone(),
            ..Default::default()
        })
    }

    fn friction(&self, _ctx: &Context, _req: FrictionRequest) -> anyhow::Result<FrictionResponse> {
        let t = &self.text;
        Ok(FrictionResponse {
            entries: vec![FrictionEntry {
                host: t.clone(),
                id: 1,
                uid: t.clone(),
                status: t.clone(),
                kind: t.clone(),
                summary: t.clone(),
                resolved_by: t.clone(),
                resolution: t.clone(),
                requested: Some(FrictionRef {
                    host: t.clone(),
                    id: 0,
                    uid: t.clone(),
                }),
                links: vec![FrictionRef {
                    host: t.clone(),
                    id: 0,
                    uid: t.clone(),
                }],
                ..Default::default()
            }],
            missing: Vec::new(),
            resolved: false,
        })
    }
}

#[test]
fn test_rpc_client_sanitizes_remote_text() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let evil = "ok\x1b]0;pwned\x07\x1b[2J\rover\u{009b}write\x00";
    let _srv = serve_rpc(
        &bus.conn,
        "h1",
        Arc::new(EvilHandler {
            text: evil.to_string(),
        }),
        ALL_OPS,
    )
    .unwrap();
    let client = RpcClient::new(bus.conn.clone(), Duration::ZERO);
    let ref_ = AgentRef::new("w", "h1");
    let ctx = Context::background();

    let st = client.status(&ctx, &ref_).unwrap();
    assert!(
        !st.detail
            .contains(['\x1b', '\x07', '\r', '\x00', '\u{009b}'])
    );
    assert_eq!(st.end_reason, st.detail, "status end_reason");
    assert_eq!(st.model, st.detail, "status model");
    let decoded_input: serde_json::Value = serde_json::from_str(&st.tool_input).unwrap();
    let cmd_str = decoded_input["command"].as_str().unwrap();
    assert!(
        !cmd_str.contains(['\x1b', '\x07']),
        "JSON-escaped ESC in tool_input survived: {cmd_str:?}"
    );
    assert_eq!(
        cmd_str,
        agent_base::util::sanitize_terminal("ok\x1b]52;c;pwned\x07\x1b[2Jls")
    );

    let send_resp = client
        .send_message(
            &ctx,
            &ref_,
            SendRequest {
                message: "reply".to_string(),
                ..Default::default()
            },
        )
        .unwrap()
        .expect("SendResponse");
    for (name, f) in [
        ("send status", &send_resp.status),
        ("send detail", &send_resp.detail),
        ("send held_until", &send_resp.held_until),
        ("send input_blocked", &send_resp.input_blocked),
        ("send unchecked", &send_resp.unchecked),
    ] {
        assert_eq!(f, &st.detail, "{name}");
    }

    let out = client.capture(&ctx, &ref_, 10, false).unwrap();
    assert!(!out.contains(['\x1b', '\x07', '\r']));

    let styled = format!("\x1b[1;38;5;196mred\x1b[0m {evil}");
    let _srv2 = serve_rpc(
        &bus.conn,
        "h2",
        Arc::new(EvilHandler { text: styled }),
        ALL_OPS,
    )
    .unwrap();
    let out_styled = client
        .capture(&ctx, &AgentRef::new("w", "h2"), 10, true)
        .unwrap();
    assert_eq!(
        out_styled,
        "\x1b[1;38;5;196mred\x1b[0m ok]0;pwned[2Joverwrite"
    );

    let send_err = client.send(&ctx, &ref_, "x").unwrap_err();
    assert!(!send_err.to_string().contains(['\x1b', '\x07']));

    let d = client.deliver(&ctx, &ref_).unwrap();
    assert!(
        !d.status
            .contains(['\x1b', '\x07', '\r', '\x00', '\u{009b}'])
    );

    let spawned = client
        .spawn(
            &ctx,
            &ref_,
            SpawnRequest {
                backend: "claude".to_string(),
                ..Default::default()
            },
        )
        .unwrap();
    for (name, f) in [
        ("name", &spawned.name),
        ("session", &spawned.session),
        ("backend", &spawned.backend),
        ("spawned_by", &spawned.spawned_by),
        ("cwd", &spawned.cwd),
        ("task_file", &spawned.task_file),
        ("model", &spawned.model),
        ("status", &spawned.status),
        ("detail", &spawned.detail),
        ("end_reason", &spawned.end_reason),
    ] {
        assert_eq!(f, &st.detail, "spawn {name}");
    }

    let logs = client
        .logs(
            &ctx,
            &ref_,
            LogsRequest {
                lines: 5,
                ..Default::default()
            },
        )
        .unwrap();
    let resumed = client
        .resume(&ctx, &ref_, ResumeRequest::default())
        .unwrap();
    let listed = client.list_host(&ctx, "h1").unwrap();
    assert_eq!(listed.agent_binary, st.detail);
    assert!(validate_agent_binary(&listed.agent_binary).is_err());

    let a0 = &listed.agents[0];
    for (name, f) in [
        ("logs output", &logs.output),
        ("logs hint", &logs.hint),
        ("resume name", &resumed.name),
        ("resume session", &resumed.session),
        ("resume backend", &resumed.backend),
        ("resume session_id", &resumed.session_id),
        ("resume cwd", &resumed.cwd),
        ("resume status", &resumed.status),
        ("resume end_reason", &resumed.end_reason),
        ("resume detail", &resumed.detail),
        ("list detail", &a0.detail),
        ("list cwd", &a0.cwd),
        ("list session_id", &a0.session_id),
        ("list spawned_by", &a0.spawned_by),
        ("list end_reason", &a0.end_reason),
    ] {
        assert_eq!(f, &st.detail, "{name}");
    }

    let fr = client
        .friction(
            &ctx,
            "h1",
            FrictionRequest {
                action: FRICTION_GET.to_string(),
                ids: vec![1],
                ..Default::default()
            },
        )
        .unwrap();
    let e = &fr.entries[0];
    let req_ref = e.requested.as_ref().unwrap();
    for (name, f) in [
        ("host", &e.host),
        ("uid", &e.uid),
        ("status", &e.status),
        ("kind", &e.kind),
        ("summary", &e.summary),
        ("resolved_by", &e.resolved_by),
        ("resolution", &e.resolution),
        ("requested.host", &req_ref.host),
        ("requested.uid", &req_ref.uid),
        ("links.host", &e.links[0].host),
        ("links.uid", &e.links[0].uid),
    ] {
        assert_eq!(f, &st.detail, "friction entry {name}");
    }
}

struct BlockingHandler {
    inner: EvilHandler,
    release: (Mutex<bool>, Condvar),
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl BlockingHandler {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: EvilHandler {
                text: String::new(),
            },
            release: (Mutex::new(false), Condvar::new()),
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }

    fn release_all(&self) {
        let (lock, cvar) = &self.release;
        *lock.lock().unwrap() = true;
        cvar.notify_all();
    }
}

impl Handler for BlockingHandler {
    fn list(&self, ctx: &Context) -> anyhow::Result<ListResponse> {
        self.inner.list(ctx)
    }

    fn status(&self, ctx: &Context, _req: StatusRequest) -> anyhow::Result<StatusResponse> {
        let n = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(n, Ordering::SeqCst);
        let (lock, cvar) = &self.release;
        let mut released = lock.lock().unwrap();
        while !*released && !ctx.is_done() {
            let (g, _) = cvar
                .wait_timeout(released, Duration::from_millis(20))
                .unwrap();
            released = g;
        }
        drop(released);
        self.running.fetch_sub(1, Ordering::SeqCst);
        Ok(StatusResponse {
            status: "idle".to_string(),
            ..Default::default()
        })
    }

    fn capture(&self, ctx: &Context, req: CaptureRequest) -> anyhow::Result<CaptureResponse> {
        self.inner.capture(ctx, req)
    }
    fn send(&self, ctx: &Context, req: SendRequest) -> anyhow::Result<Option<SendResponse>> {
        self.inner.send(ctx, req)
    }
    fn respond(&self, ctx: &Context, req: RespondRequest) -> anyhow::Result<()> {
        self.inner.respond(ctx, req)
    }
    fn deliver(&self, ctx: &Context, req: RespondRequest) -> anyhow::Result<DeliverResponse> {
        self.inner.deliver(ctx, req)
    }
    fn keys(&self, ctx: &Context, req: KeysRequest) -> anyhow::Result<()> {
        self.inner.keys(ctx, req)
    }
    fn spawn(&self, ctx: &Context, req: SpawnRequest) -> anyhow::Result<SpawnResponse> {
        self.inner.spawn(ctx, req)
    }
    fn friction(&self, ctx: &Context, req: FrictionRequest) -> anyhow::Result<FrictionResponse> {
        self.inner.friction(ctx, req)
    }
    fn logs(&self, ctx: &Context, req: LogsRequest) -> anyhow::Result<LogsResponse> {
        self.inner.logs(ctx, req)
    }
    fn resume(&self, ctx: &Context, req: ResumeRequest) -> anyhow::Result<ResumeResponse> {
        self.inner.resume(ctx, req)
    }
}

#[test]
fn test_serve_rpc_bounds_in_flight_requests() {
    let Some(bus) = TestBus::start() else {
        return;
    };
    let h = BlockingHandler::new();
    let srv = serve_rpc(&bus.conn, "h1", h.clone(), ALL_OPS).unwrap();

    let client = RpcClient::new(bus.conn.clone(), Duration::from_secs(2));
    let (tx, rx) = mpsc::channel::<bool>();
    for _ in 0..(MAX_IN_FLIGHT + 10) {
        let c = client.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            let err = c.status(&Context::background(), &AgentRef::new("w", "h1"));
            let is_busy = err
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string().contains("busy"));
            let _ = tx.send(is_busy);
        });
    }
    drop(tx);

    let mut got_busy = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while got_busy < 10 {
        let rem = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(rem) {
            Ok(true) => got_busy += 1,
            Ok(false) => {}
            Err(_) => {
                h.release_all();
                panic!("only {got_busy} busy replies; want at least 10");
            }
        }
    }
    let peak = h.peak.load(Ordering::SeqCst);
    h.release_all();
    srv.stop();
    assert!(
        peak <= MAX_IN_FLIGHT,
        "{peak} requests ran at once; limit is {MAX_IN_FLIGHT}"
    );
}

#[test]
fn test_security_focus_permissions_and_subject_binding() {
    let Some(c) = hub_and_peers(&["leaf1", "leaf2"]) else {
        return;
    };
    let leaf1 = &c.peers["leaf1"];
    let leaf2 = &c.peers["leaf2"];

    // Body host must equal subject host; invalid non-empty agent is rejected, while
    // empty agent ("") is valid ("nothing has focus").
    let forged_msg = Msg::new(
        subject_focus("leaf1"),
        serde_json::to_vec(&FocusEvent {
            host: "hub".to_string(),
            agent: "helper".to_string(),
            ..Default::default()
        })
        .unwrap(),
    );
    assert!(decode_focus_event(&forged_msg).is_err());

    let bad_agent = Msg::new(
        subject_focus("leaf1"),
        serde_json::to_vec(&FocusEvent {
            host: "leaf1".to_string(),
            agent: "../bad".to_string(),
            ..Default::default()
        })
        .unwrap(),
    );
    assert!(decode_focus_event(&bad_agent).is_err());

    let cleared = Msg::new(
        subject_focus("leaf1"),
        serde_json::to_vec(&FocusEvent {
            host: "leaf1".to_string(),
            agent: String::new(),
            at: Time::date_utc(2026, 11, 20, 12, 0, 0, 0),
            ..Default::default()
        })
        .unwrap(),
    );
    assert_eq!(decode_focus_event(&cleared).unwrap().agent, "");

    // Subscribe to all focus events on both hub and leaf2, and wait for interest to propagate.
    let mut hub_sub = subscribe_focus(&c.hub_conn, None).unwrap();
    let mut leaf2_sub = subscribe_focus(leaf2, None).unwrap();
    wait_interest(&c.hub_conn, leaf1, "leaf1");
    wait_interest(leaf2, leaf1, "leaf1");

    // leaf1 attempts to forge focus events for hub and leaf2, then publishes its own focus.
    // Because both go out on leaf1's connection in order, receiving leaf1's genuine focus
    // proves the forged publishes already hit the hub and were dropped!
    leaf1
        .publish(
            &subject_focus("hub"),
            serde_json::to_vec(&FocusEvent {
                host: "hub".to_string(),
                agent: "forged".to_string(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
    leaf1
        .publish(
            &subject_focus("leaf2"),
            serde_json::to_vec(&FocusEvent {
                host: "leaf2".to_string(),
                agent: "forged".to_string(),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
    publish_focus(
        leaf1,
        &FocusEvent {
            host: "leaf1".to_string(),
            agent: "worker".to_string(),
            session: "\x1b]52;c;evil\x07agent-worker".to_string(),
            at: Time::date_utc(2026, 11, 20, 12, 1, 0, 0),
            by: "\x1b[2Juser".to_string(),
        },
    )
    .unwrap();
    leaf1.flush().unwrap();

    let got_hub = hub_sub
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("hub did not receive leaf1 focus");
    assert_eq!(
        got_hub.host, "leaf1",
        "hub received forged focus before leaf1's genuine one: {got_hub:?}"
    );
    assert_eq!(got_hub.agent, "worker");
    assert!(!got_hub.session.contains(['\x1b', '\x07']));
    assert!(!got_hub.by.contains(['\x1b', '\x07']));
    assert_eq!(
        got_hub.session,
        agent_base::util::sanitize_terminal("\x1b]52;c;evil\x07agent-worker")
    );
    assert_eq!(
        got_hub.by,
        agent_base::util::sanitize_terminal("\x1b[2Juser")
    );

    let got_leaf2 = leaf2_sub
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("leaf2 did not receive leaf1 focus");
    assert_eq!(
        got_leaf2.host, "leaf1",
        "leaf2 received forged focus: {got_leaf2:?}"
    );
    assert_eq!(got_leaf2.agent, "worker");

    // Clearing focus (agent == "") also crosses leaf1 -> hub and leaf2.
    publish_focus(
        leaf1,
        &FocusEvent {
            host: "leaf1".to_string(),
            agent: String::new(),
            at: Time::date_utc(2026, 11, 20, 12, 2, 0, 0),
            by: "user".to_string(),
            ..Default::default()
        },
    )
    .unwrap();
    leaf1.flush().unwrap();

    let cleared_hub = hub_sub
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("hub did not receive focus clear");
    assert_eq!(cleared_hub.host, "leaf1");
    assert_eq!(cleared_hub.agent, "");

    let cleared_leaf2 = leaf2_sub
        .next_timeout(Duration::from_secs(2))
        .unwrap()
        .expect("leaf2 did not receive focus clear");
    assert_eq!(cleared_leaf2.host, "leaf1");
    assert_eq!(cleared_leaf2.agent, "");
}
