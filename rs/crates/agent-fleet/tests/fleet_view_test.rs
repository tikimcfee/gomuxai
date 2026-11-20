//! Integration and unit tests for `agent_fleet::{fleet, focus, template_op}`
//! and `agent_service::RpcExtras::{fleet, template}`.
//!
//! Hermeticity (Rule 9):
//! - All temp files, SQLite databases, git repos, transcripts, and bus dirs
//!   live under `/var/tmp` (never `/tmp` or `~/.local/bin/.gomuxai`).
//! - `HOME` and `CLAUDE_CONFIG_DIR` point to an isolated temp directory with
//!   harmless `claude` and `agy` stand-ins (`exec cat`) prepended to `PATH`.
//! - Terminal multiplexer operations run against an in-memory `FakeMux`
//!   (`agent_termd::Ops`), never touching any `g3d-termd` daemon or socket.
//! - Cross-host tests run throwaway `agent-busd` processes on unique ephemeral
//!   `127.0.0.1` ports, and every test runs under a hard 20s watchdog deadline.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::{Duration, Instant, UNIX_EPOCH};

use agent_base::config::Config;
use agent_bus::{
    ALL_OPS, AgentInfo, Conn, Context, DEFAULT_RPC_TIMEOUT, Daemon, DaemonConfig, FleetAgent,
    FleetHost, FleetRequest, FleetView, FocusEvent, Handler, ListResponse, OP_LIST, Publisher,
    ResumeRequest, ResumeResponse, RpcClient, RpcServer, TEMPLATE_LIST, TEMPLATE_RESUME,
    TEMPLATE_START, TemplateRequest, Time, connect_local, init_identity, serve_rpc, start_daemon,
    trust_peer,
};
use agent_fleet::fleet::{
    self, HELD_STUCK_AFTER, HELD_STUCK_COUNT, Options as FleetOptions, WAITING_STUCK_AFTER,
};
use agent_fleet::templates::{self, Template};
use agent_fleet::{focus, handoff, run, template_op};
use agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_IDLE, AGENT_STATUS_WAITING, AgentNotFoundError, AgentService,
    RpcExtras, STATE_KEY_MODEL, STATE_KEY_SESSION_ID, new_rpc_handler, new_rpc_handler_with,
};
use agent_store::{
    AGENT_STATUS, Agent, FRICTION_ACCEPTED, FRICTION_OPEN, FRICTION_RESOLVED, Friction, Job,
    MESSAGE_STATUS_DELIVERED, MESSAGE_STATUS_PENDING, Message, StateEntry, Store,
    Time as StoreTime,
};
use agent_termd::{Ops, PaneInfo, Session as MuxSession};
use anyhow::bail;
use chrono::{TimeDelta, TimeZone, Utc};

const HARD_TEST_DEADLINE: Duration = Duration::from_secs(20);

/// Runs `f` with a hard watchdog deadline so a broken wait/subscription fails
/// loudly and exits rather than hanging `cargo test`.
fn with_hard_deadline<F: FnOnce()>(name: &'static str, timeout: Duration, f: F) {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        if done_rx.recv_timeout(timeout).is_err() {
            eprintln!(
                "\nFATAL: fleet_view_test::{name} exceeded hard deadline ({timeout:?}); aborting"
            );
            std::process::exit(101);
        }
    });
    struct FinishGuard(std::sync::mpsc::Sender<()>);
    impl Drop for FinishGuard {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }
    let _guard = FinishGuard(done_tx);
    f();
}

struct ProcessIsolation {
    busd_bin: PathBuf,
}

static ISOLATION: OnceLock<ProcessIsolation> = OnceLock::new();
static INIT: Once = Once::new();
static ISO_ROOT: OnceLock<PathBuf> = OnceLock::new();
static ENV_SEQ: AtomicU64 = AtomicU64::new(0);

extern "C" fn cleanup_iso_root() {
    if let Some(root) = ISO_ROOT.get() {
        let _ = fs::remove_dir_all(root);
    }
}

fn temp_dir(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in("/var/tmp")
        .expect("tempdir in /var/tmp")
}

fn capture_go_env() -> Vec<(String, String)> {
    let keys = ["GOPATH", "GOMODCACHE", "GOCACHE"];
    let Ok(out) = Command::new("go").arg("env").args(keys).output() else {
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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("gomuxai repo root")
        .to_path_buf()
}

fn init_process() -> &'static ProcessIsolation {
    INIT.call_once(|| {
        let root = temp_dir("agent-fleet-view-iso-").keep();
        let _ = ISO_ROOT.set(root.clone());
        unsafe {
            libc::atexit(cleanup_iso_root);
        }

        let home = root.join("home");
        let claude_dir = root.join("claude");
        let bin = root.join("bin");
        for d in [&home, &claude_dir, &bin] {
            fs::create_dir_all(d).unwrap();
        }
        for cli in ["claude", "agy"] {
            let p = bin.join(cli);
            fs::write(&p, "#!/bin/sh\nexec cat\n").unwrap();
            fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let go_env = capture_go_env();
        let busd_bin = bin.join("agent-busd");
        let mut build = Command::new("go");
        build
            .current_dir(repo_root().join("busd"))
            .args(["build", "-o"])
            .arg(&busd_bin)
            .arg(".");
        for (k, v) in &go_env {
            build.env(k, v);
        }
        let status = build.status().expect("running `go build` for busd");
        assert!(status.success(), "`go build ./busd` failed");

        let new_path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_dir);
            std::env::set_var("PATH", new_path);
            std::env::remove_var("AGENT_NAME");
            std::env::remove_var("AGENT_HOST");
        }

        let _ = ISOLATION.set(ProcessIsolation { busd_bin });
    });
    ISOLATION.get().expect("process initialized")
}

/// Thread-safe in-memory terminal multiplexer fake implementing [`Ops`].
struct FakeMux {
    sessions: Mutex<HashMap<String, FakeSession>>,
    next_pid: AtomicI32,
}

#[derive(Clone)]
struct FakeSession {
    pid: i32,
    cwd: String,
    env: HashMap<String, String>,
    screen: String,
}

impl FakeMux {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            next_pid: AtomicI32::new(50_000),
        }
    }

    fn insert_live(&self, name: &str, cwd: &str) {
        let _ = self.create_session_in(name, cwd, &HashMap::new());
    }
}

impl Ops for FakeMux {
    fn create_session(&self, name: &str, env: &HashMap<String, String>) -> anyhow::Result<()> {
        self.create_session_in(name, "", env)
    }

    fn create_session_in(
        &self,
        name: &str,
        dir: &str,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let mut guard = self.sessions.lock().unwrap();
        if guard.contains_key(name) {
            bail!("session {name:?} already exists");
        }
        let pid = self.next_pid.fetch_add(1, Ordering::SeqCst);
        guard.insert(
            name.to_string(),
            FakeSession {
                pid,
                cwd: dir.to_string(),
                env: env.clone(),
                screen: String::new(),
            },
        );
        Ok(())
    }

    fn kill_session(&self, name: &str) -> anyhow::Result<()> {
        let mut guard = self.sessions.lock().unwrap();
        if guard.remove(name).is_none() {
            bail!("session {name:?} not found");
        }
        Ok(())
    }

    fn respawn_pane(&self, session: &str) -> anyhow::Result<()> {
        let mut guard = self.sessions.lock().unwrap();
        let Some(s) = guard.get_mut(session) else {
            bail!("session {session:?} not found");
        };
        s.screen.clear();
        Ok(())
    }

    fn session_exists(&self, name: &str) -> anyhow::Result<bool> {
        Ok(self.sessions.lock().unwrap().contains_key(name))
    }

    fn list_sessions(&self) -> anyhow::Result<Vec<MuxSession>> {
        let guard = self.sessions.lock().unwrap();
        Ok(guard
            .keys()
            .map(|name| MuxSession {
                name: name.clone(),
                created: "0".into(),
                attached: false,
                session_id: name.clone(),
            })
            .collect())
    }

    fn send_keys(&self, session: &str, text: &str) -> anyhow::Result<()> {
        let mut guard = self.sessions.lock().unwrap();
        let Some(s) = guard.get_mut(session) else {
            bail!("session {session:?} not found");
        };
        s.screen.push_str(text);
        Ok(())
    }

    fn paste_text(&self, session: &str, text: &str) -> anyhow::Result<()> {
        self.send_keys(session, text)
    }

    fn send_special_key(&self, session: &str, key: &str) -> anyhow::Result<()> {
        let mut guard = self.sessions.lock().unwrap();
        let Some(s) = guard.get_mut(session) else {
            bail!("session {session:?} not found");
        };
        if key.eq_ignore_ascii_case("enter") {
            s.screen.push('\n');
        }
        Ok(())
    }

