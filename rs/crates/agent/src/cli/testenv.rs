//! What the CLI's unit tests share (Go: setupStore in history_test.go,
//! setupCLI, createLiveAgent, serveAs and forbidBus in remote_test.go, and
//! TestMain's isolation in main_test.go): a Ctx with a config and store of its
//! own under /var/tmp, on host "local"; with with_termd, a termd client in a
//! namespace of the test's own, in the private socket directory
//! agent_termd::testing keeps for this test binary; and with serve_as, a
//! throwaway bus (agent-busd, built from busd/ with go) serving this Ctx's
//! service as another host.
//!
//! Rule 9: nothing here touches the live system. isolate() (every setup
//! calls it first) points HOME and CLAUDE_CONFIG_DIR at a temp dir, puts
//! stand-ins for `claude` and `agy` (`exec cat`) first on PATH, sets TZ=UTC
//! (Go's TestMain), and unsets AGENT_NAME and AGENT_HOST, before any termd
//! daemon starts (it inherits that environment). So a test that launches the
//! claude backend in a test session runs `cat`, not Claude Code.
//!
//! The Go tests run one at a time, swapping package globals; these run in
//! parallel, each with a Ctx, store and termd namespace of its own, so session
//! names (rpc-cli-worker) can stay fixed.

#![allow(dead_code)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Once, OnceLock};

use agent_base::config::Config;
use agent_bus::{Conn, Daemon, RpcServer};
use agent_service::{AGENT_STATUS_WAITING, StatusReport};
use agent_store::{Agent, Store, Time};
use agent_termd::testing::{self, SharedGuard, TestClient};

use super::remote_ops::bus_handler;
use super::root::{Ctx, SharedBuf};

pub struct TestEnv {
    pub ctx: Ctx,
    pub out: SharedBuf,
    pub err: SharedBuf,
    pub cfg: Config,
    /// The test bus serve_as started, and what it serves.
    rpc: Vec<RpcServer>,
    bus: Option<TestBus>,
    /// Set when forbid_bus is on and a command dialed the bus.
    dialed_forbidden: Arc<AtomicBool>,
    // Dropped in this order: the client stops its namespace's daemon while the
    // socket dir's guard is still held.
    termd: Option<(TestClient, Guard)>,
    /// Held by an env that reads HOME or PATH (reads_env), so a test
    /// changing them (set_env, which takes the lock for writing) waits.
    env_read: Option<SharedGuard>,
    pub dir: tempfile::TempDir,
}

/// Which termd socket dir an env holds: the shared one (tests run side by
/// side), or one of its own, holding the environment lock (set_env).
enum Guard {
    Shared(#[allow(dead_code)] SharedGuard),
    Private(testing::TestServer),
}

/// A config with everything under dir, as setupCLI's: session names start
/// "rpc-cli-", host "local".
pub fn test_config(dir: &Path) -> Config {
    let p = |name: &str| dir.join(name).to_string_lossy().into_owned();
    Config {
        database_path: p("gomuxai.db"),
        log_dir: p("logs"),
        state_dir: p("state"),
        session_prefix: "rpc-cli-".to_string(),
        termd_namespace: agent_base::config::DEFAULT_TERMD_NAMESPACE.to_string(),
        default_scrollback: 100,
        host: "local".to_string(),
        ..Default::default()
    }
}

/// A temp dir under /var/tmp (never /tmp: it is RAM here).
pub fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/var/tmp")
        .expect("temp dir in /var/tmp")
}

/// The stand-in home, Claude config dir, and bin dir of this test process.
pub struct Isolation {
    pub home: PathBuf,
    pub claude_config_dir: PathBuf,
    pub bin: PathBuf,
    /// The Go toolchain's settings from before HOME moved, so building
    /// agent-busd uses the caches it always has.
    go_env: Vec<(String, String)>,
}

