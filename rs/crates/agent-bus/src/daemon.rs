//! Starting and managing `agent-busd` as a child process (`internal/natsbus/daemon.go`,
//! `internal/cli/bus_macos.go`).

use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::identity::{PeerBundle, load_peers};

pub const BUS_URL_FILE: &str = "bus.url";
pub const BUS_LOG_FILE: &str = "bus.log";
pub const BUSD_READY_FILE: &str = "busd.ready";
pub const BUSD_LINKS_FILE: &str = "busd.links";
pub const BUSD_ERR_FILE: &str = "busd.err";
pub const DEFAULT_CLIENT_PORT: i32 = 7420;
pub const BUSD_BINARY_NAME: &str = "agent-busd";

/// How long the pipe-less launch (macOS) waits for `agent-busd` to write
/// `busd.ready`. The first launch of a newly installed, newly signed binary on
/// macOS can take seconds of signature checks before it runs at all, so this
/// is generous; a daemon that fails exits, and that is reported at once. It
/// must stay shorter than the wait of `agent bus start` around it
/// (`agent::cli::bus::BUS_START_TIMEOUT`), so that one sees this one's error.
pub const BUSD_READY_TIMEOUT: Duration = Duration::from_secs(25);

/// Marks a disclaimed `agent-busd` child on macOS so it watches its parent
/// process and doesn't re-spawn (`internal/cli/bus_macos.go`).
pub const DISCLAIMED_ENV: &str = "GOMUX_BUS_DISCLAIMED";

/// Note printed on macOS when the bus runs inside `g3d-termd` with a binary
/// that lacks an embedded `Info.plist` (so it cannot hold the Local Network
/// permission) and will dial a peer over the LAN (`internal/cli/bus_macos.go`).
pub const LOCAL_NETWORK_WARNING: &str = "note: macOS blocks LAN links from background daemon processes unless the agent binary carries its own Local Network permission. If linking fails with \"no route to host\", build with `make macos` on this Mac (see the macOS section of README.md).";

const MAX_STDERR_TAIL: usize = 8192;

/// Configures a host's bus daemon.
#[derive(Debug, Clone)]
pub struct DaemonConfig {
    /// Bus dir: identity, peers, runtime files.
    pub dir: PathBuf,
    /// This host's name; must match the identity.
    pub host: String,
    /// Local client port on `127.0.0.1`; `-1` = random (tests).
    pub client_port: i32,
    /// `"addr:port"` to accept peer links on; `""` = not a hub.
    pub leaf_listen: String,
    /// Ops peers may call on this host; `None` = `DEFAULT_REMOTE_OPS`.
    pub remote_ops: Option<Vec<String>>,
    /// Optional explicit path to `agent-busd` (used by tests); `None` searches
    /// beside the running executable, then `PATH`.
    pub busd_path: Option<PathBuf>,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::new(),
            host: String::new(),
            client_port: DEFAULT_CLIENT_PORT,
            leaf_listen: String::new(),
            remote_ops: None,
            busd_path: None,
        }
    }
}

/// Launch mode overrides for [`start_daemon_with`].
#[derive(Debug, Clone, Copy, Default)]
pub struct LaunchOptions {
    /// Forces the pipe-less (`--no-stdio`, `busd.ready` / `busd.links`, stop by
    /// pid) code path even on non-macOS hosts.
    pub force_pipeless: bool,
}

#[derive(Deserialize)]
struct ReadyReply {
    #[serde(default)]
    pid: i32,
    client_url: String,
    #[serde(default)]
    leaf_addr: String,
}

#[derive(Deserialize)]
struct LinksReply {
    linked: usize,
}

struct MonitorState {
    pid: i32,
    exited: Option<String>,
}

struct StderrState {
    tail: String,
    eof: bool,
}

/// Tracks child liveness via a dedicated waiter thread and forwards + retains
/// the tail of child stderr so crashes are immediately visible and inspectable.
struct ProcMonitor {
    proc: Arc<(Mutex<MonitorState>, Condvar)>,
    stderr: Arc<(Mutex<StderrState>, Condvar)>,
}

