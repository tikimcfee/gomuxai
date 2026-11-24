//! `agent index summarize` on the pg store (docs/INDEX-V2.md, "Summaries
//! and asks, by a local model"): picks the files of full-depth projects
//! that get a summary, reports what it skipped and why, calls the local
//! model (crate::summarize) a few dozen requests at a time, and writes each
//! answer into `summaries` keyed by (content key, fingerprint). A run is a
//! row in `runs` whose stats are rewritten as it goes and when it ends. The
//! sync then shows a fresh summary over the seed (sync::read_overlay) and
//! queues the changed cards for embedding.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::json;

use super::Pg;
use crate::summarize::{
    self as sm, CallError, Candidate, Client, Entry, Facts, Fingerprint, Input, ServerInfo,
};

/// Numbered files of one extension in a directory past which it is a
/// program's records.
pub const NUMBERED_SIBLINGS: usize = 50;

/// How often the health watcher asks the server; 3 failures in a row stop
/// the run.
const HEALTH_EVERY_MS: u64 = 2_000;

/// What to summarize.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Projects; empty is every full-depth project.
    pub projects: Vec<String>,
    /// The corpora whose files are candidates (`code`, `prose`); empty is
    /// code.
    pub corpora: Vec<String>,
    /// `project:dir/` prefixes the files must be under; empty is all.
    pub prefixes: Vec<String>,
    /// At most this many files per project (or prefix), sampled evenly; 0
    /// is all.
    pub limit: usize,
    /// Summarize again what failed under this fingerprint.
    pub retry_failed: bool,
    /// The build's commit, for the fingerprint.
    pub code: String,
    /// A file the stats are written to as the run goes.
    pub stats_path: Option<PathBuf>,
    /// Projects left out (their files counted as skipped).
    pub skip_projects: Vec<String>,
    /// `project:dir/` prefixes left out (their files counted as skipped).
    pub skip_prefixes: Vec<String>,
    /// The bottom-up pass: the units are directories, projects and parts
    /// (`pg::dirsum`), under the directory prompt's fingerprint.
    pub dirs: bool,
    /// For a fork of an upstream tree: the only files of that project to
    /// summarize (the fork's own commits'), by path in the project; its
    /// other files are skipped as upstream.
    pub own_files: BTreeMap<String, HashSet<String>>,
    /// One file's deadline (default [`DEFAULT_FILE_TIMEOUT`]); a file past it
    /// fails and is kept as failed, never retried by a resume.
    pub file_timeout: Option<Duration>,
    /// The watchdog: no file finished for this long stops the run.
    pub stall: Option<Duration>,
    /// The whole run's cap; past it the run stops cleanly.
    pub max_wall: Option<Duration>,
    /// Over `max_error_rate` failed among the last `error_window` files
    /// stops the run (0 window: no such check).
    pub error_window: usize,
    pub max_error_rate: f64,
}

/// A file's deadline unless the options say otherwise.
pub const DEFAULT_FILE_TIMEOUT: Duration = Duration::from_secs(600);

/// Counts of one group in the coverage report.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Group {
    pub summarize: usize,
    pub skipped: BTreeMap<String, usize>,
}

/// What would be summarized and what is skipped, and why.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Coverage {
    /// Files of full-depth projects in the selection.
    pub files: usize,
    pub summarize: usize,
    pub skipped: usize,
    pub by_reason: BTreeMap<String, usize>,
    pub by_project: BTreeMap<String, Group>,
    pub by_kind: BTreeMap<String, Group>,
    /// The 20 directories with the most skipped files: (project:dir, count,
    /// its commonest reason).
    pub top_skipped_dirs: Vec<(String, usize, String)>,
    /// After `limit`'s sampling.
    pub selected: usize,
    /// What a bottom-up pass over the selection would summarize after the
    /// files: the directories holding a selected file at any depth, and
    /// the projects.
    pub bottom_up_dirs: usize,
    pub bottom_up_projects: usize,
    /// Kept files in a crate copied from elsewhere: its nearest Cargo or
    /// npm manifest names a repository that isn't the project's (a
    /// heuristic, marked, never a skip). By the crate's directory.
    pub copied_files: usize,
    pub copied: BTreeMap<String, Copied>,
    /// The selected files' ids (only in the `--out` file).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub kept: Vec<String>,
    pub filter_version: &'static str,
}

/// A crate copied into a project from elsewhere.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Copied {
    pub files: usize,
    pub repository: String,
}

/// A manifest's `repository` (Cargo.toml's key, package.json's string or
/// `url`); empty when it names none.
pub fn manifest_repository(name: &str, text: &str) -> String {
    static TOML: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?m)^\s*repository\s*=\s*"([^"]*)""#).unwrap()
    });
    if name == "Cargo.toml" {
        return TOML
            .captures(text)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
    }
    if name == "package.json" {
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        return match v.get("repository") {
            Some(serde_json::Value::String(s)) => s.trim().to_string(),
            Some(r) => r
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string(),
            None => String::new(),
        };
    }
    String::new()
}

