//! `agent index eval` (docs/INDEX-V2.md, "Eval"): the index measured
//! against `rg` and `fd` on graded questions and session episodes, every
//! search in process through the one search path (`serve::answer`), one
//! store per named configuration. It is the Python harness
//! (`eval/run_eval.py`, blind3_run.py, the cross runner and their scoring)
//! in Rust: the same words, scopes, baselines, ranks and numbers, and the
//! same random numbers for the bootstrap.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::pg::Pg;
use crate::pg::rerank;
use crate::pg::serve::{self, Embed};

pub mod base;
pub mod boot;
pub mod paths;
pub mod score;
pub mod sets;
pub mod words;

#[cfg(test)]
mod eval_test;

use base::{Answer, Baselines};
use paths::Paths;
use sets::{Drops, Episode, Question, Scope};

/// How a run asks and keeps.
#[derive(Debug, Clone, Serialize)]
pub struct Options {
    pub dev_root: PathBuf,
    /// Files kept per answer.
    pub limit: usize,
    /// Entities asked of the index; cut to `limit` files.
    pub index_limit: usize,
    /// Words of a plain query given to either system.
    pub max_words: usize,
    /// The index's `--kind` ("" for none).
    pub kind: String,
    /// The rg and fd baselines.
    pub rg: bool,
    /// Episodes: the whole-tree systems too.
    pub tree: bool,
    /// Questions: run_eval.py's systems (words and sentence, @project, @cwd
    /// and @root, with most_cited_files) instead of the question's scope.
    pub run_eval_systems: bool,
    pub resamples: usize,
    pub seed: u64,
    /// Top-level directories the index holds no files of (crawled
    /// shallow): rg over the tree skips them too, as rg@scope.
    pub tree_skips: Vec<String>,
    /// Episodes that ran under these directories are dropped.
    pub skip_cwd: Vec<String>,
    /// Per set, for a quick trial (0: all).
    pub max_queries: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            dev_root: PathBuf::new(),
            limit: 10,
            index_limit: 50,
            max_words: 12,
            kind: "file".into(),
            rg: true,
            tree: true,
            run_eval_systems: false,
            resamples: 10_000,
            seed: 20_261_123,
            tree_skips: Vec::new(),
            skip_cwd: vec!["/tmp".into(), "/run".into()],
            max_queries: 0,
        }
    }
}

/// An index search's answer: its files, seconds, mode, its own milliseconds.
type Found = (Vec<String>, f64, String, Option<f64>);

/// A named configuration's store, searched in process.
pub struct Searcher {
    pub name: String,
    pub pg: Pg,
    pub embed: Option<Embed>,
    pub rerank: rerank::Client,
    /// By (words, cwd, under).
    cache: HashMap<(Vec<String>, String, String), Found>,
}

impl Searcher {
    pub fn new(name: &str, pg: Pg, embed: Option<Embed>) -> Searcher {
        let rerank = rerank::Client::new(&pg.cfg.reranker);
        Searcher {
            name: name.to_string(),
            pg,
            embed,
            rerank,
            cache: HashMap::new(),
        }
    }

