//! Go: internal/cli/bus_identity_test.go.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use agent_base::config::Config;
use agent_bus::Identity;
use agent_store::{Agent, Time};

use super::*;
use crate::cli::bus::bus_test::SessionName;
use crate::cli::bus::{BusInfo, BusStartArgs, start_settings};
use crate::cli::friction_links::friction_links_test::NoTerm;
use crate::cli::remote::parse_target;
use crate::cli::ssh::ssh_dest_for;
use crate::cli::testenv::{TestBus, TestEnv, temp_dir};

/// The CLI on host "local" with a bus identity, no bus running, and
/// restarts recorded instead of run (Go: setupBusIdentity).
struct BusEnv {
    env: TestEnv,
    restarts: Rc<RefCell<Vec<BusSettings>>>,
    /// The test buses running_bus answers from.
    buses: Vec<(TestBus, agent_bus::CallbackSubscription)>,
}

impl BusEnv {
    fn new(env: TestEnv) -> BusEnv {
        if env.ctx.termd().is_err() {
            env.ctx.set_termd(Arc::new(NoTerm));
        }
        env.ctx.set_caller_chain(|| Ok(Vec::new()));
        let cfg = env.ctx.cfg();
        agent_bus::init_identity(cfg.bus_dir(), &cfg.host).unwrap();
        env.no_bus();
        let restarts: Rc<RefCell<Vec<BusSettings>>> = Rc::default();
        let got = Rc::clone(&restarts);
        set_test_restart_bus(Some(Box::new(move |s| {
            got.borrow_mut().push(s.clone());
            Ok(())
        })));
        BusEnv {
            env,
            restarts,
            buses: Vec::new(),
        }
    }

    fn ctx(&self) -> &Ctx {
        &self.env.ctx
    }

    fn out(&self) -> String {
        self.env.take_out()
    }

    fn restarts(&self) -> Vec<BusSettings> {
        self.restarts.borrow().clone()
    }

    /// Answers bus status queries as a bus running with info would (Go:
    /// runningBus). None (skip the test) without agent-busd.
    #[must_use]
    fn running_bus(&mut self, info: BusInfo) -> Option<()> {
        let bus = TestBus::start()?;
        let data = serde_json::to_vec(&info).unwrap();
        let sub = bus
            .conn
            .subscribe_callback(
                &agent_bus::subject_local(&self.ctx().cfg().host, "bus"),
                move |m| {
                    let _ = m.respond(data.clone());
                },
            )
            .unwrap();
        bus.conn.flush().unwrap();
        let (dir, host) = (bus.dir.path().to_path_buf(), bus.host.clone());
        self.ctx()
            .set_bus_connect(move || agent_bus::connect_local(&dir, &host));
        self.buses.push((bus, sub));
        Some(())
    }
}

impl Drop for BusEnv {
    fn drop(&mut self) {
        set_test_restart_bus(None);
    }
}

/// Another machine's bus identity, in its own dir (Go: peerIdentity).
fn peer_identity(name: &str) -> (tempfile::TempDir, Identity) {
    let dir = temp_dir("agent-cli-peer-");
    let id = agent_bus::init_identity(dir.path(), name).unwrap();
    (dir, id)
}

fn write_bundle_file(dir: &tempfile::TempDir, b: &PeerBundle) -> String {
    let path = dir.path().join(format!("{}.pub.json", b.host));
    std::fs::write(&path, serde_json::to_vec(b).unwrap()).unwrap();
    path.display().to_string()
}

fn err_text<T>(r: anyhow::Result<T>) -> String {
    match r {
        Ok(_) => String::new(),
        Err(e) => format!("{e:#}"),
    }
}

fn trust_args(file: &str, renames: &str, no_restart: bool) -> BusTrustArgs {
    BusTrustArgs {
        no_restart,
        renames: renames.into(),
        args: vec![file.into()],
    }
}

fn untrust_args(host: &str, no_restart: bool) -> BusUntrustArgs {
    BusUntrustArgs {
        no_restart,
        args: vec![host.into()],
    }
}