/// Whether a repository URL is another's than `project`'s (its name, any
/// case, `-` and `_` aside, isn't in it).
pub fn foreign_repository(repo: &str, project: &str) -> bool {
    let norm = |s: &str| {
        s.to_lowercase()
            .chars()
            .filter(|c| c.is_alphanumeric())
            .collect::<String>()
    };
    !repo.is_empty() && !norm(repo).contains(&norm(project))
}

/// The kept files in copied crates, by crate directory (`project:dir`):
/// each file goes to its nearest manifest among the candidates.
fn copied_crates(cands: &[Candidate], keep: &[Candidate]) -> BTreeMap<String, Copied> {
    let mut repos: HashMap<String, String> = HashMap::new();
    for c in cands {
        let (dir, name) = c.rel.rsplit_once('/').unwrap_or(("", &c.rel));
        if name != "Cargo.toml" && name != "package.json" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&c.path) else {
            continue;
        };
        repos.insert(
            format!("{}:{dir}", c.project),
            manifest_repository(name, &text),
        );
    }
    let mut out: BTreeMap<String, Copied> = BTreeMap::new();
    for c in keep {
        let mut d = c.rel.as_str();
        while let Some((up, _)) = d.rsplit_once('/') {
            d = up;
            let key = format!("{}:{up}", c.project);
            if let Some(r) = repos.get(&key) {
                if foreign_repository(r, &c.project) {
                    let e = out.entry(key).or_default();
                    e.files += 1;
                    e.repository = r.clone();
                }
                break;
            }
        }
    }
    out
}

/// The directories holding one of `files` at any depth (`project:dir`),
/// and the projects: what a bottom-up pass would summarize after them.
pub fn bottom_up(files: &[Candidate]) -> (usize, usize) {
    let mut dirs = HashSet::new();
    let mut projects = HashSet::new();
    for c in files {
        projects.insert(c.project.as_str());
        let mut d = c.rel.as_str();
        while let Some((up, _)) = d.rsplit_once('/') {
            dirs.insert(format!("{}:{up}", c.project));
            d = up;
        }
    }
    (dirs.len(), projects.len())
}

/// Files of full-depth projects as candidates, in id order.
pub fn candidates(pg: &Pg, o: &Options) -> Result<Vec<Candidate>> {
    let corpora = if o.corpora.is_empty() {
        vec!["code".to_string()]
    } else {
        o.corpora.clone()
    };
    let rows = pg.query(
        "WITH f AS (
             SELECT e.id, e.project, e.rel, e.path, e.file_kind, e.size, e.content_key, e.seed, e.no_content
             FROM entities e JOIN projects p ON p.name = e.project
             WHERE e.kind = 'file' AND e.corpus = ANY($2::text[]) AND p.crawled_depth = 'full'
               AND (cardinality($1::text[]) = 0 OR e.project = ANY($1)))
         SELECT f.*, coalesce(s.names, '{}') FROM f LEFT JOIN (
             SELECT parent, array_agg(name ORDER BY line, name) AS names FROM entities
             WHERE kind = 'symbol' AND parent IN (SELECT id FROM f) GROUP BY parent) s ON s.parent = f.id
         ORDER BY f.id",
        &[&o.projects, &corpora],
    )?;
    let mut out: Vec<Candidate> = rows
        .iter()
        .map(|r| Candidate {
            id: r.get(0),
            project: r.get(1),
            rel: r.get(2),
            path: r.get(3),
            file_kind: r.get(4),
            size: r.get::<_, i64>(5).max(0) as u64,
            content_key: r.get(6),
            seed: r.get(7),
            no_content: r.get(8),
            symbols: r.get(9),
            ..Default::default()
        })
        .collect();
    if !o.prefixes.is_empty() {
        out.retain(|c| o.prefixes.iter().any(|p| c.id.starts_with(p.as_str())));
    }
    Ok(out)
}

/// Reads a file for the summarizer: its text, or why not.
fn read(c: &Candidate) -> Result<String, &'static str> {
    let b = std::fs::read(&c.path).map_err(|_| "unreadable")?;
    if b.len() as u64 != c.size {
        return Err("changed since the crawl");
    }
    if crate::files::is_binary(&b) {
        return Err("binary");
    }
    String::from_utf8(b).map_err(|_| "not UTF-8 text")
}

/// The files that get a summary, and the coverage report: name rules
/// first (the sensitive-name filter before any read), numbered siblings,
/// then the text's.
pub fn select(cands: &[Candidate], cfg: &crate::config::Summaries) -> (Vec<Candidate>, Coverage) {
    select_with(cands, cfg, &Options::default())
}

