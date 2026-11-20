//! `agent watch` (Go: internal/cli/watch.go).
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agent_base::gostd::os::{PathError, errno_text};
use agent_base::gostd::{filepath, time as gotime};
use agent_service::Context;
use agent_service::watcher;
use anyhow::Context as _;

use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::output::plural;
use super::root::Ctx;

// `agent watch`

pub const DOC: Doc = Doc {
    use_line: "watch",
    short: "Deliver queued messages as agents become idle (runs until Ctrl-C)",
    long: r#"Watch runs in the foreground, delivering queued messages ("agent queue"),
until interrupted.

Every --interval it checks each agent that has a queued message, and delivers
the oldest one when the agent is idle, or back at its prompt after a failed
turn (error). It holds messages while an agent is processing or waiting, and
fails them once the agent's session has ended. Deliveries and failures are logged on
stderr.

Several watchers can run at once (each delivery is checked against the
agent's status), but one per host is enough. Each running watcher holds
.gomuxai/watch.lock, which is how queue knows whether one is running."#,
    example: r#"  agent watch
  agent watch --interval 5s"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent watch`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "interval",
        value_name = "duration",
        default_value = "2s",
        value_parser = flags::parse_duration,
        help = "Check interval",
    )]
    pub interval: Duration,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (bg, cancel) = Context::with_cancel(&Context::background());

    let (others, checked) = watcher_running(ctx);
    let _lock = hold_watch_lock(ctx)?;

    let svc = ctx.service()?;
    let store = ctx.store()?;
    let interrupts = Interrupts::new();
    // Go's slog default writes to os.Stderr, where a closed pipe ends the
    // process: a watcher whose log no one reads any more stops.
    let logger = watcher::Logger::new(
        Box::new(super::output::GoStdio::Stderr),
        watcher::Level::Info,
        watcher::LogFormat::Default,
    );
    let mut w = watcher::new_watcher(
        Arc::clone(svc),
        Arc::clone(store),
        args.interval,
        [watcher::with_logger(logger)],
    );
    w.start(&bg);

    let agents = svc.list().unwrap_or_default();
    let pending: usize = agents
        .iter()
        .filter_map(|a| store.get_pending_messages(&a.name, 100).ok())
        .map(|m| m.len())
        .sum();
    ctx.write_out(&format!(
        "Watching {} on host {} every {}; {} queued. Ctrl-C stops.\n",
        plural(agents.len(), "agent"),
        ctx.cfg().host,
        gotime::format_duration(args.interval),
        plural(pending, "message")
    ))?;
    let _ = ctx.out().flush();
    if checked && others {
        ctx.hint("another watcher is already running for this data dir; one is enough");
    }

    // Wait for interrupt
    interrupts.wait();

    w.stop();
    cancel.cancel();
    ctx.write_out("\nWatcher stopped\n")
}

/// The file each running watcher holds a shared lock on.
fn watch_lock_path(ctx: &Ctx) -> String {
    filepath::join(&[filepath::dir(&ctx.cfg().database_path).as_str(), "watch.lock"])
}

/// A lock on a file, held until it is dropped (or the process exits, however
/// it exits).
pub(crate) struct FileLock(#[allow(dead_code)] std::fs::File);

fn open_lock_file(path: &str) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)
}