impl ProcMonitor {
    fn from_std_child(mut child: Child, stderr_pipe: Option<ChildStderr>) -> Self {
        let pid = child.id() as i32;
        let proc = Arc::new((
            Mutex::new(MonitorState { pid, exited: None }),
            Condvar::new(),
        ));
        let stderr = spawn_stderr_collector(stderr_pipe);

        let proc_bg = Arc::clone(&proc);
        std::thread::spawn(move || {
            let desc = match child.wait() {
                Ok(st) => format_std_exit(&st),
                Err(e) => format!("waiting for the bus: {e}"),
            };
            let (lock, cvar) = &*proc_bg;
            let mut st = lock.lock().unwrap();
            st.pid = -1;
            st.exited = Some(desc);
            cvar.notify_all();
        });

        Self { proc, stderr }
    }

    fn from_raw_pid(pid: i32) -> Self {
        let proc = Arc::new((
            Mutex::new(MonitorState { pid, exited: None }),
            Condvar::new(),
        ));
        let stderr = spawn_stderr_collector(None);

        let proc_bg = Arc::clone(&proc);
        std::thread::spawn(move || {
            let desc = wait_raw_pid_blocking(pid);
            let (lock, cvar) = &*proc_bg;
            let mut st = lock.lock().unwrap();
            st.pid = -1;
            st.exited = Some(desc);
            cvar.notify_all();
        });

        Self { proc, stderr }
    }

    fn pid(&self) -> Option<i32> {
        let st = self.proc.0.lock().unwrap();
        (st.exited.is_none() && st.pid > 0).then_some(st.pid)
    }

    fn is_alive(&self) -> bool {
        self.proc.0.lock().unwrap().exited.is_none()
    }

    fn exited_status(&self) -> Option<String> {
        self.proc.0.lock().unwrap().exited.clone()
    }

    fn wait_exited(&self, timeout: Duration) -> Option<String> {
        let (lock, cvar) = &*self.proc;
        let guard = lock.lock().unwrap();
        let (guard, _) = cvar
            .wait_timeout_while(guard, timeout, |st| st.exited.is_none())
            .unwrap();
        guard.exited.clone()
    }

    fn stderr_tail(&self, wait_eof: Duration) -> String {
        let (lock, cvar) = &*self.stderr;
        let guard = lock.lock().unwrap();
        let (guard, _) = cvar
            .wait_timeout_while(guard, wait_eof, |st| !st.eof)
            .unwrap();
        guard.tail.trim().to_string()
    }

    fn terminate_and_wait(&self, timeout: Duration) {
        let (lock, cvar) = &*self.proc;
        {
            let st = lock.lock().unwrap();
            if st.exited.is_some() {
                return;
            }
            if st.pid > 0 {
                // SAFETY: `st.pid` is held under the monitor mutex and only
                // cleared after `wait()`/`waitpid()` reaps the process, so the
                // kernel cannot have recycled the PID yet.
                unsafe {
                    libc::kill(st.pid, libc::SIGTERM);
                }
            }
        }

        let guard = lock.lock().unwrap();
        let (guard, _) = cvar
            .wait_timeout_while(guard, timeout, |st| st.exited.is_none())
            .unwrap();
        if guard.exited.is_none() && guard.pid > 0 {
            // SAFETY: still un-reaped under the monitor mutex after timeout.
            unsafe {
                libc::kill(guard.pid, libc::SIGKILL);
            }
            drop(cvar.wait_while(guard, |st| st.exited.is_none()).unwrap());
        }
    }
}

fn spawn_stderr_collector(stderr_pipe: Option<ChildStderr>) -> Arc<(Mutex<StderrState>, Condvar)> {
    let state = Arc::new((
        Mutex::new(StderrState {
            tail: String::new(),
            eof: stderr_pipe.is_none(),
        }),
        Condvar::new(),
    ));
    let Some(mut pipe) = stderr_pipe else {
        return state;
    };
    let state_bg = Arc::clone(&state);
    std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        while let Ok(n) = pipe.read(&mut buf) {
            if n == 0 {
                break;
            }
            let _ = std::io::stderr().write_all(&buf[..n]);
            let _ = std::io::stderr().flush();
            let chunk = String::from_utf8_lossy(&buf[..n]);
            let (lock, _) = &*state_bg;
            let mut st = lock.lock().unwrap();
            st.tail.push_str(&chunk);
            if st.tail.len() > MAX_STDERR_TAIL {
                let drop_at = st
                    .tail
                    .char_indices()
                    .find(|&(i, _)| st.tail.len() - i <= MAX_STDERR_TAIL)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                st.tail.drain(..drop_at);
            }
        }
        let (lock, cvar) = &*state_bg;
        let mut st = lock.lock().unwrap();
        st.eof = true;
        cvar.notify_all();
    });
    state
}

