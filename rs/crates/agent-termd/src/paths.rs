use crate::client::TEST_SERVER_ENV;
use agent_base::gostd::strconv::quote;
use anyhow::{Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Common installation locations checked when `PATH` omits package-manager or
/// user-local bin directories (e.g. non-login ssh shells).
const FALLBACK_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/home/linuxbrew/.linuxbrew/bin",
    "/run/current-system/sw/bin",
    "/nix/var/nix/profiles/default/bin",
];

/// Checks that `ns` is 1..=64 bytes of `[A-Za-z0-9._-]` and is neither
/// `"default"` nor `"wall"`, which belong to interactive and wall sessions.
pub fn validate_namespace(ns: &str) -> Result<()> {
    agent_base::config::validate_termd_namespace(ns)
}

/// Checks that `id` is 1..=64 bytes of `[A-Za-z0-9._-]`.
pub fn validate_terminal_id(id: &str) -> Result<()> {
    agent_base::config::validate_termd_name("terminal id", id)
}

/// Reports whether this process is a Rust test binary, so that
/// [`check_test_server`] keeps it off the user's real termd sockets.
///
/// Go keys its guard on `testing.Testing()`, true in every Go test binary. Rust
/// has no runtime equivalent, and `cfg(test)` is set only while compiling the
/// crate under test, so a test in another crate (agent-service, agent) that
/// links this one would get no guard at all. A cargo feature can't stand in
/// either: dev-dependency features are unified into the crate's own `bin`
/// when cargo builds it for its tests, so `target/debug/agent` would carry the
/// guard and refuse real work.
///
/// So the decision is made at run time from where the executable lives. Cargo
/// (and nextest, which runs the same files) puts every unit- and
/// integration-test executable at `<target>/[<triple>/]<profile>/deps/
/// <crate>-<16 hex digit hash>`, and runs it from there. A binary meant to be
/// run (`agent`) is copied ("uplifted") to `<profile>/agent`, which is what
/// `cargo run`, `CARGO_BIN_EXE_agent` and `cargo install` use; an installed
/// copy lives in a bin directory under its own name. Neither is ever a
/// hash-suffixed file in a `deps` directory beside cargo's `.fingerprint`, so a
/// real `agent` is never affected. Cargo's environment variables are not used:
/// `cargo run` sets the same `CARGO_MANIFEST_DIR` and friends as `cargo test`,
/// and children inherit them. Doc tests are not covered (rustdoc builds them in
/// a temp dir): keep termd out of doc examples, or mark them `no_run`.
///
/// If a cargo release ever moves test executables, this crate's
/// `test_is_cargo_test_exe` (which checks its own executable) fails first.
pub fn is_test_binary() -> bool {
    static IS_TEST: OnceLock<bool> = OnceLock::new();
    cfg!(test)
        || *IS_TEST.get_or_init(|| std::env::current_exe().is_ok_and(|exe| is_cargo_test_exe(&exe)))
}

/// Reports whether `exe` is laid out as a cargo test executable:
/// `<profile dir>/deps/<name>-<16 lowercase hex>` with `<profile
/// dir>/.fingerprint` beside `deps`. See [`is_test_binary`].
pub fn is_cargo_test_exe(exe: &Path) -> bool {
    let Some(name) = exe.file_name().and_then(OsStr::to_str) else {
        return false;
    };
    // Linux's /proc/self/exe names a file rebuilt while it runs "<path> (deleted)".
    let name = name.strip_suffix(" (deleted)").unwrap_or(name);
    let name = name.strip_suffix(".exe").unwrap_or(name);
    let Some((stem, hash)) = name.rsplit_once('-') else {
        return false;
    };
    if stem.is_empty()
        || hash.len() != 16
        || !hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return false;
    }
    let Some(deps) = exe.parent() else {
        return false;
    };
    if deps.file_name() != Some(OsStr::new("deps")) {
        return false;
    }
    deps.parent()
        .is_some_and(|profile| profile.join(".fingerprint").is_dir())
}

/// Refuses to touch the user's real termd sockets from a test binary unless
/// [`crate::testing`] configured a private `G3D_TERMD_DIR`.
///
/// It applies in every Rust test binary ([`is_test_binary`]), and in any
/// process that inherited `GOMUXAI_TEST_TERMD_DIR` from one (an `agent` a test
/// runs), where it checks that the pair still points at the private dir.
pub fn check_test_server() -> Result<()> {
    check_test_server_for(is_test_binary())
}