fn rename_args(name: &str) -> BusRenameArgs {
    BusRenameArgs {
        url: String::new(),
        args: vec![name.into()],
    }
}

fn restart_args() -> BusRestartArgs {
    BusRestartArgs {
        client_port: 0,
        listen: String::new(),
        remote_ops: Vec::new(),
        args: Vec::new(),
    }
}

/// A Ctx like ctx's (its store and termd, its output), with cfg changed: Go's
/// tests assign cfg's fields.
fn ctx_with(env: &TestEnv, change: impl FnOnce(&mut Config)) -> Ctx {
    let mut cfg = env.ctx.cfg().clone();
    change(&mut cfg);
    let ctx = Ctx::new(env.out.writer(), env.err.writer());
    ctx.set_cfg(cfg);
    ctx.set_store(Arc::clone(env.ctx.store().unwrap()));
    ctx.set_termd(Arc::clone(env.ctx.termd().unwrap()));
    ctx.set_caller_chain(|| Ok(Vec::new()));
    ctx
}

#[test]
fn bus_rename() {
    let b = BusEnv::new(TestEnv::new());
    let ctx = b.ctx();
    let dir = ctx.cfg().bus_dir();
    let id = agent_bus::load_identity(&dir, "local").unwrap();
    write_bundle(ctx, &id, "tls://local.lan:7422").unwrap();
    let (_hub_dir, hub) = peer_identity("hub");
    agent_bus::trust_peer(&dir, hub.public_bundle("")).unwrap();

    let e = err_text(run_bus_rename(ctx, &rename_args("HUB")));
    assert!(
        e.contains("trusted peer hub"),
        "renaming to a peer's name: {e}"
    );
    run_bus_rename(ctx, &rename_args("studio")).unwrap();
    let out = b.out();
    for want in [
        r#"from "local" to "studio""#,
        &format!("same key {}", id.public_bundle("").nkey),
        "keep AGENT_HOST=local, which now means studio",
        "on hub: agent bus trust studio.pub.json --renames local",
    ] {
        assert!(out.contains(want), "output lacks {want:?}:\n{out}");
    }
    assert!(
        agent_bus::load_identity(&dir, "studio").is_ok(),
        "the identity is studio's"
    );
    let data = std::fs::read(std::path::Path::new(&dir).join("studio.pub.json")).unwrap();
    let nb: PeerBundle = serde_json::from_slice(&data).unwrap();
    assert!(
        nb.host == "studio" && nb.url == "tls://local.lan:7422",
        "new bundle {nb:?}: want studio's, keeping the old url"
    );
    assert!(
        !std::path::Path::new(&dir).join("local.pub.json").exists(),
        "old bundle still there"
    );

    // As this host now is (Go: cfg.Host = "studio"): the old name is this
    // host's still, so replies to name@local stay here.
    let studio = ctx_with(&b.env, |c| c.host = "studio".into());
    let (r, remote) = parse_target(&studio, "lead@LOCAL").unwrap();
    assert!(
        !remote && r.name == "lead",
        "parse_target(lead@LOCAL) = {r:?}, remote {remote}"
    );
    let e = err_text(run_bus_untrust(&studio, &untrust_args("local", false)));
    assert!(
        e.contains("this host"),
        "untrusting a former name of this host: {e}"
    );
}

// A rename stops a running bus and starts it again, as the new name, with
// the settings it ran with; a bus run in the foreground isn't its to stop.
#[test]
fn bus_rename_running_bus_in_the_foreground() {
    crate::require_termd!();
    let mut b = BusEnv::new(TestEnv::with_termd());
    let _name = SessionName::set("rpc-cli-gomux-bus-absent");
    let Some(()) = b.running_bus(BusInfo {
        host: "local".into(),
        client_url: "tls://127.0.0.1:7420".into(),
        ..Default::default()
    }) else {
        return;
    };
    let e = err_text(run_bus_rename(b.ctx(), &rename_args("studio")));
    assert!(
        e.contains("agent bus run"),
        "rename with a foreground bus: {e}"
    );
    let dir = b.ctx().cfg().bus_dir();
    assert!(
        agent_bus::previous_hosts(&dir).is_empty(),
        "a refused rename renamed"
    );
    assert!(
        agent_bus::load_identity(&dir, "local").is_ok(),
        "a refused rename renamed"
    );
}

