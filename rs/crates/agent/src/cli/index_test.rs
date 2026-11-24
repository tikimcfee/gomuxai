//! agent index (no Go file): the commands over a made-up tree in the test's
//! temp dir, with index.db beside the test store.
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::testenv::TestEnv;

fn must(env: &mut TestEnv, words: &[&str]) -> String {
    let (out, res) = env.run(words);
    if let Err(e) = res {
        panic!("agent {}: {e:#}", words.join(" "));
    }
    out
}

fn fails(env: &mut TestEnv, words: &[&str]) -> String {
    let (_, res) = env.run(words);
    match res {
        Ok(()) => panic!("agent {} succeeded", words.join(" ")),
        Err(e) => format!("{e:#}"),
    }
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// A root `dev` with a git project alpha and a plain one, beta.
fn tree(env: &TestEnv) -> PathBuf {
    let root = std::fs::canonicalize(env.dir.path()).unwrap().join("dev");
    let alpha = root.join("alpha");
    std::fs::create_dir_all(&alpha).unwrap();
    git(&alpha, &["init", "-q", "-b", "main"]);
    write(&alpha, "README.md", "# Alpha\n\nAlpha draws the word wall.\n");
    write(&alpha, "word-wall/wall.rs", "//! Lays out the word wall.\n\n/// Places each word.\npub fn place_words() {}\n");
    write(&alpha, "docs/NOTES.md", "# Notes\n\nThe layout is in `word-wall/wall.rs`.\n");
    git(&alpha, &["add", "-A"]);
    git(&alpha, &["commit", "-q", "-m", "first"]);
    write(&root.join("beta"), "notes.md", "# Beta\n\nPlain notes, no git.\n");
    root
}

#[test]
fn index_root_crawl_search_show_tree() {
    let mut env = TestEnv::new();
    let e = fails(&mut env, &["index", "search", "wall"]);
    assert!(e.contains("nothing is indexed yet: agent index root add DIR"), "{e}");
    let out = must(&mut env, &["index", "root", "list"]);
    assert!(out.starts_with("The index covers nothing yet."), "{out}");

    let root = tree(&env);
    let r = root.to_str().unwrap();
    let out = must(&mut env, &["index", "root", "add", r]);
    assert_eq!(
        out,
        format!("Added root @dev ({r}), its projects at depth full.\nNext: agent index crawl reads it.\n")
    );
    let e = fails(&mut env, &["index", "root", "add", r]);
    assert!(e.contains("is already a root (@dev)"), "{e}");
    assert!(env.dir.path().join("index.db").exists());

    let out = must(&mut env, &["index", "crawl", "--dry-run"]);
    assert!(out.starts_with("Crawled 2 projects in ") && out.lines().next().unwrap().ends_with("(dry run: nothing written)"), "{out}");
    let out = must(&mut env, &["index", "crawl"]);
    assert!(out.contains("  alpha  git, full: 3 files"), "{out}");
    assert!(out.contains("  beta  not git, full: 1 files"), "{out}");
    assert!(out.contains("New project: alpha.") || !out.contains("dry run"), "{out}");

    let out = must(&mut env, &["index", "search", "word", "wall"]);
    let first: Vec<&str> = out.lines().take(3).collect();
    assert!(first[0].starts_with("Labels: \"word wall\" → `alpha:word-wall`"), "{out}");
    assert!(first[1].starts_with("1. alpha:word-wall   dir"), "{out}");
    assert_eq!(first[2], format!("   {r}/alpha/word-wall"));

    let out = must(&mut env, &["index", "search", "place_words", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let hit = &v["hits"][0];
    assert_eq!(hit["id"], "alpha:word-wall/wall.rs#place_words");
    assert_eq!(hit["kind"], "symbol");
    assert_eq!(hit["project"], "alpha");
    assert_eq!(hit["path"], format!("{r}/alpha/word-wall/wall.rs"));
    assert_eq!(hit["line"], 4);
    assert_eq!(hit["summary"], "Places each word.");
    assert_eq!(hit["freshness"], "seed");
    assert!(v["absent"].as_array().unwrap().is_empty());

    let out = must(&mut env, &["index", "search", "qqzzx"]);
    assert!(out.contains("No entity mentions `qqzzx`.") && out.contains("Nothing in the index matches"), "{out}");

    // Each search went to the query log (under the test's HOME), with its
    // top paths and the flags given; usage counts them.
    let log = crate::cli::testenv::isolate()
        .home
        .join(".local/share/gomuxai/index-queries.jsonl");
    let text = std::fs::read_to_string(&log).unwrap();
    let mine: Vec<serde_json::Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["query"] == "place_words")
        .collect();
    assert_eq!(mine.len(), 1, "{text}");
    assert_eq!(mine[0]["command"], "search");
    assert_eq!(mine[0]["store"], "sqlite");
    assert_eq!(mine[0]["flags"]["json"], true);
    assert_eq!(mine[0]["top"][0], format!("{r}/alpha/word-wall/wall.rs:4"));
    assert!(mine[0]["ms"].as_f64().unwrap() > 0.0);
    let out = must(&mut env, &["index", "usage"]);
    assert!(out.contains("By agent:") && out.contains("search "), "{out}");

    let file = format!("{r}/alpha/word-wall/wall.rs");
    let out = must(&mut env, &["index", "show", &file]);
    assert!(out.starts_with("alpha:word-wall/wall.rs   file (source, rust)  seed\n"), "{out}");
    assert!(out.contains("Summary (from the code itself): Lays out the word wall."), "{out}");
    assert!(out.contains("documented by: alpha:docs/NOTES.md"), "{out}");
    let out = must(&mut env, &["index", "show", "alpha:word-wall/wall.rs", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["entity"]["id"], "alpha:word-wall/wall.rs");
    assert_eq!(v["breadcrumb"][1]["id"], "alpha");

    let out = must(&mut env, &["index", "tree", "alpha", "--depth", "1"]);
    assert!(out.starts_with("alpha  Alpha: Alpha draws the word wall.\n"), "{out}");
    assert!(out.contains("word-wall/"), "{out}");

    let out = must(&mut env, &["index", "related", "alpha:word-wall/wall.rs"]);
    assert!(out.contains("documented by (1):\n  alpha:docs/NOTES.md"), "{out}");

    let out = must(&mut env, &["index", "stale", "--project", "alpha"]);
    assert!(out.starts_with("Freshness in alpha (projects, dirs and files): "), "{out}");

    let out = must(&mut env, &["index", "alias", "add", "alpha:docs/NOTES.md", "the layout notes"]);
    assert!(out.starts_with("Labeled alpha:docs/NOTES.md \"the layout notes\"."), "{out}");
    let out = must(&mut env, &["index", "search", "the layout notes"]);
    assert!(out.lines().nth(1).unwrap().starts_with("1. alpha:docs/NOTES.md "), "{out}");

    let lex = env.dir.path().join("lexicon.jsonl");
    std::fs::write(
        &lex,
        format!(
            "{{\"term\":\"placer\",\"canonical\":\"place_words\",\"provenance\":[\"{r}/alpha/word-wall/wall.rs:4\"],\"lay_phrasings\":[\"where words go\"]}}\n{{\"term\":\"ghost\",\"provenance\":[\"nowhere/x.rs:1\"]}}\n"
        ),
    )
    .unwrap();
    let out = must(&mut env, &["index", "lexicon", "import", lex.to_str().unwrap()]);
    assert!(out.starts_with("Imported 1 of 2 rows from "), "{out}");
    assert!(out.contains("Skipped 1 rows that resolve to no entity"), "{out}");

    let out = must(&mut env, &["index", "root", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["roots"][0]["id"], "@dev");
    assert_eq!(v["projects"].as_array().unwrap().len(), 2);

    let e = fails(&mut env, &["index", "show", "/etc"]);
    assert!(e.contains("outside every indexed project"), "{e}");
    let e = fails(&mut env, &["index", "crawl", "--project", "nope"]);
    assert!(e.contains("no project nope"), "{e}");

    let out = must(&mut env, &["index", "root", "remove", "@dev"]);
    assert!(out.starts_with("Removed root @dev ") && out.contains("2 projects found under it: alpha, beta."), "{out}");
}