/// The private socket dir a test set up, `None` outside tests (and outside
/// processes a test started), or an error if a test has none or pointed it at
/// the user's real socket directory.
fn test_private_dir(test_binary: bool) -> Result<Option<String>> {
    let dir = std::env::var(TEST_SERVER_ENV).unwrap_or_default();
    if !test_binary && dir.is_empty() {
        return Ok(None);
    }
    if dir.is_empty() {
        bail!(
            "a test tried to use termd without a private socket directory: \
             call agent_termd::testing::init(), require() or setup() first \
             (enable agent-termd's \"testing\" feature in [dev-dependencies])"
        );
    }
    if test_binary && Path::new(&dir) == live_socket_dir() {
        bail!(
            "a test pointed its private termd dir at the user's real socket directory {}",
            quote(&dir)
        );
    }
    Ok(Some(dir))
}

fn check_test_server_for(test_binary: bool) -> Result<()> {
    let Some(dir) = test_private_dir(test_binary)? else {
        return Ok(());
    };
    let sock = std::env::var("G3D_TERMD_SOCKET").unwrap_or_default();
    // A pane of the test's own daemon gives what runs in it G3D_TERMD_SOCKET,
    // a socket in the private dir, and no G3D_TERMD_DIR: an `agent` a test
    // runs there (the bus's `agent bus run`, a hook) is still the test's.
    let in_dir = !sock.is_empty() && Path::new(&sock).parent() == Some(Path::new(&dir));
    if !sock.is_empty() && !in_dir {
        bail!(
            "a test ran termd with $G3D_TERMD_SOCKET set ({sock}): \
             unset G3D_TERMD_SOCKET in tests using termd"
        );
    }
    let g3d_dir = std::env::var("G3D_TERMD_DIR").unwrap_or_default();
    if g3d_dir.is_empty() && in_dir {
        // socket_dir() is the private dir then (pane_test_dir).
        return Ok(());
    }
    if g3d_dir != dir {
        bail!(
            "a test ran termd with G3D_TERMD_DIR {}, not its private dir {}",
            quote(&g3d_dir),
            quote(&dir)
        );
    }
    Ok(())
}

/// The socket directory the user's own termd daemons use: `socket_dir()`
/// without the `G3D_TERMD_DIR` override.
fn live_socket_dir() -> PathBuf {
    g3d::paths::socket_dir_for(
        g3d::paths::user_runtime_dir(),
        rustix::process::getuid().as_raw(),
    )
}

/// Checks a connection to `sock` (every one goes through here): in a test, or
/// a process a test started, `sock` must be in the private dir the test set
/// up, so [`crate::stop_socket`] and the daemon start can't reach any other
/// socket either. [`check_test_server`]'s environment checks stay with
/// `Client::new` and the namespace dial, as in Go (`termdwire` has none).
pub(crate) fn check_test_socket(sock: &Path) -> Result<()> {
    let Some(dir) = test_private_dir(is_test_binary())? else {
        return Ok(());
    };
    let parent = sock.parent().unwrap_or_else(|| Path::new(""));
    if parent != Path::new(&dir) {
        bail!(
            "a test tried to use termd at {}, outside its private socket directory {}",
            sock.display(),
            Path::new(&dir).display()
        );
    }
    Ok(())
}

/// The private socket dir of a test, for a process running in a pane of that
/// test's own daemon: `GOMUXAI_TEST_TERMD_DIR` is set, `G3D_TERMD_DIR` isn't
/// (a daemon gives its terminals `G3D_TERMD_SOCKET` instead), and
/// `G3D_TERMD_SOCKET` is a socket in the private dir. `None` anywhere else,
/// where `G3D_TERMD_DIR` (or the user's own dir) decides as usual.
pub(crate) fn pane_test_dir() -> Option<PathBuf> {
    if std::env::var_os("G3D_TERMD_DIR").is_some_and(|v| !v.is_empty()) {
        return None;
    }
    let dir = std::env::var_os(TEST_SERVER_ENV).filter(|v| !v.is_empty())?;
    let sock = std::env::var_os("G3D_TERMD_SOCKET").filter(|v| !v.is_empty())?;
    (Path::new(&sock).parent() == Some(Path::new(&dir))).then(|| PathBuf::from(dir))
}

