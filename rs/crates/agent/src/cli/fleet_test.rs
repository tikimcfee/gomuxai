//! fleet's cases, and the two-host setup focus and template tests share: a
//! hub (a TestEnv) and a leaf (a Ctx of its own: its own store, config and
//! termd namespace, in the hub's private termd dir) on one throwaway bus,
//! each serving its own bus handler under its host name.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_base::config::Config;
use agent_bus::{FleetView, RpcServer};
use agent_service::{AGENT_STATUS_IDLE, AGENT_STATUS_WAITING, StatusReport};
use agent_store::{Agent, StateEntry, Store, Time};

use crate::cli::remote_ops::bus_handler;
use crate::cli::remote_test::trust_peers;
use crate::cli::root::{Ctx, SharedBuf, run_words};
use crate::cli::testenv::{TestEnv, temp_dir, test_config};

static LEAVES: AtomicUsize = AtomicUsize::new(0);

/// A second host for a test: its own store, config (host, session prefix
/// "rpc-<host>-") and termd namespace, in the private termd dir the hub's
/// TestEnv holds (so it must drop first).
pub(crate) struct Leaf {
    pub ctx: Ctx,
    pub out: SharedBuf,
    pub err: SharedBuf,
    pub cfg: Config,
    _termd: agent_termd::testing::TestClient,
    _dir: tempfile::TempDir,
}

impl Leaf {
    pub fn new(host: &str) -> Leaf {
        let dir = temp_dir("agent-cli-leaf-");
        let mut cfg = test_config(dir.path());
        cfg.host = host.to_string();
        cfg.session_prefix = format!("rpc-{host}-");
        let ns = format!(
            "cli-{host}-{}-{}",
            std::process::id(),
            LEAVES.fetch_add(1, Ordering::Relaxed)
        );
        cfg.termd_namespace = ns.clone();
        cfg.ensure_dirs().unwrap();
        let store = Store::open(&cfg.database_path).unwrap();
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let mut ctx = Ctx::new(out.writer(), err.writer());
        ctx.base_cfg = Some(cfg.clone());
        ctx.set_cfg(cfg.clone());
        ctx.set_store(Arc::new(store));
        ctx.set_termd(Arc::new(
            agent_termd::Client::new(agent_termd::Options {
                namespace: ns.clone(),
                ..Default::default()
            })
            .unwrap(),
        ));
        // A person at no pane, whatever runs the test.
        ctx.set_caller_chain(|| Ok(Vec::new()));
        Leaf {
            ctx,
            out,
            err,
            cfg,
            _termd: agent_termd::testing::new_test_client(&ns),
            _dir: dir,
        }
    }

    /// Runs a command line on the leaf: its stdout, its stderr, its result.
    pub fn run(&mut self, words: &[&str]) -> (String, String, anyhow::Result<()>) {
        self.out.clear();
        self.err.clear();
        self.ctx.hinted.store(false, Ordering::Relaxed);
        let res = run_words(&mut self.ctx, words);
        (self.out.contents(), self.err.contents(), res)
    }

    /// Records agent with a termd session of its own (a bare shell).
    pub fn create_live_agent(&self, agent: &Agent) {
        self.ctx
            .termd()
            .unwrap()
            .create_session(&agent.session_name, &Default::default())
            .unwrap();
        self.ctx.store().unwrap().create_agent(agent).unwrap();
    }
}

/// A hub (TestEnv, host "hub") and a leaf (host "leaf") on one test bus,
/// trusting each other; the hub also trusts "old" (the leaf's handler
/// serving only list, so not fleet) and "gone" (nothing serves
/// it). Fields drop in order: the leaf before the hub's termd guard.
pub(crate) struct TwoHosts {
    _servers: Vec<RpcServer>,
    pub leaf: Leaf,
    pub hub: TestEnv,
}