    fn capture(&self, session: &str, _lines: i32) -> anyhow::Result<String> {
        let guard = self.sessions.lock().unwrap();
        let Some(s) = guard.get(session) else {
            bail!("session {session:?} not found");
        };
        Ok(s.screen.clone())
    }

    fn capture_styled(&self, session: &str, lines: i32) -> anyhow::Result<String> {
        self.capture(session, lines)
    }

    fn pane_pids(&self) -> anyhow::Result<HashMap<i32, String>> {
        let guard = self.sessions.lock().unwrap();
        Ok(guard.iter().map(|(k, v)| (v.pid, k.clone())).collect())
    }

    fn session_env(&self, session: &str, name: &str) -> String {
        self.sessions
            .lock()
            .unwrap()
            .get(session)
            .and_then(|s| s.env.get(name))
            .cloned()
            .unwrap_or_default()
    }

    fn first_pane(&self, session: &str) -> anyhow::Result<PaneInfo> {
        let guard = self.sessions.lock().unwrap();
        let Some(s) = guard.get(session) else {
            bail!("session {session:?} not found");
        };
        Ok(PaneInfo {
            id: session.to_string(),
            pid: s.pid,
            cwd: s.cwd.clone(),
        })
    }
}

struct TestHost {
    dir: tempfile::TempDir,
    data_dir: PathBuf,
    store: Arc<Store>,
    mux: Arc<FakeMux>,
    service: Arc<AgentService>,
    cfg: Config,
}

impl TestHost {
    fn new(host: &str) -> Self {
        init_process();
        let dir = temp_dir(&format!("agent-fleet-host-{host}-"));
        let data_dir = dir.path().join(".gomuxai");
        fs::create_dir_all(&data_dir).unwrap();
        let claude_projects = dir.path().join("claude-projects");
        let agy_brain = dir.path().join("agy-brain");
        let kimi_sessions = dir.path().join("kimi-sessions");
        for d in [&claude_projects, &agy_brain, &kimi_sessions] {
            fs::create_dir_all(d).unwrap();
        }

        let db_path = data_dir.join("agents.db");
        let seq = ENV_SEQ.fetch_add(1, Ordering::SeqCst) + 1;
        let cfg = Config {
            host: host.to_string(),
            backend: "claude".into(),
            database_path: db_path.to_string_lossy().into_owned(),
            log_dir: data_dir.join("logs").to_string_lossy().into_owned(),
            state_dir: data_dir.join("state").to_string_lossy().into_owned(),
            claude_projects_dir: Some(claude_projects.to_string_lossy().into_owned()),
            agy_brain_dir: Some(agy_brain.to_string_lossy().into_owned()),
            kimi_sessions_dir: Some(kimi_sessions.to_string_lossy().into_owned()),
            session_prefix: format!("fv-{}-{seq}-", std::process::id()),
            default_scrollback: 100,
            startup_delay: Duration::ZERO,
            ..Default::default()
        };
        cfg.ensure_dirs().unwrap();

        let store = Arc::new(Store::open(&db_path).unwrap());
        let mux = Arc::new(FakeMux::new());
        let backends = Arc::new(agent_backend::new_registry());
        let mut svc = AgentService::new(
            cfg.clone(),
            Arc::clone(&store),
            Arc::clone(&mux) as Arc<dyn Ops>,
            backends,
        );
        svc.set_deliverer_starter(|_| Ok(()));
        Self {
            dir,
            data_dir,
            store,
            mux,
            service: Arc::new(svc),
            cfg,
        }
    }

    fn bus_dir(&self) -> PathBuf {
        PathBuf::from(self.cfg.bus_dir())
    }
}

fn free_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct RunningBus {
    daemon: Daemon,
    conn: Conn,
    _rpc: Option<RpcServer>,
}

impl Drop for RunningBus {
    fn drop(&mut self) {
        self._rpc.take();
        self.conn.close();
        self.daemon.shutdown();
    }
}

