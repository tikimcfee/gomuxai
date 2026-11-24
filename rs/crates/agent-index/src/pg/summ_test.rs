//! `agent index summarize` against a throwaway schema and a fake model
//! server: rows keyed by (content key, fingerprint), a resume that skips
//! what is done, a changed file that falls back to its seed and is the only
//! one summarized again, the sync's overlay and embed queue, and a server
//! that dies mid-run stopping it with an error. Skipped (passing) without
//! `AGENT_INDEX_PG`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use serde_json::{Value, json};

use super::Pg;
use super::summ::{self, Options};
use crate::config::Config;
use crate::crawl::{CrawlOptions, crawl};
use crate::db::Index;
use crate::summarize::tests::{completion, fake_server, vllm};

struct Throwaway(Pg);

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = self.0.drop_schema();
    }
}

fn throwaway(cfg: Config) -> Option<Throwaway> {
    let (url, _) = Pg::from_env()?;
    let schema = format!(
        "idx_test_summ_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    Some(Throwaway(
        Pg::connect_with(&url, &schema, cfg).expect("connect"),
    ))
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// The path a prompt is about.
fn path_of(v: &Value) -> String {
    let u = v["messages"][1]["content"].as_str().unwrap_or("");
    u.lines()
        .find_map(|l| l.strip_prefix("Path: "))
        .unwrap_or("")
        .to_string()
}

/// A server that answers every file but `bad.rs` (no JSON, twice).
fn answers(v: &Value) -> (u16, String) {
    let p = path_of(v);
    if p.ends_with("bad.rs") {
        return (200, completion("I can't summarize that."));
    }
    let stem = p
        .rsplit('/')
        .next()
        .unwrap_or("")
        .split('.')
        .next()
        .unwrap_or("")
        .to_string();
    let a = json!({
        "summary": format!("This file handles the {stem} part of the alpha tool."),
        "why": "Keeps that part in one place.",
        "asks": [format!("where the {stem} part lives"), "the alpha tool's code",
                 format!("{stem} handling in alpha"), "words shared by every entry"],
    });
    (200, completion(&a.to_string()))
}

