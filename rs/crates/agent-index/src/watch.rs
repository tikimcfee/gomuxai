//! `agent index watch` (docs/INDEX-V2.md, "Freshness"): keeps the pg store
//! current as files change, with no one running a crawl.
//!
//! inotify watches every indexed directory of the full-depth projects (the
//! directories the crawl recorded, and new ones as they appear, never the
//! build output the crawl skips: `target/`, `node_modules/`, `.git/`...), so
//! a cargo or npm build makes no events at all. A changed project is
//! flushed once its files have been quiet for `[watch].settle_ms`, or
//! `max_wait_ms` after its first change however busy it stays, and held
//! while git holds `.git/index.lock` (a checkout or rebase under way). A
//! flush is the crawl of those projects (quick: no roots, no cross-project
//! steps), a sync of their rows alone in one transaction, and the embed of
//! the cards it queued; the [`Keeper`] does it. The tidy (a full crawl and
//! sync: edges across projects, importance, word counts, new projects and
//! the side corpora) runs once all has been quiet for `tidy_quiet_s`, at
//! most every `tidy_every_s`, and once at start, which catches up on what
//! changed while nothing watched.
//!
//! A Unix socket takes the same queue's other feed: one JSON line naming
//! changed paths (`agent index touch`, an edit hook, the search's stale
//! check), or asking for the status.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::mem::MaybeUninit;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
use serde::{Deserialize, Serialize};

use crate::config::Watch as Settings;
use crate::files::SKIP_DIRS;

/// A project the watcher keeps: its name, its directory, whether git keeps
/// it, and its indexed directories (absolute).
#[derive(Debug, Clone)]
pub struct Project {
    pub name: String,
    pub path: PathBuf,
    pub git: bool,
    pub dirs: Vec<PathBuf>,
}

/// What the watcher asks of the index: the projects to watch, a flush of
/// some, and the tidy.
pub trait Keeper {
    /// The full-depth projects and their indexed directories (after a flush
    /// or tidy, to watch what is new).
    fn projects(&mut self) -> Result<Vec<Project>>;
    /// Brings these projects' rows up to date; one line saying what it did.
    fn flush(&mut self, projects: &[String]) -> Result<String>;
    /// The full crawl and sync; one line saying what it did.
    fn tidy(&mut self) -> Result<String>;
    /// Chunk windows waiting for the embedder's server (a flush with it
    /// down embeds only a few on the CPU).
    fn windows_waiting(&self) -> usize {
        0
    }
    /// Embeds the waiting windows if the server answers again (no crawl, no
    /// sync); one line when it did something. Asked every
    /// [`CATCH_UP_EVERY`] while windows wait and nothing is due.
    fn catch_up(&mut self) -> Result<Option<String>> {
        Ok(None)
    }
}

/// How often the watcher asks the keeper to catch up on waiting windows.
pub const CATCH_UP_EVERY: Duration = Duration::from_secs(30);

/// Changed projects waiting to be flushed.
#[derive(Debug, Default)]
pub struct Debounce {
    pending: BTreeMap<String, Pending>,
}

#[derive(Debug)]
struct Pending {
    first: Instant,
    last: Instant,
    paths: BTreeSet<PathBuf>,
    events: usize,
}