    /// `agent index search WORDS --json --limit N --kind K [--under U]` run
    /// in `cwd`, as the CLI builds the request: the hits' file paths (a
    /// symbol or section counts as its file; a directory or project is
    /// passed over), seconds, mode, the index's own milliseconds.
    fn search(
        &mut self,
        words: &[String],
        cwd: &str,
        under: Option<&str>,
        o: &Options,
        dev_root: &str,
    ) -> Found {
        let cwd = if Path::new(cwd).is_dir() {
            cwd
        } else {
            dev_root
        };
        let words: Vec<String> = words.iter().take(o.max_words).cloned().collect();
        let key = (
            words.clone(),
            cwd.to_string(),
            under.unwrap_or("").to_string(),
        );
        if let Some(c) = self.cache.get(&key) {
            return c.clone();
        }
        let canon = |p: &str| {
            std::fs::canonicalize(p)
                .map(|c| c.to_string_lossy().into_owned())
                .unwrap_or_else(|_| p.to_string())
        };
        let req = serve::Request {
            query: words.join(" "),
            kind: o.kind.clone(),
            under: under.map(canon).unwrap_or_default(),
            limit: o.index_limit,
            cwd: canon(cwd),
            who: "eval".into(),
            ..Default::default()
        };
        let t0 = Instant::now();
        let res = serve::answer(&self.pg, &req, self.embed.as_ref(), Some(&self.rerank));
        let dt = t0.elapsed().as_secs_f64();
        let out = match res {
            Ok(resp) => match resp.result {
                Some(r) => {
                    let own: f64 = r.ms.values().sum();
                    let hits = r
                        .hits
                        .iter()
                        .filter(|h| !matches!(h.kind.as_str(), "dir" | "project" | "root"))
                        .filter(|h| h.path.starts_with('/'))
                        .map(|h| h.path.clone())
                        .collect();
                    (hits, dt, "index".to_string(), Some(own))
                }
                None => (Vec::new(), dt, "no result".into(), None),
            },
            Err(e) => (
                Vec::new(),
                dt,
                format!(
                    "error: {}",
                    format!("{e:#}").chars().take(80).collect::<String>()
                ),
                None,
            ),
        };
        self.cache.insert(key, out.clone());
        out
    }
}

/// One system's answer to one query, as results.jsonl holds it (never the
/// question's text).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Row {
    pub set: String,
    pub qid: String,
    /// What the bootstrap resamples: the question, or the episode's session.
    pub unit: String,
    pub system: String,
    /// The configuration the system ran on ("" for rg, fd and constants).
    #[serde(default)]
    pub config: String,
    /// project or tree.
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub stratum: String,
    /// Of the first target (a graded file, an episode's file) in `top`; 0
    /// when none is.
    pub rank: usize,
    pub top: Vec<String>,
    pub found: usize,
    pub ms: f64,
    pub own_ms: Option<f64>,
    pub mode: String,
    /// The graded files and grades (questions).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub grades: BTreeMap<String, i64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub grade_project: BTreeMap<String, String>,
    /// The target files (episodes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    /// An episode's searches to reach a target: the place, in the agent's
    /// own order, of the first pattern whose top 10 holds one (0: never).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<usize>,
}

/// A set to run: graded questions or session episodes.
#[derive(Debug, Clone, Serialize)]
pub struct SetSpec {
    pub name: String,
    pub path: PathBuf,
    pub episodes: bool,
}

/// What a set held, kept and dropped.
#[derive(Debug, Clone, Serialize, Default)]
pub struct SetNote {
    pub name: String,
    pub kept: usize,
    pub dropped: Drops,
    pub detail: String,
}

/// Everything a run found.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Results {
    pub rows: Vec<Row>,
    pub sets: Vec<SetNote>,
    /// Searches the index failed (mode not "index").
    pub index_errors: usize,
}

/// A file's sha256 (hex), for a run's record of what it read.
pub fn sha256_file(p: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(p).map_err(|e| anyhow::anyhow!("reading {}: {e}", p.display()))?;
    Ok(Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// The HTTP status of a model server's `/health` (0: no answer in 5 s).
pub fn health(endpoint: &str) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(5)))
        .http_status_as_error(false)
        .build()
        .into();
    match agent
        .get(&format!("{}/health", endpoint.trim_end_matches('/')))
        .call()
    {
        Ok(r) => r.status().as_u16(),
        Err(_) => 0,
    }
}

fn rank_of(top: &[String], targets: &BTreeSet<String>) -> usize {
    top.iter()
        .position(|f| targets.contains(f))
        .map_or(0, |i| i + 1)
}

/// The first `limit` distinct files of several lists, taken a rank at a
/// time across them.
pub fn interleave(lists: &[Vec<String>], limit: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while out.len() < limit && lists.iter().any(|l| i < l.len()) {
        for l in lists {
            if i < l.len() && !out.contains(&l[i]) {
                out.push(l[i].clone());
            }
        }
        i += 1;
    }
    out.truncate(limit);
    out
}