#[test]
fn summarize_resume_overlay_and_dead_server() {
    let url = fake_server(vllm(answers));
    let mut cfg = Config::default();
    cfg.llm.summaries.endpoint = url;
    cfg.llm.summaries.backoff_ms = 1;
    cfg.llm.summaries.concurrency = 4;
    let Some(t) = throwaway(cfg.clone()) else {
        return;
    };
    let pg = &t.0;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dev");
    let body = |name: &str| {
        format!(
            "//! The {name} module.\n{}",
            format!("pub fn {name}_step() {{ let x = 1; }}\n").repeat(12)
        )
    };
    for n in ["parse", "render", "store", "bad"] {
        write(&root, &format!("alpha/src/{n}.rs"), &body(n));
    }
    write(&root, "alpha/src/tiny.rs", "fn a() {}\n");
    write(&root, "alpha/outputs/run1/0001__f.json", "{\"a\": 1}\n");
    write(&root, "alpha/.env", "KEY=1\n");
    let mut ix = Index::open(&dir.path().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    super::sync::sync(&ix.conn, pg, false).unwrap();

    let o = Options {
        code: "test".into(),
        ..Default::default()
    };
    let cands = summ::candidates(pg, &o).unwrap();
    let (files, cov) = summ::select(&cands, &cfg.llm.summaries);
    let names: Vec<&str> = files.iter().map(|c| c.rel.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "src/bad.rs",
            "src/parse.rs",
            "src/render.rs",
            "src/store.rs"
        ],
        "{cov:?}"
    );
    assert_eq!(
        cov.by_reason.get("trivial (under the floor)"),
        Some(&1),
        "{cov:?}"
    );
    assert!(cov.by_reason.contains_key("program output dir"), "{cov:?}");

    let st = summ::run(pg, files.clone(), &o, &cov, &mut |_| {}).unwrap();
    assert_eq!(st.written, 3, "{st:?}");
    assert_eq!(st.failed.get("no usable JSON"), Some(&1), "{st:?}");
    assert_eq!(st.second_attempts, 1, "{st:?}");
    assert_eq!(st.trimmed, 3, "{st:?}");
    // "words shared by every entry" was on 3 entries, under the 4 needed.
    assert_eq!(st.asks_repeated_dropped, 0, "{st:?}");
    let rows = pg
        .query(
            "SELECT entity_id, status, summary, asks, writer FROM summaries ORDER BY entity_id",
            &[],
        )
        .unwrap();
    assert_eq!(rows.len(), 4);
    let s: String = rows[1].get(2);
    assert!(s.starts_with("Handles the parse part"), "{s}");
    let w: String = rows[1].get(4);
    assert_eq!(w, "model:Org/Small-4B@0123456789ab");
    let run: Value = pg
        .query("SELECT coverage FROM runs WHERE kind = 'summarize'", &[])
        .unwrap()[0]
        .get(0);
    assert_eq!(run["finished"], true);

    // The sync shows them over the seeds and queues their cards.
    pg.execute("DELETE FROM embed_queue", &[]).unwrap();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let e = pg
        .query(
            "SELECT summary, asks, summary_writer, (SELECT count(*) FROM embed_queue q WHERE q.entity_id = e.id)
             FROM entities e WHERE id = 'alpha:src/parse.rs'",
            &[],
        )
        .unwrap();
    let (summary, asks, writer, queued): (String, Vec<String>, String, i64) =
        (e[0].get(0), e[0].get(1), e[0].get(2), e[0].get(3));
    assert!(summary.starts_with("Handles the parse part"));
    assert_eq!(asks.len(), 4);
    assert!(writer.starts_with("model:"));
    assert_eq!(queued, 1);
    // A sync with nothing new writes nothing.
    let r = super::sync::sync(&ix.conn, pg, false).unwrap();
    assert_eq!(r.written, 0, "{r:?}");

    // A resume skips everything done or failed under this fingerprint.
    let st = summ::run(pg, files.clone(), &o, &cov, &mut |_| {}).unwrap();
    assert_eq!((st.done_before, st.todo, st.written), (4, 0, 0), "{st:?}");

    // A changed file falls back to its seed, and is the only one asked again.
    write(
        &root,
        "alpha/src/parse.rs",
        &format!("{}// changed\n", body("parse")),
    );
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let s: String = pg
        .query(
            "SELECT summary FROM entities WHERE id = 'alpha:src/parse.rs'",
            &[],
        )
        .unwrap()[0]
        .get(0);
    assert_eq!(s, "");
    let cands = summ::candidates(pg, &o).unwrap();
    let (files, cov) = summ::select(&cands, &cfg.llm.summaries);
    let st = summ::run(pg, files, &o, &cov, &mut |_| {}).unwrap();
    assert_eq!((st.todo, st.written), (1, 1), "{st:?}");
    let live: i64 = pg
        .query(
            "SELECT count(*) FROM summaries WHERE entity_id = 'alpha:src/parse.rs' AND status = 'live'",
            &[],
        )
        .unwrap()[0]
        .get(0);
    assert_eq!(live, 1);

    // A server that dies mid-run: the run stops with an error, writes no
    // row for what wasn't answered, and its stats say so.
    let chats = Arc::new(AtomicUsize::new(0));
    let dead = Arc::new(AtomicBool::new(false));
    let (c2, d2) = (chats.clone(), dead.clone());
    let dying = fake_server(Arc::new(move |m: &str, p: &str, v: &Value| {
        if d2.load(Ordering::SeqCst) && !(m == "GET") {
            return (0, String::new());
        }
        if p == "/v1/chat/completions" && c2.fetch_add(1, Ordering::SeqCst) >= 3 {
            d2.store(true, Ordering::SeqCst);
            return (0, String::new());
        }
        vllm(answers)(m, p, v)
    }));
    let mut cfg2 = cfg.clone();
    cfg2.llm.summaries.endpoint = dying;
    cfg2.llm.summaries.seed = 7; // another fingerprint: everything to do again
    cfg2.llm.summaries.concurrency = 1;
    cfg2.llm.summaries.max_connection_errors = 3;
    let (url, schema) = (Pg::from_env().unwrap().0, pg.schema.clone());
    let pg2 = Pg::connect_with(&url, &schema, cfg2.clone()).unwrap();
    let cands = summ::candidates(&pg2, &o).unwrap();
    let (files, cov) = summ::select(&cands, &cfg2.llm.summaries);
    let err = summ::run(&pg2, files, &o, &cov, &mut |_| {}).unwrap_err();
    assert!(
        format!("{err:#}").contains("connection errors in a row"),
        "{err:#}"
    );
    let run: Value = pg2
        .query(
            "SELECT coverage FROM runs WHERE kind = 'summarize' ORDER BY id DESC LIMIT 1",
            &[],
        )
        .unwrap()[0]
        .get(0);
    assert_eq!(run["finished"], true);
    assert!(run["aborted"].as_str().is_some());
    assert!(run["not_answered"].as_u64().unwrap() >= 1, "{run}");
    assert_eq!(run["written"], 1, "{run}");
}