/// [`select`], with whole projects left out ("project left out") and forks
/// cut to their own files ("upstream, not the fork's own").
pub fn select_with(
    cands: &[Candidate],
    cfg: &crate::config::Summaries,
    o: &Options,
) -> (Vec<Candidate>, Coverage) {
    let numbered = sm::numbered_dirs(cands.iter(), NUMBERED_SIBLINGS);
    let mut cov = Coverage {
        files: cands.len(),
        filter_version: sm::FILTER_VERSION,
        ..Default::default()
    };
    let mut keep = Vec::new();
    let mut dirs: HashMap<String, BTreeMap<String, usize>> = HashMap::new();
    for c in cands {
        let reason = o
            .skip_projects
            .contains(&c.project)
            .then_some("project left out")
            .or_else(|| {
                o.skip_prefixes
                    .iter()
                    .any(|p| c.id.starts_with(p.as_str()))
                    .then_some("prefix left out")
            })
            .or_else(|| {
                o.own_files
                    .get(&c.project)
                    .is_some_and(|own| !own.contains(&c.rel))
                    .then_some("upstream, not the fork's own")
            })
            .or_else(|| sm::skip_by_name(c, cfg))
            .or_else(|| {
                numbered
                    .contains(&sm::dir_key(c))
                    .then_some("numbered siblings (50+)")
            })
            .or_else(|| match read(c) {
                Ok(t) => sm::skip_by_content(c, &t, cfg),
                Err(r) => Some(r),
            });
        let p = cov.by_project.entry(c.project.clone()).or_default();
        let k = cov.by_kind.entry(c.file_kind.clone()).or_default();
        match reason {
            None => {
                p.summarize += 1;
                k.summarize += 1;
                cov.summarize += 1;
                keep.push(c.clone());
            }
            Some(r) => {
                *p.skipped.entry(r.into()).or_default() += 1;
                *k.skipped.entry(r.into()).or_default() += 1;
                *cov.by_reason.entry(r.into()).or_default() += 1;
                cov.skipped += 1;
                *dirs
                    .entry(sm::dir_key(c))
                    .or_default()
                    .entry(r.into())
                    .or_default() += 1;
            }
        }
    }
    let mut top: Vec<(String, usize, String)> = dirs
        .into_iter()
        .map(|(d, rs)| {
            let n = rs.values().sum();
            let main = rs
                .iter()
                .max_by_key(|(_, k)| **k)
                .map(|(r, _)| r.clone())
                .unwrap_or_default();
            (d, n, main)
        })
        .collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(20);
    cov.top_skipped_dirs = top;
    cov.copied = copied_crates(cands, &keep);
    cov.copied_files = cov.copied.values().map(|c| c.files).sum();
    (keep, cov)
}

/// At most `limit` per project (or per prefix), evenly spaced in id order.
pub fn sample(files: Vec<Candidate>, o: &Options) -> Vec<Candidate> {
    if o.limit == 0 {
        return files;
    }
    let group = |c: &Candidate| -> String {
        o.prefixes
            .iter()
            .find(|p| c.id.starts_with(p.as_str()))
            .cloned()
            .unwrap_or_else(|| c.project.clone())
    };
    let mut groups: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for c in files {
        groups.entry(group(&c)).or_default().push(c);
    }
    let mut out = Vec::new();
    for (_, v) in groups {
        if v.len() <= o.limit {
            out.extend(v);
        } else {
            out.extend((0..o.limit).map(|i| v[i * v.len() / o.limit].clone()));
        }
    }
    out
}

/// What a run did, rewritten into its `runs` row as it goes.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Stats {
    pub run_id: i64,
    pub fingerprint: String,
    pub writer: String,
    pub selected: usize,
    /// Already summarized (or failed) under this fingerprint: skipped.
    pub done_before: usize,
    /// A summary of the same content under another id, copied over.
    pub copied: usize,
    pub todo: usize,
    pub written: usize,
    pub failed: BTreeMap<String, usize>,
    /// Failed at the first attempt, for a reason: the second may have passed.
    pub first_attempt_failed: BTreeMap<String, usize>,
    pub skipped_at_read: BTreeMap<String, usize>,
    /// Requests the run gave up on when the server was gone (not written).
    pub not_answered: usize,
    /// Files the server didn't answer in time, by reason: no row, a resume
    /// asks again (the first 500 ids in `pending_ids`).
    pub pending: BTreeMap<String, usize>,
    pub pending_ids: Vec<String>,
    pub transport_retries: usize,
    pub second_attempts: usize,
    pub salvaged: usize,
    pub clipped: usize,
    pub trimmed: usize,
    pub asks_stripped: usize,
    pub asks_repeated_dropped: usize,
    pub entries_left_under_2_asks: usize,
    pub redactions_in: usize,
    pub in_tokens: u64,
    pub out_tokens: u64,
    pub wall_s: f64,
    pub files_per_s: f64,
    pub prefill_tok_s: f64,
    pub decode_tok_s: f64,
    pub paused_s: f64,
    pub aborted: Option<String>,
    pub finished: bool,
    /// The status line's figures: files of the selection (`selected`) that
    /// are done (written, failed, done before), what is left, skipped by
    /// the filters, errors (failed and not answered), and the time left at
    /// this run's rate.
    pub total: usize,
    pub done: usize,
    pub remaining: usize,
    pub skipped: usize,
    pub errors: usize,
    pub eta_s: f64,
    pub updated: String,
}

