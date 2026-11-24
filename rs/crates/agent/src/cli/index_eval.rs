//! `agent index eval`: the index measured against rg and fd on graded
//! questions and session episodes (agent_index::eval), with a run's record
//! (run.json), its rows (results.jsonl) and tables (summary.md) in a run
//! directory, and runs compared.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_index::eval::{self, Options, Row, SetSpec, health as health_code, sha256_file};
use agent_index::pg::Pg;
use anyhow::{Context as _, bail};
use serde_json::json;

use super::{load_config, pg_target, query_embedder};
use crate::cli::command::{ArgRule, Def, Doc};
use crate::cli::flags;
use crate::cli::root::Ctx;

pub const EVAL: Doc = Doc {
    use_line: "eval",
    short: "Measure the index against rg and fd on graded questions and session episodes",
    long: r#"Eval runs question sets (--qrels: graded questions) and session episodes
(--episodes) against one or more named configurations of the pg store, in
process (no server, no copy of the binary), and against rg and fd, the
baselines an agent already has. Each run gets a directory with results.jsonl
(one line per set, query and system: ids, ranks, the top files, grades,
times; never a question's text), summary.md and run.json.

eval compare reads two runs and prints them against each other."#,
    example: r#"  agent index eval run --config base=base.toml --qrels questions.jsonl
  agent index eval compare RUN_A RUN_B --set questions"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn eval_def() -> Def {
    Def::group(&EVAL, vec![run_def(), compare_def(), show_def()])
}

// `agent index eval run`

pub const RUN: Doc = Doc {
    use_line: "run",
    short: "Run question sets and episodes against named configurations and rg",
    long: r#"Run asks each question at its own scope, as the published numbers were
measured: a question with a project is asked --under the project
(index@scope), from it with the index's soft project prior (index@cwd), and
of rg on its content words in the project (rg@scope). A question whose
project is "*", or that lists "projects", is asked over the tree: the index
from the dev root, rg over the tree, skipping what the index skips and the
top-level directories it holds only shallow (rg@tree_all keeps those).
--systems run_eval asks run_eval.py's systems instead (words and sentence,
@project, @cwd and @root, and the most-cited files).

Episodes ({session, cwd, searches, edits, ask}) are asked as run_eval.py
asked them: each pattern the agent ran, by rg and fd (rg_patterns) and by the
index on the pattern's words, in the session's project, from its directory
and over the tree; the episode's patterns as one interleaved list; and its
ask as plain words.

Every search is --kind file, the top 50 entities cut to 10 files, a plain
query cut to 12 words. The numbers: hit@1, @5 and @10, MRR@10, nDCG@10, the
kinds' checks (multihop: every grade-2 file in the top 10; cross: a grade-2
file from each project; why: a grade-2 prose file in the top 5), and
latency; each configuration against rg and against each other, by a paired
bootstrap (by question; by session for episodes) with Python's own random
numbers, so the intervals are the Python harness's.