/// A server whose requests hang and whose /health stops answering: the run
/// stops within seconds, not after the requests' timeout.
#[test]
fn hung_server_stops_the_run_quickly() {
    let hung = Arc::new(AtomicBool::new(false));
    let h2 = hung.clone();
    let url = fake_server(Arc::new(move |m: &str, p: &str, v: &Value| {
        if p == "/v1/chat/completions" {
            h2.store(true, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_secs(120));
        }
        if p == "/health" && h2.load(Ordering::SeqCst) {
            return (503, String::new());
        }
        vllm(answers)(m, p, v)
    }));
    let mut cfg = Config::default();
    cfg.llm.summaries.endpoint = url;
    cfg.llm.summaries.concurrency = 2;
    let Some(t) = throwaway(cfg.clone()) else {
        return;
    };
    let pg = &t.0;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dev");
    for n in ["one", "two", "three"] {
        write(
            &root,
            &format!("alpha/src/{n}.rs"),
            &format!("pub fn {n}_x() {{}}\n").repeat(30),
        );
    }
    let mut ix = Index::open(&dir.path().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let o = Options {
        code: "test".into(),
        ..Default::default()
    };
    let cands = summ::candidates(pg, &o).unwrap();
    let (files, cov) = summ::select(&cands, &cfg.llm.summaries);
    let t0 = std::time::Instant::now();
    let err = summ::run(pg, files, &o, &cov, &mut |_| {}).unwrap_err();
    assert!(
        format!("{err:#}").contains("stopped answering /health"),
        "{err:#}"
    );
    assert!(t0.elapsed().as_secs() < 30, "{:?}", t0.elapsed());
    let run: Value = pg
        .query("SELECT coverage FROM runs WHERE kind = 'summarize'", &[])
        .unwrap()[0]
        .get(0);
    assert_eq!(run["not_answered"], 2, "{run}");
}

/// The watchdog: a model that never answers in JSON fails every file, and
/// the error rate stops the run; a file past its deadline fails as such and
/// a project left out is counted as skipped.
#[test]
fn error_rate_deadline_and_left_out_projects() {
    let url = fake_server(vllm(|_| (200, completion("not json"))));
    let mut cfg = Config::default();
    cfg.llm.summaries.endpoint = url;
    cfg.llm.summaries.concurrency = 1;
    let Some(t) = throwaway(cfg.clone()) else {
        return;
    };
    let pg = &t.0;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dev");
    for n in ["a1", "a2", "a3", "a4", "a5", "a6"] {
        write(
            &root,
            &format!("alpha/src/{n}.rs"),
            &format!("pub fn {n}_x() {{}}\n").repeat(30),
        );
    }
    write(&root, "beta/src/b.rs", &"pub fn b_x() {}\n".repeat(30));
    let mut ix = Index::open(&dir.path().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    super::sync::sync(&ix.conn, pg, false).unwrap();

    let o = Options {
        code: "test".into(),
        skip_projects: vec!["beta".into()],
        own_files: [(
            "alpha".to_string(),
            (1..=6).map(|i| format!("src/a{i}.rs")).collect(),
        )]
        .into(),
        error_window: 3,
        max_error_rate: 0.5,
        ..Default::default()
    };
    let cands = summ::candidates(pg, &o).unwrap();
    let (files, cov) = summ::select_with(&cands, &cfg.llm.summaries, &o);
    assert_eq!(cov.by_reason.get("project left out"), Some(&1), "{cov:?}");
    assert_eq!(files.len(), 6);
    let err = summ::run(pg, files.clone(), &o, &cov, &mut |_| {}).unwrap_err();
    assert!(
        format!("{err:#}").contains("of the last 3 files failed"),
        "{err:#}"
    );
    let run: Value = pg
        .query(
            "SELECT coverage FROM runs WHERE kind = 'summarize' ORDER BY id DESC LIMIT 1",
            &[],
        )
        .unwrap()[0]
        .get(0);
    assert!(
        run["failed"]["no usable JSON"].as_u64().unwrap() >= 3,
        "{run}"
    );
    assert_eq!(run["skipped"], 1, "{run}");
    assert_eq!(run["total"], 6, "{run}");

    // A deadline already past: every file is pending (no row), so a
    // resume asks again.
    let o = Options {
        code: "test".into(),
        retry_failed: true,
        file_timeout: Some(std::time::Duration::from_nanos(1)),
        ..Default::default()
    };
    let st = summ::run(pg, files, &o, &cov, &mut |_| {}).unwrap();
    assert_eq!(st.pending.get("file deadline"), Some(&6), "{st:?}");
    assert_eq!((st.done, st.remaining, st.errors), (0, 0, 6), "{st:?}");
    assert_eq!(st.pending_ids.len(), 6);
}

/// What a directory prompt answers in the bottom-up test: its path's last
/// word, plain; and every prompt kept, to read.
fn dir_answers(seen: Arc<std::sync::Mutex<Vec<String>>>) -> impl Fn(&Value) -> (u16, String) {
    move |v: &Value| {
        let user = v["messages"][1]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let system = v["messages"][0]["content"].as_str().unwrap_or("");
        if !system.contains("directory or project") {
            return answers(v);
        }
        seen.lock().unwrap().push(user.clone());
        let p = path_of(v);
        let word = p
            .rsplit('/')
            .next()
            .unwrap_or("top")
            .replace(['(', ')'], "");
        let a = json!({
            "summary": format!("This directory holds the {word} code of the alpha tool."),
            "why": "Groups that code in one place.",
            "asks": [format!("where the {word} code lives"), "the alpha tool's layout",
                     format!("{word} code in alpha")],
        });
        (200, completion(&a.to_string()))
    }
}

/// The bottom-up pass: directories deepest first, then the project, each
/// from its README, listing and children's summaries; a directory too
/// large for one request cut into parts kept as `part`; a resume that
/// skips what is done; the sync showing a directory's summary.
#[test]
fn bottom_up_directories_parts_and_resume() {
    use super::dirsum;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = fake_server(vllm(dir_answers(seen.clone())));
    let mut cfg = Config::default();
    cfg.llm.summaries.endpoint = url;
    cfg.llm.summaries.backoff_ms = 1;
    cfg.llm.summaries.concurrency = 4;
    // A small prompt bound (an 800-character budget for entries), so
    // `wide/` (24 entries, about 1,400 characters) is cut into parts.
    cfg.llm.summaries.max_input_tokens = 4_000;
    let Some(tree) = alpha_tree(cfg) else {
        return;
    };
    let (pg, files, o) = (&tree.t.0, tree.files.clone(), tree.o.clone());
    let scope = dirsum::scope(&files);

    let units = dirsum::units(pg, &files, &o).unwrap();
    let ids: Vec<&str> = units.iter().map(|u| u.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["alpha:src/core", "alpha:src", "alpha:wide", "alpha"]
    );
    let dc = dirsum::coverage(pg, &units, &scope, &o).unwrap();
    assert_eq!((dc.units, dc.projects, dc.large), (4, 1, 1), "{dc:?}");
    assert!(dc.parts >= 2, "{dc:?}");

    let runs = dirsum::run(pg, units.clone(), &scope, &o, &mut |_, _| {}).unwrap();
    let labels: Vec<&str> = runs.iter().map(|(l, _)| l.as_str()).collect();
    assert_eq!(
        labels,
        vec![
            "depth 2",
            "depth 1, parts of 1 large directories",
            "depth 1",
            "projects"
        ],
        "{runs:?}"
    );
    let rows = pg
        .query(
            "SELECT entity_id, status, summary, coalesce((meta->>'incomplete')::int, 0) FROM summaries
             WHERE meta->>'unit' IN ('dir', 'project', 'part') ORDER BY entity_id, id",
            &[],
        )
        .unwrap();
    let live: Vec<(String, String)> = rows
        .iter()
        .filter(|r| r.get::<_, String>(1) == "live")
        .map(|r| (r.get(0), r.get(2)))
        .collect();
    assert_eq!(live.len(), 4, "{live:?}");
    // Every child settled before its parent: nothing provisional.
    assert!(rows.iter().all(|r| r.get::<_, i32>(3) == 0));
    assert!(
        live.iter()
            .any(|(id, s)| id == "alpha:src" && s.starts_with("Holds the src code")),
        "{live:?}"
    );
    let parts = rows
        .iter()
        .filter(|r| r.get::<_, String>(1) == "part")
        .count();
    assert_eq!(parts, dc.parts);
    // A directory reads its children's summaries; the large one its parts';
    // the project its README, scrubbed.
    let prompts = seen.lock().unwrap().clone();
    let src = prompts.iter().find(|p| p.contains("Path: src\n")).unwrap();
    assert!(src.contains("Holds the core code"), "{src}");
    assert!(src.contains("Handles the lib part"), "{src}");
    let wide = prompts
        .iter()
        .rfind(|p| p.contains("Path: wide\n"))
        .unwrap();
    assert!(wide.contains("Parts (each summarized"), "{wide}");
    let top = prompts
        .iter()
        .find(|p| p.contains("Path: (the project's top)"))
        .unwrap();
    assert!(
        top.contains("Alpha turns plain text into pictures"),
        "{top}"
    );
    assert!(!top.contains("hunter2pass"), "{top}");
    assert!(top.contains("Holds the wide code"), "{top}");

    // A resume: nothing to do.
    let n = seen.lock().unwrap().len();
    let runs = dirsum::run(pg, units, &scope, &o, &mut |_, _| {}).unwrap();
    let written: usize = runs.iter().map(|(_, s)| s.written).sum();
    assert_eq!(written, 0, "{runs:?}");
    assert_eq!(seen.lock().unwrap().len(), n);

    // The sync shows a directory's summary over its seed.
    super::sync::sync(&tree.ix.conn, pg, false).unwrap();
    let s: String = pg
        .query("SELECT summary FROM entities WHERE id = 'alpha:src'", &[])
        .unwrap()[0]
        .get(0);
    assert!(s.starts_with("Holds the src code"), "{s}");
}

/// The bottom-up tests' tree, crawled, synced and its files summarized:
/// `alpha/src/core/{parse,render}.rs`, `alpha/src/lib.rs`,
/// `alpha/README.md` (with a password in a URL) and 24 files in
/// `alpha/wide/`.
struct Tree {
    t: Throwaway,
    ix: Index,
    files: Vec<crate::summarize::Candidate>,
    o: Options,
    _dir: tempfile::TempDir,
}

fn alpha_tree(cfg: Config) -> Option<Tree> {
    let t = throwaway(cfg.clone())?;
    let pg = &t.0;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("dev");
    let body = |name: &str| {
        format!(
            "//! The {name} module.\n{}",
            format!("pub fn {name}_step() {{ let x = 1; }}\n").repeat(12)
        )
    };
    for n in ["parse", "render"] {
        write(&root, &format!("alpha/src/core/{n}.rs"), &body(n));
    }
    write(&root, "alpha/src/lib.rs", &body("lib"));
    write(
        &root,
        "alpha/README.md",
        "# Alpha\n\nAlpha turns plain text into pictures.\n\nMirror: https://reader:hunter2pass@example.com/alpha\n",
    );
    for i in 0..24 {
        write(
            &root,
            &format!("alpha/wide/w{i:02}.rs"),
            &body(&format!("w{i:02}")),
        );
    }
    let mut ix = Index::open(&dir.path().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let o = Options {
        code: "test".into(),
        ..Default::default()
    };
    let cands = summ::candidates(pg, &o).unwrap();
    let (files, cov) = summ::select(&cands, &cfg.llm.summaries);
    summ::run(pg, files.clone(), &o, &cov, &mut |_| {}).unwrap();
    Some(Tree {
        t,
        ix,
        files,
        o,
        _dir: dir,
    })
}

/// The pass's guards. A preflight's sample (the project alone) is written
/// provisional, and the full pass composes it again. A withheld directory
/// isn't a unit and a withheld file is named, not described. A large
/// directory whose parts failed waits (no row; its parent provisional)
/// until a resume with `--retry-failed`. The status file covers the whole
/// pass; one wall-clock cap spans its runs.
#[test]
fn bottom_up_provisional_waiting_withheld_and_one_cap() {
    use super::dirsum;
    use std::time::Duration;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let fail_parts = Arc::new(AtomicBool::new(true));
    let handler = {
        let (inner, fail) = (dir_answers(seen.clone()), fail_parts.clone());
        move |v: &Value| {
            let user = v["messages"][1]["content"].as_str().unwrap_or("");
            if fail.load(Ordering::SeqCst) && user.contains("Entries (part ") {
                return (200, completion("No JSON here."));
            }
            inner(v)
        }
    };
    let url = fake_server(vllm(handler));
    let mut cfg = Config::default();
    cfg.llm.summaries.endpoint = url;
    cfg.llm.summaries.backoff_ms = 1;
    cfg.llm.summaries.concurrency = 4;
    cfg.llm.summaries.max_input_tokens = 4_000;
    cfg.summaries.withhold = vec!["alpha:src/core/".into(), "alpha:src/lib.rs".into()];
    let Some(tree) = alpha_tree(cfg) else {
        return;
    };
    let (pg, files) = (&tree.t.0, tree.files.clone());
    let stats = tree._dir.path().join("pass.json");
    let o = Options {
        stats_path: Some(stats.clone()),
        ..tree.o.clone()
    };
    let scope = dirsum::scope(&files);
    let units = dirsum::units(pg, &files, &o).unwrap();
    let ids: Vec<&str> = units.iter().map(|u| u.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha:src", "alpha:wide", "alpha"]);
    let unit_rows = |id: &str| -> Vec<(String, i32)> {
        pg.query(
            "SELECT status, coalesce((meta->>'incomplete')::int, 0) FROM summaries
             WHERE entity_id = $1 AND meta->>'unit' IN ('dir', 'project') ORDER BY id",
            &[&id],
        )
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
    };
    let pass = || -> Value { serde_json::from_slice(&std::fs::read(&stats).unwrap()).unwrap() };

    // A preflight of the project alone: its directories aren't settled.
    let top: Vec<_> = units
        .iter()
        .filter(|u| u.kind == "project")
        .cloned()
        .collect();
    dirsum::run(pg, top, &scope, &o, &mut |_, _| {}).unwrap();
    assert_eq!(unit_rows("alpha"), vec![("live".to_string(), 2)]);

    // The full pass, its parts failing: `wide` waits, the project is
    // composed again and stays provisional; `src` names its withheld
    // children only.
    dirsum::run(pg, units.clone(), &scope, &o, &mut |_, _| {}).unwrap();
    assert!(unit_rows("alpha:wide").is_empty());
    assert_eq!(unit_rows("alpha:src"), vec![("live".to_string(), 0)]);
    assert_eq!(
        unit_rows("alpha"),
        vec![("superseded".to_string(), 2), ("live".to_string(), 1)]
    );
    let prompts = seen.lock().unwrap().clone();
    let src = prompts.iter().find(|p| p.contains("Path: src\n")).unwrap();
    assert!(src.contains("core/, lib.rs"), "{src}");
    assert!(
        !src.contains("core code") && !src.contains("lib part"),
        "{src}"
    );
    assert!(!prompts.iter().any(|p| p.contains("Path: src/core\n")));
    assert!(!prompts.iter().any(|p| p.contains("Path: wide\n")));
    let p = pass();
    assert_eq!(p["state"], "finished", "{p}");
    assert_eq!(p["wave"], "projects", "{p}");
    assert_eq!(p["waiting_on_parts"], 1, "{p}");
    assert_eq!(p["provisional"], 1, "{p}");
    let nparts = p["parts"].as_u64().unwrap();
    assert!(nparts >= 2, "{p}");
    assert_eq!(p["total"].as_u64().unwrap(), 3 + nparts, "{p}");
    assert_eq!(p["failed"].as_u64().unwrap(), nparts, "{p}");
    assert_eq!(p["done"], p["total"], "{p}");
    assert_eq!(p["waves"].as_array().unwrap().len(), 3, "{p}");

    // A resume with --retry-failed: the parts, then `wide` from them, then
    // the project, settled now.
    fail_parts.store(false, Ordering::SeqCst);
    let retry = Options {
        retry_failed: true,
        ..o.clone()
    };
    dirsum::run(pg, units.clone(), &scope, &retry, &mut |_, _| {}).unwrap();
    assert_eq!(unit_rows("alpha:wide"), vec![("live".to_string(), 0)]);
    assert_eq!(unit_rows("alpha").last().unwrap(), &("live".to_string(), 0));
    let wide = seen
        .lock()
        .unwrap()
        .iter()
        .rfind(|p| p.contains("Path: wide\n"))
        .cloned()
        .unwrap();
    assert!(wide.contains("Parts (each summarized"), "{wide}");

    // Then nothing is left.
    let n = seen.lock().unwrap().len();
    let runs = dirsum::run(pg, units.clone(), &scope, &o, &mut |_, _| {}).unwrap();
    assert_eq!(runs.iter().map(|(_, s)| s.written).sum::<usize>(), 0);
    assert_eq!(seen.lock().unwrap().len(), n);

    // One cap for the pass: a watch started 10 minutes ago stops a run
    // under a 5-minute cap before it asks anything.
    let mut c = files[0].clone();
    c.content_key.clear();
    let capped = Options {
        max_wall: Some(Duration::from_secs(300)),
        ..tree.o.clone()
    };
    let mut watch = summ::Watch::default();
    watch.t0 -= Duration::from_secs(600);
    let e = summ::run_watched(
        pg,
        vec![c],
        &capped,
        &summ::Coverage::default(),
        &mut watch,
        &mut |_| {},
    )
    .unwrap_err();
    assert!(
        format!("{e:#}").contains("wall-clock cap of 5 minutes"),
        "{e:#}"
    );
    assert_eq!(seen.lock().unwrap().len(), n);
    // And a pass past its cap says so in its status.
    let capped = Options {
        max_wall: Some(Duration::ZERO),
        ..o.clone()
    };
    pg.execute("DELETE FROM summaries WHERE meta->>'unit' = 'project'", &[])
        .unwrap();
    assert!(dirsum::run(pg, units, &scope, &capped, &mut |_, _| {}).is_err());
    let p = pass();
    assert_eq!(p["state"], "stopped", "{p}");
    assert!(
        p["aborted"].as_str().unwrap().contains("wall-clock cap"),
        "{p}"
    );
    assert_eq!(seen.lock().unwrap().len(), n);
}

/// A crate is marked copied when its manifest names a repository that
/// isn't the project's.
#[test]
fn copied_crates_by_manifest_repository() {
    let cargo =
        "[package]\nname = \"bitflags\"\nrepository = \"https://github.com/bitflags/bitflags\"\n";
    assert_eq!(
        summ::manifest_repository("Cargo.toml", cargo),
        "https://github.com/bitflags/bitflags"
    );
    assert_eq!(
        summ::manifest_repository(
            "package.json",
            r#"{"repository": {"url": "git+https://github.com/a/b.git"}}"#
        ),
        "git+https://github.com/a/b.git"
    );
    assert_eq!(
        summ::manifest_repository("Cargo.toml", "[package]\nname = \"x\"\n"),
        ""
    );
    assert!(summ::foreign_repository(
        "https://github.com/bitflags/bitflags",
        "makepad"
    ));
    assert!(!summ::foreign_repository(
        "https://github.com/makepad/makepad/",
        "makepad"
    ));
    assert!(!summ::foreign_repository(
        "https://github.com/me/glyph3d-rs",
        "glyph3d_rs"
    ));
    assert!(!summ::foreign_repository("", "makepad"));
}