fn wait_raw_pid_blocking(pid: i32) -> String {
    if pid <= 0 {
        return "exited".to_string();
    }
    loop {
        let mut status: libc::c_int = 0;
        // SAFETY: `pid` is our direct child process from `spawn_disclaimed`.
        let rc = unsafe { libc::waitpid(pid, &mut status, 0) };
        if rc == pid {
            return format_raw_wait_status(status);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::ECHILD) => return "exited".to_string(),
            _ => return format!("waiting for the bus: {err}"),
        }
    }
}

fn format_std_exit(st: &std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(sig) = st.signal() {
        format!("bus killed by signal {sig}")
    } else if let Some(code) = st.code() {
        format!("bus exited with status {code}")
    } else {
        agent_base::gostd::os::exit_status_text(st)
    }
}

fn format_raw_wait_status(status: libc::c_int) -> String {
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        format!("bus killed by signal {sig}")
    } else if libc::WIFEXITED(status) {
        let code = libc::WEXITSTATUS(status);
        format!("bus exited with status {code}")
    } else {
        format!("bus exited ({status})")
    }
}

enum DaemonIo {
    Piped {
        stdin: Option<ChildStdin>,
        stdout: BufReader<ChildStdout>,
    },
    Pipeless,
}

/// A running `agent-busd` child process.
pub struct Daemon {
    dir: PathBuf,
    client_url: String,
    leaf_addr: String,
    disclaimed: bool,
    monitor: ProcMonitor,
    io: Mutex<Option<DaemonIo>>,
}

/// Locates `agent-busd`: beside the running executable (checking both the path
/// as invoked and with symlinks resolved), else on absolute entries of `PATH`.
/// Relative and empty `PATH` entries (`.` or `::`) are skipped so `find_busd`
/// never executes an untrusted `./agent-busd` from the working directory.
pub fn find_busd() -> anyhow::Result<PathBuf> {
    let exe = std::env::current_exe().ok();
    let path_env = std::env::var_os("PATH");
    find_busd_with(exe.as_deref(), path_env.as_deref())
}