impl TwoHosts {
    /// None (skip the test) without agent-busd. private: the hub holds the
    /// environment lock (TestEnv::with_termd_private), for set_env.
    pub fn new(private: bool) -> Option<TwoHosts> {
        let mut hub = if private {
            TestEnv::with_termd_private()
        } else {
            TestEnv::with_termd()
        };
        hub.change_cfg(|c| c.host = "hub".into());
        hub.ctx.set_caller_chain(|| Ok(Vec::new()));
        hub.serve_as("hub", agent_bus::DEFAULT_REMOTE_OPS)?;
        let leaf = Leaf::new("leaf");
        let bus = hub.bus().expect("served above");
        let handler = Arc::new(bus_handler(&leaf.ctx).unwrap());
        let servers = vec![
            agent_bus::serve_rpc(&bus.conn, "leaf", handler.clone(), agent_bus::DEFAULT_REMOTE_OPS)
                .unwrap(),
            agent_bus::serve_rpc(&bus.conn, "old", handler, &[agent_bus::OP_LIST]).unwrap(),
        ];
        let (dir, bus_host) = (bus.dir.path().to_path_buf(), bus.host.clone());
        leaf.ctx
            .set_bus_connect(move || agent_bus::connect_local(&dir, &bus_host));
        trust_peers(&hub, &["leaf", "old", "gone"]);
        let peer = temp_dir("agent-cli-peer-");
        let id = agent_bus::init_identity(peer.path(), "hub").unwrap();
        agent_bus::trust_peer(leaf.cfg.bus_dir(), id.public_bundle("")).unwrap();
        Some(TwoHosts {
            _servers: servers,
            leaf,
            hub,
        })
    }
}

fn report(env: &TestEnv, name: &str, status: agent_service::AgentStatus, detail: &str) {
    env.service()
        .report(
            name,
            StatusReport {
                status,
                detail: detail.into(),
                ..Default::default()
            },
        )
        .unwrap();
}

fn live(name: &str, prefix: &str, parent: &str) -> Agent {
    Agent {
        name: name.into(),
        session_name: format!("{prefix}{name}"),
        backend: "claude".into(),
        spawned_by: parent.into(),
        created_at: Time::now(),
        ..Default::default()
    }
}

/// The hub's agents for the fleet cases: lead (idle, its Claude session
/// named "builder", not "lead"), its helper lead-tests (waiting on a
/// prompt), and gone-one (recorded, its session gone); lead has focus.
fn hub_agents(h: &mut TwoHosts) {
    let hub = &mut h.hub;
    hub.create_live_agent(&live("lead", "rpc-cli-", ""));
    hub.create_live_agent(&live("lead-tests", "rpc-cli-", "lead"));
    hub.store().create_agent(&live("gone-one", "rpc-cli-", "")).unwrap();
    report(hub, "lead", AGENT_STATUS_IDLE, "");
    report(hub, "lead-tests", AGENT_STATUS_WAITING, "permission: Bash: go test ./...");

    // lead's transcript says its session is named builder.
    let tr = hub.dir.path().join("lead.jsonl");
    std::fs::write(
        &tr,
        "{\"type\":\"user\",\"sessionId\":\"sess-lead\",\"cwd\":\"/srv/p\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n{\"type\":\"custom-title\",\"customTitle\":\"builder\",\"sessionId\":\"sess-lead\"}\n",
    )
    .unwrap();
    hub.store()
        .set_state(&StateEntry {
            agent_name: "lead".into(),
            key: agent_service::STATE_KEY_TRANSCRIPT.into(),
            value: tr.to_string_lossy().into_owned(),
            updated_at: Time::now(),
        })
        .unwrap();
    let (_, res) = hub.run(&["focus", "lead"]);
    res.unwrap();
}

