use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use agent_base::await_;
use agent_bus::{
    AgentInfo, CallbackSubscription, CaptureRequest, CaptureResponse, Conn, Context, Daemon,
    DaemonConfig, DeliverResponse, FRICTION_RESOLVE, FrictionEntry, FrictionRef, FrictionRequest,
    FrictionResponse, Handler, Identity, KeysRequest, ListRequest, ListResponse, LogsRequest,
    LogsResponse, Msg, Observation, PeerBundle, RespondRequest, ResumeRequest, ResumeResponse,
    SendRequest, SendResponse, SpawnRequest, SpawnResponse, StatusEvent, StatusRequest,
    StatusResponse, Time, WaitOptions, connect_local, init_identity, publish_status, start_daemon,
    subject_rpc, subject_status, trust_peer,
};
use tempfile::TempDir;

/// Returns a base directory on disk (`/var/tmp`), never `/tmp` (rule 9).
pub fn test_temp_root() -> PathBuf {
    std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .filter(|p| p != Path::new("/tmp") && !p.starts_with("/tmp/"))
        .unwrap_or_else(|| PathBuf::from("/var/tmp"))
}

pub fn test_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("agent-bus-test-")
        .tempdir_in(test_temp_root())
        .expect("creating tempdir in /var/tmp")
}

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