--heldout spends a set: its sha256 goes in the spent-sets ledger
(~/.local/share/gomuxai/eval/spent.jsonl), and a second run on it is
refused unless --again says why. --prereg records a pre-registration's
sha256. With a configuration whose reranker is on, the reranker's /health is
checked before (a run refuses when it is down) and every 5 s; a failed poll
marks the run. The eval never stops the reranker."#,
    example: r#"  agent index eval run --config base=~/.config/gomuxai/index.toml --qrels q.jsonl
  agent index eval run --config a=a.toml --config b=b.toml --qrels q.jsonl --episodes sessions.jsonl
  agent index eval run --config base=base.toml --qrels test.jsonl --heldout --prereg prereg.md"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct RunArgs {
    #[arg(long = "config", value_name = "stringArray", help = "A configuration to run, as NAME=FILE (repeatable)")]
    pub config: Vec<String>,
    #[arg(long = "qrels", value_name = "stringArray", help = "Graded questions, JSON lines (repeatable; the set is named for the file)")]
    pub qrels: Vec<String>,
    #[arg(long = "episodes", value_name = "stringArray", help = "Session episodes, JSON lines (repeatable)")]
    pub episodes: Vec<String>,
    #[arg(long = "dev-root", value_name = "string", default_value = "", help = "The tree the index crawled (default ~/dev)")]
    pub dev_root: String,
    #[arg(long = "systems", value_name = "string", default_value = "scope", help = "Questions: scope (each at its own scope), or run_eval (run_eval.py's systems)")]
    pub systems: String,
    #[arg(long = "no-rg", help = "Leave the rg and fd baselines out")]
    pub no_rg: bool,
    #[arg(long = "no-tree", help = "Episodes: leave the whole-tree systems out (the slow part)")]
    pub no_tree: bool,
    #[arg(long = "tree-skip", value_name = "stringArray", help = "A top-level directory rg over the tree skips (repeatable; default: the projects the index holds only shallow)")]
    pub tree_skip: Vec<String>,
    #[arg(long = "limit", value_name = "int", default_value = "10", value_parser = flags::parse_int, help = "Files kept per answer")]
    pub limit: i64,
    #[arg(long = "index-limit", value_name = "int", default_value = "50", value_parser = flags::parse_int, help = "Entities asked of the index, cut to --limit files")]
    pub index_limit: i64,
    #[arg(long = "max-words", value_name = "int", default_value = "12", value_parser = flags::parse_int, help = "Words of a plain query given to either system")]
    pub max_words: i64,
    #[arg(long = "kind", value_name = "string", default_value = "file", help = "The index's --kind (\"\" for none)")]
    pub kind: String,
    #[arg(long = "max-queries", value_name = "int", default_value = "0", value_parser = flags::parse_int, help = "Per set, for a quick trial (0: all)")]
    pub max_queries: i64,
    #[arg(long = "resamples", value_name = "int", default_value = "10000", value_parser = flags::parse_int, help = "Bootstrap resamples")]
    pub resamples: i64,
    #[arg(long = "seed", value_name = "int", default_value = "20261123", value_parser = flags::parse_int, help = "Bootstrap seed")]
    pub seed: i64,
    #[arg(long = "heldout", help = "The sets are held out: spend them (a second run is refused without --again)")]
    pub heldout: bool,
    #[arg(long = "again", value_name = "string", default_value = "", help = "Why a spent held-out set is run again (recorded)")]
    pub again: String,
    #[arg(long = "prereg", value_name = "string", default_value = "", help = "The pre-registration this run follows (its sha256 is recorded)")]
    pub prereg: String,
    #[arg(long = "allow-reranker-down", help = "Run even when a configuration's reranker is down before the run (marked)")]
    pub allow_reranker_down: bool,
    #[arg(long = "label", value_name = "string", default_value = "", help = "A word for the run's directory name")]
    pub label: String,
    #[arg(long = "out", value_name = "string", default_value = "", help = "The run's directory (default ~/.local/share/gomuxai/eval/runs/TIME-LABEL)")]
    pub out: String,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn run_def() -> Def {
    leaf!(RUN, RunArgs, run_run)
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn data_dir() -> PathBuf {
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x).join("gomuxai/eval"),
        None => home().join(".local/share/gomuxai/eval"),
    }
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

fn set_name(p: &Path) -> String {
    p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()
}

/// The reranker's health, polled on a thread while the run lasts.
struct Health {
    stop: Arc<AtomicBool>,
    polls: Arc<Mutex<Vec<(String, u16)>>>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Health {
    fn start(endpoint: String) -> Health {
        let stop = Arc::new(AtomicBool::new(false));
        let polls = Arc::new(Mutex::new(Vec::new()));
        let (s, p) = (Arc::clone(&stop), Arc::clone(&polls));
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let code = health_code(&endpoint);
                if let Ok(mut v) = p.lock() {
                    v.push((chrono::Utc::now().to_rfc3339(), code));
                }
                for _ in 0..50 {
                    if s.load(Ordering::Relaxed) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        });
        Health { stop, polls, handle: Some(handle) }
    }

    fn finish(mut self) -> Vec<(String, u16)> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.polls.lock().map(|v| v.clone()).unwrap_or_default()
    }
}

/// The store's size and newest write (the watcher is live).
fn store_state(pg: &Pg) -> serde_json::Value {
    match pg.query(
        "SELECT count(*), coalesce(to_char(max(updated_at) AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), '') FROM entities",
        &[],
    ) {
        Ok(rows) if !rows.is_empty() => json!({
            "at": chrono::Utc::now().to_rfc3339(),
            "entities": rows[0].get::<_, i64>(0),
            "newest_write": rows[0].get::<_, String>(1),
        }),
        Ok(_) => json!(null),
        Err(e) => json!({"error": format!("{e:#}")}),
    }
}

