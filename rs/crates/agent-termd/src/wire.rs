use crate::paths::{
    check_test_server, check_test_socket, daemon_binary, log_path, socket_path, validate_namespace,
};
use agent_base::gostd::strconv::quote;
use anyhow::{Result, anyhow, bail};
pub use g3d_wire::WireError;
use std::ffi::{OsStr, OsString};
use std::io;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const CLIENT_NAME: &str = "gomuxai";

const READ_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(25),
    Duration::from_millis(50),
    Duration::from_millis(100),
];

/// Indicates that the termd socket directory or peer credentials failed
/// ownership, permission, or file-type verification.
#[derive(Debug, thiserror::Error)]
#[error("unsafe termd socket: {0}")]
pub struct UnsafeSocketError(pub String);

/// Indicates that nothing listens on the target termd socket.
#[derive(Debug, thiserror::Error)]
#[error("termd is not running at {0}")]
pub struct NotRunningError(pub String);

/// Go's `fmt.Errorf("<msg>: %w", source)`, as anyhow context: `{:#}` prints
/// `"<msg>: <source>"` (the Go text) once, and `source` stays in the chain for
/// [`is_unsafe_socket`], [`is_not_running`] and [`is_wire_code`].
pub(crate) fn wrap_err(msg: impl Into<String>, source: anyhow::Error) -> anyhow::Error {
    source.context(msg.into())
}

pub(crate) fn client_err_to_anyhow(err: g3d::ClientError) -> anyhow::Error {
    match err {
        g3d::ClientError::Io(e) => anyhow::Error::new(e),
        g3d::ClientError::Wire(we) => anyhow::Error::new(we),
        other @ g3d::ClientError::Protocol(_) => anyhow::Error::new(other),
    }
}

/// How long a request waits for its reply (the connection's read timeout).
const REPLY_TIMEOUT: Duration = Duration::from_secs(30);

fn op_name(op: &g3d_wire::Op) -> &'static str {
    use g3d_wire::Op;
    match op {
        Op::Create(_) => "create",
        Op::List => "list",
        Op::Kill(_) => "kill",
        Op::Subscribe(_) => "subscribe",
        Op::Unsubscribe(_) => "unsubscribe",
        Op::Take(_) => "take",
        Op::Resize(_) => "resize",
        Op::Repaint(_) => "repaint",
        Op::View(_) => "view",
        Op::Capture(_) => "capture",
        Op::Ping => "ping",
        Op::Stop => "stop",
        Op::Upgrade(_) => "upgrade",
        Op::Status => "status",
        Op::StatusFollow(_) => "status_follow",
        Op::StatusUnfollow => "status_unfollow",
        Op::Env(_) => "env",
        Op::Key(_) => "key",
        Op::Paste(_) => "paste",
        Op::Mouse(_) => "mouse",
    }
}

/// Maps a request's error, saying Go's `termd: no reply to <op> in 30s` when
/// the connection's timeout ran out rather than the OS's EAGAIN text.
fn request_err(op: &str, err: g3d::ClientError) -> anyhow::Error {
    if let g3d::ClientError::Io(e) = &err
        && matches!(
            e.kind(),
            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
        )
    {
        return anyhow!("termd: no reply to {op} in {}s", REPLY_TIMEOUT.as_secs());
    }
    client_err_to_anyhow(err)
}

/// Sends `op` on `c` and returns its reply's JSON payload (Go's `Conn.Request`).
pub(crate) fn request_raw(c: &mut g3d::Client, op: g3d_wire::Op) -> Result<Vec<u8>> {
    let name = op_name(&op);
    c.request_raw(op).map_err(|e| request_err(name, e))
}

/// [`request_raw`], decoding the reply into `T`.
pub(crate) fn request<T: serde::de::DeserializeOwned>(
    c: &mut g3d::Client,
    op: g3d_wire::Op,
) -> Result<T> {
    let name = op_name(&op);
    c.request(op).map_err(|e| request_err(name, e))
}

/// Go's `net.DialTimeout("unix", path, 2*time.Second)` failing: its text is
/// `dial unix <path>: connect: <errno text>`, or `dial unix <path>: i/o
/// timeout`. The `io::Error` is kept for its kind but isn't a `source`, so
/// `{:#}` prints the errno once.
#[derive(Debug)]
pub(crate) struct DialError {
    path: PathBuf,
    err: io::Error,
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.err.kind() == io::ErrorKind::TimedOut {
            write!(f, "dial unix {}: i/o timeout", self.path.display())
        } else {
            write!(
                f,
                "dial unix {}: connect: {}",
                self.path.display(),
                agent_base::gostd::os::errno_text(&self.err)
            )
        }
    }
}

