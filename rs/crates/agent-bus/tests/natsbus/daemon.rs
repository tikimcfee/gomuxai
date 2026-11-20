//! Port of `tests/natsbus/daemon_test.go`.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, Instant};

use agent_base::await_;
use agent_bus::{
    ALL_OPS, AgentRef, BUSD_BINARY_NAME, Context, DEFAULT_REMOTE_OPS, OP_KEYS, OP_LOGS, OP_RESUME,
    OP_SEND, OP_SPAWN, OP_STATUS, PeerBundle, RpcClient, SpawnRequest, connect_local, connect_raw,
    find_busd_with, init_identity, is_no_bus, load_identity, load_peers, serve_rpc,
    subject_all_status, subject_status, trust_peer,
};

use crate::common::{
    BusHost, FakeRpc, RecordedReq, linked_pair, request, request_once, require_busd, serve,
    settle_link, test_tempdir, wait_interest, wait_log,
};

#[test]
fn test_bus_rpc_crosses_link_both_ways() {
    let Some(lp) = linked_pair(None, None) else {
        return;
    };
    let _s1 = serve(&lp.hub_conn, "hub");
    let _s2 = serve(&lp.leaf_conn, "leaf");

    let got = request(&lp.leaf_conn, "hub", OP_STATUS).expect("leaf -> hub status");
    assert_eq!(got, "ran gomux.hub.rpc.status");

    let got = request(&lp.hub_conn, "leaf", OP_SEND).expect("hub -> leaf send");
    assert_eq!(got, "ran gomux.leaf.rpc.send");
}

#[test]
fn test_bus_ops_not_allowed_remotely_are_blocked_both_ways() {
    let Some(lp) = linked_pair(None, None) else {
        return;
    };
    let _s1 = serve(&lp.hub_conn, "hub");
    let _s2 = serve(&lp.leaf_conn, "leaf");
    settle_link(&lp.hub_conn, &lp.leaf_conn);

    assert!(request_once(&lp.leaf_conn, "hub", OP_KEYS).is_err());
    assert!(request_once(&lp.hub_conn, "leaf", OP_KEYS).is_err());
    request(&lp.hub_conn, "hub", OP_KEYS).expect("local keys on hub");
}

#[test]
fn test_bus_spawn_is_local_only_by_default() {
    let Some(lp) = linked_pair(None, None) else {
        return;
    };
    let _s1 = serve(&lp.hub_conn, "hub");
    let _s2 = serve(&lp.leaf_conn, "leaf");
    settle_link(&lp.hub_conn, &lp.leaf_conn);

    assert!(request_once(&lp.leaf_conn, "hub", OP_SPAWN).is_err());
    assert!(request_once(&lp.hub_conn, "leaf", OP_SPAWN).is_err());

    assert_eq!(
        request(&lp.hub_conn, "hub", OP_SPAWN).unwrap(),
        "ran gomux.hub.rpc.spawn"
    );
    assert_eq!(
        request(&lp.leaf_conn, "leaf", OP_SPAWN).unwrap(),
        "ran gomux.leaf.rpc.spawn"
    );
}

#[test]
fn test_bus_resume_local_only_logs_served_by_default() {
    let mut leaf_ops: Vec<String> = DEFAULT_REMOTE_OPS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    leaf_ops.push(OP_RESUME.to_string());
    let Some(lp) = linked_pair(None, Some(leaf_ops)) else {
        return;
    };
    let _s1 = serve(&lp.hub_conn, "hub");
    let _s2 = serve(&lp.leaf_conn, "leaf");
    settle_link(&lp.hub_conn, &lp.leaf_conn);

    assert!(request_once(&lp.leaf_conn, "hub", OP_RESUME).is_err());
    assert_eq!(
        request(&lp.hub_conn, "leaf", OP_RESUME).unwrap(),
        "ran gomux.leaf.rpc.resume"
    );
    request(&lp.hub_conn, "hub", OP_RESUME).expect("local resume on hub");
    assert_eq!(
        request(&lp.leaf_conn, "hub", OP_LOGS).unwrap(),
        "ran gomux.hub.rpc.logs"
    );
}