#[test]
fn bus_trust_renames() {
    let mut b = BusEnv::new(TestEnv::new());
    let dir = b.ctx().cfg().bus_dir();
    let (peer_dir, peer) = peer_identity("old-mac");
    agent_bus::trust_peer(&dir, peer.public_bundle("")).unwrap();
    save_ssh_dests(
        b.ctx(),
        &BTreeMap::from([("old-mac".to_string(), "me@mac.lan".to_string())]),
    )
    .unwrap();
    b.env
        .store()
        .create_agent(&Agent {
            name: "helper".into(),
            session_name: "rpc-cli-helper".into(),
            backend: "claude".into(),
            spawned_by: "lead@old-mac".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();

    // The peer renames itself; its new bundle has the same keys.
    let renamed = agent_bus::rename_identity(peer_dir.path(), "old-mac", "new-mac").unwrap();
    let file = write_bundle_file(&peer_dir, &renamed.public_bundle(""));

    // A plain trust is refused, pointing at --renames.
    let e = err_text(run_bus_trust(b.ctx(), &trust_args(&file, "", false)));
    assert!(
        e.contains(&format!("agent bus trust {file} --renames old-mac")),
        "plain trust of the renamed bundle: {e}"
    );

    let Some(()) = b.running_bus(BusInfo {
        host: "local".into(),
        client_url: "tls://127.0.0.1:7420".into(),
        listen: "0.0.0.0:7422".into(),
        client_port: 7420,
        remote_ops: vec!["list".into(), "status".into()],
        ..Default::default()
    }) else {
        return;
    };
    run_bus_trust(b.ctx(), &trust_args(&file, "Old-Mac", false)).unwrap();
    let out = b.out();
    for want in [
        r#"Trusted "new-mac" as old-mac renamed"#,
        "the spawned_by of 1 agent",
        "its ssh destination (me@mac.lan)",
        "Restarted the bus (--listen 0.0.0.0:7422 --client-port 7420 --remote-ops list,status)",
    ] {
        assert!(out.contains(want), "output lacks {want:?}:\n{out}");
    }
    let restarts = b.restarts();
    assert!(
        restarts.len() == 1 && restarts[0].listen == "0.0.0.0:7422",
        "restarts = {restarts:?}"
    );
    let a = b.env.store().get_agent("helper").unwrap().unwrap();
    assert_eq!(a.spawned_by, "lead@new-mac");
    assert_eq!(ssh_dest_for(b.ctx(), "new-mac"), "me@mac.lan");
    assert_eq!(canonical_host(b.ctx(), "OLD-MAC"), "new-mac");

    // --no-restart leaves it to agent bus restart; it only goes with --renames.
    let e = err_text(run_bus_trust(b.ctx(), &trust_args(&file, "", true)));
    assert!(
        e.contains("goes with --renames"),
        "--no-restart without --renames: {e}"
    );
}

#[test]
fn bus_untrust() {
    let mut b = BusEnv::new(TestEnv::new());
    let dir = b.ctx().cfg().bus_dir();
    let (_hub_dir, hub) = peer_identity("hub");
    agent_bus::trust_peer(&dir, hub.public_bundle("tls://hub.lan:7422")).unwrap();
    save_ssh_dests(
        b.ctx(),
        &BTreeMap::from([("hub".to_string(), "me@hub.lan".to_string())]),
    )
    .unwrap();
    let e = err_text(run_bus_untrust(b.ctx(), &untrust_args("nobody", false)));
    assert!(
        e.contains("trusted: hub"),
        "untrusting a host that isn't trusted: {e}"
    );

    // With the bus running as a leaf: restarted, unless --no-restart.
    let Some(()) = b.running_bus(BusInfo {
        host: "local".into(),
        client_url: "tls://127.0.0.1:7420".into(),
        client_port: 7420,
        remote_ops: vec!["list".into()],
        ..Default::default()
    }) else {
        return;
    };
    run_bus_untrust(b.ctx(), &untrust_args("HUB", true)).unwrap();
    let out = b.out();
    for want in [
        r#"Untrusted "hub""#,
        "Removed its ssh destination (me@hub.lan)",
        "hub was the hub this host links to",
        "Restart the bus to apply: agent bus restart",
        "on hub, agent bus untrust local",
    ] {
        assert!(out.contains(want), "output lacks {want:?}:\n{out}");
    }
    assert!(b.restarts().is_empty(), "--no-restart restarted");
    assert!(
        agent_bus::load_peers(&dir).unwrap().is_empty(),
        "peers left"
    );
    assert!(
        load_ssh_dests(b.ctx()).unwrap().is_empty(),
        "ssh destinations left"
    );

    let (_leaf_dir, leaf) = peer_identity("leaf");
    agent_bus::trust_peer(&dir, leaf.public_bundle("")).unwrap();
    run_bus_untrust(b.ctx(), &untrust_args("leaf", false)).unwrap();
    let restarts = b.restarts();
    assert!(
        restarts.len() == 1 && restarts[0].client_port == 7420,
        "restarts = {restarts:?}"
    );
}

// Restart starts the bus with the settings it reports; one built before the
// reply carried them still says where it listens and its client URL.
#[test]
fn bus_restart_settings() {
    let mut b = BusEnv::new(TestEnv::new());
    let e = err_text(run_bus_restart(b.ctx(), &restart_args()));
    assert!(e.contains("nothing to restart"), "restart with no bus: {e}");
    let Some(()) = b.running_bus(BusInfo {
        host: "local".into(),
        client_url: "tls://127.0.0.1:7499".into(),
        leaf_addr: "0.0.0.0:7422".into(),
        remote_ops: vec!["list".into(), "send".into()],
        ..Default::default()
    }) else {
        return;
    };
    run_bus_restart(b.ctx(), &restart_args()).unwrap();
    let want = "--listen 0.0.0.0:7422 --client-port 7499 --remote-ops list,send";
    let restarts = b.restarts();
    assert!(
        restarts.len() == 1 && restarts[0].to_string() == want,
        "restarted with {restarts:?}"
    );
    let out = b.out();
    assert!(
        out.contains("Bus restarted in session") && out.contains(want),
        "output:\n{out}"
    );
}

// Restart saves the settings it restarts with; a flag changes just that
// one; an op the bus doesn't know is refused before anything is stopped.
#[test]
fn bus_restart_flags_and_saved() {
    let mut b = BusEnv::new(TestEnv::new());
    let Some(()) = b.running_bus(BusInfo {
        host: "local".into(),
        client_url: "tls://127.0.0.1:7420".into(),
        listen: "0.0.0.0:7422".into(),
        client_port: 7420,
        remote_ops: vec!["list".into(), "send".into()],
        ..Default::default()
    }) else {
        return;
    };
    run_bus_restart(b.ctx(), &restart_args()).unwrap();
    let saved = load_bus_settings(b.ctx()).unwrap().expect("saved settings");
    assert_eq!(
        saved.to_string(),
        "--listen 0.0.0.0:7422 --client-port 7420 --remote-ops list,send"
    );

    let ops_given = |f: &str| f == "remote-ops";
    let mut args = restart_args();
    args.remote_ops = vec!["list".into(), "send".into(), "resume".into()];
    bus_restart(b.ctx(), &args, &ops_given).unwrap();
    let want = "--listen 0.0.0.0:7422 --client-port 7420 --remote-ops list,send,resume";
    let restarts = b.restarts();
    assert_eq!(
        restarts.last().unwrap().to_string(),
        want,
        "only the ops changed"
    );
    assert_eq!(
        load_bus_settings(b.ctx()).unwrap().unwrap().to_string(),
        want
    );
    let out = b.out();
    assert!(
        out.contains("settings saved for agent bus start"),
        "output:\n{out}"
    );

    let n = b.restarts().len();
    args.remote_ops = vec!["list".into(), "sned".into()];
    let e = err_text(bus_restart(b.ctx(), &args, &ops_given));
    assert!(
        e.contains(r#"unknown op "sned" in --remote-ops: the ops are list, status"#),
        "restart with a typo: {e}"
    );
    assert_eq!(
        b.restarts().len(),
        n,
        "restarted the bus with an op it doesn't know"
    );
}

// Start uses the saved settings for the flags it isn't given, the defaults
// when none were saved, and refuses a broken settings file.
#[test]
fn bus_start_settings() {
    let b = BusEnv::new(TestEnv::new());
    let ctx = b.ctx();
    let mut args = BusStartArgs {
        client_port: 7420,
        listen: String::new(),
        remote_ops: [
            "list", "status", "capture", "send", "respond", "friction", "logs",
        ]
        .map(String::from)
        .to_vec(),
        args: Vec::new(),
    };
    let none = |_: &str| false;
    let (s, from) = start_settings(ctx, &args, &none).unwrap();
    assert_eq!(
        (s.to_string(), from.as_str()),
        (
            "--client-port 7420 --remote-ops list,status,capture,send,respond,friction,logs"
                .to_string(),
            ""
        ),
        "no saved settings: want the defaults"
    );

    save_bus_settings(
        ctx,
        &BusSettings {
            listen: "0.0.0.0:7422".into(),
            client_port: 7499,
            remote_ops: vec!["list".into(), "resume".into()],
        },
    )
    .unwrap();
    let (s, from) = start_settings(ctx, &args, &none).unwrap();
    assert_eq!(
        s.to_string(),
        "--listen 0.0.0.0:7422 --client-port 7499 --remote-ops list,resume"
    );
    assert_eq!(from, "; saved settings");

    args.client_port = 7500;
    let (s, from) = start_settings(ctx, &args, &|f| f == "client-port").unwrap();
    assert_eq!(
        s.to_string(),
        "--listen 0.0.0.0:7422 --client-port 7500 --remote-ops list,resume"
    );
    assert!(from.contains("with the flags given"), "{from}");

    std::fs::write(bus_settings_path(ctx), "{not json").unwrap();
    let e = err_text(start_settings(ctx, &args, &none));
    assert!(e.contains("fix or delete it"), "broken settings file: {e}");

    // The file as start writes it.
    save_bus_settings(
        ctx,
        &BusSettings {
            listen: String::new(),
            client_port: 7420,
            remote_ops: vec!["list".into()],
        },
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(bus_settings_path(ctx)).unwrap(),
        "{\n  \"client_port\": 7420,\n  \"remote_ops\": [\n    \"list\"\n  ]\n}\n"
    );
}

// --remote-ops defaults to agent_bus::DEFAULT_REMOTE_OPS (fleet and template
// too). A list saved before that, the old default without them, is still
// what start runs with: a person chose it, and only a flag changes it.
#[test]
fn bus_start_keeps_saved_ops_without_the_new_defaults() {
    use crate::cli::testenv::parse_args;
    let defaults: Vec<String> = agent_bus::DEFAULT_REMOTE_OPS
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert!(defaults.contains(&"fleet".to_string()) && defaults.contains(&"template".to_string()));
    let args: BusStartArgs = parse_args("bus start", &[]);
    assert_eq!(args.remote_ops, defaults, "bus start's default");
    let run: crate::cli::bus::BusRunArgs = parse_args("bus run", &[]);
    assert_eq!(run.remote_ops, defaults, "bus run's default");

    let b = BusEnv::new(TestEnv::new());
    let ctx = b.ctx();
    let none = |_: &str| false;
    let (s, from) = start_settings(ctx, &args, &none).unwrap();
    assert_eq!(
        (s.to_string(), from.as_str()),
        (
            "--client-port 7420 --remote-ops list,status,capture,send,respond,friction,logs,fleet,template,manage".to_string(),
            ""
        ),
        "nothing saved: the new defaults"
    );

    let old = [
        "list", "status", "capture", "send", "respond", "friction", "logs",
    ];
    save_bus_settings(
        ctx,
        &BusSettings {
            listen: String::new(),
            client_port: 7420,
            remote_ops: old.map(String::from).to_vec(),
        },
    )
    .unwrap();
    let (s, from) = start_settings(ctx, &args, &none).unwrap();
    assert_eq!(
        (s.to_string(), from.as_str()),
        (
            "--client-port 7420 --remote-ops list,status,capture,send,respond,friction,logs"
                .to_string(),
            "; saved settings"
        ),
        "a saved list is used as saved"
    );
    let (s, _) = start_settings(ctx, &args, &|f| f == "remote-ops").unwrap();
    assert!(
        s.to_string().ends_with(",logs,fleet,template,manage"),
        "--remote-ops given: {s}"
    );
}

// A restart keeps the bus's session, getting a fresh shell under the same
// terminal id, and the new bus run is typed into it. (/bin/echo stands in for
// the agent binary: it prints the command line instead of running a bus.)
#[test]
fn restart_bus_keeps_session() {
    crate::require_termd!();
    let mut b = BusEnv::new(TestEnv::with_termd());
    let _name = SessionName::set("rpc-cli-gomux-bus");
    let Some(()) = b.running_bus(BusInfo {
        host: "local".into(),
        client_url: "tls://127.0.0.1:7420".into(),
        client_port: 7420,
        remote_ops: vec!["list".into()],
        ..Default::default()
    }) else {
        return;
    };
    let termd = Arc::clone(b.env.termd_ops());
    let session = bus_session();
    termd.create_session(&session, &Default::default()).unwrap();
    let before = termd.first_pane(&session).unwrap();
    let (dir, host) = {
        let bus = &b.buses[0].0;
        (bus.dir.path().to_path_buf(), bus.host.clone())
    };
    let with_binary = |bin: &str| {
        let ctx = ctx_with(&b.env, |c| c.agent_binary = bin.into());
        let (dir, host) = (dir.clone(), host.clone());
        ctx.set_bus_connect(move || agent_bus::connect_local(&dir, &host));
        ctx
    };

    let s = BusSettings {
        listen: String::new(),
        client_port: 7499,
        remote_ops: vec!["list".into(), "resume".into()],
    };
    restart_bus(&with_binary("/bin/echo"), &s).unwrap();
    assert!(
        termd.session_exists(&session).unwrap(),
        "restart closed the bus session"
    );
    let after = termd.first_pane(&session).unwrap();
    assert_ne!(
        after.pid, before.pid,
        "restart kept the old shell; want a fresh one"
    );
    let want = "bus run --client-port 7499 --remote-ops list,resume";
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let out = termd.capture(&session, 20).unwrap_or_default();
        if out.contains(want) {
            break;
        }
        assert!(Instant::now() < deadline, "pane never ran {want:?}:\n{out}");
        std::thread::sleep(Duration::from_millis(50));
    }

    // A setting the bus can't start with is refused before the bus stops.
    let shell = termd.first_pane(&session).unwrap();
    let e = err_text(restart_bus(
        &with_binary(""),
        &BusSettings {
            client_port: 7499,
            ..Default::default()
        },
    ));
    assert!(
        e.contains("can't locate the agent binary"),
        "restart without a binary: {e}"
    );
    let now = termd.first_pane(&session).unwrap();
    assert_eq!(
        now.pid, shell.pid,
        "restart stopped the bus before finding it couldn't start one"
    );
    let _ = termd.kill_session(&session);
}

#[test]
fn client_ports_from_urls() {
    assert_eq!(url_port("tls://127.0.0.1:7499"), 7499);
    assert_eq!(url_port("tls://127.0.0.1"), 0);
    assert_eq!(url_port("tls://[::1]:7420"), 7420);
}