fn start_bus_daemon(
    bus_dir: &Path,
    host: &str,
    leaf_port: u16,
    handler: Option<(Arc<dyn Handler>, Vec<&'static str>)>,
) -> RunningBus {
    let iso = init_process();
    let leaf_listen = if leaf_port > 0 {
        format!("127.0.0.1:{leaf_port}")
    } else {
        String::new()
    };
    // Note: in nats-server, `client_port == 0` binds default port 4222, while
    // `-1` (`nats.RANDOM_PORT`) lets the OS pick a race-free ephemeral port.
    let dcfg = DaemonConfig {
        dir: bus_dir.to_path_buf(),
        host: host.to_string(),
        client_port: -1,
        leaf_listen,
        remote_ops: None,
        busd_path: Some(iso.busd_bin.clone()),
    };
    let daemon = start_daemon(dcfg).expect("starting throwaway agent-busd");
    let conn = connect_local(bus_dir, host).expect("connecting to local busd");
    let rpc = handler.map(|(hdl, ops)| serve_rpc(&conn, host, hdl, &ops).unwrap());
    RunningBus {
        daemon,
        conn,
        _rpc: rpc,
    }
}

fn wait_until_peer_reachable(rpc: &RpcClient, host: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let ctx = Context::background();
    loop {
        let (call_ctx, _cancel) = Context::with_timeout(&ctx, Duration::from_millis(250));
        if rpc.list_host(&call_ctx, host).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for leaf link to {host}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn fixed_time(year: i32, month: u32, day: u32, hour: u32, min: u32, sec: u32) -> Time {
    let dt = Utc
        .with_ymd_and_hms(year, month, day, hour, min, sec)
        .single()
        .unwrap();
    Time::from(dt)
}

fn store_time(t: Time) -> StoreTime {
    StoreTime(t.0)
}

fn bus_time_of(t: StoreTime) -> Time {
    Time::from(t.0)
}

fn set_mtime(path: &Path, t: Time) {
    let secs = t.0.timestamp() as u64;
    let when = UNIX_EPOCH + Duration::from_secs(secs);
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(when).unwrap();
}

fn run_git(repo: &Path, args: &[&str], commit_time: Option<Time>) {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if let Some(ts) = commit_time {
        let date = format!("@{} +0000", ts.0.timestamp());
        cmd.env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date);
    }
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn flags_at_every_threshold_and_session_name_todo() {
    with_hard_deadline(
        "flags_at_every_threshold_and_session_name_todo",
        HARD_TEST_DEADLINE,
        || {
            let th = TestHost::new("alpha");
            let now = fixed_time(2026, 11, 20, 12, 0, 0);

            // 1. Direct threshold boundary checks on `fleet::agent_flags`.
            assert_eq!(WAITING_STUCK_AFTER, Duration::from_secs(600));
            assert_eq!(HELD_STUCK_COUNT, 3);
            assert_eq!(HELD_STUCK_AFTER, Duration::from_secs(600));

            let waiting_exact_10m = FleetAgent {
                name: "w1".into(),
                status: "waiting".into(),
                since: Time::from(now.0 - TimeDelta::seconds(600)),
                ..Default::default()
            };
            assert!(
                fleet::agent_flags(&waiting_exact_10m, now).is_empty(),
                "exact 10m00s must not trigger > 10m waiting flag"
            );

            let waiting_over_10m = FleetAgent {
                since: Time::from(now.0 - TimeDelta::seconds(14 * 60)),
                ..waiting_exact_10m
            };
            assert_eq!(
                fleet::agent_flags(&waiting_over_10m, now),
                vec!["stuck: waiting 14m"]
            );

            let held_below = FleetAgent {
                name: "h1".into(),
                status: "processing".into(),
                held: 2,
                oldest_held: Time::from(now.0 - TimeDelta::seconds(600)),
                ..Default::default()
            };
            assert!(
                fleet::agent_flags(&held_below, now).is_empty(),
                "2 held at exact 10m00s must not flag"
            );

            let held_age_over = FleetAgent {
                oldest_held: Time::from(now.0 - TimeDelta::seconds(12 * 60)),
                ..held_below.clone()
            };
            assert_eq!(
                fleet::agent_flags(&held_age_over, now),
                vec!["stuck: 2 held, oldest 12m"]
            );

            let held_count_3 = FleetAgent {
                held: 3,
                oldest_held: Time::from(now.0 - TimeDelta::seconds(25)),
                ..held_below
            };
            assert_eq!(
                fleet::agent_flags(&held_count_3, now),
                vec!["stuck: 3 held, oldest 25s"]
            );

            let ended_quit = FleetAgent {
                name: "e1".into(),
                status: "ended".into(),
                end_reason: "quit".into(),
                ..Default::default()
            };
            assert_eq!(
                fleet::agent_flags(&ended_quit, now),
                vec!["ended (quit), still recorded"]
            );

            let mismatch_unknown = FleetAgent {
                name: "m1".into(),
                status: "idle".into(),
                session_name: String::new(),
                ..Default::default()
            };
            assert!(
                fleet::agent_flags(&mismatch_unknown, now).is_empty(),
                "empty/unknown session_name must never raise name mismatch"
            );

            let mismatch_same = FleetAgent {
                session_name: "m1".into(),
                ..mismatch_unknown.clone()
            };
            assert!(fleet::agent_flags(&mismatch_same, now).is_empty());

            let mismatch_diff = FleetAgent {
                session_name: "wrong-name".into(),
                ..mismatch_unknown
            };
            assert_eq!(
                fleet::agent_flags(&mismatch_diff, now),
                vec![r#"name mismatch: session is "wrong-name""#]
            );

            // 2. End-to-end check through `fleet::local` from store state + live mux session.
            let waiter_session = th.cfg.session_name("waiter");
            let waiter_cwd = th.dir.path().to_string_lossy().into_owned();
            th.mux.insert_live(&waiter_session, &waiter_cwd);
            let a = Agent {
                name: "waiter".into(),
                session_name: waiter_session,
                backend: "agy".into(),
                created_at: store_time(Time::from(now.0 - TimeDelta::minutes(30))),
                cwd: waiter_cwd,
                ..Default::default()
            };
            th.store.create_agent(&a).unwrap();
            let waited_since = store_time(Time::from(now.0 - TimeDelta::minutes(15)));
            th.store
                .record_report(
                    &[
                        StateEntry {
                            agent_name: "waiter".into(),
                            key: "_status".into(),
                            value: AGENT_STATUS_WAITING.to_string(),
                            updated_at: waited_since,
                        },
                        StateEntry {
                            agent_name: "waiter".into(),
                            key: "_detail".into(),
                            value: "Bash: cargo test".into(),
                            updated_at: waited_since,
                        },
                        StateEntry {
                            agent_name: "waiter".into(),
                            key: STATE_KEY_SESSION_ID.into(),
                            value: "sess-waiter-1".into(),
                            updated_at: waited_since,
                        },
                    ],
                    "waiter",
                    &[],
                    Some(&agent_store::AgentEvent {
                        kind: AGENT_STATUS.into(),
                        agent: "waiter".into(),
                        at: waited_since,
                        status: AGENT_STATUS_WAITING.to_string(),
                        detail: "Bash: cargo test".into(),
                        ..Default::default()
                    }),
                )
                .unwrap();

            for idx in 0..3 {
                let mut msg = Message {
                    sender_id: "lead".into(),
                    receiver_id: "waiter".into(),
                    content: format!("msg-{idx}"),
                    status: MESSAGE_STATUS_PENDING,
                    created_at: store_time(Time::from(now.0 - TimeDelta::minutes(12 - idx))),
                    ..Default::default()
                };
                th.store.create_message(&mut msg).unwrap();
            }

            // Default `fleet::local` leaves `session_name` empty (omitted in JSON).
            let snap = fleet::local(
                &th.service,
                &th.store,
                &th.cfg,
                &FleetOptions {
                    now: Some(now),
                    ..Default::default()
                },
            )
            .unwrap();
            assert_eq!(snap.agents.len(), 1);
            let fa = &snap.agents[0];
            assert_eq!(fa.status, "waiting");
            assert_eq!(fa.since, bus_time_of(waited_since));
            assert_eq!(fa.held, 3);
            assert!(fa.session_name.is_empty());
            assert!(
                fa.flags.contains(&"stuck: waiting 15m".to_string())
                    && fa.flags.contains(&"stuck: 3 held, oldest 12m".to_string()),
                "got flags: {:?}",
                fa.flags
            );
            let raw_json = serde_json::to_string(&fa).unwrap();
            assert!(
                !raw_json.contains("session_name"),
                "unset session_name should be omitted on the wire: {raw_json}"
            );

            // Supplying a session-name reader triggers `name mismatch` when it differs.
            let snap_mismatch = fleet::local(
                &th.service,
                &th.store,
                &th.cfg,
                &FleetOptions {
                    now: Some(now),
                    session_name: Some(Arc::new(|_, _| Some("renamed-in-cli".into()))),
                    ..Default::default()
                },
            )
            .unwrap();
            assert!(
                snap_mismatch.agents[0]
                    .flags
                    .contains(&r#"name mismatch: session is "renamed-in-cli""#.to_string())
            );
        },
    );
}

#[test]
fn local_tree_sections_render_and_baseline_report() {
    with_hard_deadline(
        "local_tree_sections_render_and_baseline_report",
        HARD_TEST_DEADLINE,
        || {
            let th = TestHost::new("hub-main");
            let now = fixed_time(2026, 11, 20, 15, 0, 0);

            // Git repo with one stale handoff (`lead`) and one fresh handoff (`helper-a`).
            let repo = th.dir.path().join("project");
            let lanes = repo.join("docs/lanes");
            fs::create_dir_all(&lanes).unwrap();
            let lead_doc = lanes.join("lead.md");
            let helper_doc = lanes.join("helper-a.md");
            fs::write(&lead_doc, "# Lead handoff\n").unwrap();
            fs::write(&helper_doc, "# Helper handoff\n").unwrap();
            run_git(&repo, &["init"], None);
            run_git(&repo, &["add", "."], None);
            let commit_at = Time::from(now.0 - TimeDelta::hours(2));
            run_git(&repo, &["commit", "-m", "init"], Some(commit_at));
            set_mtime(&lead_doc, Time::from(now.0 - TimeDelta::hours(5)));
            set_mtime(&helper_doc, Time::from(now.0 - TimeDelta::minutes(30)));

            let repo_s = repo.to_string_lossy().into_owned();

            // Long-running Claude transcript (> 3 days old) with 2 compactions, plus a
            // recent agy session (1 day old) and an old Claude session (10 days old).
            let claude_proj =
                PathBuf::from(th.cfg.claude_projects_dir.as_deref().unwrap()).join("-srv-project");
            fs::create_dir_all(&claude_proj).unwrap();
            let lead_sid = "11111111-1111-4111-8111-111111111111";
            let lead_tr = claude_proj.join(format!("{lead_sid}.jsonl"));
            let claude_lines = [
                r#"{"type":"user","sessionId":"11111111-1111-4111-8111-111111111111","cwd":"/srv/project","timestamp":"2026-11-15T10:00:00Z","uuid":"u1","message":{"role":"user","content":"start"}}"#,
                r#"{"type":"system","subtype":"compact_boundary","sessionId":"11111111-1111-4111-8111-111111111111","timestamp":"2026-11-17T10:00:00Z","uuid":"c1","content":"Conversation compacted"}"#,
                r#"{"type":"user","isCompactSummary":true,"sessionId":"11111111-1111-4111-8111-111111111111","timestamp":"2026-11-17T10:00:01Z","uuid":"cs1","message":{"role":"user","content":"summary 1"}}"#,
                r#"{"type":"system","subtype":"compact_boundary","sessionId":"11111111-1111-4111-8111-111111111111","timestamp":"2026-11-19T10:00:00Z","uuid":"c2","content":"Conversation compacted"}"#,
            ];
            fs::write(&lead_tr, format!("{}\n", claude_lines.join("\n"))).unwrap();
            set_mtime(&lead_tr, Time::from(now.0 - TimeDelta::hours(3)));

            let old_sid = "22222222-2222-4222-8222-222222222222";
            let old_tr = claude_proj.join(format!("{old_sid}.jsonl"));
            fs::write(
                &old_tr,
                r#"{"type":"user","sessionId":"22222222-2222-4222-8222-222222222222","cwd":"/srv/project","timestamp":"2026-11-01T10:00:00Z","uuid":"u0","message":{"role":"user","content":"old"}}"#,
            )
            .unwrap();
            set_mtime(&old_tr, Time::from(now.0 - TimeDelta::days(10)));

            let agy_conv = PathBuf::from(th.cfg.agy_brain_dir.as_deref().unwrap())
                .join("33333333-3333-4333-8333-333333333333/.system_generated/logs");
            fs::create_dir_all(&agy_conv).unwrap();
            let agy_tr = agy_conv.join("transcript.jsonl");
            fs::write(
                &agy_tr,
                r#"{"step_index":0,"type":"USER_INPUT","status":"DONE","created_at":"2026-11-19T12:00:00Z","content":"hello"}"#,
            )
            .unwrap();
            set_mtime(&agy_tr, Time::from(now.0 - TimeDelta::days(1)));

            let specs = [
                (
                    "lead",
                    "",
                    "claude",
                    Time::from(now.0 - TimeDelta::days(4)),
                    "idle",
                    "",
                    "claude-opus-4-6",
                    "",
                ),
                (
                    "helper-a",
                    "lead",
                    "agy",
                    Time::from(now.0 - TimeDelta::hours(2)),
                    "processing",
                    "",
                    "gemini-2.5-pro",
                    "33333333-3333-4333-8333-333333333333",
                ),
                (
                    "sub-a",
                    "helper-a",
                    "claude",
                    Time::from(now.0 - TimeDelta::minutes(50)),
                    "idle",
                    "",
                    "",
                    "",
                ),
                (
                    "helper-b",
                    "lead",
                    "claude",
                    Time::from(now.0 - TimeDelta::minutes(40)),
                    "ended",
                    "quit",
                    "",
                    "",
                ),
                (
                    "remote-worker",
                    "coord@peer-one",
                    "claude",
                    Time::from(now.0 - TimeDelta::minutes(20)),
                    "idle",
                    "",
                    "",
                    "",
                ),
            ];

            for (name, parent, backend, created, status, end_reason, model, sid) in specs {
                let sess_name = th.cfg.session_name(name);
                if status != "ended" {
                    th.mux.insert_live(&sess_name, &repo_s);
                }
                let agent = Agent {
                    name: name.into(),
                    session_name: sess_name,
                    backend: backend.into(),
                    spawned_by: parent.into(),
                    created_at: store_time(created),
                    cwd: repo_s.clone(),
                    ..Default::default()
                };
                th.store.create_agent(&agent).unwrap();

                let status_at = store_time(Time::from(now.0 - TimeDelta::minutes(5)));
                let mut entries = vec![StateEntry {
                    agent_name: name.into(),
                    key: "_status".into(),
                    value: status.into(),
                    updated_at: status_at,
                }];
                if !end_reason.is_empty() {
                    entries.push(StateEntry {
                        agent_name: name.into(),
                        key: "_end_reason".into(),
                        value: end_reason.into(),
                        updated_at: status_at,
                    });
                }
                if !model.is_empty() {
                    entries.push(StateEntry {
                        agent_name: name.into(),
                        key: STATE_KEY_MODEL.into(),
                        value: model.into(),
                        updated_at: status_at,
                    });
                }
                if !sid.is_empty() {
                    entries.push(StateEntry {
                        agent_name: name.into(),
                        key: STATE_KEY_SESSION_ID.into(),
                        value: sid.into(),
                        updated_at: status_at,
                    });
                }
                if name == "lead" {
                    entries.push(StateEntry {
                        agent_name: name.into(),
                        key: "_transcript".into(),
                        value: lead_tr.to_string_lossy().into_owned(),
                        updated_at: status_at,
                    });
                }
                if name == "helper-a" {
                    entries.push(StateEntry {
                        agent_name: name.into(),
                        key: handoff::STATE_KEY_HANDOFF.into(),
                        value: "docs/lanes/helper-a.md".into(),
                        updated_at: status_at,
                    });
                }
                th.store
                    .record_report(
                        &entries,
                        name,
                        &[],
                        Some(&agent_store::AgentEvent {
                            kind: AGENT_STATUS.into(),
                            agent: name.into(),
                            at: status_at,
                            status: status.into(),
                            end_reason: end_reason.into(),
                            ..Default::default()
                        }),
                    )
                    .unwrap();
            }

            focus::set(&th.store, None, "hub-main", "helper-a", "lead").unwrap();

            let my_pid = std::process::id() as i64;
            let mut job = Job {
                script: "scripts/heavy-check.sh".into(),
                path: repo
                    .join("scripts/heavy-check.sh")
                    .to_string_lossy()
                    .into_owned(),
                sha256: "abc123".into(),
                content: "#!/bin/sh\ntrue\n".into(),
                by: "lead".into(),
                host: "hub-main".into(),
                cwd: repo_s,
                started_at: store_time(Time::from(now.0 - TimeDelta::minutes(2))),
                ..Default::default()
            };
            th.store.create_job(&mut job).unwrap();
            job.pid = my_pid;
            job.pid_start = run::process_start(my_pid).unwrap_or_default();
            job.memory_max = "24G".into();
            th.store.update_job_launch(&job).unwrap();

            for (idx, (status, kind, summary)) in [
                (FRICTION_OPEN, "bug", "flag edge case"),
                (FRICTION_ACCEPTED, "docs", "clarify template start"),
                (FRICTION_RESOLVED, "bug", "fixed earlier"),
            ]
            .into_iter()
            .enumerate()
            {
                let mut f = Friction {
                    uid: format!("fr-uid-{idx}"),
                    status: status.into(),
                    kind: kind.into(),
                    summary: summary.into(),
                    host: "hub-main".into(),
                    created_at: store_time(now),
                    updated_at: store_time(now),
                    ..Default::default()
                };
                th.store.add_friction(&mut f).unwrap();
            }

            for (ts, status) in [
                (fixed_time(2026, 11, 19, 9, 0, 0), MESSAGE_STATUS_DELIVERED),
                (fixed_time(2026, 11, 19, 18, 0, 0), MESSAGE_STATUS_DELIVERED),
                (fixed_time(2026, 11, 20, 11, 0, 0), MESSAGE_STATUS_PENDING),
                (fixed_time(2026, 11, 8, 12, 0, 0), MESSAGE_STATUS_DELIVERED),
            ] {
                let mut m = Message {
                    sender_id: "lead".into(),
                    receiver_id: "remote-worker".into(),
                    content: "ping".into(),
                    status,
                    created_at: store_time(ts),
                    ..Default::default()
                };
                th.store.create_message(&mut m).unwrap();
            }

            let fh = fleet::local(
                &th.service,
                &th.store,
                &th.cfg,
                &FleetOptions {
                    report: true,
                    now: Some(now),
                    ..Default::default()
                },
            )
            .unwrap();

            assert_eq!(fh.host, "hub-main");
            assert!(fh.reachable);
            assert!(!fh.build.is_empty());
            assert_eq!(fh.focus.as_ref().unwrap().agent, "helper-a");

            assert_eq!(fh.agents.len(), 2);
            assert_eq!(fh.agents[0].name, "lead");
            assert_eq!(fh.agents[0].children.len(), 2);
            assert_eq!(fh.agents[0].children[0].name, "helper-a");
            assert_eq!(fh.agents[0].children[0].children.len(), 1);
            assert_eq!(fh.agents[0].children[0].children[0].name, "sub-a");
            assert_eq!(fh.agents[0].children[1].name, "helper-b");
            assert_eq!(
                fh.agents[0].children[1].flags,
                vec!["ended (quit), still recorded"]
            );
            assert_eq!(fh.agents[1].name, "remote-worker");
            assert_eq!(fh.agents[1].spawned_by, "coord@peer-one");

            assert_eq!(fh.jobs.len(), 1);
            assert_eq!(fh.jobs[0].script, "scripts/heavy-check.sh");
            assert_eq!(fh.jobs[0].memory_max, "24G");
            assert_eq!(fh.handoffs.len(), 1);
            assert_eq!(fh.handoffs[0].agent, "lead");
            let fr = fh.friction.as_ref().unwrap();
            assert_eq!(fr.open, 2);
            assert_eq!(
                fr.by_kind,
                BTreeMap::from([("bug".into(), 1), ("docs".into(), 1)])
            );

            let rep = fh.report.as_ref().unwrap();
            assert_eq!(
                rep.agents_by_status,
                BTreeMap::from([
                    ("ended".into(), 1),
                    ("idle".into(), 3),
                    ("processing".into(), 1),
                ])
            );
            assert_eq!(
                rep.agents_by_backend,
                BTreeMap::from([("agy".into(), 1), ("claude".into(), 4)])
            );
            assert_eq!(
                rep.sessions_7d_by_harness,
                BTreeMap::from([("agy".into(), 1), ("claude".into(), 1)])
            );
            assert_eq!(
                rep.messages_per_day_7d,
                BTreeMap::from([("2026-11-19".into(), 2), ("2026-11-20".into(), 1)])
            );
            assert_eq!(
                rep.friction_open_by_kind,
                BTreeMap::from([("bug".into(), 1), ("docs".into(), 1)])
            );
            assert_eq!(
                rep.friction_resolved_by_kind,
                BTreeMap::from([("bug".into(), 1)])
            );
            assert_eq!(rep.long_sessions.len(), 1);
            assert_eq!(rep.long_sessions[0].agent, "lead");
            assert_eq!(rep.long_sessions[0].compactions, 2);

            let view = fleet::gather(
                fh,
                None,
                &[],
                &FleetOptions {
                    now: Some(now),
                    ..Default::default()
                },
            );
            let text = fleet::render(&view);
            assert!(text.contains("hub-main (build "), "text:\n{text}");
            assert!(text.contains("  focus: helper-a"), "text:\n{text}");
            assert!(
                text.contains("  lead: idle (claude claude-opus-4-6)"),
                "text:\n{text}"
            );
            assert!(
                text.contains("    └ helper-a: processing (agy gemini-2.5-pro)"),
                "text:\n{text}"
            );
            assert!(
                text.contains("      └ sub-a: idle (claude)"),
                "text:\n{text}"
            );
            assert!(
                text.contains("    └ helper-b: ended (quit) (claude)"),
                "text:\n{text}"
            );
            assert!(text.contains("parent: coord@peer-one"), "text:\n{text}");
            assert!(text.contains("  jobs:\n    #"), "text:\n{text}");
            assert!(
                text.contains("  stale handoffs:\n    lead: "),
                "text:\n{text}"
            );
            assert!(
                text.contains("  friction: 2 open (bug: 1, docs: 1)"),
                "text:\n{text}"
            );

            let rep_text = fleet::render_report(&view);
            assert!(rep_text.contains("agents by status: ended: 1, idle: 3, processing: 1"));
            assert!(rep_text.contains("sessions (7d): agy: 1, claude: 1"));
            assert!(rep_text.contains("messages/day (7d): 2026-11-19: 2, 2026-11-20: 1"));
            assert!(rep_text.contains(
                "long sessions (>3d):\n    lead (claude, started 2026-11-16, 2 compactions)"
            ));

            let empty_host = TestHost::new("quiet-host");
            let empty_fh = fleet::local(
                &empty_host.service,
                &empty_host.store,
                &empty_host.cfg,
                &FleetOptions::default(),
            )
            .unwrap();
            let empty_rendered = fleet::render(&single_view(empty_fh));
            assert!(!empty_rendered.contains("focus:"));
            assert!(!empty_rendered.contains("jobs:"));
            assert!(!empty_rendered.contains("stale handoffs:"));
            assert!(!empty_rendered.contains("friction:"));
        },
    );
}

fn single_view(h: FleetHost) -> FleetView {
    FleetView {
        at: Time::now(),
        hosts: vec![h],
    }
}

/// Minimal handler for a peer whose bus serves `list` but not `fleet`.
struct ListOnlyHandler {
    agents: Vec<AgentInfo>,
}

impl Handler for ListOnlyHandler {
    fn list(&self, _ctx: &Context) -> anyhow::Result<ListResponse> {
        Ok(ListResponse {
            agents: self.agents.clone(),
            agent_binary: "0.1.0-go".into(),
        })
    }
    fn status(
        &self,
        _: &Context,
        _: agent_bus::StatusRequest,
    ) -> anyhow::Result<agent_bus::StatusResponse> {
        bail!("unused")
    }
    fn capture(
        &self,
        _: &Context,
        _: agent_bus::CaptureRequest,
    ) -> anyhow::Result<agent_bus::CaptureResponse> {
        bail!("unused")
    }
    fn send(
        &self,
        _: &Context,
        _: agent_bus::SendRequest,
    ) -> anyhow::Result<Option<agent_bus::SendResponse>> {
        bail!("unused")
    }
    fn respond(&self, _: &Context, _: agent_bus::RespondRequest) -> anyhow::Result<()> {
        bail!("unused")
    }
    fn deliver(
        &self,
        _: &Context,
        _: agent_bus::RespondRequest,
    ) -> anyhow::Result<agent_bus::DeliverResponse> {
        bail!("unused")
    }
    fn keys(&self, _: &Context, _: agent_bus::KeysRequest) -> anyhow::Result<()> {
        bail!("unused")
    }
    fn spawn(
        &self,
        _: &Context,
        _: agent_bus::SpawnRequest,
    ) -> anyhow::Result<agent_bus::SpawnResponse> {
        bail!("unused")
    }
    fn friction(
        &self,
        _: &Context,
        _: agent_bus::FrictionRequest,
    ) -> anyhow::Result<agent_bus::FrictionResponse> {
        bail!("unused")
    }
    fn logs(
        &self,
        _: &Context,
        _: agent_bus::LogsRequest,
    ) -> anyhow::Result<agent_bus::LogsResponse> {
        bail!("unused")
    }
    fn resume(&self, _: &Context, _: ResumeRequest) -> anyhow::Result<ResumeResponse> {
        bail!("unused")
    }
}

#[test]
fn gather_with_rust_peer_list_list_peer_and_unreachable_peer() {
    with_hard_deadline(
        "gather_with_rust_peer_list_list_peer_and_unreachable_peer",
        HARD_TEST_DEADLINE,
        || {
            let hub = TestHost::new("hub-g");
            let peer_rust = TestHost::new("peer-rust");
            let peer_list = TestHost::new("peer-list");
            let peer_dead = TestHost::new("peer-dead");

            let builder_session = peer_rust.cfg.session_name("builder");
            let builder_cwd = peer_rust.dir.path().to_string_lossy().into_owned();
            peer_rust.mux.insert_live(&builder_session, &builder_cwd);
            let rust_agent = Agent {
                name: "builder".into(),
                session_name: builder_session,
                backend: "claude".into(),
                created_at: StoreTime::now(),
                cwd: builder_cwd,
                ..Default::default()
            };
            peer_rust.store.create_agent(&rust_agent).unwrap();
            focus::set(&peer_rust.store, None, "peer-rust", "builder", "coord").unwrap();

            let hub_id = init_identity(hub.bus_dir(), "hub-g").unwrap();
            let rust_id = init_identity(peer_rust.bus_dir(), "peer-rust").unwrap();
            let list_id = init_identity(peer_list.bus_dir(), "peer-list").unwrap();
            let dead_id = init_identity(peer_dead.bus_dir(), "peer-dead").unwrap();

            let hub_leaf_port = free_local_port();
            let hub_tls_url = format!("tls://127.0.0.1:{hub_leaf_port}");

            for peer in [&rust_id, &list_id, &dead_id] {
                trust_peer(hub.bus_dir(), peer.public_bundle("")).unwrap();
            }
            trust_peer(peer_rust.bus_dir(), hub_id.public_bundle(&hub_tls_url)).unwrap();
            trust_peer(peer_list.bus_dir(), hub_id.public_bundle(&hub_tls_url)).unwrap();

            let rust_handler = Arc::new(new_rpc_handler_with(
                Arc::clone(&peer_rust.service),
                RpcExtras {
                    fleet: Some(fleet::rpc_handler(
                        Arc::clone(&peer_rust.service),
                        Arc::clone(&peer_rust.store),
                        peer_rust.cfg.clone(),
                    )),
                    ..Default::default()
                },
            )) as Arc<dyn Handler>;

            let list_handler = Arc::new(ListOnlyHandler {
                agents: vec![
                    AgentInfo {
                        name: "old-lead".into(),
                        session: "s-old-lead".into(),
                        backend: "claude".into(),
                        status: "idle".into(),
                        ..Default::default()
                    },
                    AgentInfo {
                        name: "old-helper".into(),
                        session: "s-old-helper".into(),
                        backend: "claude".into(),
                        spawned_by: "old-lead".into(),
                        status: "ended".into(),
                        end_reason: "quit".into(),
                        ..Default::default()
                    },
                ],
            }) as Arc<dyn Handler>;

            let hub_bus = start_bus_daemon(&hub.bus_dir(), "hub-g", hub_leaf_port, None);
            let _rust_bus = start_bus_daemon(
                &peer_rust.bus_dir(),
                "peer-rust",
                0,
                Some((rust_handler, ALL_OPS.to_vec())),
            );
            let _list_bus = start_bus_daemon(
                &peer_list.bus_dir(),
                "peer-list",
                0,
                Some((list_handler, vec![OP_LIST])),
            );

            let rpc = RpcClient::new(hub_bus.conn.clone(), DEFAULT_RPC_TIMEOUT);
            wait_until_peer_reachable(&rpc, "peer-rust");
            wait_until_peer_reachable(&rpc, "peer-list");

            let local_snap =
                fleet::local(&hub.service, &hub.store, &hub.cfg, &FleetOptions::default()).unwrap();

            let peers = vec![
                "peer-rust".to_string(),
                "peer-list".to_string(),
                "peer-dead".to_string(),
            ];
            let view = fleet::gather(
                local_snap.clone(),
                Some(&rpc),
                &peers,
                &FleetOptions::default(),
            );
            assert_eq!(view.hosts.len(), 4);

            assert_eq!(view.hosts[0].host, "hub-g");
            assert!(view.hosts[0].reachable);

            let h_rust = &view.hosts[1];
            assert_eq!(h_rust.host, "peer-rust");
            assert!(h_rust.reachable);
            assert!(!h_rust.build.is_empty());
            assert_eq!(h_rust.focus.as_ref().unwrap().agent, "builder");
            assert_eq!(h_rust.agents.len(), 1);
            assert_eq!(h_rust.agents[0].name, "builder");

            let h_list = &view.hosts[2];
            assert_eq!(h_list.host, "peer-list");
            // A peer that doesn't serve fleet is unreachable for it: no
            // falling back to its list.
            assert!(!h_list.reachable);
            assert!(!h_list.error.is_empty());
            assert!(h_list.agents.is_empty());

            let h_dead = &view.hosts[3];
            assert_eq!(h_dead.host, "peer-dead");
            assert!(!h_dead.reachable);
            assert!(!h_dead.error.is_empty());

            let rendered = fleet::render(&view);
            assert!(rendered.contains("peer-rust (build "));
            assert!(rendered.contains("peer-list: unreachable ("));
            assert!(rendered.contains("peer-dead: unreachable ("));

            let local_only = fleet::gather(local_snap, None, &peers, &FleetOptions::default());
            assert_eq!(local_only.hosts.len(), 1);
            assert_eq!(local_only.hosts[0].host, "hub-g");
        },
    );
}

#[test]
fn focus_set_clear_cleanup_and_follow_across_leaf() {
    with_hard_deadline(
        "focus_set_clear_cleanup_and_follow_across_leaf",
        HARD_TEST_DEADLINE,
        || {
            let hub = TestHost::new("hub-focus");
            let leaf = TestHost::new("leaf-focus");

            let err = focus::set(&hub.store, None, "hub-focus", "ghost", "user").unwrap_err();
            assert!(
                err.downcast_ref::<AgentNotFoundError>().is_some(),
                "expected AgentNotFoundError, got: {err:#}"
            );
            assert!(focus::get(&hub.store).unwrap().is_none());

            let hub_cwd = hub.dir.path().to_string_lossy().into_owned();
            for name in ["alpha", "beta"] {
                let sess = hub.cfg.session_name(name);
                hub.mux.insert_live(&sess, &hub_cwd);
                hub.store
                    .create_agent(&Agent {
                        name: name.into(),
                        session_name: sess,
                        backend: "claude".into(),
                        created_at: StoreTime::now(),
                        cwd: hub_cwd.clone(),
                        ..Default::default()
                    })
                    .unwrap();
            }

            let ev = focus::set(&hub.store, None, "hub-focus", "alpha", "lead").unwrap();
            assert_eq!(ev.agent, "alpha");
            assert_eq!(ev.session, hub.cfg.session_name("alpha"));
            assert!(!focus::clear_if(&hub.store, "beta").unwrap());
            assert_eq!(focus::get(&hub.store).unwrap().unwrap().agent, "alpha");
            assert!(focus::clear_if(&hub.store, "alpha").unwrap());
            assert!(focus::get(&hub.store).unwrap().is_none());

            focus::set(&hub.store, None, "hub-focus", "alpha", "lead").unwrap();

            let hub_leaf_port = free_local_port();
            let hub_id = init_identity(hub.bus_dir(), "hub-focus").unwrap();
            let leaf_id = init_identity(leaf.bus_dir(), "leaf-focus").unwrap();
            trust_peer(hub.bus_dir(), leaf_id.public_bundle("")).unwrap();
            trust_peer(
                leaf.bus_dir(),
                hub_id.public_bundle(&format!("tls://127.0.0.1:{hub_leaf_port}")),
            )
            .unwrap();

            let hub_handler = Arc::new(new_rpc_handler_with(
                Arc::clone(&hub.service),
                RpcExtras {
                    fleet: Some(fleet::rpc_handler(
                        Arc::clone(&hub.service),
                        Arc::clone(&hub.store),
                        hub.cfg.clone(),
                    )),
                    ..Default::default()
                },
            )) as Arc<dyn Handler>;

            let hub_bus = start_bus_daemon(
                &hub.bus_dir(),
                "hub-focus",
                hub_leaf_port,
                Some((hub_handler, ALL_OPS.to_vec())),
            );
            let leaf_bus = start_bus_daemon(&leaf.bus_dir(), "leaf-focus", 0, None);

            let leaf_rpc = RpcClient::new(leaf_bus.conn.clone(), DEFAULT_RPC_TIMEOUT);
            wait_until_peer_reachable(&leaf_rpc, "hub-focus");

            let hub_bus_dir = hub.bus_dir();
            let publisher = Publisher::new(move || connect_local(&hub_bus_dir, "hub-focus"));

            let seen: Arc<Mutex<Vec<FocusEvent>>> = Arc::new(Mutex::new(Vec::new()));
            let (initial_tx, initial_rx) = std::sync::mpsc::channel::<()>();
            let seen_cl = Arc::clone(&seen);
            let leaf_conn = leaf_bus.conn.clone();

            let follower = std::thread::spawn(move || {
                let (ctx, _cancel) =
                    Context::with_timeout(&Context::background(), Duration::from_secs(5));
                let mut notified = false;
                focus::follow(&ctx, None, Some(&leaf_conn), Some("hub-focus"), |ev| {
                    seen_cl.lock().unwrap().push(ev.clone());
                    if !notified {
                        notified = true;
                        let _ = initial_tx.send(());
                    }
                    !ev.agent.is_empty()
                })
            });

            initial_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("follower should emit initial focus from `fleet`");
            // Give leafnode subscription interest (`LS+ gomux.hub-focus.focus`)
            // a brief moment to propagate to the hub before publishing.
            std::thread::sleep(Duration::from_millis(50));

            focus::set(&hub.store, Some(&publisher), "hub-focus", "beta", "lead").unwrap();
            focus::clear(&hub.store, Some(&publisher), "hub-focus", "lead").unwrap();

            follower
                .join()
                .expect("follower thread panicked")
                .expect("focus::follow should finish cleanly");

            let events = seen.lock().unwrap().clone();
            let agents: Vec<&str> = events.iter().map(|e| e.agent.as_str()).collect();
            assert_eq!(
                agents,
                vec!["alpha", "beta", ""],
                "expected initial alpha, update beta, then clear; got: {events:?}"
            );
            drop(hub_bus);
        },
    );
}

#[test]
fn template_op_list_start_resume_across_leaf_and_refusals() {
    with_hard_deadline(
        "template_op_list_start_resume_across_leaf_and_refusals",
        HARD_TEST_DEADLINE,
        || {
            let hub = TestHost::new("hub-tmpl");
            let leaf = TestHost::new("leaf-tmpl");

            let bare = new_rpc_handler(Arc::clone(&hub.service));
            let ctx = Context::background();
            let fleet_refused = bare.fleet(&ctx, FleetRequest::default()).unwrap_err();
            assert!(
                format!("{fleet_refused:#}").contains("this host's bus doesn't serve fleet"),
                "got: {fleet_refused:#}"
            );
            let tmpl_refused = bare
                .template(
                    &ctx,
                    TemplateRequest {
                        action: TEMPLATE_LIST.into(),
                        ..Default::default()
                    },
                )
                .unwrap_err();
            assert!(
                format!("{tmpl_refused:#}").contains("this host's bus doesn't serve template"),
                "got: {tmpl_refused:#}"
            );

            let proj = hub.dir.path().join("builder-proj");
            let lanes = proj.join("docs/lanes");
            fs::create_dir_all(&lanes).unwrap();
            let brief_file = lanes.join("builder.md");
            fs::write(
                &brief_file,
                "# Builder\nCONFIDENTIAL_SENTINEL_DO_NOT_LEAK_OVER_LIST\n",
            )
            .unwrap();

            templates::add(
                &hub.data_dir,
                Template::new(
                    "builder",
                    "claude",
                    "claude-sonnet-4-6",
                    proj.to_string_lossy().into_owned(),
                    "docs/lanes/builder.md",
                ),
            )
            .unwrap();

            let resume_calls: Arc<Mutex<Vec<ResumeRequest>>> = Arc::new(Mutex::new(Vec::new()));
            let resume_calls_cl = Arc::clone(&resume_calls);
            let hub_store_for_resume = Arc::clone(&hub.store);
            let hub_svc_for_resume = Arc::clone(&hub.service);
            let hub_mux_for_resume = Arc::clone(&hub.mux);
            let resume_fn: Arc<template_op::ResumeAgentFn> = Arc::new(move |_ctx, req| {
                resume_calls_cl.lock().unwrap().push(req.clone());
                let a = hub_svc_for_resume
                    .storage()
                    .get_agent(&req.agent)?
                    .expect("resumed agent record");
                hub_mux_for_resume.insert_live(&a.session_name, &a.cwd);
                let now = StoreTime::now();
                hub_store_for_resume.record_report(
                    &[StateEntry {
                        agent_name: req.agent.clone(),
                        key: "_status".into(),
                        value: AGENT_STATUS_IDLE.to_string(),
                        updated_at: now,
                    }],
                    &req.agent,
                    &["_end_reason".to_string()],
                    Some(&agent_store::AgentEvent {
                        kind: AGENT_STATUS.into(),
                        agent: req.agent.clone(),
                        at: now,
                        status: AGENT_STATUS_IDLE.to_string(),
                        ..Default::default()
                    }),
                )?;
                Ok(ResumeResponse {
                    name: a.name,
                    session: a.session_name,
                    backend: a.backend,
                    cwd: a.cwd,
                    status: AGENT_STATUS_IDLE.to_string(),
                    ..Default::default()
                })
            });

            let hub_leaf_port = free_local_port();
            let hub_id = init_identity(hub.bus_dir(), "hub-tmpl").unwrap();
            let leaf_id = init_identity(leaf.bus_dir(), "leaf-tmpl").unwrap();
            trust_peer(hub.bus_dir(), leaf_id.public_bundle("")).unwrap();
            trust_peer(
                leaf.bus_dir(),
                hub_id.public_bundle(&format!("tls://127.0.0.1:{hub_leaf_port}")),
            )
            .unwrap();

            let hub_handler = Arc::new(new_rpc_handler_with(
                Arc::clone(&hub.service),
                RpcExtras {
                    fleet: Some(fleet::rpc_handler(
                        Arc::clone(&hub.service),
                        Arc::clone(&hub.store),
                        hub.cfg.clone(),
                    )),
                    template: Some(template_op::rpc_handler(
                        Arc::clone(&hub.service),
                        hub.cfg.clone(),
                        Some(resume_fn),
                    )),
                    ..Default::default()
                },
            )) as Arc<dyn Handler>;

            let _hub_bus = start_bus_daemon(
                &hub.bus_dir(),
                "hub-tmpl",
                hub_leaf_port,
                Some((hub_handler, ALL_OPS.to_vec())),
            );
            let leaf_bus = start_bus_daemon(&leaf.bus_dir(), "leaf-tmpl", 0, None);

            let rpc = RpcClient::new(leaf_bus.conn.clone(), DEFAULT_RPC_TIMEOUT);
            wait_until_peer_reachable(&rpc, "hub-tmpl");

            let list_resp = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_LIST.into(),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(list_resp.templates.len(), 1);
            assert_eq!(list_resp.templates[0].name, "builder");
            assert_eq!(list_resp.templates[0].model, "claude-sonnet-4-6");
            assert_eq!(list_resp.templates[0].brief, "docs/lanes/builder.md");
            let list_json = serde_json::to_string(&list_resp).unwrap();
            assert!(!list_json.contains("CONFIDENTIAL_SENTINEL_DO_NOT_LEAK_OVER_LIST"));

            let unknown_err = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_START.into(),
                        template: "nonexistent".into(),
                        from: "coord@leaf-tmpl".into(),
                    },
                )
                .unwrap_err();
            assert!(
                format!("{unknown_err:#}").contains("unknown template"),
                "got: {unknown_err:#}"
            );

            let spoof_err = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_START.into(),
                        template: "builder".into(),
                        from: "coord@hub-tmpl".into(),
                    },
                )
                .unwrap_err();
            assert!(
                format!("{spoof_err:#}").contains("hub-tmpl"),
                "expected same-host from refusal, got: {spoof_err:#}"
            );

            let start_resp = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_START.into(),
                        template: "builder".into(),
                        from: "coord@leaf-tmpl".into(),
                    },
                )
                .unwrap();
            let started = start_resp.agent.expect("started agent info");
            assert_eq!(started.name, "builder");
            assert_eq!(started.backend, "claude");
            assert_eq!(started.spawned_by, "coord@leaf-tmpl");
            assert_eq!(started.cwd, proj.to_string_lossy());
            assert_eq!(
                hub.store
                    .get_state("builder", STATE_KEY_MODEL)
                    .unwrap()
                    .unwrap()
                    .value,
                "claude-sonnet-4-6"
            );

            let dup_err = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_START.into(),
                        template: "builder".into(),
                        from: "coord@leaf-tmpl".into(),
                    },
                )
                .unwrap_err();
            assert!(
                format!("{dup_err:#}").contains("already running"),
                "got: {dup_err:#}"
            );

            let resume_running_err = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_RESUME.into(),
                        template: "builder".into(),
                        from: "coord@leaf-tmpl".into(),
                    },
                )
                .unwrap_err();
            assert!(
                format!("{resume_running_err:#}").contains("already running"),
                "got: {resume_running_err:#}"
            );

            let builder_rec = hub.store.get_agent("builder").unwrap().unwrap();
            hub.mux.kill_session(&builder_rec.session_name).unwrap();

            let ended_at = StoreTime::now();
            hub.store
                .record_report(
                    &[
                        StateEntry {
                            agent_name: "builder".into(),
                            key: "_status".into(),
                            value: AGENT_STATUS_ENDED.to_string(),
                            updated_at: ended_at,
                        },
                        StateEntry {
                            agent_name: "builder".into(),
                            key: "_end_reason".into(),
                            value: "quit".into(),
                            updated_at: ended_at,
                        },
                    ],
                    "builder",
                    &[],
                    Some(&agent_store::AgentEvent {
                        kind: AGENT_STATUS.into(),
                        agent: "builder".into(),
                        at: ended_at,
                        status: AGENT_STATUS_ENDED.to_string(),
                        end_reason: "quit".into(),
                        ..Default::default()
                    }),
                )
                .unwrap();

            let start_ended_err = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_START.into(),
                        template: "builder".into(),
                        from: "coord@leaf-tmpl".into(),
                    },
                )
                .unwrap_err();
            assert!(
                format!("{start_ended_err:#}").contains("resume"),
                "got: {start_ended_err:#}"
            );

            let resumed_resp = rpc
                .template(
                    &ctx,
                    "hub-tmpl",
                    TemplateRequest {
                        action: TEMPLATE_RESUME.into(),
                        template: "builder".into(),
                        from: "coord@leaf-tmpl".into(),
                    },
                )
                .unwrap();
            assert_eq!(resumed_resp.agent.as_ref().unwrap().name, "builder");
            let calls = resume_calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].agent, "builder");
            assert_eq!(calls[0].resumed_by, "coord@leaf-tmpl");

            let _ = hub.service.cleanup("builder");
        },
    );
}