/// The top-level directories of the dev tree that the index holds only
/// shallow (no file of theirs is indexed): rg over the tree skips them too.
fn shallow_top_dirs(pg: &Pg, dev: &Path) -> Vec<String> {
    let dev = std::fs::canonicalize(dev).unwrap_or_else(|_| dev.to_path_buf());
    let rows = pg
        .query(
            "SELECT path FROM projects WHERE coalesce(nullif(crawled_depth, ''), depth) = 'shallow' ORDER BY path",
            &[],
        )
        .unwrap_or_default();
    rows.iter()
        .map(|r| PathBuf::from(r.get::<_, String>(0)))
        .filter(|p| p.parent() == Some(dev.as_path()))
        .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .collect()
}

fn run_run(ctx: &Ctx, a: &RunArgs) -> anyhow::Result<()> {
    #[cfg(feature = "embed")]
    // SAFETY: first thing in the command, before anything starts a thread
    // (as agent index embed).
    unsafe {
        agent_index::embed::single_threaded_products()
    };
    if a.qrels.is_empty() && a.episodes.is_empty() {
        bail!("nothing to run: give --qrels FILE or --episodes FILE");
    }
    if a.config.is_empty() && a.no_rg {
        bail!("nothing to compare: give --config NAME=FILE, or leave out --no-rg");
    }
    let dev = if a.dev_root.is_empty() { home().join("dev") } else { expand(&a.dev_root) };
    if !dev.is_dir() {
        bail!("--dev-root {}: not a directory", dev.display());
    }
    let mut sets: Vec<SetSpec> = Vec::new();
    for (list, episodes) in [(&a.qrels, false), (&a.episodes, true)] {
        for p in list {
            let path = expand(p);
            if !path.is_file() {
                bail!("{}: no such file", path.display());
            }
            sets.push(SetSpec { name: set_name(&path), path, episodes });
        }
    }
    // Held-out sets: spent once.
    let ledger = data_dir().join("spent.jsonl");
    let mut set_records = Vec::new();
    let mut spent_now = Vec::new();
    let spent: Vec<serde_json::Value> = std::fs::read_to_string(&ledger)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect();
    for s in &sets {
        let sha = sha256_file(&s.path)?;
        let before: Vec<&serde_json::Value> = spent.iter().filter(|v| v["sha256"] == sha.as_str()).collect();
        if a.heldout && !before.is_empty() && a.again.is_empty() {
            bail!(
                "{} is a held-out set already spent (first run {}): a second run needs --again \"why\"",
                s.path.display(),
                before[0]["run"].as_str().unwrap_or("?")
            );
        }
        if a.heldout && before.is_empty() {
            spent_now.push(sha.clone());
        }
        set_records.push(json!({"name": s.name, "path": s.path, "episodes": s.episodes, "sha256": sha,
                                "spent_before": !before.is_empty()}));
    }
    let prereg = if a.prereg.is_empty() {
        json!(null)
    } else {
        let p = expand(&a.prereg);
        json!({"path": p, "sha256": sha256_file(&p)?})
    };
    // The configurations, each its own store connection.
    let mut searchers = Vec::new();
    let mut cfg_records = Vec::new();
    let mut rerankers: Vec<String> = Vec::new();
    for spec in &a.config {
        let Some((name, file)) = spec.split_once('=') else {
            bail!("--config {spec}: give it as NAME=FILE");
        };
        if name.is_empty() || name.contains(':') || searchers.iter().any(|s: &eval::Searcher| s.name == name) {
            bail!("--config {spec}: a name of its own, without ':'");
        }
        let path = expand(file);
        let cfg = load_config(&path.to_string_lossy())?.config;
        let (url, schema) = pg_target(&cfg)?;
        let pg = Pg::connect_with(&url, &schema, cfg.clone())?;
        if cfg.reranker.enabled && !rerankers.contains(&cfg.reranker.endpoint) {
            rerankers.push(cfg.reranker.endpoint.clone());
        }
        cfg_records.push(json!({"name": name, "path": path, "sha256": sha256_file(&path)?,
                                "fingerprint": cfg.fingerprint(), "schema": schema,
                                "reranker": if cfg.reranker.enabled { cfg.reranker.endpoint.clone() } else { String::new() }}));
        let embed = query_embedder(ctx, &cfg);
        searchers.push(eval::Searcher::new(name, pg, embed));
    }
    // The reranker, before.
    let mut down_before = Vec::new();
    for e in &rerankers {
        if health_code(e) != 200 {
            down_before.push(e.clone());
        }
    }
    if !down_before.is_empty() && !a.allow_reranker_down {
        bail!(
            "the reranker at {} is down: nothing run (--allow-reranker-down runs anyway, marked)",
            down_before.join(", ")
        );
    }
    let tree_skips = if !a.tree_skip.is_empty() {
        a.tree_skip.clone()
    } else {
        searchers.first().map(|s| shallow_top_dirs(&s.pg, &dev)).unwrap_or_default()
    };
    let o = Options {
        dev_root: dev.clone(),
        limit: flag(a.limit, "limit")?,
        index_limit: flag(a.index_limit, "index-limit")?,
        max_words: flag(a.max_words, "max-words")?,
        kind: a.kind.clone(),
        rg: !a.no_rg,
        tree: !a.no_tree,
        run_eval_systems: match a.systems.as_str() {
            "scope" => false,
            "run_eval" => true,
            x => bail!("--systems takes scope or run_eval, not {x:?}"),
        },
        resamples: flag(a.resamples, "resamples")?,
        seed: flag(a.seed, "seed")? as u64,
        tree_skips,
        max_queries: flag(a.max_queries, "max-queries")?,
        ..Default::default()
    };
    let started = chrono::Utc::now();
    let label = if a.label.is_empty() {
        sets.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join("+")
    } else {
        a.label.clone()
    };
    let out = if a.out.is_empty() {
        data_dir().join("runs").join(format!("{}-{label}", started.format("%Y%m%dT%H%M%SZ")))
    } else {
        expand(&a.out)
    };
    if out.join("run.json").exists() {
        bail!("{} holds a run already", out.display());
    }
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;
    let store_before = searchers.first().map(|s| store_state(&s.pg));
    let health: Vec<Health> = rerankers.iter().map(|e| Health::start(e.clone())).collect();
    ctx.notice(format!("Eval run in {}", out.display()));
    let mut dots = 0usize;
    let mut progress = |s: &str| {
        if s == "." {
            dots += 1;
            eprint!(".");
            let _ = std::io::stderr().flush();
        } else {
            if dots > 0 {
                eprintln!();
                dots = 0;
            }
            eprintln!("{s}");
        }
    };
    let t0 = std::time::Instant::now();
    let result = eval::run(&o, &sets, &mut searchers, &mut progress);
    eprintln!();
    let polls: Vec<(String, Vec<(String, u16)>)> =
        rerankers.iter().cloned().zip(health.into_iter().map(Health::finish)).collect();
    let res = result?;
    let store_after = searchers.first().map(|s| store_state(&s.pg));
    let failed_polls: usize = polls.iter().map(|(_, v)| v.iter().filter(|(_, c)| *c != 200).count()).sum();
    // results.jsonl, summary.md, run.json.
    let mut f = std::io::BufWriter::new(std::fs::File::create(out.join("results.jsonl"))?);
    for r in &res.rows {
        serde_json::to_writer(&mut f, r)?;
        f.write_all(b"\n")?;
    }
    f.flush()?;
    let mut summary = format!(
        "# Eval {label}\n\nRun {} ({:.0} s); configurations {}; dev root {}; tree skips {:?}.\n",
        started.to_rfc3339(),
        t0.elapsed().as_secs_f64(),
        cfg_records.iter().map(|c| format!("{} ({})", c["name"].as_str().unwrap_or(""), c["fingerprint"].as_str().unwrap_or(""))).collect::<Vec<_>>().join(", "),
        dev.display(),
        o.tree_skips
    );
    if !down_before.is_empty() || failed_polls > 0 {
        let _ = writeln!(
            summary,
            "\n**RERANKER DOWN** during this run ({} before, {failed_polls} failed polls): run it again.",
            if down_before.is_empty() { "up" } else { "down" }
        );
    }
    summary.push('\n');
    summary.push_str(&eval::score::summary(&res, o.resamples, o.seed));
    std::fs::write(out.join("summary.md"), &summary)?;
    let exe = std::env::current_exe().ok();
    let record = json!({
        "started": started.to_rfc3339(),
        "finished": chrono::Utc::now().to_rfc3339(),
        "seconds": t0.elapsed().as_secs_f64(),
        "build": agent_index::pg::serve::build_id(),
        "binary_sha256": exe.as_deref().and_then(|p| sha256_file(p).ok()),
        "configs": cfg_records,
        "sets": set_records,
        "set_notes": res.sets,
        "options": o,
        "store_before": store_before,
        "store_after": store_after,
        "reranker": {
            "down_before": down_before,
            "failed_polls": failed_polls,
            "polls": polls.iter().map(|(e, v)| json!({"endpoint": e, "polls": v.len(),
                "not_200": v.iter().filter(|(_, c)| *c != 200).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        },
        "heldout": a.heldout,
        "again": if a.again.is_empty() { json!(null) } else { json!(a.again) },
        "prereg": prereg,
        "index_errors": res.index_errors,
        "rows": res.rows.len(),
    });
    std::fs::write(out.join("run.json"), serde_json::to_string_pretty(&record)?)?;
    if !spent_now.is_empty() {
        std::fs::create_dir_all(data_dir())?;
        let mut l = std::fs::OpenOptions::new().create(true).append(true).open(&ledger)?;
        for sha in spent_now {
            writeln!(l, "{}", json!({"sha256": sha, "run": out, "at": started.to_rfc3339()}))?;
        }
    }
    ctx.write_out(&summary)?;
    ctx.notice(format!("Rows, tables and record in {}", out.display()));
    if res.index_errors > 0 {
        bail!("{} index searches failed (see results.jsonl, mode)", res.index_errors);
    }
    Ok(())
}

fn flag(v: i64, name: &str) -> anyhow::Result<usize> {
    if v < 0 {
        bail!("--{name} must be 0 or more");
    }
    Ok(v as usize)
}

// `agent index eval compare`

pub const COMPARE: Doc = Doc {
    use_line: "compare RUN_A RUN_B",
    short: "Compare two eval runs, set by set and system by system",
    long: r#"Compare reads two runs' results.jsonl and, for each set and system both
hold, prints each run's hit@1, @5 and @10, MRR@10, nDCG@10 and median times
over the queries both answered, then first minus second for recall@5 and
MRR@10 with a 95% interval (a bootstrap by question, or by session for
episodes, as cmp.py drew it), how many queries flip in or out of the top 5,
and how many top 10s differ."#,
    example: r#"  agent index eval compare runs/A runs/B
  agent index eval compare runs/A runs/B --set blind3 --system base:index@scope"#,
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct CompareArgs {
    #[arg(long = "set", value_name = "string", default_value = "", help = "Only this set")]
    pub set: String,
    #[arg(long = "system", value_name = "string", default_value = "", help = "Only this system")]
    pub system: String,
    #[arg(long = "resamples", value_name = "int", default_value = "10000", value_parser = flags::parse_int, help = "Bootstrap resamples")]
    pub resamples: i64,
    #[arg(long = "seed", value_name = "int", default_value = "20261123", value_parser = flags::parse_int, help = "Bootstrap seed")]
    pub seed: i64,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn compare_def() -> Def {
    leaf!(COMPARE, CompareArgs, run_compare)
}

fn run_dir(arg: &str) -> anyhow::Result<PathBuf> {
    let p = expand(arg);
    if p.join("results.jsonl").is_file() {
        return Ok(p);
    }
    let under = data_dir().join("runs").join(arg);
    if under.join("results.jsonl").is_file() {
        return Ok(under);
    }
    bail!("{arg}: no eval run there (no results.jsonl)")
}

fn read_rows(dir: &Path) -> anyhow::Result<Vec<Row>> {
    let text = std::fs::read_to_string(dir.join("results.jsonl"))?;
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).context("a results.jsonl line"))
        .collect()
}

fn run_compare(ctx: &Ctx, a: &CompareArgs) -> anyhow::Result<()> {
    let (da, db) = (run_dir(&a.args[0])?, run_dir(&a.args[1])?);
    let (ra, rb) = (read_rows(&da)?, read_rows(&db)?);
    let mut o = format!("First: {}\nSecond: {}\n\n", da.display(), db.display());
    o.push_str(&eval::score::compare(
        &ra,
        &rb,
        (!a.set.is_empty()).then_some(a.set.as_str()),
        (!a.system.is_empty()).then_some(a.system.as_str()),
        flag(a.resamples, "resamples")?,
        flag(a.seed, "seed")? as u64,
    ));
    ctx.write_out(&o)
}

// `agent index eval show`

pub const SHOW: Doc = Doc {
    use_line: "show RUN",
    short: "Print an eval run's summary again",
    long: r#"Show prints a run's summary.md (RUN is its directory, or its name under
~/.local/share/gomuxai/eval/runs)."#,
    example: r#"  agent index eval show 20261124T200000Z-blind3"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ShowArgs {
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn show_def() -> Def {
    leaf!(SHOW, ShowArgs, run_show)
}

fn run_show(ctx: &Ctx, a: &ShowArgs) -> anyhow::Result<()> {
    let d = run_dir(&a.args[0])?;
    ctx.write_out(&std::fs::read_to_string(d.join("summary.md"))?)
}