impl Debounce {
    /// A change in `project` (at `path`, when known) at `now`.
    pub fn note(&mut self, project: &str, path: Option<PathBuf>, now: Instant) {
        let p = self
            .pending
            .entry(project.to_string())
            .or_insert_with(|| Pending {
                first: now,
                last: now,
                paths: BTreeSet::new(),
                events: 0,
            });
        p.last = now;
        p.events += 1;
        if let Some(path) = path {
            p.paths.insert(path);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// The projects to flush now, taken out of the queue with their paths
    /// and event counts: quiet for `settle`, or waiting `max_wait` since
    /// their first change. One `locked` (git busy in it) waits on, up to
    /// `max_wait + git_lock`.
    pub fn due(
        &mut self,
        now: Instant,
        s: &Settings,
        locked: impl Fn(&str) -> bool,
    ) -> Vec<(String, Vec<PathBuf>, usize)> {
        let settle = Duration::from_millis(s.settle_ms);
        let max_wait = Duration::from_millis(s.max_wait_ms);
        let cap = max_wait + Duration::from_millis(s.git_lock_ms);
        let ready: Vec<String> = self
            .pending
            .iter()
            .filter(|(name, p)| {
                let quiet = now.duration_since(p.last) >= settle;
                let waited = now.duration_since(p.first);
                (quiet || waited >= max_wait) && (waited >= cap || !locked(name))
            })
            .map(|(n, _)| n.clone())
            .collect();
        ready
            .into_iter()
            .filter_map(|n| {
                let p = self.pending.remove(&n)?;
                Some((n, p.paths.into_iter().collect(), p.events))
            })
            .collect()
    }

    /// How long until the next project could be due (None: nothing waits).
    pub fn next(&self, now: Instant, s: &Settings) -> Option<Duration> {
        let settle = Duration::from_millis(s.settle_ms);
        let max_wait = Duration::from_millis(s.max_wait_ms);
        self.pending
            .values()
            .map(|p| {
                let a = (p.last + settle).saturating_duration_since(now);
                let b = (p.first + max_wait).saturating_duration_since(now);
                a.min(b)
            })
            .min()
    }
}

/// A file or directory name no flush needs: editors' swap and backup files,
/// and directories the crawl never reads.
pub fn ignored_name(name: &str) -> bool {
    SKIP_DIRS.contains(&name)
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swx")
        || name.ends_with(".swo")
        || name == "4913"
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#'))
        || name.starts_with(".goutputstream-")
}

/// Whether git is busy in a project (a checkout, rebase or commit holds its
/// index lock).
fn git_busy(dir: &Path) -> bool {
    dir.join(".git/index.lock").exists()
}

/// The paths git ignores among these (none when it can't say, so nothing
/// is dropped by mistake). A tracked file is never ignored.
fn git_ignored(dir: &Path, paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    let mut out = BTreeSet::new();
    if paths.is_empty() {
        return out;
    }
    let Ok(mut child) = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "--stdin", "-z"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return out;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let mut buf = Vec::new();
        for p in paths {
            buf.extend_from_slice(p.as_os_str().as_encoded_bytes());
            buf.push(0);
        }
        let _ = stdin.write_all(&buf);
    }
    let Ok(o) = child.wait_with_output() else {
        return out;
    };
    for p in o.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
        out.insert(PathBuf::from(String::from_utf8_lossy(p).into_owned()));
    }
    out
}

/// inotify over the projects' directories: watch descriptor to directory
/// and project.
struct Watches {
    fd: std::os::fd::OwnedFd,
    by_wd: HashMap<i32, (PathBuf, String)>,
    by_dir: HashMap<PathBuf, i32>,
    /// Directories past the watch limit (fs.inotify.max_user_watches).
    over_limit: usize,
}

const MASK: WatchFlags = WatchFlags::CLOSE_WRITE
    .union(WatchFlags::MODIFY)
    .union(WatchFlags::CREATE)
    .union(WatchFlags::DELETE)
    .union(WatchFlags::MOVED_FROM)
    .union(WatchFlags::MOVED_TO)
    .union(WatchFlags::DELETE_SELF)
    .union(WatchFlags::DONT_FOLLOW)
    .union(WatchFlags::EXCL_UNLINK);

impl Watches {
    fn new() -> Result<Watches> {
        let fd = inotify::init(CreateFlags::CLOEXEC).context("starting inotify")?;
        Ok(Watches {
            fd,
            by_wd: HashMap::new(),
            by_dir: HashMap::new(),
            over_limit: 0,
        })
    }

    /// Watches a directory for a project (a directory already watched is
    /// given the deeper project's name).
    fn add(&mut self, dir: &Path, project: &str) {
        match inotify::add_watch(&self.fd, dir, MASK) {
            Ok(wd) => {
                self.by_wd
                    .insert(wd, (dir.to_path_buf(), project.to_string()));
                self.by_dir.insert(dir.to_path_buf(), wd);
            }
            Err(rustix::io::Errno::NOSPC) => self.over_limit += 1,
            Err(_) => {}
        }
    }