pub fn has_go() -> bool {
    Command::new("go")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Builds `busd/` once per test binary and returns `Some(path)` or `None` if `go` is absent.
pub fn busd_binary() -> Option<PathBuf> {
    BUSD.get_or_init(|| {
        if !has_go() {
            eprintln!("skipping test: `go` toolchain not found on PATH to build agent-busd");
            return None;
        }
        let base = option_env!("CARGO_TARGET_TMPDIR")
            .map(PathBuf::from)
            .filter(|p| p != Path::new("/tmp") && !p.starts_with("/tmp/"))
            .unwrap_or_else(test_temp_root);
        let build_dir = base.join("agent-busd-test-bin");
        std::fs::create_dir_all(&build_dir).expect("busd test bin dir");
        sweep_busd(&build_dir);
        let bin = build_dir.join(format!("{BUSD_PREFIX}{}", std::process::id()));
        let busd_src = repo_root().join("busd");
        let out = Command::new("go")
            .arg("build")
            .arg("-trimpath")
            .arg("-o")
            .arg(&bin)
            .arg(".")
            .current_dir(&busd_src)
            .env("TMPDIR", test_temp_root())
            .output()
            .expect("invoking go build for agent-busd");
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
const BUSD_PREFIX: &str = "agent-busd-";

/// Removes this test binary's agent-busd when the process exits.
extern "C" fn remove_busd() {
    if let Some(Some(p)) = BUSD.get() {
        let _ = std::fs::remove_file(p);
    }
}

/// Removes the agent-busd binaries in dir that earlier test processes left
/// (a killed run skips its exit hook): files named BUSD_PREFIX + a pid that
/// no longer runs. A test binary running now keeps its own.
fn sweep_busd(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix(BUSD_PREFIX))
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

pub fn require_busd() -> Option<PathBuf> {
    busd_binary()
}

/// One simulated machine: a bus dir with an identity.
pub struct BusHost {
    pub name: String,
    pub temp: TempDir,
    pub id: Identity,
}

impl BusHost {
    pub fn new(name: &str) -> Self {
        let temp = test_tempdir();
        let id = init_identity(temp.path(), name).unwrap_or_else(|e| panic!("init {name}: {e}"));
        Self {
            name: name.to_string(),
            temp,
            id,
        }
    }

    pub fn dir(&self) -> &Path {
        self.temp.path()
    }

    pub fn start(&self, leaf_listen: &str, remote_ops: Option<Vec<String>>) -> Daemon {
        let bin = require_busd().expect("agent-busd required");
        start_daemon(DaemonConfig {
            dir: self.dir().to_path_buf(),
            host: self.name.clone(),
            client_port: -1,
            leaf_listen: leaf_listen.to_string(),
            remote_ops,
            busd_path: Some(bin),
        })
        .unwrap_or_else(|e| panic!("start {}: {e}", self.name))
    }

    pub fn connect(&self) -> Conn {
        connect_local(self.dir(), &self.name)
            .unwrap_or_else(|e| panic!("connect {}: {e}", self.name))
    }

    pub fn trust(&self, b: PeerBundle) {
        let peer = b.host.clone();
        trust_peer(self.dir(), b).unwrap_or_else(|e| panic!("{} trusting {peer}: {e}", self.name));
    }
}

/// Starts a throwaway single-host bus (replacing Go's `natstest.Start`).
pub struct TestBus {
    pub host: BusHost,
    pub daemon: Daemon,
    pub conn: Conn,
}

impl TestBus {
    pub fn start() -> Option<Self> {
        let _ = require_busd()?;
        let host = BusHost::new("test");
        let daemon = host.start("", None);
        let conn = host.connect();
        Some(Self { host, daemon, conn })
    }

    pub fn connect_another(&self) -> Conn {
        self.host.connect()
    }
}

#[allow(dead_code)]
pub struct LinkedPair {
    pub hub: BusHost,
    pub leaf: BusHost,
    pub hub_daemon: Daemon,
    pub leaf_daemon: Daemon,
    pub hub_conn: Conn,
    pub leaf_conn: Conn,
}

pub fn linked_pair(
    hub_ops: Option<Vec<String>>,
    leaf_ops: Option<Vec<String>>,
) -> Option<LinkedPair> {
    let _ = require_busd()?;
    let hub = BusHost::new("hub");
    let leaf = BusHost::new("leaf");
    hub.trust(leaf.id.public_bundle(""));
    let hub_daemon = hub.start("127.0.0.1:-1", hub_ops);
    leaf.trust(
        hub.id
            .public_bundle(&format!("tls://{}", hub_daemon.leaf_addr())),
    );
    let leaf_daemon = leaf.start("", leaf_ops);
    wait_linked(&hub_daemon, 1);
    let hub_conn = hub.connect();
    let leaf_conn = leaf.connect();
    Some(LinkedPair {
        hub,
        leaf,
        hub_daemon,
        leaf_daemon,
        hub_conn,
        leaf_conn,
    })
}

pub fn wait_linked(d: &Daemon, n: usize) {
    await_::until(&format!("{n} linked peers"), || {
        let have = d.linked_peers();
        (format!("{have} linked peers"), have == n)
    });
}

pub fn wait_log(h: &BusHost, want: &str) {
    await_::until(&format!("{}'s bus.log to show {want:?}", h.name), || {
        let log = std::fs::read_to_string(h.dir().join("bus.log")).unwrap_or_default();
        let ok = log.contains(want);
        (log, ok)
    });
}

pub fn wait_interest(sub: &Conn, pub_conn: &Conn, pub_host: &str) {
    let subj = subject_status(pub_host, "interest-probe");
    let mut probe = sub.subscribe(&subj).expect("subscribe probe");
    sub.flush().expect("flush probe");
    await_::until(&format!("a probe on {subj} to cross"), || {
        let _ = pub_conn.publish(&subj, b"probe".to_vec());
        let _ = pub_conn.flush();
        match probe.next_msg(Duration::from_millis(50)) {
            Ok(_) => ("ok".to_string(), true),
            Err(e) => (e.to_string(), false),
        }
    });
}

pub fn settle_link(hub_conn: &Conn, leaf_conn: &Conn) {
    wait_interest(hub_conn, leaf_conn, "leaf");
    wait_interest(leaf_conn, hub_conn, "hub");
}

pub fn serve(nc: &Conn, host: &str) -> CallbackSubscription {
    answer_all(nc, host, |m| format!("ran {}", m.subject))
}

pub fn serve_secret(nc: &Conn, host: &str, secret: &str) -> CallbackSubscription {
    let s = secret.to_string();
    answer_all(nc, host, move |_| s.clone())
}

pub fn answer_all(
    nc: &Conn,
    host: &str,
    answer: impl Fn(&Msg) -> String + Send + Sync + 'static,
) -> CallbackSubscription {
    let sub = nc
        .subscribe_callback(&subject_rpc(host, "*"), move |m| {
            let reply = answer(&m);
            let _ = m.respond(reply.into_bytes());
        })
        .expect("subscribe wildcard rpc");
    nc.flush().expect("flush wildcard rpc");
    sub
}

pub fn request_once(nc: &Conn, host: &str, op: &str) -> anyhow::Result<String> {
    let msg = nc.request(
        &subject_rpc(host, op),
        b"{}".to_vec(),
        Duration::from_secs(1),
    )?;
    Ok(String::from_utf8_lossy(&msg.data).into_owned())
}

pub fn request(nc: &Conn, host: &str, op: &str) -> anyhow::Result<String> {
    let mut data = String::new();
    let mut last_err: Option<anyhow::Error> = None;
    await_::poll(Duration::from_secs(5), || {
        match nc.request(
            &subject_rpc(host, op),
            b"{}".to_vec(),
            Duration::from_millis(200),
        ) {
            Ok(msg) => {
                data = String::from_utf8_lossy(&msg.data).into_owned();
                last_err = None;
                ("ok".to_string(), true)
            }
            Err(e) => {
                let s = e.to_string();
                last_err = Some(e);
                (s, false)
            }
        }
    });
    match last_err {
        None => Ok(data),
        Some(e) => Err(e),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedReq {
    List(ListRequest),
    Status(StatusRequest),
    Capture(CaptureRequest),
    Send(SendRequest),
    Respond(RespondRequest),
    Deliver(RespondRequest),
    Keys(KeysRequest),
    Spawn(SpawnRequest),
    Friction(String),
    Logs(LogsRequest),
    Resume(ResumeRequest),
}

pub const LAZY_DELAY: Duration = Duration::from_millis(200);

pub struct FakeRpc {
    got: Mutex<Vec<RecordedReq>>,
    slow_started: (Mutex<bool>, Condvar),
}

impl FakeRpc {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            got: Mutex::new(Vec::new()),
            slow_started: (Mutex::new(false), Condvar::new()),
        })
    }

    fn record(&self, r: RecordedReq) {
        self.got.lock().unwrap().push(r);
    }

    pub fn last(&self) -> Option<RecordedReq> {
        self.got.lock().unwrap().last().cloned()
    }

    pub fn wait_slow_started(&self) {
        let (lock, cvar) = &self.slow_started;
        let mut started = lock.lock().unwrap();
        while !*started {
            started = cvar.wait(started).unwrap();
        }
    }

    fn check(&self, ctx: &Context, agent: &str) -> anyhow::Result<()> {
        match agent {
            "ghost" => anyhow::bail!("agent \"ghost\" not found"),
            "slow" => {
                {
                    let (lock, cvar) = &self.slow_started;
                    *lock.lock().unwrap() = true;
                    cvar.notify_all();
                }
                let err = ctx.wait_done();
                Err(err.into())
            }
            "lazy" => {
                if let Some(err) = ctx.sleep_or_done(LAZY_DELAY) {
                    return Err(err.into());
                }
                Ok(())
            }
            "boom" => panic!("handler bug"),
            _ => Ok(()),
        }
    }
}

impl Handler for FakeRpc {
    fn list(&self, _ctx: &Context) -> anyhow::Result<ListResponse> {
        self.record(RecordedReq::List(ListRequest {}));
        Ok(ListResponse {
            agents: vec![AgentInfo {
                name: "worker".to_string(),
                session: "agent-worker".to_string(),
                backend: "claude".to_string(),
                ..Default::default()
            }],
            agent_binary: "/opt/gomux/agent".to_string(),
        })
    }

    fn status(&self, ctx: &Context, req: StatusRequest) -> anyhow::Result<StatusResponse> {
        self.record(RecordedReq::Status(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(StatusResponse {
            status: "waiting".to_string(),
            source: "report".to_string(),
            tool: "Bash".to_string(),
            ..Default::default()
        })
    }

    fn capture(&self, ctx: &Context, req: CaptureRequest) -> anyhow::Result<CaptureResponse> {
        self.record(RecordedReq::Capture(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(CaptureResponse {
            output: format!("screen of {}", req.agent),
        })
    }

    fn send(&self, ctx: &Context, req: SendRequest) -> anyhow::Result<Option<SendResponse>> {
        self.record(RecordedReq::Send(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(Some(SendResponse {
            status: "idle".to_string(),
            typed: 1,
            ..Default::default()
        }))
    }

    fn respond(&self, ctx: &Context, req: RespondRequest) -> anyhow::Result<()> {
        self.record(RecordedReq::Respond(req.clone()));
        self.check(ctx, &req.agent)
    }

    fn deliver(&self, ctx: &Context, req: RespondRequest) -> anyhow::Result<DeliverResponse> {
        self.record(RecordedReq::Deliver(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(DeliverResponse {
            typed: 2,
            ..Default::default()
        })
    }

    fn keys(&self, ctx: &Context, req: KeysRequest) -> anyhow::Result<()> {
        self.record(RecordedReq::Keys(req.clone()));
        self.check(ctx, &req.agent)
    }

    fn spawn(&self, ctx: &Context, req: SpawnRequest) -> anyhow::Result<SpawnResponse> {
        self.record(RecordedReq::Spawn(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(SpawnResponse {
            name: req.agent.clone(),
            session: format!("agent-{}", req.agent),
            backend: req.backend,
            spawned_by: req.spawned_by,
            cwd: req.cwd,
            task_file: req.task_file,
            status: "idle".to_string(),
            ..Default::default()
        })
    }

    fn friction(&self, _ctx: &Context, req: FrictionRequest) -> anyhow::Result<FrictionResponse> {
        self.record(RecordedReq::Friction(req.action.clone()));
        Ok(FrictionResponse {
            entries: vec![FrictionEntry {
                host: "h1".to_string(),
                id: 34,
                uid: "h1-34".to_string(),
                status: "open".to_string(),
                summary: "trust \x1b]0;evil\x07prompt".to_string(),
                links: vec![FrictionRef {
                    host: "laptop".to_string(),
                    id: 4,
                    uid: "laptop-4".to_string(),
                }],
                ..Default::default()
            }],
            missing: vec![35],
            resolved: req.action == FRICTION_RESOLVE,
        })
    }

    fn logs(&self, ctx: &Context, req: LogsRequest) -> anyhow::Result<LogsResponse> {
        self.record(RecordedReq::Logs(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(LogsResponse {
            output: "12:04:31 say    \x1b]0;evil\x07hi\n".to_string(),
            hint: "printing \x1b[2Jterminal log".to_string(),
            truncated: 3,
        })
    }

    fn resume(&self, ctx: &Context, req: ResumeRequest) -> anyhow::Result<ResumeResponse> {
        self.record(RecordedReq::Resume(req.clone()));
        self.check(ctx, &req.agent)?;
        Ok(ResumeResponse {
            name: req.agent.clone(),
            session: format!("agent-{}", req.agent),
            backend: "claude".to_string(),
            session_id: "5f0c".to_string(),
            cwd: "/srv/project".to_string(),
            held: 2,
            status: "idle".to_string(),
            ..Default::default()
        })
    }
}

pub const TEST_HOST: &str = "hosta";

pub fn publish(nc: &Conn, agent: &str, status: &str) {
    publish_status(
        nc,
        &StatusEvent {
            host: TEST_HOST.to_string(),
            agent: agent.to_string(),
            status: status.to_string(),
            at: Time::now(),
            ..Default::default()
        },
    )
    .expect("publish status");
    nc.flush().expect("flush status");
}

struct SourceState {
    status: String,
    calls: usize,
    reported: String,
    reported_at: Time,
    sent_at: Time,
    turn_at: Time,
}

pub struct StatusSource {
    state: Mutex<SourceState>,
    on_first: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl StatusSource {
    pub fn new(status: &str) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(SourceState {
                status: status.to_string(),
                calls: 0,
                reported: String::new(),
                reported_at: Time::zero(),
                sent_at: Time::zero(),
                turn_at: Time::zero(),
            }),
            on_first: Mutex::new(None),
        })
    }

    pub fn set_on_first(&self, f: impl FnOnce() + Send + 'static) {
        *self.on_first.lock().unwrap() = Some(Box::new(f));
    }

    pub fn send(&self) {
        self.state.lock().unwrap().sent_at = Time::now();
    }

    pub fn start_turn(&self) {
        self.record("processing");
        let mut st = self.state.lock().unwrap();
        st.turn_at = st.reported_at;
    }

    pub fn set(&self, status: &str) {
        self.state.lock().unwrap().status = status.to_string();
    }

    pub fn record(&self, status: &str) {
        let mut st = self.state.lock().unwrap();
        st.status = status.to_string();
        st.reported = status.to_string();
        st.reported_at = Time::now();
    }

    pub fn reported_at(&self) -> Time {
        self.state.lock().unwrap().reported_at
    }

    pub fn call_count(&self) -> usize {
        self.state.lock().unwrap().calls
    }

    pub fn await_calls(&self, n: usize) {
        await_::poll(await_::TIMEOUT, || {
            let c = self.call_count();
            (c.to_string(), c >= n)
        });
    }

    pub fn current(&self) -> Observation {
        let (obs, first) = {
            let mut st = self.state.lock().unwrap();
            let obs = Observation {
                status: st.status.clone(),
                detail: String::new(),
                reported_status: st.reported.clone(),
                reported_at: st.reported_at,
                sent_at: st.sent_at,
                turn_at: st.turn_at,
            };
            st.calls += 1;
            (obs, st.calls == 1)
        };
        if first && let Some(cb) = self.on_first.lock().unwrap().take() {
            cb();
        }
        obs
    }
}

pub fn wait_opts<'a>(
    src: &'a Arc<StatusSource>,
    interval: Duration,
    statuses: &[&str],
) -> WaitOptions<'a> {
    let src_cl = Arc::clone(src);
    WaitOptions {
        host: TEST_HOST.to_string(),
        agent: "worker".to_string(),
        for_statuses: statuses.iter().map(|s| (*s).to_string()).collect(),
        fail_statuses: vec!["exited".to_string()],
        next: false,
        after: Time::zero(),
        current: Some(Box::new(move |_| Ok(src_cl.current()))),
        interval,
    }
}