#[test]
fn test_bus_spawn_served_when_a_host_allows_it() {
    let mut leaf_ops: Vec<String> = DEFAULT_REMOTE_OPS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    leaf_ops.push(OP_SPAWN.to_string());
    let Some(lp) = linked_pair(None, Some(leaf_ops)) else {
        return;
    };
    let _s1 = serve(&lp.hub_conn, "hub");
    let _s2 = serve(&lp.leaf_conn, "leaf");
    settle_link(&lp.hub_conn, &lp.leaf_conn);

    assert_eq!(
        request(&lp.hub_conn, "leaf", OP_SPAWN).unwrap(),
        "ran gomux.leaf.rpc.spawn"
    );
    assert!(request_once(&lp.leaf_conn, "hub", OP_SPAWN).is_err());
    request(&lp.hub_conn, "hub", OP_SPAWN).expect("local spawn on hub");
}

#[test]
fn test_bus_spawn_round_trips_across_the_link() {
    let mut hub_ops: Vec<String> = DEFAULT_REMOTE_OPS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    hub_ops.push(OP_SPAWN.to_string());
    let Some(lp) = linked_pair(Some(hub_ops), None) else {
        return;
    };
    let fake = FakeRpc::new();
    let _srv = serve_rpc(&lp.hub_conn, "hub", fake.clone(), ALL_OPS).unwrap();
    let client = RpcClient::new(lp.leaf_conn.clone(), Duration::from_secs(2));

    let req = SpawnRequest {
        agent: String::new(),
        backend: "claude".to_string(),
        cwd: "/srv/project".to_string(),
        task_file: "tasks/build.md".to_string(),
        task_content: String::new(),
        spawned_by: "lead@leaf".to_string(),
        model: String::new(),
    };
    let mut res: Option<anyhow::Result<_>> = None;
    await_::poll(Duration::from_secs(5), || {
        let r = client.spawn(
            &Context::background(),
            &AgentRef::new("helper", "hub"),
            req.clone(),
        );
        let done = match &r {
            Ok(_) => true,
            Err(e) => !e.to_string().contains("no bus answering"),
        };
        let state = format!("{r:?}");
        res = Some(r);
        (state, done)
    });
    let resp = res.unwrap().expect("Spawn across link");
    assert_eq!(resp.name, "helper");
    assert_eq!(resp.session, "agent-helper");
    assert_eq!(resp.spawned_by, "lead@leaf");
    assert_eq!(resp.cwd, "/srv/project");

    let mut want_req = req;
    want_req.agent = "helper".to_string();
    assert_eq!(fake.last(), Some(RecordedReq::Spawn(want_req)));
}

#[test]
fn test_bus_remote_ops_are_configurable_per_host() {
    let Some(lp) = linked_pair(
        Some(vec![OP_STATUS.to_string()]),
        Some(vec![OP_STATUS.to_string(), OP_KEYS.to_string()]),
    ) else {
        return;
    };
    let _s1 = serve(&lp.hub_conn, "hub");
    let _s2 = serve(&lp.leaf_conn, "leaf");
    settle_link(&lp.hub_conn, &lp.leaf_conn);

    assert!(request_once(&lp.leaf_conn, "hub", OP_SEND).is_err());
    request(&lp.hub_conn, "leaf", OP_KEYS).expect("leaf allows keys remotely");
}

#[test]
fn test_bus_events_cross_link_but_peers_cant_speak_for_others() {
    let Some(lp) = linked_pair(None, None) else {
        return;
    };

    let mut hub_sub = lp.hub_conn.subscribe(&subject_all_status("*")).unwrap();
    let mut leaf_sub = lp.leaf_conn.subscribe(&subject_all_status("*")).unwrap();
    lp.hub_conn.flush().unwrap();
    lp.leaf_conn.flush().unwrap();
    wait_interest(&lp.hub_conn, &lp.leaf_conn, "leaf");
    wait_interest(&lp.leaf_conn, &lp.hub_conn, "hub");
    while hub_sub.try_next().is_some() {}
    while leaf_sub.try_next().is_some() {}

    lp.leaf_conn
        .publish(&subject_status("leaf", "a"), b"leaf-real".to_vec())
        .unwrap();
    lp.hub_conn
        .publish(&subject_status("hub", "b"), b"hub-real".to_vec())
        .unwrap();
    lp.leaf_conn
        .publish(&subject_status("hub", "x"), b"leaf-as-hub".to_vec())
        .unwrap();
    lp.leaf_conn
        .publish(&subject_status("other", "y"), b"leaf-as-other".to_vec())
        .unwrap();
    lp.leaf_conn
        .publish(&subject_status("leaf", "end"), b"leaf-end".to_vec())
        .unwrap();
    lp.hub_conn
        .publish(&subject_status("hub", "end"), b"hub-end".to_vec())
        .unwrap();
    lp.leaf_conn.flush().unwrap();
    lp.hub_conn.flush().unwrap();

    let collect_until = |sub: &mut agent_bus::Subscription, end: &str| -> HashSet<String> {
        let mut seen = HashSet::new();
        while !seen.contains(end) {
            let msg = sub
                .next_msg(await_::TIMEOUT)
                .unwrap_or_else(|e| panic!("waiting for {end}: {e}; saw {seen:?}"));
            seen.insert(String::from_utf8_lossy(&msg.data).into_owned());
        }
        seen
    };
    let hub_seen = collect_until(&mut hub_sub, "leaf-end");
    let leaf_seen = collect_until(&mut leaf_sub, "hub-end");

    assert!(hub_seen.contains("leaf-real"), "hub saw {hub_seen:?}");
    assert!(leaf_seen.contains("hub-real"), "leaf saw {leaf_seen:?}");
    assert!(
        !hub_seen.contains("leaf-as-hub"),
        "hub accepted an event a peer published as the hub"
    );
}

