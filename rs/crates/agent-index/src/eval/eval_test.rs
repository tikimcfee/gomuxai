//! The eval on a small fixture tree, its own throwaway schema, and rg and
//! fd on that tree (skipped without AGENT_INDEX_PG or without rg).

use std::path::Path;

use super::*;
use crate::crawl::{CrawlOptions, crawl};
use crate::db::Index;

struct Throwaway(Option<Pg>);

impl Drop for Throwaway {
    fn drop(&mut self) {
        if let Some(pg) = self.0.take() {
            let _ = pg.drop_schema();
        }
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn have(cmd: &str) -> bool {
    std::process::Command::new(cmd)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Questions at a project's scope and over the tree, and episodes, through
/// the index in process and rg and fd; the rows, ranks and tables.
#[test]
fn questions_and_episodes() {
    let Some((url, _)) = Pg::from_env() else {
        return;
    };
    if !have("rg") || !have("fd") {
        return;
    }
    let schema = format!("idx_test_eval_{}", std::process::id());
    let t = tempfile::tempdir().unwrap();
    let dev = t.path().join("dev");
    write(
        &dev,
        "alpha/README.md",
        "# Alpha\n\nAlpha delivers held messages to busy agents.\n",
    );
    write(
        &dev,
        "alpha/src/deliver.rs",
        "//! Held-message delivery.\npub fn wake_numbat() { deliver_held(); }\n",
    );
    write(
        &dev,
        "alpha/src/lib.rs",
        "pub fn deliver_held() {}\npub struct WallLayout;\n",
    );
    write(
        &dev,
        "beta/src/parse.rs",
        "pub fn parse_config() {}\n// the wombat parser\n",
    );
    write(
        &dev,
        "beta/docs/GUIDE.md",
        "# Guide\n\nHow the wombat parser reads configs.\n",
    );
    let mut ix = Index::open(&t.path().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &dev, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let guard = Throwaway(Some(Pg::connect(&url, &schema).unwrap()));
    crate::pg::sync::sync(&ix.conn, guard.0.as_ref().unwrap(), false).unwrap();
    let real = std::fs::canonicalize(&dev).unwrap();
    let r = real.to_string_lossy();

    let qrels = t.path().join("q.jsonl");
    std::fs::write(
        &qrels,
        [
            r#"{"qid": "q1", "kind": "find", "project": "alpha", "query": "where is the numbat woken", "rel": {"alpha:src/deliver.rs": 2}}"#,
            r#"{"qid": "q2", "kind": "cross", "projects": ["alpha", "beta"], "query": "wombat parser and held delivery", "rel": {"beta:src/parse.rs": 2, "alpha:src/lib.rs": 2, "beta:docs/GUIDE.md": 1}}"#,
            r#"{"qid": "q3", "project": "alpha", "query": "a file that is gone", "rel": {"alpha:src/gone.rs": 2}}"#,
        ]
        .join("\n"),
    )
    .unwrap();
    let eps = t.path().join("e.jsonl");
    std::fs::write(
        &eps,
        format!(
            "{}\n{}\n",
            serde_json::json!({"session": "s1", "cwd": format!("{r}/alpha"), "ask": "make the numbat wake faster when messages are held",
                               "searches": ["grep:wake_numbat", "grep:wake_numbat", "glob:**/deliver*.rs", "grep:^#"], "edits": ["src/deliver.rs"]}),
            serde_json::json!({"session": "s1", "episode": 2, "cwd": format!("{r}/beta"), "searches": ["rg:parse_config"], "edits": [format!("{r}/beta/src/parse.rs")]}),
        ),
    )
    .unwrap();
    // A git repo makes alpha a project for the episodes' scopes.
    for p in ["alpha", "beta"] {
        std::fs::create_dir_all(dev.join(p).join(".git")).unwrap();
    }

    let o = Options {
        dev_root: dev.clone(),
        resamples: 200,
        ..Default::default()
    };
    let pg = Pg::connect(&url, &schema).unwrap();
    let mut searchers = vec![Searcher::new("base", pg, None)];
    let sets = vec![
        SetSpec {
            name: "q".into(),
            path: qrels,
            episodes: false,
        },
        SetSpec {
            name: "e".into(),
            path: eps,
            episodes: true,
        },
    ];
    let res = run(&o, &sets, &mut searchers, &mut |_| {}).unwrap();
    assert_eq!(res.index_errors, 0);
    let get = |set: &str, qid: &str, system: &str| -> &Row {
        res.rows
            .iter()
            .find(|x| x.set == set && x.qid == qid && x.system == system)
            .unwrap_or_else(|| panic!("no row {set} {qid} {system}"))
    };
    // q3's only relevant file is gone: dropped, and said.
    assert_eq!(res.sets[0].kept, 2);
    assert_eq!(
        res.sets[0].dropped["relevant entity: not a file in the tree now"],
        1
    );
    // A project question: rg in the project, the index --under it and from it.
    let q1 = get("q", "q1", "base:index@scope");
    assert_eq!((q1.scope.as_str(), q1.rank), ("project", 1), "{q1:?}");
    assert!(q1.top.iter().all(|f| f.starts_with(&format!("{r}/alpha/"))));
    // rg -w finds no word "numbat" inside wake_numbat.
    assert_eq!(get("q", "q1", "rg@scope").rank, 0);
    get("q", "q1", "base:index@cwd");
    // A tree question: no --under, rg over the tree; never question text.
    let q2 = get("q", "q2", "base:index@scope");
    assert_eq!(q2.scope, "tree");
    assert!(q2.rank >= 1, "{q2:?}");
    assert!(
        !serde_json::to_string(&res.rows)
            .unwrap()
            .contains("wombat parser and")
    );
    // Episodes: the repeated pattern is one search; "^#" has no word.
    let pats: Vec<&Row> = res
        .rows
        .iter()
        .filter(|x| x.set == "pattern" && x.system == "rg_patterns@project")
        .collect();
    assert_eq!(pats.len(), 3, "{pats:?}");
    assert_eq!(get("pattern", "a0000#1", "rg_patterns@project").rank, 1);
    assert_eq!(get("pattern", "a0000#2", "rg_patterns@project").mode, "fd");
    assert_eq!(get("episode", "a0000", "rg_patterns@project").turn, Some(1));
    assert_eq!(
        get("pattern", "a0000#1", "base:index_patterns@project").rank,
        1
    );
    // Both episodes are one session: one resampling unit.
    assert_eq!(get("episode", "a0001", "rg_patterns@project").unit, "s0");
    assert_eq!(get("ask_opening", "a0000", "rg_words@project").unit, "s0");
    let text = score::summary(&res, o.resamples, o.seed);
    assert!(
        text.contains("## q: base minus rg, at the question's scope"),
        "{text}"
    );
    assert!(text.contains("Episodes: index minus baseline"), "{text}");
    // A run against itself: nothing differs.
    let cmp = score::compare(&res.rows, &res.rows, Some("q"), None, 100, 1);
    assert!(cmp.contains("Queries whose top 10 differ: 0"), "{cmp}");
    drop(guard);
}