#[test]
fn test_fleet_across_hosts() {
    crate::require_termd!();
    let Some(mut h) = TwoHosts::new(false) else {
        eprintln!("skipping: no agent-busd");
        return;
    };
    hub_agents(&mut h);
    h.leaf.create_live_agent(&live("w", "rpc-leaf-", ""));

    let (out, res) = h.hub.run(&["fleet"]);
    res.unwrap();
    eprintln!("agent fleet (on hub):\n{out}");
    let build = agent_base::buildinfo::id();
    for want in [
        format!("hub (build {build})\n  focus: lead (rpc-cli-lead) by user\n"),
        "  lead: idle (claude)\n    flags:  name mismatch: session is \"builder\"\n    └ lead-tests: waiting (claude)\n        detail: permission: Bash: go test ./...\n".to_string(),
        "  gone-one: ended (gone) (claude)\n    detail: its session rpc-cli-gone-one is gone".to_string(),
        "    flags:  ended (gone), still recorded\n".to_string(),
        format!("leaf (build {build})\n  w: "),
        "old: unreachable (".to_string(),
        "gone: unreachable (".to_string(),
    ] {
        assert!(out.contains(&want), "want {want:?} in:\n{out}");
    }
    assert!(out.starts_with("hub ("), "this host first:\n{out}");
    assert_eq!(h.hub.take_err(), "", "a hint with a bus running");

    // --json: one document, every host.
    let (out, res) = h.hub.run(&["fleet", "--json"]);
    res.unwrap();
    let view: FleetView = serde_json::from_str(&out).unwrap();
    let mut hosts: Vec<(&str, bool)> =
        view.hosts.iter().map(|x| (x.host.as_str(), x.reachable)).collect();
    hosts.sort();
    assert_eq!(
        hosts,
        [("gone", false), ("hub", true), ("leaf", true), ("old", false)]
    );
    let hub = &view.hosts[0];
    assert_eq!(hub.focus.as_ref().map(|f| f.agent.as_str()), Some("lead"));
    let lead = hub.agents.iter().find(|a| a.name == "lead").expect("lead in the hub's tree");
    assert_eq!(lead.session_name, "builder");
    assert_eq!(lead.children[0].name, "lead-tests");

    // --report: the baseline per host.
    let (out, res) = h.hub.run(&["fleet", "--report"]);
    res.unwrap();
    eprintln!("agent fleet --report (on hub):\n{out}");
    assert!(
        out.contains("hub (build ")
            && out.contains("  agents by status: ended: 1, idle: 1, waiting: 1\n")
            && out.contains("  agents by backend: claude: 3\n")
            && out.contains("old: unreachable ("),
        "{out}"
    );

    // --host: one host, through the bus.
    let (out, res) = h.hub.run(&["fleet", "--host", "LEAF"]);
    res.unwrap();
    assert!(out.starts_with("leaf (build ") && !out.contains("hub"), "{out}");
    // One whose bus doesn't serve fleet is an error too: no falling back to
    // its list.
    let (out, res) = h.hub.run(&["fleet", "--host", "old"]);
    assert!(res.is_err() && out.is_empty(), "{out}");
    // review-4 D1: one that answers nothing, or a host nobody knows, is an
    // error naming the hosts this one knows, as for agent status name@host.
    for host in ["gone", "typo"] {
        let (out, res) = h.hub.run(&["fleet", "--host", host]);
        let err = format!("{:#}", res.expect_err(host));
        assert!(
            err.contains(&format!("no bus answering for host \"{host}\""))
                && err.ends_with("; this host is hub, and its bus trusts gone, leaf, old"),
            "{host}: {err}"
        );
        assert_eq!(out, "", "{host}");
    }

    // From the leaf: it and the hub it trusts, the hub's part from its fleet
    // op, its session name read back as here (review-4 C1).
    let (out, _, res) = h.leaf.run(&["fleet"]);
    res.unwrap();
    eprintln!("agent fleet (on leaf):\n{out}");
    assert!(
        out.starts_with("leaf (build ")
            && out.contains("\nhub (build ")
            && out.contains("  focus: lead")
            && out.contains("    flags:  name mismatch: session is \"builder\"\n"),
        "{out}"
    );
}

// No bus: this host only, and a hint saying so.
#[test]
fn test_fleet_without_bus() {
    crate::require_termd!();
    let mut env = TestEnv::setup_cli();
    env.no_bus();
    trust_peers(&env, &["hub"]);
    let (out, res) = env.run(&["fleet"]);
    res.unwrap();
    assert!(out.starts_with("local (build ") && out.contains("  worker: waiting (claude)\n"), "{out}");
    assert!(!out.contains("hub"), "{out}");
    assert_eq!(
        env.take_err(),
        "hint: no agent bus runs here, so this is this host only, without hub (agent bus start)\n"
    );
    let (_, res) = env.run(&["fleet", "--host", "hub"]);
    let err = format!("{:#}", res.unwrap_err());
    assert!(err.contains("needs a running agent bus"), "{err}");
    let (_, res) = env.run(&["fleet", "--host", "a b"]);
    assert!(res.is_err());
}