static ISOLATION: OnceLock<Isolation> = OnceLock::new();
static ISOLATE: Once = Once::new();
static ROOT: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn remove_root() {
    if let Some(root) = ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// GOPATH, GOMODCACHE and GOCACHE as `go env` gives them now.
fn go_env() -> Vec<(String, String)> {
    let keys = ["GOPATH", "GOMODCACHE", "GOCACHE"];
    let Ok(out) = std::process::Command::new("go")
        .arg("env")
        .args(keys)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    keys.iter()
        .zip(text.lines())
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// Isolates this test process from the live system (see the module comment).
pub fn isolate() -> &'static Isolation {
    ISOLATE.call_once(|| {
        let root = temp_dir("agent-cli-iso-").keep();
        let _ = ROOT.set(root.clone());
        // SAFETY: registering a plain extern "C" function.
        unsafe {
            libc::atexit(remove_root);
        }
        let home = root.join("home");
        let claude_config_dir = root.join("claude");
        let bin = root.join("bin");
        for d in [&home, &claude_config_dir.join("sessions"), &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        for cli in ["claude", "agy"] {
            let p = bin.join(cli);
            // A CLI that takes what is typed and does nothing with it.
            std::fs::write(&p, "#!/bin/sh\nexec cat\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Found before HOME moves (it may be found under the real home).
        let termd_bin = agent_termd::daemon_binary("").ok();
        let go_env = go_env();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        // Holds the environment lock while it changes the environment for good.
        let _lock = testing::EnvGuard::lock();
        // SAFETY: under the exclusive environment lock; tests that read the
        // environment take the shared one (testing::require) or run after this.
        unsafe {
            if let Some(b) = &termd_bin {
                std::env::set_var("G3D_TERMD_BIN", b);
            }
            std::env::set_var("HOME", &home);
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);
            std::env::set_var("PATH", path);
            std::env::set_var("TZ", "UTC");
            std::env::remove_var("AGENT_NAME");
            std::env::remove_var("AGENT_HOST");
            // The index's configuration and query log follow HOME (under
            // the temp dir), never a person's own: no store, config or log
            // of theirs steers or receives a test's commands.
            for k in [
                "XDG_CONFIG_HOME",
                "XDG_DATA_HOME",
                "AGENT_INDEX_CONFIG",
                "AGENT_INDEX_STORE",
            ] {
                std::env::remove_var(k);
            }
        }
        let _ = ISOLATION.set(Isolation {
            home,
            claude_config_dir,
            bin,
            go_env,
        });
    });
    ISOLATION.get().expect("isolated")
}

/// Builds agent-busd from busd/ once per test binary: None (skip the test)
/// when there is no go toolchain.
pub fn busd_binary() -> Option<PathBuf> {
    BUSD.get_or_init(|| {
        let iso = isolate();
        let ok = std::process::Command::new("go")
            .arg("version")
            .output()
            .is_ok_and(|o| o.status.success());
        if !ok {
            return None;
        }
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(3)
            .expect("the repo root")
            .to_path_buf();
        let build_dir = PathBuf::from("/var/tmp").join("agent-busd-test-bin");
        std::fs::create_dir_all(&build_dir).expect("busd test bin dir");
        sweep_busd(&build_dir, BUSD_PREFIX);
        let bin = build_dir.join(format!("{BUSD_PREFIX}{}", std::process::id()));
        let out = std::process::Command::new("go")
            .args(["build", "-trimpath", "-o"])
            .arg(&bin)
            .arg(".")
            .current_dir(repo.join("busd"))
            .env("TMPDIR", "/var/tmp")
            .envs(iso.go_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .expect("running go build");
        if !out.status.success() {
            panic!(
                "building agent-busd failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        // SAFETY: registering a plain extern "C" function.
        unsafe {
            libc::atexit(remove_busd);
        }
        Some(bin)
    })
    .clone()
}

static BUSD: OnceLock<Option<PathBuf>> = OnceLock::new();
const BUSD_PREFIX: &str = "agent-busd-cli-";

/// Removes this test binary's agent-busd when the process exits.
extern "C" fn remove_busd() {
    if let Some(Some(p)) = BUSD.get() {
        let _ = std::fs::remove_file(p);
    }
}

/// Removes the agent-busd binaries in dir that earlier test processes left
/// (a killed run skips its exit hook): files named prefix + a pid that no
/// longer runs. A test binary running now keeps its own.
fn sweep_busd(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(prefix))
            .and_then(|p| p.parse::<i32>().ok())
        else {
            continue;
        };
        // SAFETY: signal 0 only asks whether the process exists.
        let gone = unsafe { libc::kill(pid, 0) } == -1
            && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
        if gone {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// A single-host bus with its own identity, on a random port (Go:
/// natstest.Start).
pub struct TestBus {
    pub host: String,
    pub dir: tempfile::TempDir,
    pub daemon: Daemon,
    pub conn: Conn,
}

impl TestBus {
    /// None when agent-busd can't be built (no go).
    pub fn start() -> Option<TestBus> {
        let bin = busd_binary()?;
        let dir = temp_dir("agent-cli-bus-");
        let host = "test".to_string();
        agent_bus::init_identity(dir.path(), &host).expect("bus identity");
        let daemon = agent_bus::start_daemon(agent_bus::DaemonConfig {
            dir: dir.path().to_path_buf(),
            host: host.clone(),
            client_port: -1,
            leaf_listen: String::new(),
            remote_ops: None,
            busd_path: Some(bin),
        })
        .expect("starting the test bus");
        let conn = agent_bus::connect_local(dir.path(), &host).expect("connecting to the test bus");
        Some(TestBus {
            host,
            dir,
            daemon,
            conn,
        })
    }

    /// Another client connection (Go: natstest.Connect).
    pub fn connect(&self) -> Conn {
        agent_bus::connect_local(self.dir.path(), &self.host).expect("connecting to the test bus")
    }
}

/// Skips (returns from) a test that needs g3d-termd when there is none, as
/// Go's termdtest.Require skips.
#[macro_export]
macro_rules! require_termd {
    () => {
        if !$crate::cli::testenv::termd_available() {
            eprintln!("skipping: g3d-termd not available");
            return;
        }
    };
}

/// Whether a g3d-termd binary is here to run the termd tests.
pub fn termd_available() -> bool {
    isolate();
    testing::available()
}

static NS: AtomicUsize = AtomicUsize::new(0);

impl TestEnv {
    /// A config and an open store, and no termd (Go: setupStore).
    pub fn new() -> TestEnv {
        let dir = temp_dir("agent-cli-");
        let cfg = test_config(dir.path());
        let store = Store::open(&cfg.database_path).expect("opening the test store");
        let (out, err) = (SharedBuf::default(), SharedBuf::default());
        let mut ctx = Ctx::new(out.writer(), err.writer());
        ctx.base_cfg = Some(cfg.clone());
        ctx.set_cfg(cfg.clone());
        ctx.set_store(Arc::new(store));
        TestEnv {
            ctx,
            out,
            err,
            cfg,
            rpc: Vec::new(),
            bus: None,
            dialed_forbidden: Arc::new(AtomicBool::new(false)),
            termd: None,
            env_read: None,
            dir,
        }
    }

    /// new, holding the environment lock for reading until it drops: for a
    /// test whose commands read HOME or PATH (where CLIs are, plan limits),
    /// which a with_termd_private test's set_env would otherwise change
    /// under it.
    pub fn reads_env() -> TestEnv {
        let mut env = TestEnv::new();
        env.env_read = Some(testing::init());
        env
    }

    /// new, isolated (isolate()), with a termd client for a namespace of this
    /// test's own (Go: setupCLI's termdtest.Require and termd.NewClient), the
    /// built-in backends, and the service over them (ctx.service()).
    pub fn with_termd() -> TestEnv {
        isolate();
        let guard = testing::require();
        TestEnv::termd_env(Guard::Shared(guard))
    }

    /// with_termd, alone in a termd socket dir of its own and holding the
    /// environment lock, for a test that sets environment variables (Go:
    /// t.Setenv): env.set_env, restored when the env drops. It runs while no
    /// with_termd test does.
    pub fn with_termd_private() -> TestEnv {
        isolate();
        let server = testing::setup();
        agent_termd::check_test_server().expect("a private termd dir");
        TestEnv::termd_env(Guard::Private(server))
    }

    /// setup_cli on with_termd_private (Go: setupCLI in a test that calls
    /// t.Setenv).
    pub fn setup_cli_private() -> TestEnv {
        let env = TestEnv::with_termd_private();
        env.add_worker();
        env
    }

    /// Sets an environment variable until the env drops (Go: t.Setenv; ""
    /// unsets it). Only in a with_termd_private env.
    pub fn set_env(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        match &mut self.termd {
            Some((_, Guard::Private(s))) => s.env().set(key, value),
            _ => panic!("set_env needs TestEnv::with_termd_private"),
        }
    }

    fn termd_env(guard: Guard) -> TestEnv {
        let ns = format!(
            "cli-{}-{}",
            std::process::id(),
            NS.fetch_add(1, Ordering::Relaxed)
        );
        let mut env = TestEnv::new();
        env.cfg.termd_namespace = ns.clone();
        cfg_dirs(&env.cfg);
        let client = agent_termd::Client::new(agent_termd::Options {
            namespace: ns.clone(),
            ..Default::default()
        })
        .expect("a termd client in the test socket dir");
        env.ctx = {
            let mut ctx = Ctx::new(env.out.writer(), env.err.writer());
            ctx.base_cfg = Some(env.cfg.clone());
            ctx.set_cfg(env.cfg.clone());
            ctx.set_store(Arc::clone(env.ctx.store().expect("the test store")));
            ctx.set_termd(Arc::new(client));
            ctx
        };
        env.termd = Some((testing::new_test_client(&ns), guard));
        env
    }

    /// Go: setupCLI: with_termd, and one agent, "worker" (session
    /// rpc-cli-worker, backend claude, created 2026-10-01 09:30 UTC), live in
    /// a bare shell and waiting on a Bash permission prompt.
    pub fn setup_cli() -> TestEnv {
        let env = TestEnv::with_termd();
        env.add_worker();
        env
    }

    fn add_worker(&self) {
        let env = self;
        env.create_live_agent(&Agent {
            name: "worker".into(),
            session_name: "rpc-cli-worker".into(),
            backend: "claude".into(),
            created_at: Time::date_utc(2026, 10, 1, 9, 30, 0, 0),
            ..Default::default()
        });
        env.service()
            .report(
                "worker",
                StatusReport {
                    status: AGENT_STATUS_WAITING,
                    detail: "permission: Bash: ls".into(),
                    tool: "Bash".into(),
                    ..Default::default()
                },
            )
            .expect("reporting worker's prompt");
    }

    /// Records agent with a termd session of its own (a bare shell): status
    /// calls an agent with no session ended (gone), whatever it last
    /// reported (Go: createLiveAgent). The session goes with the namespace.
    pub fn create_live_agent(&self, agent: &Agent) {
        self.termd_ops()
            .create_session(&agent.session_name, &HashMap::new())
            .expect("creating the agent's session");
        self.store()
            .create_agent(agent)
            .expect("recording the agent");
    }

    pub fn store(&self) -> &Store {
        self.ctx.store().expect("the test store")
    }

    pub fn service(&self) -> &Arc<agent_service::AgentService> {
        self.ctx.service().expect("the test service")
    }

    pub fn termd_ops(&self) -> &Arc<dyn agent_termd::Ops> {
        self.ctx.termd().expect("the test termd client")
    }

    /// Runs a command line (the words after "agent") on this env's Ctx, as
    /// the CLI would (setup included: change the config through
    /// ctx.base_cfg), and returns its stdout and result (Go: stdout(t, func()
    /// { return runX(xCmd, args) })). It writes no command-log row.
    pub fn run(&mut self, words: &[&str]) -> (String, anyhow::Result<()>) {
        self.out.clear();
        let res = super::root::run_words(&mut self.ctx, words);
        (self.take_out(), res)
    }

    /// The command's stdout so far, emptied.
    pub fn take_out(&self) -> String {
        let s = self.out.contents();
        self.out.clear();
        s
    }

    /// The command's stderr (hints, notices) so far, emptied (Go: hints).
    pub fn take_err(&self) -> String {
        let s = self.err.contents();
        self.err.clear();
        s
    }

    /// Makes the bus look absent: bus_connect returns ErrNoBus (Go: busConnect
    /// = func() { return nil, natsbus.ErrNoBus }).
    pub fn no_bus(&self) {
        self.ctx.set_bus_connect(|| Err(agent_bus::ErrNoBus.into()));
    }

    /// Fails the test (when the env drops) if a command dials the bus (Go:
    /// forbidBus).
    pub fn forbid_bus(&self) {
        let dialed = Arc::clone(&self.dialed_forbidden);
        self.ctx.set_bus_connect(move || {
            dialed.store(true, Ordering::Relaxed);
            Err(agent_bus::ErrNoBus.into())
        });
    }

    /// Whether a command dialed the bus since forbid_bus.
    pub fn dialed_forbidden(&self) -> bool {
        self.dialed_forbidden.load(Ordering::Relaxed)
    }

    /// Serves the CLI's own service as host "h1" (every op), and makes
    /// bus_connect reach it, so "worker@h1" is the same agent as local
    /// "worker". It returns a counter of bus connections, or None (skip the
    /// test) when agent-busd can't be built (Go: serveAsH1).
    pub fn serve_as_h1(&mut self) -> Option<Arc<AtomicUsize>> {
        let mut ops = vec![agent_bus::OP_KEYS];
        ops.extend_from_slice(agent_bus::DEFAULT_REMOTE_OPS);
        self.serve_as("h1", &ops)
    }

    /// serve_as_h1 serving only ops (Go: serveAsH1With).
    pub fn serve_as_h1_with(&mut self, ops: &[&str]) -> Option<Arc<AtomicUsize>> {
        self.serve_as("h1", ops)
    }

    /// Serves ops as host over a test bus with this Ctx's service (and the
    /// CLI's logs and resume: bus_handler), and points the CLI at that bus
    /// (Go: serveAs).
    pub fn serve_as(&mut self, host: &str, ops: &[&str]) -> Option<Arc<AtomicUsize>> {
        let handler = Arc::new(bus_handler(&self.ctx).expect("the bus handler"));
        self.serve_handler(host, handler, ops)
    }

    /// A service over this env's store, termd and backends, with this env's
    /// config changed by f (Go tests changing cfg after setupCLI, say
    /// cfg.AgentBinary).
    pub fn service_with(&self, f: impl FnOnce(&mut Config)) -> Arc<agent_service::AgentService> {
        let mut cfg = self.ctx.cfg().clone();
        f(&mut cfg);
        Arc::new(agent_service::AgentService::new(
            cfg,
            Arc::clone(self.ctx.store().expect("the test store")),
            Arc::clone(self.termd_ops()),
            Arc::clone(self.ctx.backends()),
        ))
    }

    /// Serves svc's ops as host, in place of whatever this env served
    /// before (Go: serveAs(t, host, svc, ops)).
    pub fn serve_svc_as(
        &mut self,
        host: &str,
        svc: Arc<agent_service::AgentService>,
        ops: &[&str],
    ) -> Option<Arc<AtomicUsize>> {
        self.rpc.clear();
        let handler =
            Arc::new(super::remote_ops::bus_handler_with(&self.ctx, svc).expect("the bus handler"));
        self.serve_handler(host, handler, ops)
    }

    /// Records execs instead of making them (exec_into), finds every program
    /// named without a path at /usr/bin/<name>, and says stdin is a terminal
    /// (Go: stubExec). It returns the calls.
    pub fn stub_exec(&mut self) -> Arc<std::sync::Mutex<Vec<super::exec::ExecCall>>> {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        self.ctx.exec_calls = Some(Arc::clone(&calls));
        self.ctx.attach_terminal = Some(true);
        calls
    }

    /// serve_as with a handler of the test's own.
    pub fn serve_handler(
        &mut self,
        host: &str,
        handler: Arc<dyn agent_bus::Handler>,
        ops: &[&str],
    ) -> Option<Arc<AtomicUsize>> {
        if self.bus.is_none() {
            self.bus = Some(TestBus::start()?);
        }
        let bus = self.bus.as_ref().expect("started above");
        let server = agent_bus::serve_rpc(&bus.conn, host, handler, ops).expect("ServeRPC");
        self.rpc.push(server);
        let dials = Arc::new(AtomicUsize::new(0));
        let (d, dir, bus_host) = (
            Arc::clone(&dials),
            bus.dir.path().to_path_buf(),
            bus.host.clone(),
        );
        self.ctx.set_bus_connect(move || {
            d.fetch_add(1, Ordering::Relaxed);
            agent_bus::connect_local(&dir, &bus_host)
        });
        Some(dials)
    }

    /// The test bus serve_as started, if it did.
    pub fn bus(&self) -> Option<&TestBus> {
        self.bus.as_ref()
    }
}

/// A command's Args, parsed from the words after its path as the command
/// line would ("spawn", &["x@hub", "--resume", "3f"]): for a test that calls
/// a run fn directly, as the Go tests set flag variables.
pub fn parse_args<A: clap::FromArgMatches>(path: &str, words: &[&str]) -> A {
    let cmd = super::root::command(path);
    let rest: Vec<String> = words.iter().map(|s| s.to_string()).collect();
    let m = super::root::parse_flags(&cmd, &rest).expect("the test's flags parse");
    A::from_arg_matches(&m).expect("the test's flags make Args")
}

impl TestEnv {
    /// Changes the config the commands see (Go: cfg.X = ... after
    /// setupCLI); the service keeps its own.
    pub fn change_cfg(&mut self, f: impl FnOnce(&mut Config)) {
        let mut cfg = self.ctx.cfg().clone();
        f(&mut cfg);
        self.cfg = cfg.clone();
        self.ctx.base_cfg = Some(cfg.clone());
        self.ctx.replace_cfg(cfg);
    }
}

/// Creates the config's directories (Go: setup's EnsureDirs).
fn cfg_dirs(cfg: &Config) {
    cfg.ensure_dirs().expect("creating the test dirs");
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        if self.dialed_forbidden() && !std::thread::panicking() {
            panic!("local command dialed the bus");
        }
    }
}

/// Skips (returns from) a test when no test bus can be had (no go to build
/// agent-busd): `let dials = require_bus!(env.serve_as_h1());`.
#[macro_export]
macro_rules! require_bus {
    ($e:expr) => {
        match $e {
            Some(v) => v,
            None => {
                eprintln!("skipping: no agent-busd (go isn't installed)");
                return;
            }
        }
    };
}