pub fn find_busd_with(exe: Option<&Path>, path_env: Option<&OsStr>) -> anyhow::Result<PathBuf> {
    if let Some(exe) = exe {
        if exe.is_absolute()
            && let Some(parent) = exe.parent()
        {
            let candidate = parent.join(BUSD_BINARY_NAME);
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
        if let Ok(real_exe) = std::fs::canonicalize(exe)
            && let Some(parent) = real_exe.parent()
        {
            let candidate = parent.join(BUSD_BINARY_NAME);
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    if let Some(paths) = path_env {
        for dir in std::env::split_paths(paths) {
            if dir.as_os_str().is_empty() || !dir.is_absolute() {
                continue;
            }
            let candidate = dir.join(BUSD_BINARY_NAME);
            if is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    anyhow::bail!("cannot find {BUSD_BINARY_NAME} beside the agent binary or on PATH")
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && (m.permissions().mode() & 0o111) != 0)
        .unwrap_or(false)
}

/// Returns Go's macOS Local Network warning (`internal/cli/bus_macos.go`) when
/// a binary without an embedded `Info.plist` runs inside `g3d-termd`, isn't a
/// hub (`leaf_listen` is empty), hasn't been disclaimed, and trusts a peer with
/// a `url` it will dial over the network.
pub fn local_network_warning(
    has_info_plist: bool,
    in_termd: bool,
    disclaimed_env_set: bool,
    leaf_listen: &str,
    peers: &[PeerBundle],
) -> Option<&'static str> {
    if has_info_plist || !in_termd || disclaimed_env_set || !leaf_listen.is_empty() {
        return None;
    }
    peers
        .iter()
        .any(|p| !p.url.is_empty())
        .then_some(LOCAL_NETWORK_WARNING)
}

/// Starts `agent-busd` and waits until it is ready. On macOS, uses the
/// pipe-less path (spawning disclaimed via `agent_base::macos::spawn_disclaimed`
/// when `agent-busd` embeds an `Info.plist`). Stop it with [`Daemon::shutdown`]
/// (also called on drop).
pub fn start_daemon(cfg: DaemonConfig) -> anyhow::Result<Daemon> {
    start_daemon_with(cfg, LaunchOptions::default())
}

/// Starts `agent-busd` forcing the pipe-less (`--no-stdio`, `busd.ready` /
/// `busd.links`, stop by pid) path on any platform.
pub fn start_daemon_pipeless(cfg: DaemonConfig) -> anyhow::Result<Daemon> {
    start_daemon_with(
        cfg,
        LaunchOptions {
            force_pipeless: true,
        },
    )
}

/// Starts `agent-busd` with explicit [`LaunchOptions`].
pub fn start_daemon_with(cfg: DaemonConfig, opts: LaunchOptions) -> anyhow::Result<Daemon> {
    let bin = match &cfg.busd_path {
        Some(p) => p.clone(),
        None => find_busd()?,
    };

    if cfg!(target_os = "macos") || opts.force_pipeless {
        start_pipeless(cfg, &bin)
    } else {
        start_piped(cfg, &bin)
    }
}

fn busd_args(cfg: &DaemonConfig, no_stdio: bool) -> Vec<String> {
    let mut args = vec![
        "--dir".to_string(),
        cfg.dir.to_string_lossy().into_owned(),
        "--host".to_string(),
        cfg.host.clone(),
        "--client-port".to_string(),
        cfg.client_port.to_string(),
        "--parent-pid".to_string(),
        std::process::id().to_string(),
    ];
    if !cfg.leaf_listen.is_empty() {
        args.push("--listen".to_string());
        args.push(cfg.leaf_listen.clone());
    }
    if let Some(ref ops) = cfg.remote_ops {
        args.push("--remote-ops".to_string());
        args.push(ops.join(","));
    }
    if no_stdio {
        args.push("--no-stdio".to_string());
    }
    args
}

fn clean_pipeless_files(dir: &Path) {
    for name in [BUSD_READY_FILE, BUSD_LINKS_FILE, BUSD_ERR_FILE] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

fn last_bus_log_line(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(BUS_LOG_FILE)).ok()?;
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(ToOwned::to_owned)
}

fn start_pipeless(cfg: DaemonConfig, bin: &Path) -> anyhow::Result<Daemon> {
    clean_pipeless_files(&cfg.dir);
    let args = busd_args(&cfg, true);
    let bin_str = bin.to_string_lossy();

    let already_disclaimed = std::env::var_os(DISCLAIMED_ENV).is_some_and(|v| !v.is_empty());
    let has_plist = agent_base::macos::has_info_plist(&bin_str);

    let (monitor, disclaimed) = if agent_base::macos::SUPPORTED && has_plist && !already_disclaimed
    {
        let mut argv = Vec::with_capacity(args.len() + 1);
        argv.push(bin_str.as_ref().to_string());
        argv.extend(args);

        let mut env_pairs: Vec<String> = std::env::vars()
            .filter(|(k, _)| k != DISCLAIMED_ENV)
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        env_pairs.push(format!("{DISCLAIMED_ENV}=1"));

        let pid = agent_base::macos::spawn_disclaimed(&bin_str, &argv, &env_pairs)?;
        (ProcMonitor::from_raw_pid(pid), true)
    } else {
        let mut cmd = Command::new(bin);
        cmd.args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("starting {}: {e}", bin.display()))?;
        let stderr = child.stderr.take();
        (ProcMonitor::from_std_child(child, stderr), false)
    };

    let expected_pid = monitor.pid().unwrap_or(0);
    let ready_path = cfg.dir.join(BUSD_READY_FILE);
    let err_path = cfg.dir.join(BUSD_ERR_FILE);
    let deadline = Instant::now() + BUSD_READY_TIMEOUT;

    let ready: ReadyReply = loop {
        if let Ok(bytes) = std::fs::read(&ready_path)
            && let Ok(r) = serde_json::from_slice::<ReadyReply>(&bytes)
            && !r.client_url.is_empty()
            && (r.pid == 0 || expected_pid <= 0 || r.pid == expected_pid)
        {
            break r;
        }

        if let Some(exit_desc) = monitor.exited_status() {
            if let Ok(err_bytes) = std::fs::read(&err_path) {
                let _ = std::fs::remove_file(&err_path);
                let msg = String::from_utf8_lossy(&err_bytes).trim().to_string();
                if !msg.is_empty() {
                    anyhow::bail!("{msg}");
                }
            }
            let err_tail = monitor.stderr_tail(Duration::from_millis(200));
            if !err_tail.is_empty() {
                anyhow::bail!("{err_tail}");
            }
            if let Some(log_line) = last_bus_log_line(&cfg.dir) {
                anyhow::bail!("{exit_desc}: {log_line}");
            }
            anyhow::bail!("{exit_desc}");
        }

        if Instant::now() >= deadline {
            monitor.terminate_and_wait(Duration::from_secs(1));
            if let Ok(err_bytes) = std::fs::read(&err_path) {
                let _ = std::fs::remove_file(&err_path);
                let msg = String::from_utf8_lossy(&err_bytes).trim().to_string();
                if !msg.is_empty() {
                    anyhow::bail!("{msg}");
                }
            }
            anyhow::bail!(
                "{} did not write {} within {}s (see {})",
                bin.display(),
                BUSD_READY_FILE,
                BUSD_READY_TIMEOUT.as_secs(),
                cfg.dir.join(BUS_LOG_FILE).display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    if cfg!(target_os = "macos") {
        let in_termd = std::env::var_os("G3D_TERMD_ID").is_some_and(|v| !v.is_empty());
        let peers = load_peers(&cfg.dir).unwrap_or_default();
        if let Some(note) = local_network_warning(
            has_plist,
            in_termd,
            already_disclaimed || disclaimed,
            &cfg.leaf_listen,
            &peers,
        ) {
            eprintln!("{note}");
        }
    }

    Ok(Daemon {
        dir: cfg.dir,
        client_url: ready.client_url,
        leaf_addr: ready.leaf_addr,
        disclaimed,
        monitor,
        io: Mutex::new(Some(DaemonIo::Pipeless)),
    })
}

fn start_piped(cfg: DaemonConfig, bin: &Path) -> anyhow::Result<Daemon> {
    let args = busd_args(&cfg, false);
    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("starting {}: {e}", bin.display()))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take();
    let monitor = ProcMonitor::from_std_child(child, stderr);

    let mut reader = BufReader::new(stdout);
    let mut ready_line = String::new();
    match reader.read_line(&mut ready_line) {
        Ok(0) => {
            let exit_desc = monitor
                .wait_exited(Duration::from_secs(2))
                .unwrap_or_else(|| "exited".to_string());
            let msg = monitor.stderr_tail(Duration::from_millis(500));
            if !msg.is_empty() {
                anyhow::bail!("{msg}");
            }
            if let Some(log_line) = last_bus_log_line(&cfg.dir) {
                anyhow::bail!(
                    "{} exited without ready line ({exit_desc}): {log_line}",
                    bin.display()
                );
            }
            anyhow::bail!("{} exited without ready line ({exit_desc})", bin.display());
        }
        Ok(_) => {}
        Err(e) => {
            drop(stdin);
            monitor.terminate_and_wait(Duration::from_secs(1));
            anyhow::bail!("reading {} ready line: {e}", BUSD_BINARY_NAME);
        }
    }

    let ready: ReadyReply = match serde_json::from_str(ready_line.trim()) {
        Ok(r) => r,
        Err(e) => {
            drop(stdin);
            monitor.terminate_and_wait(Duration::from_secs(1));
            anyhow::bail!(
                "malformed {} ready line {:?}: {e}",
                BUSD_BINARY_NAME,
                ready_line.trim()
            );
        }
    };

    Ok(Daemon {
        dir: cfg.dir,
        client_url: ready.client_url,
        leaf_addr: ready.leaf_addr,
        disclaimed: false,
        monitor,
        io: Mutex::new(Some(DaemonIo::Piped {
            stdin: Some(stdin),
            stdout: reader,
        })),
    })
}

/// Stops an `agent-busd` process by PID (`SIGTERM`, waiting up to 5s, falling
/// back to `SIGKILL`) and cleans up its runtime files in `dir`.
pub fn stop_pid(dir: &Path, pid: i32) {
    if pid > 0 {
        // SAFETY: sending `SIGTERM` to a positive process id.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !pid_alive_or_unreaped(pid) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        if pid_alive_or_unreaped(pid) {
            // SAFETY: process did not exit within 5s of `SIGTERM`.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
                let mut status: libc::c_int = 0;
                let _ = libc::waitpid(pid, &mut status, libc::WNOHANG);
            }
        }
    }
    remove_runtime_files(dir);
}

fn pid_alive_or_unreaped(pid: i32) -> bool {
    let mut status: libc::c_int = 0;
    // SAFETY: if `pid` is our child, `waitpid(..., WNOHANG)` reaps it once it
    // exits; if not (`ECHILD` — e.g. already reaped by `ProcMonitor`), `kill(pid, 0)`
    // probes liveness.
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == pid {
        return false;
    }
    if rc == 0 {
        return true;
    }
    let krc = unsafe { libc::kill(pid, 0) };
    if krc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

fn remove_runtime_files(dir: &Path) {
    for name in [BUS_URL_FILE, BUSD_READY_FILE, BUSD_LINKS_FILE] {
        let _ = std::fs::remove_file(dir.join(name));
    }
}

impl Daemon {
    /// Where local clients connect (`tls://127.0.0.1:<port>`).
    pub fn client_url(&self) -> &str {
        &self.client_url
    }

    /// Where the hub accepts peer links (`"host:port"`), or `""` if not a hub.
    pub fn leaf_addr(&self) -> &str {
        &self.leaf_addr
    }

    /// Whether `agent-busd` was spawned as a disclaimed responsible process on macOS.
    pub fn disclaimed(&self) -> bool {
        self.disclaimed
    }

    /// The running `agent-busd` child's PID, or `None` if already stopped or exited.
    pub fn pid(&self) -> Option<i32> {
        self.monitor.pid()
    }

    /// Reports whether the `agent-busd` child process is still running.
    pub fn is_alive(&self) -> bool {
        self.monitor.is_alive()
    }

    /// Returns `Ok(())` while `agent-busd` is running, or an error describing
    /// its exit status and captured stderr output once it has died.
    pub fn check_alive(&self) -> anyhow::Result<()> {
        let Some(exit_desc) = self.monitor.exited_status() else {
            return Ok(());
        };
        let tail = self.monitor.stderr_tail(Duration::from_millis(100));
        if tail.is_empty() {
            anyhow::bail!("{exit_desc}");
        }
        anyhow::bail!("{exit_desc}: {tail}");
    }

    /// Captured tail of `agent-busd`'s stderr (also forwarded live to parent stderr).
    pub fn stderr_output(&self) -> String {
        self.monitor.stderr_tail(Duration::ZERO)
    }

    /// Queries `agent-busd` for the number of currently established peer links.
    pub fn linked_peers(&self) -> usize {
        if !self.monitor.is_alive() {
            return 0;
        }
        let mut guard = self.io.lock().unwrap();
        let Some(io) = guard.as_mut() else {
            return 0;
        };
        match io {
            DaemonIo::Piped { stdin, stdout } => {
                let Some(stdin) = stdin.as_mut() else {
                    return 0;
                };
                if stdin.write_all(b"links\n").is_err() || stdin.flush().is_err() {
                    return 0;
                }
                let mut line = String::new();
                if stdout.read_line(&mut line).unwrap_or(0) == 0 {
                    return 0;
                }
                serde_json::from_str::<LinksReply>(line.trim())
                    .map(|r| r.linked)
                    .unwrap_or(0)
            }
            DaemonIo::Pipeless => std::fs::read(self.dir.join(BUSD_LINKS_FILE))
                .ok()
                .and_then(|b| serde_json::from_slice::<LinksReply>(&b).ok())
                .map(|r| r.linked)
                .unwrap_or(0),
        }
    }

    /// Stops `agent-busd` with `SIGTERM` by PID and waits for it to clean up
    /// its runtime files and exit. Idempotent.
    pub fn shutdown(&self) {
        let Some(mut io) = self.io.lock().unwrap().take() else {
            return;
        };
        if let DaemonIo::Piped { stdin, .. } = &mut io {
            drop(stdin.take());
        }
        self.monitor.terminate_and_wait(Duration::from_secs(5));
        remove_runtime_files(&self.dir);
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.shutdown();
    }
}