/// Returns the directory where `g3d-termd` sockets live.
pub fn socket_dir() -> PathBuf {
    pane_test_dir().unwrap_or_else(g3d::paths::socket_dir)
}

/// Returns the Unix socket path for namespace `ns`.
pub fn socket_path(ns: &str) -> PathBuf {
    match pane_test_dir() {
        Some(dir) => dir.join(format!("termd-{ns}.sock")),
        None => g3d::paths::socket_path(ns),
    }
}

/// Returns the daemon log path for namespace `ns`.
pub fn log_path(ns: &str) -> PathBuf {
    match pane_test_dir() {
        Some(dir) => dir.join(format!("termd-{ns}.log")),
        None => g3d::paths::log_path(ns),
    }
}

/// Locates `g3d-termd`: `explicit`, `$G3D_TERMD_BIN`, beside the running
/// executable, `PATH`, or common install directories.
pub fn daemon_binary(explicit: &str) -> Result<PathBuf> {
    if !explicit.is_empty() {
        let p = PathBuf::from(explicit);
        if is_executable(&p) {
            return Ok(p);
        }
        bail!("termd binary {explicit}: not an executable file");
    }
    if let Some(env_bin) = std::env::var_os("G3D_TERMD_BIN").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(&env_bin);
        if is_executable(&p) {
            return Ok(p);
        }
        bail!("G3D_TERMD_BIN={}: not an executable file", p.display());
    }
    find_tool("g3d-termd", None)
}

/// Locates `g3d`: `explicit`, `$G3D_BIN`, beside `daemon_bin`, beside the
/// running executable, `PATH`, or common install directories.
pub fn attach_binary(explicit: &str, daemon_bin: &str) -> Result<PathBuf> {
    if !explicit.is_empty() {
        let p = PathBuf::from(explicit);
        if is_executable(&p) {
            return Ok(p);
        }
        bail!("g3d binary {explicit}: not an executable file");
    }
    if let Some(env_bin) = std::env::var_os("G3D_BIN").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(&env_bin);
        if is_executable(&p) {
            return Ok(p);
        }
        bail!("G3D_BIN={}: not an executable file", p.display());
    }
    let sibling_dir = if !daemon_bin.is_empty() {
        Path::new(daemon_bin).parent().map(Path::to_path_buf)
    } else {
        daemon_binary("")
            .ok()
            .and_then(|d| d.parent().map(Path::to_path_buf))
    };
    find_tool("g3d", sibling_dir.as_deref())
}

fn find_tool(name: &str, extra_dir: Option<&Path>) -> Result<PathBuf> {
    let mut looked: Vec<String> = Vec::new();
    let mut check_dir = |dir: &Path| -> Option<PathBuf> {
        if dir.as_os_str().is_empty() {
            return None;
        }
        let p = dir.join(name);
        looked.push(p.display().to_string());
        is_executable(&p).then_some(p)
    };

    if let Some(dir) = extra_dir
        && let Some(p) = check_dir(dir)
    {
        return Ok(p);
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        && let Some(p) = check_dir(dir)
    {
        return Ok(p);
    }
    if let Some(p) = look_path(name) {
        return Ok(p);
    }
    looked.push("$PATH".to_owned());

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
    }
    dirs.extend(FALLBACK_DIRS.iter().map(PathBuf::from));

    for dir in &dirs {
        let p = dir.join(name);
        if is_executable(&p) {
            return Ok(p);
        }
    }

    let dirs_str = dirs
        .iter()
        .map(|d| d.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no {name} binary found (looked at {} and {dirs_str})",
        looked.join(", ")
    )
}

/// Go's `exec.LookPath(name)` for a bare name, as `findTool` uses it: the
/// first `PATH` entry (an empty one meaning `.`) holding an executable `name`
/// decides. When that entry is relative, Go returns `exec.ErrDot` and
/// `findTool` treats it as not found, so a `g3d-termd` in the current
/// directory is never run as the user's daemon: the search goes on to the
/// install directories, not to later `PATH` entries.
fn look_path(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH").filter(|v| !v.is_empty())?;
    for dir in std::env::split_paths(&path_var) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let p = dir.join(name);
        if is_executable(&p) {
            return p.is_absolute().then_some(p);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
    }
    #[cfg(not(unix))]
    {
        std::fs::metadata(path).is_ok_and(|m| m.is_file())
    }
}