/// flock(2) on f; Err(EWOULDBLOCK) when non-blocking and another holds it.
fn flock(f: &std::fs::File, op: libc::c_int) -> std::io::Result<()> {
    // SAFETY: flock on an open file descriptor this process owns.
    if unsafe { libc::flock(f.as_raw_fd(), op) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn would_block(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::EWOULDBLOCK)
}

/// Takes a shared lock on the watch lock file, held until the returned lock is
/// dropped (Go: holdWatchLock).
pub(crate) fn hold_watch_lock(ctx: &Ctx) -> anyhow::Result<FileLock> {
    let path = watch_lock_path(ctx);
    let f = open_lock_file(&path)
        .map_err(|e| anyhow::Error::new(PathError::new("open", &path, e)))
        .context("opening watch lock")?;
    flock(&f, libc::LOCK_SH).map_err(|e| anyhow::anyhow!("locking {path}: {}", errno_text(&e)))?;
    Ok(FileLock(f))
}

/// Reports whether a watcher holds the watch lock: (running, checked), where
/// checked is false if it couldn't tell. Two checks at the same instant can
/// see each other as a watcher, which errs toward "running", the side that
/// prints no hint (Go: watcherRunning).
pub(crate) fn watcher_running(ctx: &Ctx) -> (bool, bool) {
    let Ok(f) = open_lock_file(&watch_lock_path(ctx)) else {
        return (false, false);
    };
    match flock(&f, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => {
            let _ = flock(&f, libc::LOCK_UN);
            (false, true)
        }
        Err(e) if would_block(&e) => (true, true),
        Err(_) => (false, false),
    }
}

/// Takes a file's exclusive lock without blocking: Ok(None) if another holds
/// it (for deliver's lock, what names it in errors).
pub(crate) fn try_lock(path: &str, what: &str) -> anyhow::Result<Option<FileLock>> {
    let f = open_lock_file(path)
        .map_err(|e| anyhow::Error::new(PathError::new("open", path, e)))
        .with_context(|| format!("opening {what}"))?;
    match flock(&f, libc::LOCK_EX | libc::LOCK_NB) {
        Ok(()) => Ok(Some(FileLock(f))),
        Err(e) if would_block(&e) => Ok(None),
        Err(e) => anyhow::bail!("locking {path}: {}", errno_text(&e)),
    }
}

/// Set by the SIGINT/SIGTERM handler Interrupts installs.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_interrupt(_: libc::c_int) {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

/// SIGINT and SIGTERM caught while it lives, as Go's signal.Notify(ch,
/// os.Interrupt, syscall.SIGTERM): the commands that run until Ctrl-C (watch,
/// events, wait --follow) stop cleanly instead of dying. Dropping it puts the
/// previous handlers back (Go: signal.Stop).
pub(crate) struct Interrupts {
    int: libc::sighandler_t,
    term: libc::sighandler_t,
}

impl Interrupts {
    pub(crate) fn new() -> Interrupts {
        INTERRUPTED.store(false, Ordering::SeqCst);
        let handler = on_interrupt as extern "C" fn(libc::c_int) as libc::sighandler_t;
        // SAFETY: signal() swaps a disposition; the handler only stores to an
        // atomic, which is async-signal-safe.
        unsafe {
            Interrupts {
                int: libc::signal(libc::SIGINT, handler),
                term: libc::signal(libc::SIGTERM, handler),
            }
        }
    }

    /// Whether a signal has come.
    pub(crate) fn fired(&self) -> bool {
        INTERRUPTED.load(Ordering::SeqCst)
    }

    /// Blocks until a signal comes.
    pub(crate) fn wait(&self) {
        while !self.fired() {
            std::thread::sleep(INTERRUPT_POLL);
        }
    }
}

/// How often a command waiting for Ctrl-C looks for it.
pub(crate) const INTERRUPT_POLL: Duration = Duration::from_millis(50);

impl Drop for Interrupts {
    fn drop(&mut self) {
        // SAFETY: puts back the dispositions new() replaced.
        unsafe {
            libc::signal(libc::SIGINT, self.int);
            libc::signal(libc::SIGTERM, self.term);
        }
    }
}

/// Held by every unit test that runs a command waiting for Ctrl-C (watch,
/// events, wait --follow): INTERRUPTED is one flag for the process, so one
/// test's interrupt would stop another's command, and each Interrupts::new
/// clears it.
#[cfg(test)]
pub(crate) static INTERRUPT_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Interrupts, as Ctrl-C would, the command a test runs on another thread,
/// until done says it has returned. The flag is set again and again, since
/// the command clears it when it starts waiting (Interrupts::new); a real
/// signal could reach the test binary while no handler is installed.
#[cfg(test)]
pub(crate) fn interrupt_until(done: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !done() {
        assert!(std::time::Instant::now() < deadline, "the command never stopped");
        INTERRUPTED.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
#[path = "watch_test.rs"]
mod watch_test;
