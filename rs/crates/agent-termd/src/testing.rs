//! Isolates test binaries in a private `0700` `g3d-termd` socket directory
//! under `/var/tmp` and stops every daemon started in it when tests finish.
//!
//! Port of Go `internal/termdtest`, adapted for Rust's parallel test runner:
//! - [`require`] (and [`init`]) sets up a process-wide private `G3D_TERMD_DIR`
//!   once via `OnceLock`, cleans up all daemons via `atexit`, and returns a
//!   shared read-lock guard so tests using the shared directory run in parallel
//!   without racing with tests that temporarily mutate environment variables.
//! - [`setup`] isolates a single test in its own private `G3D_TERMD_DIR` while
//!   holding an exclusive write-lock guard, restoring all environment variables
//!   and stopping its daemons on `Drop`.

use crate::client::{Client, DEFAULT_NAMESPACE, Options, TEST_SERVER_ENV};
use crate::paths::{check_test_server, daemon_binary};
use crate::wire::stop_socket;
use anyhow::{Result, bail};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::OsString;
use std::marker::PhantomData;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard};

/// Keeps `"<dir>/termd-<64-char-ns>.sock"` within macOS's 104-byte `sun_path`
/// limit for reasonable test namespaces.
const MAX_SOCKET_PATH: usize = 103;

static ENV_LOCK: RwLock<()> = RwLock::new(());
static SHARED_DIR: OnceLock<PathBuf> = OnceLock::new();