    /// Watches a new directory and what is under it (a checkout can make a
    /// tree at once), skipping what the crawl skips; returns what it added.
    fn add_tree(&mut self, dir: &Path, project: &str, git_root: Option<&Path>) -> usize {
        let mut added = 0;
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Some(root) = git_root
                && git_ignored(root, std::slice::from_ref(&d)).contains(&d)
            {
                continue;
            }
            self.add(&d, project);
            added += 1;
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let Ok(ft) = e.file_type() else { continue };
                let name = e.file_name();
                if ft.is_dir() && !ignored_name(&name.to_string_lossy()) {
                    stack.push(e.path());
                }
            }
        }
        added
    }

    fn forget(&mut self, wd: i32) {
        if let Some((dir, _)) = self.by_wd.remove(&wd) {
            self.by_dir.remove(&dir);
        }
    }
}

/// What the socket is asked: changed paths, or the status.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Request {
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub status: bool,
}

/// What the watcher says back.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Status {
    pub started: String,
    pub projects: usize,
    pub watched_dirs: usize,
    /// Directories left unwatched at the watch limit.
    pub over_limit: usize,
    pub pending: usize,
    pub flushes: usize,
    pub tidies: usize,
    pub events: usize,
    /// Paths named on the socket (hooks, the stale check).
    pub touched: usize,
    /// Of them, outside every watched project.
    pub outside: usize,
    pub last_flush: String,
    pub last_flush_at: String,
    pub last_tidy: String,
    pub last_error: String,
    /// When the last flush's rows were committed (UTC, ms), for measuring.
    pub last_commit_ms: i64,
    /// Paths queued by this request.
    #[serde(default)]
    pub queued: usize,
    /// Chunk windows waiting for the embedder's server ([watch].cpu_windows).
    #[serde(default)]
    pub windows_waiting: usize,
}

/// Names changed paths to the watcher at `socket` (or asks its status);
/// an error when no watcher answers.
pub fn ask(socket: &Path, req: &Request, timeout: Duration) -> Result<Status> {
    let mut s = UnixStream::connect(socket)
        .with_context(|| format!("no index watcher at {}", socket.display()))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let mut line = serde_json::to_vec(req)?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut buf = String::new();
    BufReader::new(&s).read_line(&mut buf)?;
    serde_json::from_str(&buf).context("the index watcher's answer")
}

enum Msg {
    Fs(Vec<(i32, ReadFlags, Option<String>)>),
    Touch(Vec<PathBuf>),
}

fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Runs the watcher until the process ends: the tidy first (catching up),
/// then watches and flushes. `log` gets one line per flush, tidy or
/// problem.
pub fn run(
    keeper: &mut dyn Keeper,
    s: &Settings,
    socket: &Path,
    mut log: impl FnMut(&str),
) -> Result<()> {
    // The socket first, so a second watcher is refused before any work.
    if socket.exists() {
        if UnixStream::connect(socket).is_ok() {
            bail!("an index watcher already answers on {}", socket.display());
        }
        std::fs::remove_file(socket)
            .with_context(|| format!("removing the stale socket {}", socket.display()))?;
    }
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    let status = Arc::new(Mutex::new(Status {
        started: now_rfc3339(),
        ..Default::default()
    }));
    let (tx, rx) = mpsc::channel::<Msg>();

    // Catch up on what changed while nothing watched.
    match keeper.tidy() {
        Ok(line) => {
            log(&format!("caught up: {line}"));
            let mut st = status.lock().unwrap();
            st.tidies += 1;
            st.last_tidy = now_rfc3339();
        }
        Err(e) => log(&format!("catching up failed ({e:#}); watching anyway")),
    }
    let mut last_tidy = Instant::now();

    let mut w = Watches::new()?;
    let mut projects: Vec<Project> = keeper.projects()?;
    for p in &projects {
        for d in &p.dirs {
            w.add(d, &p.name);
        }
    }
    log(&format!(
        "watching {} directories of {} projects{}",
        w.by_wd.len(),
        projects.len(),
        if w.over_limit > 0 {
            format!(
                "; {} more past the inotify watch limit (raise fs.inotify.max_user_watches)",
                w.over_limit
            )
        } else {
            String::new()
        }
    ));
    {
        let mut st = status.lock().unwrap();
        st.projects = projects.len();
        st.watched_dirs = w.by_wd.len();
        st.over_limit = w.over_limit;
    }

    // inotify's reader: whole reads as one message each.
    {
        let fd = w.fd.try_clone().context("sharing the inotify descriptor")?;
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = vec![MaybeUninit::<u8>::uninit(); 256 * 1024];
            let mut r = inotify::Reader::new(&fd, &mut buf);
            loop {
                let mut batch = Vec::new();
                match r.next() {
                    Ok(e) => batch.push((
                        e.wd(),
                        e.events(),
                        e.file_name().map(|n| n.to_string_lossy().into_owned()),
                    )),
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(_) => return,
                }
                while !r.is_buffer_empty() {
                    match r.next() {
                        Ok(e) => batch.push((
                            e.wd(),
                            e.events(),
                            e.file_name().map(|n| n.to_string_lossy().into_owned()),
                        )),
                        Err(_) => break,
                    }
                }
                if tx.send(Msg::Fs(batch)).is_err() {
                    return;
                }
            }
        });
    }
    // The socket: paths into the queue; the status out.
    {
        let tx = tx.clone();
        let status = Arc::clone(&status);
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(conn) = conn else { continue };
                let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
                let mut line = String::new();
                if BufReader::new(&conn).read_line(&mut line).is_err() {
                    continue;
                }
                let req: Request = serde_json::from_str(&line).unwrap_or_default();
                let queued = req.paths.len();
                if queued > 0 {
                    let paths = req.paths.iter().map(PathBuf::from).collect();
                    if tx.send(Msg::Touch(paths)).is_err() {
                        return;
                    }
                }
                let mut st = status.lock().map(|s| s.clone()).unwrap_or_default();
                st.queued = queued;
                let mut out = serde_json::to_vec(&st).unwrap_or_default();
                out.push(b'\n');
                let mut c = &conn;
                let _ = c.write_all(&out);
            }
        });
    }
    drop(tx);

    let mut q = Debounce::default();
    let mut activity = false;
    let mut last_activity = Instant::now();
    let tidy_quiet = Duration::from_secs(s.tidy_quiet_s);
    let tidy_every = Duration::from_secs(s.tidy_every_s);
    let mut last_catch_up = Instant::now();
    loop {
        let now = Instant::now();
        let mut wait = q.next(now, s).unwrap_or(Duration::from_secs(60));
        // Windows waiting for the embedder's server: asked again now and
        // then, with nothing else due.
        if keeper.windows_waiting() > 0 {
            if q.is_empty() && now.duration_since(last_catch_up) >= CATCH_UP_EVERY {
                last_catch_up = now;
                match keeper.catch_up() {
                    Ok(Some(line)) => log(&format!("caught up on waiting windows: {line}")),
                    Ok(None) => {}
                    Err(e) => log(&format!("catching up on waiting windows failed: {e:#}")),
                }
                status.lock().unwrap().windows_waiting = keeper.windows_waiting();
            }
            wait = wait.min(CATCH_UP_EVERY);
        }
        if activity {
            let t = (last_activity + tidy_quiet)
                .max(last_tidy + tidy_every)
                .saturating_duration_since(now);
            wait = wait.min(t);
        }
        // Held projects (git busy) are looked at again within the second.
        if !q.is_empty() {
            wait = wait.min(Duration::from_millis(250));
        }
        match rx.recv_timeout(wait.max(Duration::from_millis(10))) {
            Ok(Msg::Fs(batch)) => {
                let now = Instant::now();
                let mut st = status.lock().unwrap();
                st.events += batch.len();
                drop(st);
                for (wd, flags, name) in batch {
                    if flags.contains(ReadFlags::QUEUE_OVERFLOW) {
                        log("inotify's queue overflowed: flushing every project");
                        for p in &projects {
                            q.note(&p.name, None, now);
                        }
                        continue;
                    }
                    if flags.contains(ReadFlags::IGNORED) {
                        w.forget(wd);
                        continue;
                    }
                    let Some((dir, project)) = w.by_wd.get(&wd).cloned() else {
                        continue;
                    };
                    let Some(name) = name else {
                        // The directory itself went (DELETE_SELF).
                        q.note(&project, Some(dir), now);
                        continue;
                    };
                    if ignored_name(&name) || crate::files::is_secret(&name) {
                        continue;
                    }
                    let path = dir.join(&name);
                    if flags.contains(ReadFlags::ISDIR)
                        && (flags.contains(ReadFlags::CREATE)
                            || flags.contains(ReadFlags::MOVED_TO))
                    {
                        let git_root = projects
                            .iter()
                            .find(|p| p.name == project && p.git)
                            .map(|p| p.path.clone());
                        w.add_tree(&path, &project, git_root.as_deref());
                    }
                    q.note(&project, Some(path), now);
                }
                activity = true;
                last_activity = now;
            }
            Ok(Msg::Touch(paths)) => {
                let now = Instant::now();
                let mut outside = 0;
                for p in &paths {
                    // The deepest project holding the path.
                    let owner = projects
                        .iter()
                        .filter(|pr| p.starts_with(&pr.path))
                        .max_by_key(|pr| pr.path.as_os_str().len());
                    match owner {
                        Some(pr) => q.note(&pr.name, Some(p.clone()), now),
                        None => outside += 1,
                    }
                }
                let mut st = status.lock().unwrap();
                st.touched += paths.len();
                st.outside += outside;
                drop(st);
                activity = true;
                last_activity = now;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the watcher's readers stopped"),
        }

        // Flush what is due, all in one go.
        let now = Instant::now();
        let by_name: HashMap<&str, &Project> =
            projects.iter().map(|p| (p.name.as_str(), p)).collect();
        let due = q.due(now, s, |name| {
            by_name
                .get(name)
                .is_some_and(|p| p.git && git_busy(&p.path))
        });
        if !due.is_empty() {
            // Drop a project whose every changed path git ignores (a log,
            // a build file in a source dir): nothing of it is indexed.
            let mut names = Vec::new();
            let mut events = 0;
            for (name, paths, n) in &due {
                events += n;
                let Some(p) = by_name.get(name.as_str()) else {
                    continue;
                };
                if p.git && !paths.is_empty() {
                    let ig = git_ignored(&p.path, paths);
                    if paths.iter().all(|x| ig.contains(x)) {
                        continue;
                    }
                }
                names.push(name.clone());
            }
            if names.is_empty() {
                continue;
            }
            let t = Instant::now();
            let res = keeper.flush(&names);
            let ms = t.elapsed().as_millis();
            let mut st = status.lock().unwrap();
            match &res {
                Ok(line) => {
                    st.flushes += 1;
                    st.windows_waiting = keeper.windows_waiting();
                    st.last_flush = format!("{} ({events} events): {line}", names.join(", "));
                    st.last_flush_at = now_rfc3339();
                    st.last_commit_ms = chrono::Utc::now().timestamp_millis();
                    log(&format!(
                        "flushed {} ({events} events) in {ms} ms: {line}",
                        names.join(", ")
                    ));
                }
                Err(e) => {
                    st.last_error = format!("{e:#}");
                    log(&format!("flushing {} failed: {e:#}", names.join(", ")));
                }
            }
            drop(st);
            // New directories the crawl now knows, and the projects' list.
            if let Ok(ps) = keeper.projects() {
                projects = ps;
                for p in &projects {
                    if names.contains(&p.name) {
                        for d in &p.dirs {
                            if !w.by_dir.contains_key(d) {
                                w.add(d, &p.name);
                            }
                        }
                    }
                }
                let mut st = status.lock().unwrap();
                st.projects = projects.len();
                st.watched_dirs = w.by_wd.len();
                st.over_limit = w.over_limit;
            }
        }
        st_pending(&status, q.len());

        // The tidy, once all is quiet.
        let now = Instant::now();
        if activity
            && q.is_empty()
            && now.duration_since(last_activity) >= tidy_quiet
            && now.duration_since(last_tidy) >= tidy_every
        {
            let t = Instant::now();
            match keeper.tidy() {
                Ok(line) => {
                    log(&format!("tidied in {} ms: {line}", t.elapsed().as_millis()));
                    let mut st = status.lock().unwrap();
                    st.tidies += 1;
                    st.last_tidy = now_rfc3339();
                    st.windows_waiting = keeper.windows_waiting();
                }
                Err(e) => log(&format!("the tidy failed: {e:#}")),
            }
            last_tidy = Instant::now();
            activity = false;
            if let Ok(ps) = keeper.projects() {
                for p in &ps {
                    for d in &p.dirs {
                        if !w.by_dir.contains_key(d) {
                            w.add(d, &p.name);
                        }
                    }
                }
                projects = ps;
            }
        }
    }
}