#[test]
fn rpc_handler_populates_cli_session_name_and_flags_mismatch() {
    with_hard_deadline(
        "rpc_handler_populates_cli_session_name_and_flags_mismatch",
        HARD_TEST_DEADLINE,
        || {
            let th = TestHost::new("hub-sname");
            let cwd = th.dir.path().to_string_lossy().into_owned();
            let sess = th.cfg.session_name("worker");
            th.mux.insert_live(&sess, &cwd);

            let claude_proj =
                PathBuf::from(th.cfg.claude_projects_dir.as_deref().unwrap()).join("-srv-worker");
            fs::create_dir_all(&claude_proj).unwrap();
            let tr_path = claude_proj.join("44444444-4444-4444-8444-444444444444.jsonl");
            fs::write(
                &tr_path,
                concat!(
                    r#"{"type":"user","sessionId":"44444444-4444-4444-8444-444444444444","cwd":"/srv/worker","timestamp":"2026-11-20T10:00:00Z","uuid":"u1","message":{"role":"user","content":"hi"}}"#,
                    "\n",
                    r#"{"type":"custom-title","sessionId":"44444444-4444-4444-8444-444444444444","customTitle":"wrong-cli-title"}"#,
                    "\n",
                ),
            )
            .unwrap();

            let now = StoreTime::now();
            th.store
                .create_agent(&Agent {
                    name: "worker".into(),
                    session_name: sess,
                    backend: "claude".into(),
                    created_at: now,
                    cwd,
                    ..Default::default()
                })
                .unwrap();
            th.store
                .record_report(
                    &[
                        StateEntry {
                            agent_name: "worker".into(),
                            key: "_status".into(),
                            value: AGENT_STATUS_IDLE.to_string(),
                            updated_at: now,
                        },
                        StateEntry {
                            agent_name: "worker".into(),
                            key: "_transcript".into(),
                            value: tr_path.to_string_lossy().into_owned(),
                            updated_at: now,
                        },
                    ],
                    "worker",
                    &[],
                    Some(&agent_store::AgentEvent {
                        kind: AGENT_STATUS.into(),
                        agent: "worker".into(),
                        at: now,
                        status: AGENT_STATUS_IDLE.to_string(),
                        ..Default::default()
                    }),
                )
                .unwrap();

            // Invoke `fleet::rpc_handler` directly (no custom `Options.session_name` closure).
            let handler_fn = fleet::rpc_handler(
                Arc::clone(&th.service),
                Arc::clone(&th.store),
                th.cfg.clone(),
            );
            let fh = handler_fn(&Context::background(), FleetRequest { report: false }).unwrap();
            assert_eq!(fh.agents.len(), 1);
            assert_eq!(fh.agents[0].session_name, "wrong-cli-title");
            assert!(
                fh.agents[0]
                    .flags
                    .contains(&r#"name mismatch: session is "wrong-cli-title""#.to_string()),
                "got flags: {:?}",
                fh.agents[0].flags
            );
        },
    );
}

#[test]
fn focus_set_not_found_suggests_close_names_and_session_owner() {
    with_hard_deadline(
        "focus_set_not_found_suggests_close_names_and_session_owner",
        HARD_TEST_DEADLINE,
        || {
            let th = TestHost::new("hub-nf");

            // 1. Empty store -> count == 0 ("it has no agents").
            let empty_err = focus::set(&th.store, None, "hub-nf", "alpha", "user").unwrap_err();
            let nf0 = empty_err.downcast_ref::<AgentNotFoundError>().unwrap();
            assert_eq!(nf0.count, 0);
            assert!(nf0.to_string().contains("it has no agents"));

            // 2. Populate `alpha` and `builder`.
            let cwd = th.dir.path().to_string_lossy().into_owned();
            let alpha_sess = th.cfg.session_name("alpha");
            let builder_sess = th.cfg.session_name("builder");
            for (name, sess) in [("alpha", &alpha_sess), ("builder", &builder_sess)] {
                th.store
                    .create_agent(&Agent {
                        name: name.into(),
                        session_name: sess.clone(),
                        backend: "claude".into(),
                        created_at: StoreTime::now(),
                        cwd: cwd.clone(),
                        ..Default::default()
                    })
                    .unwrap();
            }

            // Close typo suggests `alpha`.
            let typo_err = focus::set(&th.store, None, "hub-nf", "alph", "user").unwrap_err();
            let nf_typo = typo_err.downcast_ref::<AgentNotFoundError>().unwrap();
            assert_eq!(nf_typo.count, 2);
            assert_eq!(nf_typo.similar, vec!["alpha"]);
            assert!(
                nf_typo.to_string().contains("did you mean alpha?"),
                "got: {nf_typo}"
            );

            // Passing the tmux/termd session name identifies `session_of`.
            let sess_err = focus::set(&th.store, None, "hub-nf", &alpha_sess, "user").unwrap_err();
            let nf_sess = sess_err.downcast_ref::<AgentNotFoundError>().unwrap();
            assert_eq!(nf_sess.session_of, "alpha");
            assert!(
                nf_sess.to_string().contains("session of agent alpha"),
                "got: {nf_sess}"
            );

            // Unrelated name still reports the host's agent count.
            let other_err = focus::set(&th.store, None, "hub-nf", "zzzzz", "user").unwrap_err();
            let nf_other = other_err.downcast_ref::<AgentNotFoundError>().unwrap();
            assert_eq!(nf_other.count, 2);
            assert!(
                nf_other.to_string().contains("2 agents there: agent list"),
                "got: {nf_other}"
            );
        },
    );
}

#[test]
fn template_list_marks_broken_entries_while_start_and_resume_refuse_broken_only() {
    with_hard_deadline(
        "template_list_marks_broken_entries_while_start_and_resume_refuse_broken_only",
        HARD_TEST_DEADLINE,
        || {
            let hub = TestHost::new("hub-broken");
            let leaf = TestHost::new("leaf-broken");

            let healthy_dir = hub.dir.path().join("healthy-proj");
            let broken_brief_dir = hub.dir.path().join("broken-brief-proj");
            let gone_cwd_dir = hub.dir.path().join("gone-cwd-proj");
            for d in [&healthy_dir, &broken_brief_dir, &gone_cwd_dir] {
                fs::create_dir_all(d).unwrap();
            }
            let healthy_brief = healthy_dir.join("brief.md");
            let doomed_brief = broken_brief_dir.join("doomed.md");
            fs::write(&healthy_brief, "# Healthy\n").unwrap();
            fs::write(&doomed_brief, "# Doomed\n").unwrap();

            templates::add(
                &hub.data_dir,
                Template::new(
                    "healthy",
                    "claude",
                    "",
                    healthy_dir.to_string_lossy().into_owned(),
                    "brief.md",
                ),
            )
            .unwrap();
            templates::add(
                &hub.data_dir,
                Template::new(
                    "missing-brief",
                    "claude",
                    "",
                    broken_brief_dir.to_string_lossy().into_owned(),
                    "doomed.md",
                ),
            )
            .unwrap();
            templates::add(
                &hub.data_dir,
                Template::new(
                    "missing-cwd",
                    "claude",
                    "",
                    gone_cwd_dir.to_string_lossy().into_owned(),
                    "",
                ),
            )
            .unwrap();

            // Break two of the three templates on disk AFTER they were added.
            fs::remove_file(&doomed_brief).unwrap();
            fs::remove_dir_all(&gone_cwd_dir).unwrap();

            let resume_fn: Arc<template_op::ResumeAgentFn> =
                Arc::new(|_ctx, _req| bail!("resume should not be reached for broken template"));

            let hub_leaf_port = free_local_port();
            let hub_id = init_identity(hub.bus_dir(), "hub-broken").unwrap();
            let leaf_id = init_identity(leaf.bus_dir(), "leaf-broken").unwrap();
            trust_peer(hub.bus_dir(), leaf_id.public_bundle("")).unwrap();
            trust_peer(
                leaf.bus_dir(),
                hub_id.public_bundle(&format!("tls://127.0.0.1:{hub_leaf_port}")),
            )
            .unwrap();

            let hub_handler = Arc::new(new_rpc_handler_with(
                Arc::clone(&hub.service),
                RpcExtras {
                    template: Some(template_op::rpc_handler(
                        Arc::clone(&hub.service),
                        hub.cfg.clone(),
                        Some(resume_fn),
                    )),
                    ..Default::default()
                },
            )) as Arc<dyn Handler>;

            let _hub_bus = start_bus_daemon(
                &hub.bus_dir(),
                "hub-broken",
                hub_leaf_port,
                Some((hub_handler, ALL_OPS.to_vec())),
            );
            let leaf_bus = start_bus_daemon(&leaf.bus_dir(), "leaf-broken", 0, None);
            let rpc = RpcClient::new(leaf_bus.conn.clone(), DEFAULT_RPC_TIMEOUT);
            wait_until_peer_reachable(&rpc, "hub-broken");

            let ctx = Context::background();
            // 1. `list` over the bus succeeds, returning the valid template with
            //    empty `problem` (omitted in JSON) and marking both broken ones.
            let list_resp = rpc
                .template(
                    &ctx,
                    "hub-broken",
                    TemplateRequest {
                        action: TEMPLATE_LIST.into(),
                        ..Default::default()
                    },
                )
                .expect("template list must not fail when an entry is broken on disk");
            assert_eq!(list_resp.templates.len(), 3);
            assert_eq!(list_resp.templates[0].name, "healthy");
            assert!(
                list_resp.templates[0].problem.is_empty(),
                "healthy template had problem: {}",
                list_resp.templates[0].problem
            );
            let healthy_json = serde_json::to_string(&list_resp.templates[0]).unwrap();
            assert!(!healthy_json.contains("problem"));

            assert_eq!(list_resp.templates[1].name, "missing-brief");
            assert!(
                list_resp.templates[1].problem.contains("doomed.md"),
                "got problem: {}",
                list_resp.templates[1].problem
            );

            assert_eq!(list_resp.templates[2].name, "missing-cwd");
            assert!(
                list_resp.templates[2].problem.contains("gone-cwd-proj"),
                "got problem: {}",
                list_resp.templates[2].problem
            );

            // 2. `start` and `resume` of a broken template both refuse, naming the problem.
            for action in [TEMPLATE_START, TEMPLATE_RESUME] {
                let err = rpc
                    .template(
                        &ctx,
                        "hub-broken",
                        TemplateRequest {
                            action: action.into(),
                            template: "missing-brief".into(),
                            from: "coord@leaf-broken".into(),
                        },
                    )
                    .unwrap_err();
                let msg = format!("{err:#}");
                assert!(
                    msg.contains("doomed.md"),
                    "{action} should name missing brief problem, got: {msg}"
                );
            }

            // 3. `start` of the healthy sibling still succeeds despite broken entries in templates.json.
            let started = rpc
                .template(
                    &ctx,
                    "hub-broken",
                    TemplateRequest {
                        action: TEMPLATE_START.into(),
                        template: "healthy".into(),
                        from: "coord@leaf-broken".into(),
                    },
                )
                .expect("healthy sibling template should still start")
                .agent
                .unwrap();
            assert_eq!(started.name, "healthy");
            let _ = hub.service.cleanup("healthy");
        },
    );
}
