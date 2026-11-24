//! The question sets, read as the Python harness read them: graded
//! questions (run_eval.py's `load_questions`, with blind3_run.py's and the
//! cross runner's scopes) and session episodes (`load_sessions`).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use regex::Regex;
use serde_json::Value;

use super::paths::{Paths, inside, join};
use super::words::{GLOB_KINDS, content_words, pattern_words};

/// Why rows or parts of them were dropped, with counts.
pub type Drops = BTreeMap<String, usize>;

fn drop(d: &mut Drops, why: &str) {
    *d.entry(why.to_string()).or_default() += 1;
}

/// Where a question is asked.
#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    /// A project's directory: the index --under it and asked from it, rg in it.
    Project(String),
    /// The whole tree: project "*", or a "projects" list.
    Tree,
}

/// A graded question.
#[derive(Debug, Clone)]
pub struct Question {
    pub id: String,
    pub query: String,
    pub words: Vec<String>,
    /// Canonical file path to grade.
    pub grades: BTreeMap<String, i64>,
    /// Canonical file path to the project its grade came from.
    pub grade_project: BTreeMap<String, String>,
    pub stratum: String,
    pub kind: String,
    /// run_eval.py's project: the one named (when it is a directory), else
    /// the project most of its relevant files are in.
    pub project: String,
    /// blind3_run.py's scope.
    pub scope: Scope,
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .enumerate()
        .map(|(i, l)| {
            serde_json::from_str(l).with_context(|| format!("{} line {}", path.display(), i + 1))
        })
        .collect()
}

fn s(v: &Value, k: &str) -> String {
    match v.get(k) {
        Some(Value::String(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(x) => x.to_string(),
    }
}

fn grade_of(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n
            .as_i64()
            .unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(x) => x.trim().parse().unwrap_or(0),
        Value::Bool(b) => *b as i64,
        _ => 0,
    }
}

/// Graded questions: `{qid, query, stratum|kind, rel: {PROJECT:PATH: grade}}`
/// and `project` (a name, or "*") or `projects` (a list: a tree question).
pub fn load_questions(path: &Path, paths: &Paths, drops: &mut Drops) -> Result<Vec<Question>> {
    let mut out = Vec::new();
    for r in read_jsonl(path)? {
        let mut grades: BTreeMap<String, i64> = BTreeMap::new();
        let mut best: BTreeMap<String, (i64, String)> = BTreeMap::new();
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        if let Some(Value::Object(rel)) = r.get("rel") {
            for (ent, g) in rel {
                let (project, rest) = ent.split_once(':').unwrap_or((ent.as_str(), ""));
                let file = rest.split('#').next().unwrap_or("");
                let f = join(&join(&paths.dev_root, project), file);
                if Path::new(&f).is_file() {
                    let c = paths.canon(&f);
                    let g = grade_of(g);
                    let e = grades.entry(c.clone()).or_insert(0);
                    *e = (*e).max(g);
                    // blind3_run.py: the project of the highest grade, the
                    // later one on a tie.
                    if g >= best.get(&c).map(|b| b.0).unwrap_or(0) {
                        best.insert(c, (g, project.to_string()));
                    }
                    *counts.entry(join(&paths.dev_root, project)).or_default() += 1;
                } else {
                    drop(drops, "relevant entity: not a file in the tree now");
                }
            }
        }
        let query = s(&r, "query");
        let words = content_words(&query);
        if grades.is_empty() || words.is_empty() {
            drop(drops, "question: no relevant file left, or no words");
            continue;
        }
        let named = s(&r, "project");
        let named_dir = join(&paths.dev_root, &named);
        let majority = counts
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then(a.0.cmp(b.0)))
            .map(|(p, _)| p.clone())
            .unwrap_or_default();
        let project = if !named.is_empty() && Path::new(&named_dir).is_dir() {
            named_dir.clone()
        } else {
            majority
        };
        let listed = r.get("projects").is_some_and(|v| v.is_array());
        let tree = named == "*" || (listed && named.is_empty());
        let scope = if tree {
            Scope::Tree
        } else if !named.is_empty() {
            Scope::Project(named_dir)
        } else {
            Scope::Project(project.clone())
        };
        let mut kind = s(&r, "kind");
        if kind.is_empty() && listed && named.is_empty() {
            kind = "cross".into();
        }
        out.push(Question {
            id: s(&r, "qid"),
            query,
            words,
            grade_project: best.into_iter().map(|(f, (_, p))| (f, p)).collect(),
            grades,
            stratum: s(&r, "stratum"),
            kind,
            project,
            scope,
        });
    }
    Ok(out)
}

/// A pattern the agent searched with: its place in the episode's own
/// order (counting the distinct ones), its kind, the pattern, its words.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub pos: usize,
    pub kind: String,
    pub pat: String,
    pub words: Vec<String>,
}

/// One episode of a session.
#[derive(Debug, Clone)]
pub struct Episode {
    pub id: String,
    /// What is resampled: its session.
    pub cluster: String,
    pub opening: bool,
    pub cwd: String,
    pub project: String,
    pub project_known: bool,
    pub scored: Vec<Pattern>,
    pub elsewhere: bool,
    pub ask: Vec<String>,
    pub targets: BTreeSet<String>,
    pub harness: String,
    pub target_kind: String,
}