impl std::error::Error for DialError {}

const DIAL_TIMEOUT: Duration = Duration::from_secs(2);

/// Connects to the Unix socket at `path`, giving up after `timeout`, as Go's
/// `net.DialTimeout` does: the connect is non-blocking, so a daemon that is
/// alive but not accepting (hung, stopped) can't hang every command. On Linux
/// a full accept queue fails at once with EAGAIN, as in Go; elsewhere an
/// in-progress connect is polled until `timeout`.
pub(crate) fn connect_unix(path: &Path, timeout: Duration) -> io::Result<UnixStream> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    use std::os::fd::AsRawFd;

    let addr = SocketAddrUnix::new(path).map_err(io::Error::from)?;
    let fd = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None)
        .map_err(io::Error::from)?;
    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC).map_err(io::Error::from)?;
    rustix::io::ioctl_fionbio(&fd, true).map_err(io::Error::from)?;

    match rustix::net::connect(&fd, &addr) {
        Ok(()) => {}
        Err(e) if e == rustix::io::Errno::INPROGRESS || e == rustix::io::Errno::INTR => {
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ms = libc::c_int::try_from(left.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
                // SAFETY: `pfd` is one valid pollfd for an fd we own, for the length we pass.
                let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
                if rc < 0 {
                    let e = io::Error::last_os_error();
                    if e.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                if rc > 0 {
                    break;
                }
            }
            if let Err(e) = rustix::net::sockopt::socket_error(&fd).map_err(io::Error::from)? {
                return Err(io::Error::from(e));
            }
        }
        Err(e) => return Err(io::Error::from(e)),
    }

    rustix::io::ioctl_fionbio(&fd, false).map_err(io::Error::from)?;
    Ok(UnixStream::from(fd))
}

/// Reports whether `err` wraps [`UnsafeSocketError`].
pub fn is_unsafe_socket(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<UnsafeSocketError>().is_some())
}

/// Reports whether `err` wraps [`NotRunningError`].
pub fn is_not_running(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<NotRunningError>().is_some())
}

/// Reports whether `err` wraps a [`WireError`] with the given `code`.
pub fn is_wire_code(err: &anyhow::Error, code: &str) -> bool {
    err.chain().any(|e| {
        e.downcast_ref::<WireError>()
            .is_some_and(|w| w.code == code)
            || e.downcast_ref::<g3d::ClientError>()
                .is_some_and(|ce| ce.code() == Some(code))
    })
}

fn is_stripped_daemon_var(name: &str) -> bool {
    name == "G3D_TERMD_SOCKET" || name == "G3D_TERMD_ID" || name.starts_with("G3D_RELAY_")
}

/// Returns `environ` with `G3D_TERMD_SOCKET`, `G3D_TERMD_ID`, and any
/// `G3D_RELAY_*` variables stripped so a spawned `g3d-termd` daemon never
/// inherits terminal handles or relay capability tokens/controls.
pub fn daemon_env<I, S>(environ: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    environ
        .into_iter()
        .filter_map(|kv| {
            let s = kv.as_ref();
            let k = s.split_once('=').map_or(s, |(k, _)| k);
            (!is_stripped_daemon_var(k)).then(|| s.to_owned())
        })
        .collect()
}

pub(crate) fn daemon_env_os() -> Vec<(OsString, OsString)> {
    let mut env: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(k, _)| k.to_str().is_none_or(|name| !is_stripped_daemon_var(name)))
        .collect();
    // Started from a pane of a test's daemon (crate::paths::pane_test_dir),
    // the new daemon goes in the test's private dir too, not the user's.
    if let Some(dir) = crate::paths::pane_test_dir() {
        env.push((OsString::from("G3D_TERMD_DIR"), dir.into_os_string()));
    }
    env
}

fn clean_path(path: &Path) -> PathBuf {
    path.components().collect()
}