fn st_pending(status: &Arc<Mutex<Status>>, n: usize) {
    if let Ok(mut st) = status.lock() {
        st.pending = n;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            settle_ms: 1000,
            max_wait_ms: 10_000,
            git_lock_ms: 60_000,
            ..Default::default()
        }
    }

    /// A burst is one flush, after it settles; a busy project goes at the
    /// max wait; git's lock holds it, up to its own cap.
    #[test]
    fn debounce() {
        let s = settings();
        let t0 = Instant::now();
        let ms = |n: u64| t0 + Duration::from_millis(n);
        let mut q = Debounce::default();
        // A checkout's burst: 500 events over 800 ms.
        for i in 0..500u64 {
            q.note(
                "alpha",
                Some(PathBuf::from(format!("/a/f{i}"))),
                ms(i * 8 / 5),
            );
        }
        assert!(q.due(ms(900), &s, |_| false).is_empty(), "still settling");
        // The last event was at 798 ms: due 1 s after it.
        assert_eq!(q.next(ms(900), &s), Some(Duration::from_millis(898)));
        let d = q.due(ms(1800), &s, |_| false);
        assert_eq!(d.len(), 1);
        assert_eq!((d[0].0.as_str(), d[0].1.len(), d[0].2), ("alpha", 500, 500));
        assert!(q.is_empty());

        // A build writing every 500 ms never settles: it goes at 10 s.
        for i in 0..30u64 {
            q.note("beta", None, ms(10_000 + i * 500));
            let due = q.due(ms(10_000 + i * 500), &s, |_| false);
            if i * 500 >= 10_000 {
                assert_eq!(due.len(), 1, "at {i}");
                break;
            }
            assert!(due.is_empty(), "at {i}");
        }

        // Held while git's lock is there; let go at max_wait + git_lock.
        let mut q = Debounce::default();
        q.note("gamma", None, ms(0));
        assert!(q.due(ms(5_000), &s, |_| true).is_empty());
        assert!(q.due(ms(69_999), &s, |_| true).is_empty());
        assert_eq!(q.due(ms(70_000), &s, |_| true).len(), 1);
        // Two projects: each on its own clock.
        q.note("a", None, ms(0));
        q.note("b", None, ms(900));
        let d = q.due(ms(1_000), &s, |_| false);
        assert_eq!(d.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!(q.len(), 1);
    }

    /// A keeper that records the flushes it is asked for.
    struct Fake {
        project: Project,
        flushes: Arc<Mutex<Vec<Vec<String>>>>,
    }

    impl Keeper for Fake {
        fn projects(&mut self) -> Result<Vec<Project>> {
            Ok(vec![self.project.clone()])
        }
        fn flush(&mut self, projects: &[String]) -> Result<String> {
            self.flushes.lock().unwrap().push(projects.to_vec());
            Ok("ok".into())
        }
        fn tidy(&mut self) -> Result<String> {
            Ok("tidy".into())
        }
    }

    /// Real inotify on a temp tree: a burst is one flush; build output
    /// (target/) makes none; a delete flushes; a new directory is watched
    /// from then on; a path named on the socket flushes its project.
    #[test]
    fn watcher_end_to_end() {
        let t = tempfile::Builder::new()
            .prefix("agent-index-watch")
            .tempdir_in("/var/tmp")
            .unwrap();
        let p = t.path().join("p");
        std::fs::create_dir_all(p.join("src")).unwrap();
        std::fs::create_dir_all(p.join("target/debug")).unwrap();
        std::fs::write(p.join("src/keep.rs"), "fn a() {}\n").unwrap();
        let flushes = Arc::new(Mutex::new(Vec::new()));
        let socket = t.path().join("w.sock");
        {
            let project = Project {
                name: "p".into(),
                path: p.clone(),
                git: false,
                dirs: vec![p.clone(), p.join("src")],
            };
            let flushes = Arc::clone(&flushes);
            let socket = socket.clone();
            std::thread::spawn(move || {
                let s = Settings {
                    settle_ms: 150,
                    max_wait_ms: 3000,
                    tidy_quiet_s: 3600,
                    ..Default::default()
                };
                let mut k = Fake { project, flushes };
                let _ = run(&mut k, &s, &socket, |_| {});
            });
        }
        let ask_status = || {
            ask(
                &socket,
                &Request {
                    status: true,
                    ..Default::default()
                },
                Duration::from_secs(2),
            )
        };
        let until = |n: usize| {
            let t0 = Instant::now();
            while flushes.lock().unwrap().len() < n && t0.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            flushes.lock().unwrap().len()
        };
        let t0 = Instant::now();
        while ask_status().map(|s| s.watched_dirs).unwrap_or(0) < 2 {
            assert!(
                t0.elapsed() < Duration::from_secs(5),
                "the watcher didn't start"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        // A burst: 50 files over about 100 ms.
        for i in 0..50 {
            std::fs::write(p.join(format!("src/f{i}.rs")), "fn x() {}\n").unwrap();
            if i % 10 == 0 {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert_eq!(until(1), 1);
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(flushes.lock().unwrap().len(), 1, "one flush for the burst");
        assert_eq!(flushes.lock().unwrap()[0], ["p"]);
        // Build output: nothing.
        for i in 0..100 {
            std::fs::write(p.join(format!("target/debug/o{i}")), "x").unwrap();
        }
        std::fs::create_dir_all(p.join("target/release")).unwrap();
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(flushes.lock().unwrap().len(), 1, "target/ makes no flush");
        // A delete.
        std::fs::remove_file(p.join("src/f0.rs")).unwrap();
        assert_eq!(until(2), 2);
        // A new directory, then a file in it.
        std::fs::create_dir_all(p.join("src/new")).unwrap();
        assert_eq!(until(3), 3);
        std::fs::write(p.join("src/new/n.rs"), "fn n() {}\n").unwrap();
        assert_eq!(until(4), 4, "the new directory is watched");
        // A path named on the socket (a hook).
        let st = ask(
            &socket,
            &Request {
                paths: vec![p.join("src/keep.rs").to_string_lossy().into_owned()],
                status: false,
            },
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(st.queued, 1);
        assert_eq!(until(5), 5);
        let st = ask_status().unwrap();
        assert_eq!((st.flushes, st.touched, st.outside), (5, 1, 0), "{st:?}");
    }

    #[test]
    fn names() {
        for n in [
            "target",
            "node_modules",
            ".git",
            "x.rs~",
            ".x.rs.swp",
            "4913",
            ".#x",
        ] {
            assert!(ignored_name(n), "{n}");
        }
        for n in ["src", "main.rs", "README.md", "build.rs"] {
            assert!(!ignored_name(n), "{n}");
        }
    }
}