/// The files a set cites most, the same list for every query, and how
/// many files it cites.
fn most_cited<'a>(
    lists: impl Iterator<Item = &'a BTreeSet<String>>,
    limit: usize,
) -> (Vec<String>, usize) {
    let mut cited: BTreeMap<String, usize> = BTreeMap::new();
    for l in lists {
        for t in l {
            *cited.entry(t.clone()).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = cited.into_iter().collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let n = v.len();
    (v.into_iter().take(limit).map(|(f, _)| f).collect(), n)
}

struct Ctx<'a, 'b> {
    o: &'a Options,
    paths: &'a Paths,
    /// rg held to what the index reads.
    rg: Baselines<'a>,
    /// rg over the tree, also skipping what the index holds only shallow.
    rg_tree: Baselines<'a>,
    searchers: &'b mut [Searcher],
    out: &'b mut Results,
    progress: &'b mut dyn FnMut(&str),
}

impl Ctx<'_, '_> {
    fn index(&mut self, i: usize, words: &[String], cwd: &str, under: Option<&str>) -> Answer {
        let dev = self.paths.dev_root.clone();
        let (hits, secs, mode, own) = self.searchers[i].search(words, cwd, under, self.o, &dev);
        if mode != "index" {
            self.out.index_errors += 1;
        }
        Answer {
            found: hits.len(),
            top: self.rg.top(&hits, &[]),
            secs,
            mode,
            own_ms: own,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        set: &str,
        qid: &str,
        unit: &str,
        system: &str,
        config: &str,
        a: &Answer,
        targets: &BTreeSet<String>,
        extra: impl FnOnce(&mut Row),
    ) {
        let mut row = Row {
            set: set.to_string(),
            qid: qid.to_string(),
            unit: unit.to_string(),
            system: system.to_string(),
            config: config.to_string(),
            rank: rank_of(&a.top, targets),
            top: a.top.clone(),
            found: a.found,
            ms: (a.secs * 10_000.0).round() / 10.0,
            own_ms: a.own_ms,
            mode: a.mode.clone(),
            ..Default::default()
        };
        extra(&mut row);
        self.out.rows.push(row);
    }
}

/// Runs the sets against the searchers (and the baselines). `progress` gets
/// a short line per set and a dot per query.
pub fn run(
    o: &Options,
    sets: &[SetSpec],
    searchers: &mut [Searcher],
    progress: &mut dyn FnMut(&str),
) -> Result<Results> {
    let paths = Paths::new(&o.dev_root);
    let mut out = Results::default();
    let mut seen = sets::Seen::default();
    {
        let mut c = Ctx {
            o,
            paths: &paths,
            rg: Baselines::new(&paths, o.limit, o.max_words, &[]),
            rg_tree: Baselines::new(&paths, o.limit, o.max_words, &o.tree_skips),
            searchers,
            out: &mut out,
            progress,
        };
        let mut file_no = 0usize;
        for s in sets {
            let mut drops = Drops::new();
            if s.episodes {
                let tag = "abcdefgh".as_bytes()[file_no % 8] as char;
                file_no += 1;
                let mut eps =
                    sets::load_sessions(&s.path, tag, &paths, &mut drops, &mut seen, &o.skip_cwd)?;
                if o.max_queries > 0 {
                    eps.truncate(o.max_queries);
                }
                let detail = episodes_detail(&eps);
                (c.progress)(&format!("{}: {} episodes", s.name, eps.len()));
                for e in &eps {
                    run_episode(&mut c, e);
                    (c.progress)(".");
                }
                c.out.sets.push(SetNote {
                    name: s.name.clone(),
                    kept: eps.len(),
                    dropped: drops,
                    detail,
                });
            } else {
                let mut qs = sets::load_questions(&s.path, &paths, &mut drops)?;
                if o.max_queries > 0 {
                    qs.truncate(o.max_queries);
                }
                (c.progress)(&format!("{}: {} questions", s.name, qs.len()));
                let mut by: BTreeMap<String, usize> = BTreeMap::new();
                for q in &qs {
                    *by.entry(if q.kind.is_empty() {
                        q.stratum.clone()
                    } else {
                        q.kind.clone()
                    })
                    .or_default() += 1;
                }
                if o.run_eval_systems {
                    run_questions_run_eval(&mut c, &s.name, &qs);
                } else {
                    for q in &qs {
                        run_question(&mut c, &s.name, q);
                        (c.progress)(".");
                    }
                }
                c.out.sets.push(SetNote {
                    name: s.name.clone(),
                    kept: qs.len(),
                    dropped: drops,
                    detail: format!("by kind or stratum {by:?}"),
                });
            }
        }
    }
    Ok(out)
}

fn episodes_detail(eps: &[Episode]) -> String {
    let sessions: BTreeSet<&str> = eps.iter().map(|e| e.cluster.as_str()).collect();
    let mut harness: BTreeMap<&str, usize> = BTreeMap::new();
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    for e in eps {
        *harness.entry(&e.harness).or_default() += 1;
        *kinds.entry(&e.target_kind).or_default() += 1;
    }
    format!(
        "{} rows from {} sessions ({} with an ask, {} of them opening a session; {} patterns scored; project unknown for {}, which are scored over the tree only; {} have every target outside the session's project; by harness {harness:?}; targets {kinds:?})",
        eps.len(),
        sessions.len(),
        eps.iter().filter(|e| !e.ask.is_empty()).count(),
        eps.iter()
            .filter(|e| !e.ask.is_empty() && e.opening)
            .count(),
        eps.iter().map(|e| e.scored.len()).sum::<usize>(),
        eps.iter().filter(|e| !e.project_known).count(),
        eps.iter().filter(|e| e.elsewhere).count(),
    )
}

/// A graded question at its own scope, as blind3_run.py asks it.
fn run_question(c: &mut Ctx, set: &str, q: &Question) {
    let targets: BTreeSet<String> = q
        .grades
        .iter()
        .filter(|(_, g)| **g >= 1)
        .map(|(f, _)| f.clone())
        .collect();
    let dev = c.paths.dev_root.clone();
    let tag = |row: &mut Row, scope: &str| {
        row.scope = scope.into();
        row.kind = q.kind.clone();
        row.stratum = q.stratum.clone();
        row.grades = q.grades.clone();
        row.grade_project = q.grade_project.clone();
    };
    match &q.scope {
        Scope::Tree => {
            if c.o.rg {
                let a = c.rg_tree.rg_words(&q.words, &dev);
                c.push(set, &q.id, &q.id, "rg@scope", "", &a, &targets, |r| {
                    tag(r, "tree")
                });
            }
            for i in 0..c.searchers.len() {
                let name = c.searchers[i].name.clone();
                let a = c.index(i, &q.words, &dev, None);
                c.push(
                    set,
                    &q.id,
                    &q.id,
                    &format!("{name}:index@scope"),
                    &name,
                    &a,
                    &targets,
                    |r| tag(r, "tree"),
                );
            }
            if c.o.rg && !c.o.tree_skips.is_empty() {
                let a = c.rg.rg_words(&q.words, &dev);
                c.push(set, &q.id, &q.id, "rg@tree_all", "", &a, &targets, |r| {
                    tag(r, "tree")
                });
            }
        }
        Scope::Project(root) => {
            let root = root.clone();
            if c.o.rg {
                let a = c.rg.rg_words(&q.words, &root);
                c.push(set, &q.id, &q.id, "rg@scope", "", &a, &targets, |r| {
                    tag(r, "project")
                });
            }
            for i in 0..c.searchers.len() {
                let name = c.searchers[i].name.clone();
                let a = c.index(i, &q.words, &root, Some(&root));
                c.push(
                    set,
                    &q.id,
                    &q.id,
                    &format!("{name}:index@scope"),
                    &name,
                    &a,
                    &targets,
                    |r| tag(r, "project"),
                );
                let a = c.index(i, &q.words, &root, None);
                c.push(
                    set,
                    &q.id,
                    &q.id,
                    &format!("{name}:index@cwd"),
                    &name,
                    &a,
                    &targets,
                    |r| tag(r, "project"),
                );
            }
        }
    }
}

/// run_eval.py's questions: rg on the content words in the project and over
/// the tree, the most-cited files, and the index on the words and on the
/// sentence as typed, limited to the project, from it, and from the root.
fn run_questions_run_eval(c: &mut Ctx, name: &str, qs: &[Question]) {
    let (popular, cited) = most_cited(
        qs.iter()
            .map(|q| q.grades.keys().cloned().collect::<BTreeSet<_>>())
            .collect::<Vec<_>>()
            .iter(),
        c.o.limit,
    );
    let strata: BTreeSet<&str> = qs.iter().map(|q| q.stratum.as_str()).collect();
    let dev = c.paths.dev_root.clone();
    for q in qs {
        let targets: BTreeSet<String> = q.grades.keys().cloned().collect();
        let sentence: Vec<String> = q.query.split_whitespace().map(str::to_string).collect();
        let sets = if strata.len() > 1 {
            vec![name.to_string(), format!("{name}:{}", q.stratum)]
        } else {
            vec![name.to_string()]
        };
        let root = q.project.clone();
        let mut answers: Vec<(String, String, Answer)> = vec![(
            "most_cited_files".into(),
            String::new(),
            Answer {
                top: popular.clone(),
                found: cited,
                secs: 0.0,
                mode: "constant".into(),
                own_ms: None,
            },
        )];
        if c.o.rg {
            answers.push((
                "rg_words@project".into(),
                String::new(),
                c.rg.rg_words(&q.words, &root),
            ));
            answers.push((
                "rg_words@tree".into(),
                String::new(),
                c.rg.rg_words(&q.words, &dev),
            ));
        }
        for i in 0..c.searchers.len() {
            let n = c.searchers[i].name.clone();
            for (label, words) in [("words", &q.words), ("sentence", &sentence)] {
                let a = c.index(i, words, &root, Some(&root));
                answers.push((format!("{n}:index_{label}@project"), n.clone(), a));
                let a = c.index(i, words, &root, None);
                answers.push((format!("{n}:index_{label}@cwd"), n.clone(), a));
                let a = c.index(i, words, &dev, None);
                answers.push((format!("{n}:index_{label}@root"), n.clone(), a));
            }
        }
        for (system, config, a) in &answers {
            for set in &sets {
                c.push(set, &q.id, &q.id, system, config, a, &targets, |r| {
                    r.scope = "project".into();
                    r.kind = q.kind.clone();
                    r.stratum = q.stratum.clone();
                    r.grades = q.grades.clone();
                });
            }
        }
        (c.progress)(".");
    }
}

/// One episode, as run_eval.py's run_session: its ask as plain words, each
/// pattern the agent ran, and the episode's patterns as one interleaved
/// list, for rg and the index alike, in the project and over the tree.
fn run_episode(c: &mut Ctx, e: &Episode) {
    let dev = c.paths.dev_root.clone();
    let targets = &e.targets;
    let tlist: Vec<String> = targets.iter().cloned().collect();
    // (scope, rg's root, rg held to the index's skips, [(index variant, cwd, under)])
    type Variant<'v> = (&'v str, String, Option<String>);
    let mut scopes: Vec<(&str, String, bool, Vec<Variant>)> = Vec::new();
    if e.project_known {
        scopes.push((
            "project",
            e.project.clone(),
            false,
            vec![
                ("project", e.cwd.clone(), Some(e.project.clone())),
                ("cwd", e.cwd.clone(), None),
            ],
        ));
    }
    if c.o.tree {
        scopes.push(("tree", dev.clone(), true, vec![("root", dev.clone(), None)]));
    }
    let set_tag = |r: &mut Row, scope: &str| {
        r.scope = scope.to_string();
        r.targets = tlist.clone();
    };
    for (scope, root, like_index, variants) in &scopes {
        if !e.ask.is_empty() {
            let sets: &[&str] = if e.opening {
                &["ask_all", "ask_opening"]
            } else {
                &["ask_all"]
            };
            let a = if c.o.rg {
                Some(c.rg.rg_words(&e.ask, root))
            } else {
                None
            };
            for set in sets {
                if let Some(a) = &a {
                    c.push(
                        set,
                        &e.id,
                        &e.cluster,
                        &format!("rg_words@{scope}"),
                        "",
                        a,
                        targets,
                        |r| set_tag(r, scope),
                    );
                }
            }
            for (name, icwd, under) in variants {
                for i in 0..c.searchers.len() {
                    let n = c.searchers[i].name.clone();
                    let a = c.index(i, &e.ask, icwd, under.as_deref());
                    for set in sets {
                        c.push(
                            set,
                            &e.id,
                            &e.cluster,
                            &format!("{n}:index_words@{name}"),
                            &n,
                            &a,
                            targets,
                            |r| set_tag(r, scope),
                        );
                    }
                }
            }
        }
        if e.scored.is_empty() {
            continue;
        }
        // (system, config, its answer to each pattern)
        let mut systems: Vec<(String, String, Vec<Answer>)> = Vec::new();
        if c.o.rg {
            let answers = e
                .scored
                .iter()
                .map(|p| c.rg.rg_pattern_at(&p.kind, &p.pat, root, *like_index))
                .collect();
            systems.push((format!("rg_patterns@{scope}"), String::new(), answers));
        }
        for (name, icwd, under) in variants {
            for i in 0..c.searchers.len() {
                let n = c.searchers[i].name.clone();
                let answers = e
                    .scored
                    .iter()
                    .map(|p| c.index(i, &p.words, icwd, under.as_deref()))
                    .collect();
                systems.push((format!("{n}:index_patterns@{name}"), n, answers));
            }
        }
        for (system, config, answers) in &systems {
            for (p, a) in e.scored.iter().zip(answers) {
                c.push(
                    "pattern",
                    &format!("{}#{}", e.id, p.pos),
                    &e.cluster,
                    system,
                    config,
                    a,
                    targets,
                    |r| set_tag(r, scope),
                );
            }
            let secs: f64 = answers.iter().map(|a| a.secs).sum();
            let own: Vec<f64> = answers.iter().filter_map(|a| a.own_ms).collect();
            let merged = interleave(
                &answers.iter().map(|a| a.top.clone()).collect::<Vec<_>>(),
                c.o.limit,
            );
            let ep = Answer {
                top: merged,
                found: 0,
                secs,
                mode: "interleaved".into(),
                own_ms: if own.is_empty() {
                    None
                } else {
                    Some(own.iter().sum())
                },
            };
            // The searches to reach a target, the agent's order: its turn.
            let turn = answers
                .iter()
                .position(|a| rank_of(&a.top, targets) > 0)
                .map_or(0, |i| i + 1);
            c.push(
                "episode",
                &e.id,
                &e.cluster,
                system,
                config,
                &ep,
                targets,
                |r| {
                    set_tag(r, scope);
                    r.turn = Some(turn);
                },
            );
            if system.starts_with("rg") {
                let best = answers
                    .iter()
                    .map(|a| rank_of(&a.top, targets))
                    .filter(|r| *r > 0)
                    .min()
                    .unwrap_or(0);
                c.out.rows.push(Row {
                    set: "episode".into(),
                    qid: e.id.clone(),
                    unit: e.cluster.clone(),
                    system: system.replace("rg_patterns", "rg_patterns_best"),
                    rank: best,
                    ms: (secs * 10_000.0).round() / 10.0,
                    mode: "best pattern".into(),
                    scope: scope.to_string(),
                    targets: tlist.clone(),
                    ..Default::default()
                });
            }
        }
    }
}