/// One file's outcome, from a worker.
enum Outcome {
    Done {
        c: Candidate,
        e: Entry,
        w: Work,
    },
    Failed {
        c: Candidate,
        reason: String,
        w: Work,
    },
    Skipped(&'static str),
    /// The run stopped (server gone) before it was answered.
    NotAnswered,
    /// The server didn't answer in time (5xx, timeouts, the file's
    /// deadline): no row, so a resume asks again; logged in the stats.
    Pending {
        id: String,
        reason: &'static str,
    },
}

/// What one file's requests cost.
#[derive(Default)]
struct Work {
    input_sha: String,
    attempt: u32,
    clipped: bool,
    salvaged: bool,
    in_tokens: u64,
    out_tokens: u64,
    ms: u128,
    redactions: usize,
    retries: usize,
    first_fail: Option<String>,
}

/// Shared between workers: stop flags and the connection-error count.
struct Shared {
    cfg: crate::config::Summaries,
    client: Client,
    served: String,
    stop: AtomicBool,
    conn_errors: AtomicUsize,
    abort: Mutex<Option<String>>,
    paused_ms: AtomicUsize,
    /// One file's deadline, all its calls and retries together.
    file_timeout: Duration,
}

impl Shared {
    /// One with no server, for the stats before the workers start.
    fn idle(cfg: &crate::config::Summaries) -> Shared {
        Shared {
            cfg: cfg.clone(),
            client: Client::new(cfg),
            served: String::new(),
            stop: AtomicBool::new(false),
            conn_errors: AtomicUsize::new(0),
            abort: Mutex::new(None),
            paused_ms: AtomicUsize::new(0),
            file_timeout: DEFAULT_FILE_TIMEOUT,
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Stops the run, saying why (the first reason wins).
    fn give_up(&self, why: String) {
        let mut a = self.abort.lock().unwrap();
        if a.is_none() {
            *a = Some(why);
        }
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Counts a call's result toward the dead-server rule: a connection
    /// error counts, an answer resets the count, a timeout or 5xx neither.
    fn note<T>(&self, r: &Result<T, CallError>) {
        match r {
            Err(CallError::Connection(e)) => {
                let n = self.conn_errors.fetch_add(1, Ordering::SeqCst) + 1;
                if n >= self.cfg.max_connection_errors.max(1) {
                    self.give_up(format!(
                        "{n} connection errors in a row (last: {e}): the server at {} is gone",
                        self.cfg.endpoint
                    ));
                }
            }
            Ok(_) => self.conn_errors.store(0, Ordering::SeqCst),
            Err(_) => {}
        }
    }

    /// A call with transport retries, counted; none starts once the file's
    /// deadline has passed.
    fn call<T>(
        &self,
        retries: &Cell<usize>,
        deadline: Instant,
        mut f: impl FnMut() -> Result<T, CallError>,
    ) -> Result<T, CallError> {
        let stop = || self.stopped() || Instant::now() > deadline;
        sm::with_retries(
            &self.cfg,
            &stop,
            |_| retries.set(retries.get() + 1),
            || {
                if Instant::now() > deadline {
                    return Err(CallError::Deadline);
                }
                let r = f();
                self.note(&r);
                r
            },
        )
    }

    fn wait_unpaused(&self) {
        if self.cfg.pause_file.is_empty() {
            return;
        }
        let t = Instant::now();
        while std::path::Path::new(&self.cfg.pause_file).exists() && !self.stopped() {
            std::thread::sleep(Duration::from_millis(500));
        }
        self.paused_ms
            .fetch_add(t.elapsed().as_millis() as usize, Ordering::SeqCst);
    }
}

/// One file: read, scrub, clip, ask (a second attempt at seed + 1 when the
/// answer fails its checks), check.
fn one(sh: &Shared, c: Candidate) -> Outcome {
    let t0 = Instant::now();
    let deadline = t0 + sh.file_timeout;
    let cfg = &sh.cfg;
    // A directory unit's input is composed (README, listing, children's
    // entries), not read; the content filters are a file's.
    let text = match &c.composed {
        Some(t) => t.clone(),
        None => match read(&c) {
            Ok(t) => t,
            Err(r) => return Outcome::Skipped(r),
        },
    };
    if c.composed.is_none()
        && let Some(r) = sm::skip_by_content(&c, &text, cfg)
    {
        return Outcome::Skipped(r);
    }
    let mut w = Work::default();
    let (clean, n) = sm::scrub(&text, &cfg.scrub_words);
    w.redactions = n;
    let (seed, _) = sm::scrub(&c.seed, &cfg.scrub_words);
    let served = sh.served.clone();
    let retries = Cell::new(0usize);
    let mut count = |s: &str| sh.call(&retries, deadline, || sh.client.tokens(&served, s));
    let fail = |c: Candidate, mut w: Work, e: CallError| {
        w.retries = retries.get();
        w.ms = t0.elapsed().as_millis();
        match e {
            _ if sh.stopped() => Outcome::NotAnswered,
            // The server's slowness, not the file's: no row, a resume asks again.
            _ if Instant::now() > deadline => Outcome::Pending {
                id: c.id,
                reason: "file deadline",
            },
            CallError::Connection(_) => Outcome::NotAnswered,
            CallError::Transient(_) => Outcome::Pending {
                id: c.id,
                reason: "transient (retries spent)",
            },
            CallError::TooLong(_) => Outcome::Failed {
                c,
                reason: "too long for the server".into(),
                w,
            },
            CallError::Bad(_) => Outcome::Failed {
                c,
                reason: "bad request".into(),
                w,
            },
            CallError::Deadline => Outcome::Pending {
                id: c.id,
                reason: "file deadline",
            },
        }
    };
    let mut clip = if c.composed.is_some() {
        // Composed to fit (`dirsum` groups a large directory's entries);
        // the loop below still cuts it if the server counts more.
        sm::Clipped {
            content: clean.clone(),
            clipped: false,
            note: String::new(),
        }
    } else {
        match sm::clip_file(&clean, &c.file_kind, cfg, &mut count) {
            Ok(x) => x,
            Err(e) => return fail(c, w, e),
        }
    };
    // The whole prompt within its bound: shrink the text while it isn't.
    let mut user = String::new();
    for _ in 0..4 {
        user = if c.composed.is_some() {
            sm::render_dir(&sm::DirInput {
                project: &c.project,
                rel: &c.rel,
                kind: &c.file_kind,
                part: &c.part,
                seed: &seed,
                content: &clip.content,
            })
        } else {
            let (syms, sampled) = sm::symbol_list(&c.symbols, cfg.symbols_chars, clip.clipped);
            sm::render(&Input {
                project: &c.project,
                rel: &c.rel,
                kind: &c.file_kind,
                seed: &seed,
                symbols: &syms,
                sampled,
                clip_note: &clip.note,
                content: &clip.content,
            })
        };
        let total = match count(&user) {
            Ok(n) => n + 120,
            Err(e) => return fail(c, w, e),
        };
        if total <= cfg.max_input_tokens {
            break;
        }
        let ctoks = match count(&clip.content) {
            Ok(n) => n,
            Err(e) => return fail(c, w, e),
        };
        let target = ctoks
            .saturating_sub(total - cfg.max_input_tokens + 50)
            .max(200);
        let tail = false;
        match sm::clip_tokens(&clip.content, target, tail, &mut count) {
            Ok((s, _)) => {
                clip.content = s;
                clip.clipped = true;
                if clip.note.is_empty() {
                    clip.note = " (clipped: its first part only)".into();
                }
            }
            Err(e) => return fail(c, w, e),
        }
    }
    w.clipped = clip.clipped;
    w.input_sha = sm::sha256(user.as_bytes());
    let facts = Facts {
        rel: &c.rel,
        project: &c.project,
        text: &text,
        symbols: &c.symbols,
        scrub_words: &cfg.scrub_words,
    };
    let mut reason = String::new();
    for attempt in 1..=2u32 {
        w.attempt = attempt;
        let body = if c.composed.is_some() {
            sm::body_dir(cfg, &served, &user, attempt)
        } else {
            sm::body(cfg, &served, &user, attempt)
        };
        let ans = match sh.call(&retries, deadline, || sh.client.chat(&body)) {
            Ok(a) => a,
            Err(e) => return fail(c, w, e),
        };
        w.in_tokens += ans.prompt_tokens;
        w.out_tokens += ans.completion_tokens;
        let (obj, salvaged) = sm::salvage(&sm::strip_reasoning(&ans.content));
        let res = match obj {
            Some(mut v) if v.is_object() => {
                if c.composed.is_some()
                    && let Some(t) = v.get("summary").and_then(|x| x.as_str())
                {
                    v["summary"] = json!(sm::strip_dir_lead(t));
                }
                sm::check(&v, &facts).map_err(String::from)
            }
            _ => Err("no usable JSON".to_string()),
        };
        match res {
            Ok(e) => {
                w.salvaged = salvaged;
                w.retries = retries.get();
                w.ms = t0.elapsed().as_millis();
                return Outcome::Done { c, e, w };
            }
            Err(r) => {
                if attempt == 1 {
                    w.first_fail = Some(r.clone());
                }
                reason = r;
            }
        }
        if sh.stopped() {
            return Outcome::NotAnswered;
        }
    }
    w.retries = retries.get();
    w.ms = t0.elapsed().as_millis();
    Outcome::Failed { c, reason, w }
}

/// Writes `stats` into the run's row and the stats file.
fn save(pg: &Pg, stats: &Stats, path: Option<&PathBuf>) -> Result<()> {
    let v = serde_json::to_value(stats)?;
    pg.execute(
        "UPDATE runs SET coverage = $2, finished = CASE WHEN $3 THEN now() ELSE finished END WHERE id = $1",
        &[&stats.run_id, &v, &stats.finished],
    )?;
    if let Some(p) = path {
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(stats)?)?;
        std::fs::rename(&tmp, p)?;
    }
    Ok(())
}

/// The server's identity and the fingerprint, asking the server.
pub fn fingerprint(
    cfg: &crate::config::Summaries,
    code: &str,
) -> Result<(Client, ServerInfo, Fingerprint)> {
    if cfg.endpoint.is_empty() {
        bail!("no summarizer endpoint: set [llm.summaries] endpoint in the index configuration");
    }
    let client = Client::new(cfg);
    let info = client.info(&cfg.model)?;
    let fp = Fingerprint::new(cfg, &info, code);
    Ok((client, info, fp))
}

/// Summarizes `files` (already selected). Returns the run's stats; Err
/// after the stats are saved when the server went away.
pub fn run(
    pg: &Pg,
    files: Vec<Candidate>,
    o: &Options,
    coverage: &Coverage,
    progress: &mut dyn FnMut(&Stats),
) -> Result<Stats> {
    let cfg = &pg.cfg.llm.summaries;
    let (client, info, mut fp) = fingerprint(cfg, &o.code)?;
    if o.dirs {
        fp = sm::Fingerprint::new_dirs(cfg, &info, &o.code);
    }
    let key = fp.key();
    let writer = fp.writer();
    let run_id: i64 = pg
        .query(
            "INSERT INTO runs (kind, config, fingerprint, started) VALUES ('summarize', $1, $2, now()) RETURNING id",
            &[
                &json!({"fingerprint": fp, "server": info, "options": {
                    "projects": o.projects, "prefixes": o.prefixes, "limit": o.limit,
                    "retry_failed": o.retry_failed, "dirs": o.dirs,
                    "skip_prefixes": o.skip_prefixes}, "coverage": coverage,
                    "index_config": pg.cfg.fingerprint()}),
                &key,
            ],
        )?
        .first()
        .map(|r| r.get(0))
        .context("no run id")?;
    let mut stats = Stats {
        run_id,
        fingerprint: key.clone(),
        writer: writer.clone(),
        selected: files.len(),
        skipped: coverage.skipped,
        ..Default::default()
    };

    // Done under this fingerprint: by content key, never by id alone.
    let statuses: &[&str] = if o.retry_failed {
        &["live", "superseded", "part"]
    } else {
        &["live", "superseded", "failed", "part"]
    };
    let statuses: Vec<String> = statuses.iter().map(|s| s.to_string()).collect();
    let done: HashSet<String> = pg
        .query(
            "SELECT DISTINCT content_key FROM summaries WHERE fingerprint = $1 AND status = ANY($2)",
            &[&key, &statuses],
        )?
        .iter()
        .map(|r| r.get(0))
        .collect();
    let mut todo = Vec::new();
    for c in files {
        if c.content_key.is_empty() || !done.contains(&c.content_key) {
            todo.push(c);
            continue;
        }
        stats.done_before += 1;
        // The same content summarized under another id (a rename, a copy):
        // that summary, for this id too.
        let n = pg.execute(
            "INSERT INTO summaries (entity_id, content_key, fingerprint, run_id, status, summary, why, asks,
                                    dropped_asks, writer, input_sha, attempt, clipped, meta)
             SELECT $1, content_key, fingerprint, $3, 'live', summary, why, asks, dropped_asks, writer,
                    input_sha, attempt, clipped, jsonb_build_object('copied_from', entity_id)
             FROM summaries s WHERE s.content_key = $2 AND s.fingerprint = $4 AND s.status = 'live'
               AND NOT EXISTS (SELECT 1 FROM summaries t WHERE t.entity_id = $1 AND t.content_key = $2
                               AND t.fingerprint = $4 AND t.status IN ('live', 'failed'))
             ORDER BY s.id DESC LIMIT 1",
            &[&c.id, &c.content_key, &run_id, &key],
        )?;
        if n > 0 {
            pg.execute(
                "UPDATE summaries SET status = 'superseded' WHERE entity_id = $1 AND status = 'live'
                   AND id < (SELECT max(id) FROM summaries WHERE entity_id = $1)",
                &[&c.id],
            )?;
            stats.copied += 1;
        }
    }
    stats.todo = todo.len();
    rates(&mut stats, Instant::now(), &Shared::idle(cfg));
    save(pg, &stats, o.stats_path.as_ref())?;

    let sh = Arc::new(Shared {
        cfg: cfg.clone(),
        client: client.clone(),
        served: info.served.clone(),
        stop: AtomicBool::new(false),
        conn_errors: AtomicUsize::new(0),
        abort: Mutex::new(None),
        paused_ms: AtomicUsize::new(0),
        file_timeout: o.file_timeout.unwrap_or(DEFAULT_FILE_TIMEOUT),
    });
    let t0 = Instant::now();
    let next = Arc::new(AtomicUsize::new(0));
    let todo = Arc::new(todo);
    let (tx, rx) = mpsc::channel::<Outcome>();
    let mut write_err: Option<anyhow::Error> = None;
    // Detached workers: one stuck in a request to a server that died
    // without closing its connections can't hold the run open; once the
    // run stops, it waits a few seconds for answers in flight and goes.
    for _ in 0..cfg.concurrency.max(1) {
        let (tx, sh, next, todo) = (tx.clone(), sh.clone(), next.clone(), todo.clone());
        std::thread::spawn(move || {
            loop {
                sh.wait_unpaused();
                if sh.stopped() {
                    break;
                }
                let i = next.fetch_add(1, Ordering::SeqCst);
                let Some(c) = todo.get(i) else { break };
                let out = one(&sh, c.clone());
                if tx.send(out).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);
    // The health watcher: a server that stops answering /health stops the
    // run within seconds, whatever the requests in flight are doing.
    let finished = Arc::new(AtomicBool::new(false));
    {
        let (sh, finished) = (sh.clone(), finished.clone());
        std::thread::spawn(move || {
            let mut bad = 0;
            while !sh.stopped() && !finished.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(HEALTH_EVERY_MS));
                if sh.client.healthy() {
                    bad = 0;
                    continue;
                }
                bad += 1;
                if bad >= 3 {
                    sh.give_up(format!(
                        "the server at {} stopped answering /health",
                        sh.cfg.endpoint
                    ));
                }
            }
        });
    }
    let mut last = Instant::now();
    let mut processed = 0usize;
    let mut stopped_at: Option<Instant> = None;
    // The watchdog's view: when a file last finished, and the last
    // `error_window` files' outcomes (true: failed).
    let mut last_done = Instant::now();
    let mut window: std::collections::VecDeque<bool> = std::collections::VecDeque::new();
    let stall = o.stall.unwrap_or(Duration::MAX);
    let max_wall = o.max_wall.unwrap_or(Duration::MAX);
    loop {
        let got = rx.recv_timeout(Duration::from_millis(500));
        if let Ok(out) = got {
            processed += 1;
            let settled = matches!(out, Outcome::Done { .. } | Outcome::Failed { .. });
            if settled {
                last_done = Instant::now();
            }
            // Failed and pending files are the error rate's.
            if (settled || matches!(out, Outcome::Pending { .. })) && o.error_window > 0 {
                window.push_back(!matches!(out, Outcome::Done { .. }));
                if window.len() > o.error_window {
                    window.pop_front();
                }
            }
            if write_err.is_none()
                && let Err(e) = record(pg, &mut stats, out, run_id, &key, &writer)
            {
                // The store is gone: stop asking.
                sh.stop.store(true, Ordering::SeqCst);
                write_err = Some(e);
            }
        } else if let Err(mpsc::RecvTimeoutError::Disconnected) = got {
            break;
        }
        // The watchdog. Time paused for the speech server isn't a stall.
        if !sh.cfg.pause_file.is_empty() && std::path::Path::new(&sh.cfg.pause_file).exists() {
            last_done = Instant::now();
        }
        if !sh.stopped() {
            if last_done.elapsed() > stall {
                sh.give_up(format!(
                    "no file finished for {} minutes",
                    stall.as_secs() / 60
                ));
            } else if t0.elapsed() > max_wall {
                sh.give_up(format!(
                    "the wall-clock cap of {} minutes was reached",
                    max_wall.as_secs() / 60
                ));
            } else if o.error_window > 0 && window.len() >= o.error_window {
                let bad = window.iter().filter(|b| **b).count();
                if bad as f64 > o.max_error_rate * window.len() as f64 {
                    sh.give_up(format!(
                        "{bad} of the last {} files failed (over {:.0}%)",
                        window.len(),
                        o.max_error_rate * 100.0
                    ));
                }
            }
        }
        if sh.stopped() {
            let at = *stopped_at.get_or_insert_with(Instant::now);
            if at.elapsed() > Duration::from_secs(5) {
                break;
            }
        }
        if last.elapsed() > Duration::from_secs(10) {
            last = Instant::now();
            rates(&mut stats, t0, &sh);
            let _ = save(pg, &stats, o.stats_path.as_ref());
            progress(&stats);
        }
    }
    finished.store(true, Ordering::SeqCst);
    // Files started and never answered (their workers left behind).
    let started = next.load(Ordering::SeqCst).min(todo.len());
    stats.not_answered += started.saturating_sub(processed);
    rates(&mut stats, t0, &sh);
    stats.aborted = sh.abort.lock().unwrap().clone();
    if write_err.is_none() {
        match drop_repeated_asks(pg, &key) {
            Ok((n, under)) => {
                stats.asks_repeated_dropped = n;
                stats.entries_left_under_2_asks = under;
            }
            Err(e) => write_err = Some(e),
        }
    }
    stats.finished = true;
    if let Some(e) = &write_err {
        stats.aborted = Some(format!("writing to the store failed: {e:#}"));
    }
    let _ = save(pg, &stats, o.stats_path.as_ref());
    progress(&stats);
    if let Some(e) = write_err {
        return Err(e);
    }
    if let Some(a) = &stats.aborted {
        bail!(
            "summarize stopped: {a}; {} written, {} not answered (a resume does them)",
            stats.written,
            stats.not_answered
        );
    }
    Ok(stats)
}

fn rates(stats: &mut Stats, t0: Instant, sh: &Shared) {
    let wall = t0.elapsed().as_secs_f64();
    stats.wall_s = (wall * 10.0).round() / 10.0;
    stats.paused_s =
        sh.paused_ms.load(Ordering::SeqCst) as f64 / 1000.0 / sh.cfg.concurrency.max(1) as f64;
    let failed = stats.failed.values().sum::<usize>();
    let n = stats.written + failed;
    stats.total = stats.selected;
    stats.done = stats.done_before + n;
    let at_read: usize = stats.skipped_at_read.values().sum();
    let pending: usize = stats.pending.values().sum();
    stats.remaining = stats
        .todo
        .saturating_sub(n + stats.not_answered + at_read + pending);
    stats.errors = failed + stats.not_answered + pending;
    if wall > 0.0 {
        stats.files_per_s = (n as f64 / wall * 100.0).round() / 100.0;
        stats.prefill_tok_s = (stats.in_tokens as f64 / wall).round();
        stats.decode_tok_s = (stats.out_tokens as f64 / wall).round();
    }
    stats.eta_s = if stats.files_per_s > 0.0 {
        (stats.remaining as f64 / stats.files_per_s).round()
    } else {
        0.0
    };
    stats.updated = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
}

/// One outcome into the store and the stats.
fn record(
    pg: &Pg,
    stats: &mut Stats,
    out: Outcome,
    run_id: i64,
    key: &str,
    writer: &str,
) -> Result<()> {
    let (c, w, status, reason, e) = match out {
        Outcome::Skipped(r) => {
            *stats.skipped_at_read.entry(r.into()).or_default() += 1;
            stats.skipped += 1;
            return Ok(());
        }
        Outcome::NotAnswered => {
            stats.not_answered += 1;
            return Ok(());
        }
        Outcome::Pending { id, reason } => {
            *stats.pending.entry(reason.into()).or_default() += 1;
            if stats.pending_ids.len() < 500 {
                stats.pending_ids.push(id);
            }
            return Ok(());
        }
        // A part of a large directory is kept for its directory's answer
        // (and a resume), never shown.
        Outcome::Done { c, e, w } if c.file_kind == "part" => (c, w, "part", String::new(), e),
        Outcome::Done { c, e, w } => (c, w, "live", String::new(), e),
        Outcome::Failed { c, reason, w } => (c, w, "failed", reason, Entry::default()),
    };
    stats.in_tokens += w.in_tokens;
    stats.out_tokens += w.out_tokens;
    stats.transport_retries += w.retries;
    stats.redactions_in += w.redactions;
    stats.clipped += w.clipped as usize;
    stats.salvaged += w.salvaged as usize;
    if w.attempt > 1 {
        stats.second_attempts += 1;
    }
    if let Some(r) = &w.first_fail {
        *stats.first_attempt_failed.entry(r.clone()).or_default() += 1;
    }
    if status == "live" || status == "part" {
        stats.written += 1;
        stats.trimmed += e.trimmed as usize;
        stats.asks_stripped += e.stripped.len();
    } else {
        *stats.failed.entry(reason.clone()).or_default() += 1;
    }
    let (attempt, ms) = (w.attempt as i32, w.ms.min(i32::MAX as u128) as i32);
    let (tin, tout) = (w.in_tokens as i32, w.out_tokens as i32);
    let mut cl = pg.client();
    let mut tx = cl.transaction()?;
    if status == "live" {
        tx.execute(
            "UPDATE summaries SET status = 'superseded' WHERE entity_id = $1 AND status = 'live'",
            &[&c.id],
        )?;
    }
    tx.execute(
        "INSERT INTO summaries (entity_id, content_key, fingerprint, run_id, status, reason, summary, why, asks,
                                dropped_asks, writer, input_sha, attempt, clipped, in_tokens, out_tokens, ms, meta)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18)",
        &[
            &c.id,
            &c.content_key,
            &key,
            &run_id,
            &status,
            &reason,
            &e.summary,
            &e.why,
            &e.asks,
            &e.stripped,
            &writer,
            &w.input_sha,
            &attempt,
            &w.clipped,
            &tin,
            &tout,
            &ms,
            &json!({"salvaged": w.salvaged, "trimmed": e.trimmed, "redactions_in": w.redactions,
                    "first_attempt_failed": w.first_fail, "unit": c.file_kind, "part": c.part}),
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Asks found on 4 or more entries (by distinct content) under this
/// fingerprint are generic, not a way to find one file: dropped from every
/// live entry that has them. Returns (asks dropped, entries left with
/// fewer than 2).
pub fn drop_repeated_asks(pg: &Pg, key: &str) -> Result<(usize, usize)> {
    let rows = pg.query(
        "SELECT id, content_key, asks, dropped_asks FROM summaries WHERE fingerprint = $1 AND status = 'live'",
        &[&key],
    )?;
    let mut by: HashMap<String, HashSet<String>> = HashMap::new();
    for r in &rows {
        let ck: String = r.get(1);
        for a in r.get::<_, Vec<String>>(2) {
            by.entry(sm::ask_norm(&a)).or_default().insert(ck.clone());
        }
    }
    let generic: HashSet<&String> = by
        .iter()
        .filter(|(_, v)| v.len() >= 4)
        .map(|(k, _)| k)
        .collect();
    let (mut dropped, mut under) = (0, 0);
    for r in &rows {
        let asks: Vec<String> = r.get(2);
        let (keep, gone): (Vec<String>, Vec<String>) = asks
            .into_iter()
            .partition(|a| !generic.contains(&sm::ask_norm(a)));
        if gone.is_empty() {
            continue;
        }
        dropped += gone.len();
        under += (keep.len() < 2) as usize;
        let mut d: Vec<String> = r.get(3);
        d.extend(gone);
        let id: i64 = r.get(0);
        pg.execute(
            "UPDATE summaries SET asks = $2, dropped_asks = $3 WHERE id = $1",
            &[&id, &keep, &d],
        )?;
    }
    Ok((dropped, under))
}