const MIN_ASK_WORDS: usize = 5;
static TASK_FILE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\btask\.md\b").unwrap());
static WIDE_CONTEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.\{\d{3,}\}").unwrap());
static GLOB_SPLIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[/*]+").unwrap());
const TOOLING_DIRS: &[&str] = &[".git", "target", "node_modules"];

/// What the loaders share across files: rows and sessions seen, and the
/// clusters by session.
#[derive(Default)]
pub struct Seen {
    keys: HashSet<String>,
    sessions: HashSet<String>,
    clusters: HashMap<String, usize>,
}

fn expanduser(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(h)) => format!("{h}/{rest}"),
        _ if p == "~" => std::env::var("HOME").unwrap_or_else(|_| p.into()),
        _ => p.to_string(),
    }
}

/// Session episodes: `{session, cwd, searches, edits}` and optionally
/// `{episode, ask, reads, harness}`; `tag` names the file's rows.
pub fn load_sessions(
    path: &Path,
    tag: char,
    paths: &Paths,
    drops: &mut Drops,
    seen: &mut Seen,
    skip_cwd: &[String],
) -> Result<Vec<Episode>> {
    let raw = read_jsonl(path)?;
    let earlier: HashSet<String> = seen.sessions.clone();
    let mut rows = Vec::new();
    for (n, r) in raw.iter().enumerate() {
        let all: Vec<String> = r
            .get("searches")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| x.as_str().unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default();
        // A pattern repeated straight after itself is one search, not two.
        let searches: Vec<String> = all
            .iter()
            .enumerate()
            .filter(|(i, x)| *i == 0 || **x != all[i - 1])
            .map(|(_, x)| x.clone())
            .collect();
        let cwd = s(r, "cwd");
        let list = |k: &str| -> Vec<String> {
            r.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        let edits = list("edits");
        let wanted_rel = if edits.is_empty() {
            list("reads")
        } else {
            edits.clone()
        };
        let wanted: Vec<String> = wanted_rel
            .iter()
            .map(|p| join(&cwd, &expanduser(p)))
            .collect();
        if skip_cwd.iter().any(|d| inside(&cwd, d)) || format!("{cwd}/").contains("/.gomuxai/") {
            drop(
                drops,
                "row: ran in a temp dir or in the agent tool's own data",
            );
            continue;
        }
        let session = s(r, "session");
        if !session.is_empty() && earlier.contains(&session) {
            drop(drops, "row: its session is in an earlier file");
            continue;
        }
        let key = if !searches.is_empty() {
            serde_json::to_string(&(&session, &cwd, &searches)).unwrap_or_default()
        } else {
            serde_json::to_string(&(&session, &cwd, s(r, "ask"), &wanted)).unwrap_or_default()
        };
        if seen.keys.contains(&key) {
            drop(drops, "row: same session and searches as an earlier row");
            continue;
        }
        seen.keys.insert(key);
        if !session.is_empty() {
            seen.sessions.insert(session.clone());
        }
        let mut targets = BTreeSet::new();
        for e in &wanted {
            let c = paths.canon(e);
            if !paths.in_dev(&c) {
                drop(
                    drops,
                    "target: outside the dev root (memory, scratch, tool state)",
                );
            } else if !Path::new(&c).is_file() {
                drop(drops, "target: file is not in its repo's main checkout now");
            } else {
                targets.insert(c);
            }
        }
        if targets.is_empty() {
            drop(drops, "row: no target file left");
            continue;
        }
        let mut scored = Vec::new();
        let mut had: Vec<(String, String)> = Vec::new();
        for sx in &searches {
            let (kind, pat) = sx.split_once(':').unwrap_or((sx.as_str(), ""));
            let k = (kind.to_string(), pat.to_string());
            if had.contains(&k) {
                continue;
            }
            had.push(k);
            let words = pattern_words(kind, pat);
            let tooling = GLOB_KINDS.contains(&kind)
                && GLOB_SPLIT.split(pat).any(|p| TOOLING_DIRS.contains(&p));
            if WIDE_CONTEXT.is_match(pat) {
                drop(
                    drops,
                    "pattern: wide-context read of a bundle (.{N}word.{N})",
                );
            } else if tooling {
                drop(drops, "pattern: names a build or VCS directory");
            } else if words.is_empty() {
                drop(drops, "pattern: no word in it (^#, *.go, pub fn)");
            } else {
                scored.push(Pattern {
                    pos: had.len(),
                    kind: kind.to_string(),
                    pat: pat.to_string(),
                    words,
                });
            }
        }
        let ask_text = s(r, "ask");
        let mut ask = content_words(&ask_text);
        if !ask.is_empty() && (ask.len() < MIN_ASK_WORDS || TASK_FILE.is_match(&ask_text)) {
            drop(
                drops,
                "ask: a greeting or a pointer to a task file, not a request",
            );
            ask.clear();
        }
        if scored.is_empty() && ask.is_empty() {
            drop(drops, "row: no usable pattern and no ask");
            continue;
        }
        let (project, known) = paths.project_of(&cwd);
        let ckey = if session.is_empty() {
            format!("{tag}{n}")
        } else {
            session.clone()
        };
        let next = seen.clusters.len();
        let cluster = *seen.clusters.entry(ckey).or_insert(next);
        let episode = r.get("episode").is_some_and(|e| match e {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64() != Some(0.0),
            Value::String(x) => !x.is_empty(),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
        });
        let harness = s(r, "harness");
        rows.push(Episode {
            id: format!("{tag}{n:04}"),
            cluster: format!("s{cluster}"),
            opening: !episode,
            cwd: paths.canon(&cwd),
            elsewhere: known && !targets.iter().any(|t| inside(t, &project)),
            project,
            project_known: known,
            scored,
            ask,
            targets,
            harness: if harness.is_empty() {
                "claude".into()
            } else {
                harness
            },
            target_kind: if edits.is_empty() { "reads" } else { "edits" }.into(),
        });
    }
    Ok(rows)
}