#[test]
fn test_bus_untrusted_peers_cant_link() {
    if require_busd().is_none() {
        return;
    }
    for tamper_cert in [true, false] {
        let hub = BusHost::new("hub");
        let leaf = BusHost::new("leaf");
        let impostor = BusHost::new("leaf");

        let mut bundle = leaf.id.public_bundle("");
        if tamper_cert {
            bundle.cert_sha = impostor.id.cert_sha.clone();
        } else {
            bundle.nkey = impostor.id.public_bundle("").nkey;
        }
        hub.trust(bundle);
        let hub_d = hub.start("127.0.0.1:-1", None);
        leaf.trust(
            hub.id
                .public_bundle(&format!("tls://{}", hub_d.leaf_addr())),
        );
        let _leaf_d = leaf.start("", None);

        wait_log(&hub, "Leafnode connection closed");
        assert_eq!(hub_d.linked_peers(), 0);
    }
}

#[test]
fn test_bus_leaf_rejects_an_impostor_hub() {
    if require_busd().is_none() {
        return;
    }
    let hub = BusHost::new("hub");
    let leaf = BusHost::new("leaf");
    let impostor = BusHost::new("hub");
    hub.trust(leaf.id.public_bundle(""));
    let hub_d = hub.start("127.0.0.1:-1", None);

    let mut bundle = hub
        .id
        .public_bundle(&format!("tls://{}", hub_d.leaf_addr()));
    bundle.cert_sha = impostor.id.cert_sha.clone();
    leaf.trust(bundle);
    let _leaf_d = leaf.start("", None);

    wait_log(&hub, "Leafnode connection closed");
    assert_eq!(hub_d.linked_peers(), 0);
}

#[test]
fn test_bus_local_clients_must_authenticate() {
    if require_busd().is_none() {
        return;
    }
    let h = BusHost::new("solo");
    let d = h.start("", None);

    assert!(
        d.client_url().contains("://127.0.0.1:"),
        "client listener should be loopback-only, got {}",
        d.client_url()
    );
    // Unauthenticated client (even pinning the server cert) must be rejected.
    assert!(
        connect_raw(
            d.client_url(),
            Some(&h.id.cert_sha),
            None,
            Duration::from_secs(1)
        )
        .is_err(),
        "unauthenticated client connected"
    );

    // Someone else's local key (even with the real bus.url and server cert pin!) must be rejected.
    let other = BusHost::new("solo");
    let other_local_seed = other.id.local_key.seed().unwrap();
    assert!(
        connect_raw(
            d.client_url(),
            Some(&h.id.cert_sha),
            Some(&other_local_seed),
            Duration::from_secs(1)
        )
        .is_err(),
        "client with wrong local key connected"
    );

    let _ok = h.connect();
}

#[test]
fn test_bus_peers_cant_connect_as_local_clients() {
    if require_busd().is_none() {
        return;
    }
    let hub = BusHost::new("hub");
    let leaf = BusHost::new("leaf");
    hub.trust(leaf.id.public_bundle(""));
    let d = hub.start("", None);

    let leaf_host_seed = leaf.id.host_key.seed().unwrap();
    assert!(
        connect_raw(
            d.client_url(),
            Some(&hub.id.cert_sha),
            Some(&leaf_host_seed),
            Duration::from_secs(1)
        )
        .is_err(),
        "peer host key accepted on the local client port"
    );
}