/// Opens `dir` with `O_RDONLY|O_DIRECTORY|O_NOFOLLOW|O_CLOEXEC` and verifies
/// via `fstat` on the open descriptor that it is a real directory owned by the
/// current uid with no group/other permission bits. Missing directories return
/// an `io::Error` with `ErrorKind::NotFound`; symlinks, wrong file types,
/// foreign owners, or permissive modes wrap [`UnsafeSocketError`].
pub fn check_socket_dir(dir: &Path) -> Result<()> {
    if dir.as_os_str().is_empty() || dir.to_string_lossy().trim().is_empty() {
        return Err(UnsafeSocketError("empty socket directory".to_owned()).into());
    }
    let clean = clean_path(dir);

    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, fstat, open};
        let fd = match open(
            &clean,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => {
                return Err(io::Error::from_raw_os_error(libc::ENOENT).into());
            }
            Err(e) => {
                return Err(UnsafeSocketError(format!(
                    "open socket directory {}: {}",
                    clean.display(),
                    io::Error::from(e)
                ))
                .into());
            }
        };

        let st = fstat(&fd).map_err(|e| {
            UnsafeSocketError(format!(
                "fstat socket directory {}: {}",
                clean.display(),
                io::Error::from(e)
            ))
        })?;
        let raw_mode = st.st_mode as libc::mode_t;
        if (raw_mode & libc::S_IFMT) != libc::S_IFDIR {
            return Err(UnsafeSocketError(format!(
                "socket directory {} is not a real directory (mode {raw_mode:#o})",
                clean.display()
            ))
            .into());
        }
        let uid = rustix::process::getuid().as_raw();
        if st.st_uid != uid {
            return Err(UnsafeSocketError(format!(
                "socket directory {} is owned by uid {}, want {uid}",
                clean.display(),
                st.st_uid
            ))
            .into());
        }
        let perm = raw_mode & 0o777;
        if perm & 0o077 != 0 {
            return Err(UnsafeSocketError(format!(
                "socket directory {} has unsafe mode {perm:04o} (group/other access must be 0)",
                clean.display()
            ))
            .into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    {
        let _ = clean;
        Err(UnsafeSocketError(format!("termd is unsupported on {}", std::env::consts::OS)).into())
    }
}

/// Verifies kernel peer credentials on `sock` against the current process UID.
pub fn verify_peer_creds(sock: &UnixStream) -> Result<()> {
    let uid = rustix::process::getuid().as_raw();
    verify_peer_creds_for_uid(sock, uid)
}

/// Verifies kernel peer credentials on `sock` against `want_uid`.
pub fn verify_peer_creds_for_uid(sock: &UnixStream, want_uid: u32) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let cred = rustix::net::sockopt::socket_peercred(sock)
            .map_err(|e| UnsafeSocketError(format!("termd peercred: {}", io::Error::from(e))))?;
        let got = cred.uid.as_raw();
        if got != want_uid {
            return Err(
                UnsafeSocketError(format!("socket peer uid is {got}, want {want_uid}")).into(),
            );
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: `sock` is an open connected Unix stream socket fd and `uid`/`gid` are valid out-pointers.
        let rc = unsafe { libc::getpeereid(sock.as_raw_fd(), &mut uid, &mut gid) };
        if rc != 0 {
            return Err(UnsafeSocketError(format!(
                "termd peercred: {}",
                io::Error::last_os_error()
            ))
            .into());
        }
        if uid != want_uid {
            return Err(
                UnsafeSocketError(format!("socket peer uid is {uid}, want {want_uid}")).into(),
            );
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (sock, want_uid);
        Err(UnsafeSocketError(format!("termd is unsupported on {}", std::env::consts::OS)).into())
    }
}

/// Opens `path` with `O_RDONLY|O_NOFOLLOW|O_NONBLOCK|O_CLOEXEC`, verifies on
/// the open descriptor that it is a regular file owned by the current uid, and
/// reads at most the trailing `max_bytes` bytes (default 64 KiB when `0`).
pub fn read_owned_regular_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>> {
    if path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty() {
        return Err(UnsafeSocketError("empty file path".to_owned()).into());
    }
    let clean = clean_path(path);

    #[cfg(unix)]
    {
        use rustix::fs::{Mode, OFlags, SeekFrom, fstat, open, seek};
        let fd = open(
            &clean,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;

        let st = fstat(&fd).map_err(io::Error::from)?;
        let raw_mode = st.st_mode as libc::mode_t;
        if (raw_mode & libc::S_IFMT) != libc::S_IFREG {
            bail!(
                "{} is not a regular file (mode {raw_mode:#o})",
                clean.display()
            );
        }
        let uid = rustix::process::getuid().as_raw();
        if st.st_uid != uid {
            bail!(
                "{} is owned by uid {}, want {uid}",
                clean.display(),
                st.st_uid
            );
        }

        let limit = if max_bytes > 0 { max_bytes } else { 64 << 10 };
        let size = u64::try_from(st.st_size).unwrap_or(0);
        if size > limit as u64 {
            seek(&fd, SeekFrom::Start(size - limit as u64)).map_err(io::Error::from)?;
        }

        let mut file = std::fs::File::from(fd);
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        while out.len() < limit {
            let want = buf.len().min(limit - out.len());
            match std::io::Read::read(&mut file, &mut buf[..want]) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(out)
    }

    #[cfg(not(unix))]
    {
        let _ = (clean, max_bytes);
        Err(UnsafeSocketError(format!("termd is unsupported on {}", std::env::consts::OS)).into())
    }
}

pub(crate) fn dial_socket(path: &Path) -> Result<g3d::Client> {
    if path.as_os_str().is_empty() || path.to_string_lossy().trim().is_empty() {
        return Err(UnsafeSocketError("empty socket path".to_owned()).into());
    }
    // Every connection goes through here (dial_namespace, stop_socket, the
    // daemon start's polls), so a test can't reach a socket outside its
    // private dir whatever it calls.
    check_test_socket(path)?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Err(err) = check_socket_dir(parent) {
        if err.chain().any(|e| {
            e.downcast_ref::<io::Error>()
                .is_some_and(|ioe| ioe.kind() == io::ErrorKind::NotFound)
        }) {
            return Err(NotRunningError(path.display().to_string()).into());
        }
        return Err(wrap_err(format!("termd at {}", path.display()), err));
    }

    let sock = match connect_unix(path, DIAL_TIMEOUT) {
        Ok(s) => s,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(NotRunningError(path.display().to_string()).into());
        }
        Err(e) => {
            return Err(wrap_err(
                format!("termd at {}", path.display()),
                anyhow::Error::new(DialError {
                    path: path.to_path_buf(),
                    err: e,
                }),
            ));
        }
    };

    if let Err(err) = verify_peer_creds(&sock) {
        drop(sock);
        return Err(wrap_err(format!("termd at {}", path.display()), err));
    }

    let _ = sock.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = sock.set_write_timeout(Some(Duration::from_secs(5)));

    let client = match g3d::Client::handshake(sock, CLIENT_NAME) {
        Ok(c) => c,
        Err(g3d::ClientError::Wire(we)) => {
            return Err(wrap_err(
                format!("termd at {} refused us", path.display()),
                anyhow::Error::new(we),
            ));
        }
        Err(e) => {
            return Err(wrap_err(
                format!("termd at {}: handshake", path.display()),
                client_err_to_anyhow(e),
            ));
        }
    };

    let _ = client.set_read_timeout(Some(REPLY_TIMEOUT));
    let _ = client.stream().set_write_timeout(Some(REPLY_TIMEOUT));
    Ok(client)
}

pub(crate) fn dial_namespace(ns: &str) -> Result<g3d::Client> {
    check_test_server()?;
    validate_namespace(ns)?;
    dial_socket(&socket_path(ns))
}

fn is_transient_read_err(err: &anyhow::Error) -> bool {
    if is_not_running(err) || is_unsafe_socket(err) {
        return false;
    }
    for code in ["restart", "restarting", "busy"] {
        if is_wire_code(err, code) {
            return true;
        }
    }
    err.chain().any(|e| {
        e.downcast_ref::<io::Error>().is_some_and(|ioe| {
            matches!(
                ioe.kind(),
                io::ErrorKind::UnexpectedEof
                    | io::ErrorKind::ConnectionReset
                    | io::ErrorKind::BrokenPipe
            )
        })
    })
}

/// Runs a read-only op against `ns`, briefly redialing if the daemon reports a
/// transient brain restart or busy state.
pub(crate) fn with_read_conn<T>(
    ns: &str,
    mut f: impl FnMut(&mut g3d::Client) -> Result<T>,
) -> Result<T> {
    let mut attempt = 0;
    loop {
        let res = dial_namespace(ns).and_then(|mut c| f(&mut c));
        match res {
            Ok(v) => return Ok(v),
            Err(err) => {
                if attempt >= READ_RETRY_DELAYS.len() || !is_transient_read_err(&err) {
                    return Err(err);
                }
                std::thread::sleep(READ_RETRY_DELAYS[attempt]);
                attempt += 1;
            }
        }
    }
}

pub(crate) fn ensure_daemon(ns: &str, daemon_bin: &str) -> Result<g3d::Client> {
    match dial_namespace(ns) {
        Ok(c) => return Ok(c),
        Err(err) if !is_not_running(&err) => return Err(err),
        Err(_) => {}
    }
    let bin = daemon_binary(daemon_bin)?;
    ensure_daemon_with_bin(ns, &bin)
}

pub(crate) fn ensure_daemon_with_bin(ns: &str, bin: &Path) -> Result<g3d::Client> {
    match dial_socket(&socket_path(ns)) {
        Ok(c) => return Ok(c),
        Err(err) if !is_not_running(&err) => return Err(err),
        Err(_) => {}
    }
    if bin.as_os_str().is_empty() || bin.to_string_lossy().trim().is_empty() {
        bail!("termd: empty daemon binary path");
    }

    let spawn_res = Command::new(bin)
        .args(["--ns", ns, "--daemon"])
        .env_clear()
        .envs(daemon_env_os())
        .output();

    let deadline = Instant::now() + Duration::from_secs(3);
    let mut wait = Duration::from_millis(20);
    let last_err = loop {
        match dial_socket(&socket_path(ns)) {
            Ok(c) => return Ok(c),
            Err(err) if is_unsafe_socket(&err) => return Err(err),
            Err(err) if !is_not_running(&err) || Instant::now() > deadline => break err,
            Err(_) => {
                std::thread::sleep(wait);
                if wait < Duration::from_millis(250) {
                    wait *= 2;
                }
            }
        }
    };

    let log = log_path(ns);
    let tail = read_owned_regular_file(&log, 64 << 10)
        .ok()
        .map(|b| {
            let s = String::from_utf8_lossy(&b);
            let lines: Vec<&str> = s.trim_end_matches('\n').split('\n').collect();
            let start = lines.len().saturating_sub(10);
            lines[start..].join("\n")
        })
        .unwrap_or_default();

    // Go: "%v %s" of cmd.CombinedOutput()'s error and its trimmed output.
    let (start_err, out) = match spawn_res {
        Ok(out) => {
            let mut combined = out.stdout;
            combined.extend_from_slice(&out.stderr);
            (
                go_exit_text(out.status),
                String::from_utf8_lossy(&combined).trim().to_owned(),
            )
        }
        Err(e) => (
            format!(
                "fork/exec {}: {}",
                bin.display(),
                agent_base::gostd::os::errno_text(&e)
            ),
            String::new(),
        ),
    };

    Err(anyhow!(
        "termd did not come up for namespace {} ({last_err:#}); {} --ns {ns} --daemon: {start_err} {out}; log {}:\n{tail}",
        quote(ns),
        bin.display(),
        log.display()
    ))
}

/// What Go's `%v` prints for the error `cmd.Run` returns given `status`:
/// `<nil>` for success, `exit status N`, or `signal: <name>`.
pub(crate) fn go_exit_text(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if status.success() {
        return "<nil>".to_owned();
    }
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    match status.signal() {
        Some(sig) => format!("signal: {}", signal_text(sig)),
        None => status.to_string(),
    }
}

/// The C library's name for `sig`, lowercased at the start as Go's signal
/// table writes them ("killed", "terminated", "segmentation fault").
fn signal_text(sig: i32) -> String {
    // SAFETY: strsignal returns a pointer to a NUL-terminated string that stays
    // valid until the next strsignal call on this thread; it is copied at once.
    let ptr = unsafe { libc::strsignal(sig) };
    if ptr.is_null() {
        return format!("signal {sig}");
    }
    // SAFETY: `ptr` is non-null and NUL-terminated (above).
    let text = unsafe { std::ffi::CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned();
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => format!("signal {sig}"),
    }
}

/// Stops a `g3d-termd` daemon listening at `sock_path`, if any.
pub fn stop_socket(sock_path: impl AsRef<Path>) -> Result<()> {
    let mut c = match dial_socket(sock_path.as_ref()) {
        Ok(c) => c,
        Err(err) if is_not_running(&err) => return Ok(()),
        Err(err) => return Err(err),
    };
    request::<g3d_wire::Empty>(&mut c, g3d_wire::Op::Stop)?;
    Ok(())
}

/// Convenience helper for `OsStr` path arguments when Needed.
#[allow(dead_code)]
pub(crate) fn os_str_is_empty(s: &OsStr) -> bool {
    s.is_empty()
}