extern "C" fn cleanup_shared_dir() {
    if let Some(dir) = SHARED_DIR.get() {
        stop_all(dir);
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Creates a short `0700` temporary socket directory preferring `/var/tmp`
/// (never `/tmp` unless `/var/tmp` is unavailable or too long).
pub fn create_socket_dir(prefix: &str) -> Result<PathBuf> {
    let mut parents = vec![PathBuf::from("/var/tmp"), std::env::temp_dir()];
    parents.dedup();
    for parent in parents {
        if !parent.is_dir() {
            continue;
        }
        let Ok(tmp) = tempfile::Builder::new().prefix(prefix).tempdir_in(&parent) else {
            continue;
        };
        let dir = tmp.keep();
        if std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            continue;
        }
        let sample = dir.join("termd-agents-test.sock");
        if sample.as_os_str().len() <= MAX_SOCKET_PATH {
            return Ok(dir);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    bail!("no temp directory short enough for a termd socket (at most {MAX_SOCKET_PATH} bytes)")
}

/// Stops every `termd-*.sock` daemon in `dir`.
pub fn stop_all(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(s) = name.to_str() else {
            continue;
        };
        if s.starts_with("termd-") && s.ends_with(".sock") {
            let _ = stop_socket(dir.join(s));
        }
    }
}

fn ensure_shared_initialized() -> &'static Path {
    SHARED_DIR
        .get_or_init(|| {
            let _write = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
            let dir = create_socket_dir("gmxt").expect("termdtest: create socket dir");
            let zdotdir = dir.join("zdotdir");
            std::fs::create_dir_all(&zdotdir).expect("termdtest: create zdotdir");
            std::fs::set_permissions(&zdotdir, std::fs::Permissions::from_mode(0o700))
                .expect("termdtest: chmod zdotdir");
            // SAFETY: protected by exclusive `ENV_LOCK` write guard.
            unsafe {
                std::env::remove_var("G3D_TERMD_SOCKET");
                std::env::remove_var("G3D_TERMD_ID");
                std::env::set_var("G3D_TERMD_DIR", &dir);
                std::env::set_var(TEST_SERVER_ENV, &dir);
                std::env::set_var("ZDOTDIR", &zdotdir);
                libc::atexit(cleanup_shared_dir);
            }
            dir
        })
        .as_path()
}

struct ThreadRead {
    depth: usize,
    guard: Option<RwLockReadGuard<'static, ()>>,
}

thread_local! {
    static THREAD_READ: RefCell<ThreadRead> = const {
        RefCell::new(ThreadRead { depth: 0, guard: None })
    };
    static THREAD_WRITE: Cell<bool> = const { Cell::new(false) };
}

/// Hold for the duration of a test using the shared test socket directory.
///
/// Re-entrant per thread so a test holding one `SharedGuard` (e.g. a multi-host
/// test) can acquire another without deadlocking behind a waiting [`EnvGuard`]
/// writer.
pub struct SharedGuard {
    dir: &'static Path,
    _not_send: PhantomData<RwLockReadGuard<'static, ()>>,
}

impl SharedGuard {
    pub fn dir(&self) -> &Path {
        self.dir
    }
}

impl Drop for SharedGuard {
    fn drop(&mut self) {
        let _ = THREAD_READ.try_with(|cell| {
            let mut state = cell.borrow_mut();
            state.depth = state.depth.saturating_sub(1);
            if state.depth == 0 {
                state.guard = None;
            }
        });
    }
}

/// Initializes the process-wide isolated `G3D_TERMD_DIR` (if not yet started)
/// and returns a [`SharedGuard`].
pub fn init() -> SharedGuard {
    assert!(
        !THREAD_WRITE.get(),
        "termdtest::init called on a thread already holding EnvGuard or setup()"
    );
    let dir = ensure_shared_initialized();
    THREAD_READ.with(|cell| {
        let mut state = cell.borrow_mut();
        if state.depth == 0 {
            state.guard = Some(ENV_LOCK.read().unwrap_or_else(|e| e.into_inner()));
        }
        state.depth += 1;
    });
    SharedGuard {
        dir,
        _not_send: PhantomData,
    }
}

/// Reports whether a `g3d-termd` binary is available on this machine.
pub fn available() -> bool {
    daemon_binary("").is_ok()
}

/// Ensures the test binary has an isolated `G3D_TERMD_DIR`, verifies
/// [`check_test_server`], and asserts that `g3d-termd` is available.
pub fn require() -> SharedGuard {
    let guard = init();
    check_test_server().expect("termdtest: check_test_server");
    daemon_binary("").expect("g3d-termd not available");
    guard
}

/// Exclusive environment guard for tests that mutate process environment
/// variables (`G3D_TERMD_DIR`, `G3D_TERMD_SOCKET`, `HOME`, etc.). Restores all
/// modified variables when dropped.
pub struct EnvGuard {
    saved: HashMap<String, Option<OsString>>,
    _write: RwLockWriteGuard<'static, ()>,
}

impl EnvGuard {
    pub fn lock() -> Self {
        let read_depth = THREAD_READ.with(|cell| cell.borrow().depth);
        assert_eq!(
            read_depth, 0,
            "termdtest::EnvGuard::lock called on a thread already holding SharedGuard"
        );
        assert!(
            !THREAD_WRITE.get(),
            "termdtest::EnvGuard::lock called twice on the same thread"
        );
        let write = ENV_LOCK.write().unwrap_or_else(|e| e.into_inner());
        THREAD_WRITE.set(true);
        Self {
            saved: HashMap::new(),
            _write: write,
        }
    }

    fn record(&mut self, key: &str) {
        self.saved
            .entry(key.to_owned())
            .or_insert_with(|| std::env::var_os(key));
    }

    pub fn set(&mut self, key: &str, val: impl AsRef<std::ffi::OsStr>) {
        self.record(key);
        let v = val.as_ref();
        // SAFETY: protected by exclusive `ENV_LOCK` write guard.
        unsafe {
            if v.is_empty() {
                std::env::remove_var(key);
            } else {
                std::env::set_var(key, v);
            }
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.record(key);
        // SAFETY: protected by exclusive `ENV_LOCK` write guard.
        unsafe {
            std::env::remove_var(key);
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain() {
            // SAFETY: protected by exclusive `ENV_LOCK` write guard.
            unsafe {
                match v {
                    Some(val) => std::env::set_var(&k, val),
                    None => std::env::remove_var(&k),
                }
            }
        }
        let _ = THREAD_WRITE.try_with(|c| c.set(false));
    }
}

/// Isolates a single test in its own private `G3D_TERMD_DIR`, stopping any
/// daemons in it and removing the directory on `Drop`.
pub struct TestServer {
    dir: PathBuf,
    env: EnvGuard,
}

impl TestServer {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn env(&mut self) -> &mut EnvGuard {
        &mut self.env
    }

    pub fn new_client(&self, ns: &str) -> Result<Client> {
        Client::new(Options {
            namespace: ns.to_owned(),
            ..Default::default()
        })
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        stop_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Isolates a single test in its own private `G3D_TERMD_DIR` separate from the
/// shared test directory.
pub fn setup() -> TestServer {
    let mut env = EnvGuard::lock();
    let dir = create_socket_dir("gmxt").expect("termdtest::setup: create socket dir");
    let zdotdir = dir.join("zdotdir");
    std::fs::create_dir_all(&zdotdir).expect("termdtest::setup: create zdotdir");
    std::fs::set_permissions(&zdotdir, std::fs::Permissions::from_mode(0o700))
        .expect("termdtest::setup: chmod zdotdir");

    env.remove("G3D_TERMD_SOCKET");
    env.remove("G3D_TERMD_ID");
    env.set("G3D_TERMD_DIR", &dir);
    env.set(TEST_SERVER_ENV, &dir);
    env.set("ZDOTDIR", &zdotdir);

    TestServer { dir, env }
}

/// RAII wrapper around [`Client`] for tests: stops dedicated test-namespace
/// daemons on `Drop` while leaving [`DEFAULT_NAMESPACE`] alive across tests in
/// the same binary.
pub struct TestClient {
    pub client: Client,
    stop_on_drop: bool,
}

impl std::ops::Deref for TestClient {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Drop for TestClient {
    fn drop(&mut self) {
        if self.stop_on_drop {
            let _ = self.client.stop_daemon();
        }
    }
}

/// Creates a [`TestClient`] against `ns` inside an active [`SharedGuard`] or
/// [`TestServer`].
pub fn new_test_client(ns: &str) -> TestClient {
    let client = Client::new(Options {
        namespace: ns.to_owned(),
        ..Default::default()
    })
    .unwrap_or_else(|e| panic!("Client::new({ns:?}): {e:#}"));
    let stop_on_drop = ns != DEFAULT_NAMESPACE;
    TestClient {
        client,
        stop_on_drop,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    #[test]
    fn reentrant_shared_guard_does_not_deadlock_behind_waiting_writer() {
        let (done_tx, done_rx) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let g1 = init();
            let writer_entered = Arc::new(Barrier::new(2));
            let writer_entered_bg = Arc::clone(&writer_entered);
            let writer_acquired = Arc::new(AtomicBool::new(false));
            let writer_acquired_bg = Arc::clone(&writer_acquired);

            let writer = std::thread::spawn(move || {
                writer_entered_bg.wait();
                let mut env = EnvGuard::lock();
                writer_acquired_bg.store(true, Ordering::SeqCst);
                env.set("GOMUXAI_TESTING_PROBE", "1");
            });

            writer_entered.wait();
            // Give the writer thread time to park on ENV_LOCK.write() and set
            // std::sync::RwLock's WRITERS_WAITING bit.
            std::thread::sleep(Duration::from_millis(50));
            assert!(!writer_acquired.load(Ordering::SeqCst));

            // Acquiring a second SharedGuard on the same thread must succeed
            // without blocking behind the waiting writer.
            let g2 = init();
            assert_eq!(g1.dir(), g2.dir());

            // Dropping only the inner guard must keep the read lock held.
            drop(g2);
            std::thread::sleep(Duration::from_millis(20));
            assert!(
                !writer_acquired.load(Ordering::SeqCst),
                "writer acquired ENV_LOCK while outer SharedGuard was still alive"
            );

            drop(g1);
            writer.join().expect("writer thread");
            assert!(writer_acquired.load(Ordering::SeqCst));
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("re-entrant SharedGuard deadlocked behind waiting EnvGuard writer");
        reader.join().expect("reader thread");
    }

    #[test]
    fn concurrent_nested_shared_guards_and_writers() {
        let (done_tx, done_rx) = mpsc::channel();
        let coordinator = std::thread::spawn(move || {
            let start = Arc::new(Barrier::new(6));
            let mut handles = Vec::new();
            for _ in 0..4 {
                let start = Arc::clone(&start);
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..10 {
                        let g1 = init();
                        std::thread::yield_now();
                        let g2 = init();
                        assert_eq!(g1.dir(), g2.dir());
                        check_test_server().expect("shared dir remains active under SharedGuard");
                        drop(g2);
                        check_test_server()
                            .expect("shared dir remains active under outer SharedGuard");
                        drop(g1);
                    }
                }));
            }
            for _ in 0..2 {
                let start = Arc::clone(&start);
                handles.push(std::thread::spawn(move || {
                    start.wait();
                    for _ in 0..5 {
                        let _srv = setup();
                        check_test_server().expect("private dir active under TestServer");
                    }
                }));
            }
            for h in handles {
                h.join().expect("worker thread");
            }
            let _ = done_tx.send(());
        });

        done_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("concurrent SharedGuard + setup() stress deadlocked");
        coordinator.join().expect("coordinator thread");
    }
}