#[test]
fn test_connect_local_no_bus() {
    let h = BusHost::new("idle");
    let start = Instant::now();
    let err = connect_local(h.dir(), "idle").unwrap_err();
    assert!(is_no_bus(&err), "err = {err}, want ErrNoBus");

    // Stale URL file (bus died without cleanup) also means ErrNoBus.
    std::fs::write(h.dir().join("bus.url"), b"nats://127.0.0.1:1").unwrap();
    let err = connect_local(h.dir(), "idle").unwrap_err();
    assert!(is_no_bus(&err), "stale URL: err = {err}, want ErrNoBus");
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "no-bus detection took {:?}; hooks need it fast",
        start.elapsed()
    );
}

#[test]
fn test_identity_refuses_overwrite_and_loose_secrets() {
    let h = BusHost::new("box");
    assert!(
        init_identity(h.dir(), "box").is_err(),
        "init_identity replaced an existing identity"
    );
    for f in ["host.nk", "local.nk", "tls.key"] {
        let perm = std::fs::metadata(h.dir().join(f))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(perm, 0o600, "{f} mode {perm:o}, want 600");
    }
    std::fs::set_permissions(
        h.dir().join("host.nk"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let err = load_identity(h.dir(), "box").unwrap_err();
    assert!(
        err.to_string().contains("readable by other users"),
        "loose secret permissions not refused: {err}"
    );
}

#[test]
fn test_trust_peer_validates() {
    let h = BusHost::new("box");
    let good = BusHost::new("peer").id.public_bundle("tls://peer:7422");
    let bad = [
        PeerBundle {
            host: "a.b".to_string(),
            nkey: good.nkey.clone(),
            cert_sha: good.cert_sha.clone(),
            ..Default::default()
        },
        PeerBundle {
            host: "peer".to_string(),
            nkey: "not-a-key".to_string(),
            cert_sha: good.cert_sha.clone(),
            ..Default::default()
        },
        PeerBundle {
            host: "peer".to_string(),
            nkey: good.nkey.clone(),
            cert_sha: "abc".to_string(),
            ..Default::default()
        },
        PeerBundle {
            host: "peer".to_string(),
            nkey: good.nkey.clone(),
            cert_sha: good.cert_sha.clone(),
            url: "nats://peer:7422".to_string(),
            aka: Vec::new(),
        },
    ];
    for (i, b) in bad.iter().enumerate() {
        assert!(
            trust_peer(h.dir(), b.clone()).is_err(),
            "bad bundle {i} accepted: {b:?}"
        );
    }
    trust_peer(h.dir(), good.clone()).expect("good bundle rejected");
    assert_eq!(load_peers(h.dir()).unwrap(), vec![good]);
}

#[test]
fn test_identity_bound_to_its_host_name() {
    let h = BusHost::new("box");
    let err = load_identity(h.dir(), "other").unwrap_err();
    assert!(
        err.to_string().contains("for host \"box\""),
        "loading as another host: err = {err}"
    );
    let err = connect_local(h.dir(), "other").unwrap_err();
    assert!(is_no_bus(&err));
    load_identity(h.dir(), "box").expect("loading as its own host");
}

#[test]
fn test_find_busd_beside_exe_and_on_path() {
    let tmp = test_tempdir();
    let dir_a = tmp.path().join("beside");
    let dir_b = tmp.path().join("onpath");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();

    let fake_exe = dir_a.join("agent");
    std::fs::write(&fake_exe, b"").unwrap();

    let busd_b = dir_b.join(BUSD_BINARY_NAME);
    std::fs::write(&busd_b, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&busd_b, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Found on PATH when not beside executable.
    let got = find_busd_with(Some(&fake_exe), Some(dir_b.as_os_str())).unwrap();
    assert_eq!(got, busd_b);

    // Beside executable wins over PATH.
    let busd_a = dir_a.join(BUSD_BINARY_NAME);
    std::fs::write(&busd_a, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&busd_a, std::fs::Permissions::from_mode(0o755)).unwrap();
    let got = find_busd_with(Some(&fake_exe), Some(dir_b.as_os_str())).unwrap();
    assert_eq!(got, busd_a);

    // Relative and empty PATH entries (`.`, `rel/dir`, leading/middle/trailing `:`)
    // must NEVER be used, even when a real executable exists at that relative path!
    let planted = tmp.path().join("planted");
    std::fs::create_dir_all(&planted).unwrap();
    let planted_busd = planted.join(BUSD_BINARY_NAME);
    std::fs::write(&planted_busd, b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&planted_busd, std::fs::Permissions::from_mode(0o755)).unwrap();

    // Construct a valid relative path from the current working directory to `planted`.
    let cwd = std::env::current_dir().unwrap();
    let ups = std::iter::repeat_n("..", cwd.components().count().saturating_sub(1))
        .collect::<Vec<_>>()
        .join("/");
    let rel_planted = std::path::PathBuf::from(ups).join(planted.strip_prefix("/").unwrap());
    assert!(!rel_planted.is_absolute());
    assert!(rel_planted.join(BUSD_BINARY_NAME).exists());

    let bad_only = format!(".:{}:/nonexistent:", rel_planted.display());
    assert!(
        find_busd_with(None, Some(std::ffi::OsStr::new(&bad_only))).is_err(),
        "find_busd_with accepted relative or empty PATH entry: {bad_only}"
    );

    // When followed by a valid absolute PATH entry, skips the relative/empty ones and picks `dir_b`.
    let mixed = format!(".:{}:::{}:", rel_planted.display(), dir_b.display());
    let got_safe = find_busd_with(None, Some(std::ffi::OsStr::new(&mixed))).unwrap();
    assert_eq!(got_safe, busd_b);
}

// The pipe-less launch (macOS's) waits out a slow first launch: on macOS a
// newly installed, newly signed agent-busd can take seconds of signature
// checks before it runs. The wait used to give up at 6s.
#[test]
fn test_daemon_pipeless_waits_out_a_slow_launch() {
    let Some(bin) = require_busd() else {
        return;
    };
    assert!(
        agent_bus::BUSD_READY_TIMEOUT > Duration::from_secs(7),
        "the test's launch takes 7s"
    );
    let host = BusHost::new("slow");
    let slow = host.dir().join("slow-busd");
    std::fs::write(
        &slow,
        format!("#!/bin/sh\nsleep 7\nexec '{}' \"$@\"\n", bin.display()),
    )
    .unwrap();
    std::fs::set_permissions(&slow, std::fs::Permissions::from_mode(0o755)).unwrap();
    let start = Instant::now();
    let d = agent_bus::start_daemon_pipeless(agent_bus::DaemonConfig {
        dir: host.dir().to_path_buf(),
        host: "slow".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(slow),
    })
    .expect("a slow agent-busd comes up");
    assert!(start.elapsed() >= Duration::from_secs(7));
    assert!(!d.client_url().is_empty());
    let nc = connect_local(host.dir(), "slow").expect("connecting to the slow bus");
    nc.close();
    d.shutdown();
}

// A daemon that fails is reported as soon as it exits, not at the deadline:
// a leafnode port that is taken, and a binary that exits at once.
#[test]
fn test_daemon_pipeless_reports_a_failure_at_once() {
    let Some(bin) = require_busd() else {
        return;
    };
    let host = BusHost::new("taken");
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    let start = Instant::now();
    let err = match agent_bus::start_daemon_pipeless(agent_bus::DaemonConfig {
        dir: host.dir().to_path_buf(),
        host: "taken".to_string(),
        client_port: -1,
        leaf_listen: format!("127.0.0.1:{port}"),
        remote_ops: None,
        busd_path: Some(bin),
    }) {
        Ok(_) => panic!("agent-busd came up on a taken port"),
        Err(e) => format!("{e:#}"),
    };
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "port in use took {took:?}: {err}"
    );
    drop(held);

    let bad = host.dir().join("bad-busd");
    std::fs::write(
        &bad,
        "#!/bin/sh\necho 'unknown flag: --bogus' >&2\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
    let start = Instant::now();
    let err = match agent_bus::start_daemon_pipeless(agent_bus::DaemonConfig {
        dir: host.dir().to_path_buf(),
        host: "taken".to_string(),
        client_port: -1,
        leaf_listen: String::new(),
        remote_ops: None,
        busd_path: Some(bad),
    }) {
        Ok(_) => panic!("a failing agent-busd came up"),
        Err(e) => format!("{e:#}"),
    };
    let took = start.elapsed();
    assert!(took < Duration::from_secs(2), "a bad flag took {took:?}");
    assert_eq!(err, "unknown flag: --bogus");
}
