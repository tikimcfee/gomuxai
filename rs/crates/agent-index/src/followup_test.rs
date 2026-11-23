//! Tests for the items after the review: Python and WGSL seeds, fixture
//! files, and the full-text list.

use crate::crawl::{CrawlOptions, crawl};
use crate::db::Index;
use crate::extract::extract;
use crate::files::file_kind;
use crate::index_test::{commit, git, temp, write};
use crate::query::{NoSemantic, SearchOptions, search};

#[test]
fn fixtures_have_their_own_kind_and_rank_down() {
    assert_eq!(
        file_kind("rs/crates/x/testdata/corpus/held.jsonl"),
        "fixture"
    );
    assert_eq!(file_kind("tests/fixtures/sample.rs"), "fixture");
    assert_eq!(file_kind("tests/data/run.txt"), "fixture");
    assert_eq!(file_kind("src/__snapshots__/a.snap"), "fixture");
    assert_eq!(file_kind("tests/it.rs"), "test");
    assert_eq!(file_kind("data/rows.jsonl"), "data");
    assert_eq!(file_kind("docs/notes.txt"), "doc");

    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let r = root.join("alpha");
    std::fs::create_dir_all(&r).unwrap();
    git(&r, &["init", "-q", "-b", "main"]);
    write(
        &r,
        "src/held.rs",
        "/// Delivers held messages.\npub fn deliver_held() {}\n",
    );
    write(
        &r,
        "testdata/held-delivery.jsonl",
        "{\"held\": \"delivery\"}\n",
    );
    write(&r, "tests/data/held-delivery.txt", "held delivery\n");
    commit(&r, "first");
    let mut ix = Index::open(&root.join("index.db")).unwrap();
    crate::roots::add_project(&ix, &r, "full", "").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let run = |kind: &str| {
        search(
            &ix.conn,
            &SearchOptions {
                query: "held delivery".into(),
                kind: kind.into(),
                limit: 10,
                ..Default::default()
            },
            &NoSemantic,
        )
        .unwrap()
    };
    let r = run("");
    assert!(
        r.hits[0].id.starts_with("alpha:src/held.rs"),
        "{:#?}",
        r.hits
    );
    let fx = r.hits.iter().find(|h| h.file_kind == "fixture").unwrap();
    assert!(fx.explain.iter().any(|e| e == "fixture −"));
    let r = run("fixture");
    assert_eq!(r.hits.len(), 2);
    assert!(r.hits.iter().all(|h| h.file_kind == "fixture"));
}

#[test]
fn python_docstrings_after_shebangs_comments_and_boms() {
    for text in [
        "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n# Copyright someone\n\n\"\"\"Loads rows.\n\nMore.\"\"\"\nimport os\n",
        "\u{feff}\"\"\"Loads rows.\"\"\"\n",
        "#!/usr/bin/env python3\n\n'''\nLoads rows.\n'''\n",
        "# a comment\n\"Loads rows.\"\nimport os\n",
    ] {
        let x = extract("tool.py", "python", text, true);
        assert_eq!(x.seed.as_deref(), Some("Loads rows."), "{text:?}");
    }
    // Code first: no module docstring.
    let x = extract(
        "tool.py",
        "python",
        "import os\n\"\"\"Not a docstring.\"\"\"\n",
        true,
    );
    assert_eq!(x.seed, None);
}

#[test]
fn wgsl_seeds_and_symbols() {
    let text = "// Lays out glyph quads.\n// One instance per glyph.\n\nstruct Glyph {\n  pos: vec2f,\n}\n\n@group(0) @binding(0) var<uniform> u: Uniforms;\nconst MAX_GLYPHS: u32 = 4096u;\n\n/// The vertex stage.\n@vertex\nfn vs_main(@builtin(vertex_index) i: u32) -> @builtin(position) vec4f {\n  return vec4f();\n}\nfn helper() {}\n";
    let x = extract(
        "glyph.wgsl",
        crate::files::language("glyph.wgsl"),
        text,
        true,
    );
    assert_eq!(
        x.seed.as_deref(),
        Some("Lays out glyph quads. One instance per glyph.")
    );
    let syms: Vec<(&str, &str)> = x
        .symbols
        .iter()
        .map(|s| (s.kind.as_str(), s.name.as_str()))
        .collect();
    assert_eq!(
        syms,
        vec![
            ("struct", "Glyph"),
            ("var", "u"),
            ("var", "MAX_GLYPHS"),
            ("fn", "vs_main"),
            ("fn", "helper")
        ]
    );
    assert_eq!(x.symbols[3].doc, "The vertex stage.");
    assert_eq!(crate::files::file_kind("shaders/glyph.wgsl"), "source");
}

/// A semantic list for tests: fixed pairs, whatever the query.
struct Fixed(Vec<(String, f64)>);

impl crate::query::Semantic for Fixed {
    fn ranked(
        &self,
        _: &rusqlite::Connection,
        _: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<(String, f64)>> {
        Ok(self.0.iter().take(limit).cloned().collect())
    }
}

#[test]
fn full_text_and_semantic_lists_join_the_fusion() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let a = root.join("alpha");
    std::fs::create_dir_all(&a).unwrap();
    git(&a, &["init", "-q", "-b", "main"]);
    // The word is only in a function's body: no name, doc or path has it.
    write(
        &a,
        "src/rot.rs",
        "pub fn turn(a: f32) -> f32 {\n    let quaternion_slerp = a * 2.0;\n    quaternion_slerp\n}\n",
    );
    write(&a, "src/other.rs", "pub fn other() {}\n");
    write(&a, "testdata/rot.txt", "quaternion_slerp\n");
    commit(&a, "first");
    let b = root.join("beta");
    write(
        &b,
        "src/rot.rs",
        "fn turn() { let quaternion_slerp = 1; }\n",
    );
    let mut ix = Index::open(&root.join("index.db")).unwrap();
    crate::roots::add_project(&ix, &a, "full", "").unwrap();
    crate::roots::add_project(&ix, &b, "files", "").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let c = &ix.conn;
    let q = |query: &str, sem: &dyn crate::query::Semantic| {
        search(
            c,
            &SearchOptions {
                query: query.into(),
                limit: 10,
                ..Default::default()
            },
            sem,
        )
        .unwrap()
    };
    let r = q("quaternion_slerp", &NoSemantic);
    assert_eq!(r.hits[0].id, "alpha:src/rot.rs", "{:#?}", r.hits);
    assert!(
        r.hits[0]
            .explain
            .iter()
            .any(|e| e.starts_with("full text #1"))
    );
    assert!(r.absent.is_empty());
    // Only full-depth projects, and not fixtures, are in the full text.
    let n = |id: &str| -> i64 {
        c.query_row(
            "SELECT count(*) FROM search_full_ids WHERE id = ?",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        (
            n("alpha:src/rot.rs"),
            n("beta:src/rot.rs"),
            n("alpha:testdata/rot.txt")
        ),
        (1, 0, 0)
    );
    // An edit drops the old text; a removed file drops its row.
    write(&a, "src/rot.rs", "pub fn turn(a: f32) -> f32 { a }\n");
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let c = &ix.conn;
    let r = search(
        c,
        &SearchOptions {
            query: "quaternion_slerp".into(),
            limit: 10,
            ..Default::default()
        },
        &NoSemantic,
    )
    .unwrap();
    assert!(
        !r.hits.iter().any(|h| h.id == "alpha:src/rot.rs"),
        "{:#?}",
        r.hits
    );
    std::fs::remove_file(a.join("src/other.rs")).unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let c = &ix.conn;
    let rows: i64 = c
        .query_row("SELECT count(*) FROM search_full_ids", [], |r| r.get(0))
        .unwrap();
    let fts: i64 = c
        .query_row("SELECT count(*) FROM search_full", [], |r| r.get(0))
        .unwrap();
    assert_eq!((rows, fts), (1, 1));

    // The semantic seam: its pairs join the fusion in their order, unknown ids
    // are dropped, the filters apply, and the score shows in the explanation.
    let sem = Fixed(vec![
        ("alpha:no/such.rs".into(), 0.99),
        ("beta:src/rot.rs".into(), 0.91),
        ("alpha:src/rot.rs".into(), 0.42),
    ]);
    let r = search(
        c,
        &SearchOptions {
            query: "spin something".into(),
            limit: 10,
            ..Default::default()
        },
        &sem,
    )
    .unwrap();
    let ids: Vec<&str> = r.hits.iter().map(|h| h.id.as_str()).collect();
    assert_eq!(
        &ids[..2],
        &["beta:src/rot.rs", "alpha:src/rot.rs"],
        "{ids:?}"
    );
    assert!(
        r.hits[0].explain.iter().any(|e| e == "semantic #1 (0.910)"),
        "{:?}",
        r.hits[0].explain
    );
    let r = search(
        c,
        &SearchOptions {
            query: "spin something".into(),
            project: "alpha".into(),
            limit: 10,
            ..Default::default()
        },
        &sem,
    )
    .unwrap();
    assert_eq!(r.hits[0].id, "alpha:src/rot.rs");
}

#[test]
fn a_schema_2_file_reads_every_file_again_to_fill_the_full_text() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let a = root.join("alpha");
    std::fs::create_dir_all(&a).unwrap();
    git(&a, &["init", "-q", "-b", "main"]);
    write(
        &a,
        "src/rot.rs",
        "fn turn() { let quaternion_slerp = 1; }\n",
    );
    commit(&a, "first");
    let path = root.join("index.db");
    {
        let mut ix = Index::open(&path).unwrap();
        crate::roots::add_project(&ix, &a, "full", "").unwrap();
        crawl(&mut ix, &CrawlOptions::default()).unwrap();
        // As a schema 2 file: no full text yet.
        ix.conn
            .execute_batch(
                "DELETE FROM search_full; DELETE FROM search_full_ids; PRAGMA user_version = 2;",
            )
            .unwrap();
    }
    let mut ix = Index::open(&path).unwrap();
    let r = crawl(&mut ix, &CrawlOptions::default()).unwrap();
    assert_eq!(r.projects[0].changed, 1, "{:?}", r.projects[0]);
    let n: i64 = ix
        .conn
        .query_row("SELECT count(*) FROM search_full_ids", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
    let again = crawl(&mut ix, &CrawlOptions::default()).unwrap();
    assert_eq!(again.projects[0].changed, 0);
}

#[test]
fn the_callers_project_and_its_dependencies_come_first() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    let mk = |name: &str, files: &[(&str, &str)]| {
        let p = root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        git(&p, &["init", "-q", "-b", "main"]);
        for (rel, text) in files {
            write(&p, rel, text);
        }
        commit(&p, "first");
    };
    mk(
        "alpha",
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"alpha\"\n\n[dependencies]\ncore = { path = \"../core\" }\n",
            ),
            (
                "src/lib.rs",
                "/// Sets the effort.\npub fn effort() {}\n/// Turns the wheel to spin it.\npub fn turn() {}\n",
            ),
        ],
    );
    mk(
        "core",
        &[
            ("Cargo.toml", "[package]\nname = \"core\"\n"),
            ("src/lib.rs", "pub fn helper() {}\npub fn wheel() {}\n"),
        ],
    );
    // Another project with many exact matches, whose ids sort first.
    mk(
        "aaa",
        &[
            (
                "src/a.rs",
                "pub fn effort() {}\npub fn helper() {}\npub fn spin() {}\npub fn zebra() {}\n",
            ),
            ("src/b.rs", "pub fn effort() {}\n"),
            ("src/c.rs", "pub fn effort() {}\n"),
        ],
    );
    let mut ix = Index::open(&root.parent().unwrap().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let top = |q: &str, caller: &str| -> Vec<String> {
        search(
            &ix.conn,
            &SearchOptions {
                query: q.into(),
                caller_project: caller.into(),
                limit: 10,
                ..Default::default()
            },
            &NoSemantic,
        )
        .unwrap()
        .hits
        .into_iter()
        .map(|h| h.id)
        .collect()
    };
    // An exact match at home beats exact matches elsewhere.
    let r = top("effort", "alpha");
    assert_eq!(r[0], "alpha:src/lib.rs#effort", "{r:?}");
    // One in a project yours depends on beats one in an unrelated project.
    let r = top("helper", "alpha");
    assert_eq!(r[0], "core:src/lib.rs#helper", "{r:?}");
    // A word used at home beats its definition in a dependency ...
    let r = top("wheel", "alpha");
    let pos = |id: &str| r.iter().position(|x| x == id).unwrap_or(99);
    assert!(
        pos("alpha:src/lib.rs#turn") < pos("core:src/lib.rs#wheel"),
        "{r:?}"
    );
    assert!(pos("core:src/lib.rs#wheel") < 99, "{r:?}");
    // ... and elsewhere ...
    let r = top("spin", "alpha");
    assert!(r[0].starts_with("alpha:src/lib.rs"), "{r:?}");
    assert!(r.iter().any(|id| id == "aaa:src/a.rs#spin"), "{r:?}");
    // ... and with nothing at home, the exact match elsewhere leads.
    let r = top("zebra", "alpha");
    assert_eq!(r[0], "aaa:src/a.rs#zebra", "{r:?}");
    // From inside aaa, its own come first; with no caller, by name.
    let r = top("effort", "aaa");
    assert!(r[..3].iter().all(|id| id.starts_with("aaa:")), "{r:?}");
    let r = top("effort", "");
    assert_eq!(r[0], "aaa:src/a.rs#effort", "{r:?}");
}

#[test]
fn what_made_a_dev_tree_index_eighteen_gigabytes_stays_out() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    // A generated header: thousands of object-like macros, a few real ones.
    let mut header = String::from("#define REG_FN(x) ((x) + 1)\nstruct reg_block { int a; };\n");
    for i in 0..3000 {
        header.push_str(&format!("#define REG_{i}_MASK 0x{i:x}\n"));
    }
    // A file with more definitions than are kept.
    let many: String = (0..800).map(|i| format!("pub fn f{i}() {{}}\n")).collect();
    let app = root.join("app");
    write(&app, "include/regs.h", &header);
    write(&app, "src/many.rs", &many);
    // A shelf of clones, kept out of the way, and a huge vendored tree.
    write(
        &root.join("_shelf"),
        "clone/src/lib.rs",
        "pub fn shelved() {}\n",
    );
    for i in 0..40 {
        write(
            &root.join("vendor_tree"),
            &format!("src/f{i}.c"),
            "int vendored(void) {\n  return 0;\n}\n",
        );
    }
    let mut ix = Index::open(&root.parent().unwrap().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    let r = crawl(
        &mut ix,
        &CrawlOptions {
            large_project: 30,
            ..Default::default()
        },
    )
    .unwrap();
    let c = &ix.conn;
    let n = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    // Only the function-like macro and the struct from the header.
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE kind = 'symbol' AND rel = 'include/regs.h'"),
        2
    );
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE kind = 'symbol' AND rel = 'src/many.rs'"),
        crate::crawl::MAX_SYMBOLS_PER_FILE as i64
    );
    // `_shelf` starts shallow: no files.
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE project = '_shelf' AND kind = 'file'"),
        0
    );
    let shelf: String = c
        .query_row(
            "SELECT depth FROM projects WHERE name = '_shelf'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(shelf, "shallow");
    // The huge one is crawled at files depth, and the report says so.
    let v = r.projects.iter().find(|p| p.name == "vendor_tree").unwrap();
    assert!(v.capped && v.depth == "shallow", "{v:?}");
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE project = 'vendor_tree' AND kind = 'file'"),
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE project = 'vendor_tree' AND kind = 'symbol'"),
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM search_full_ids WHERE id LIKE 'vendor_tree:%'"),
        0
    );
    // Symbols: a search row, no trigram row, no embedding queued.
    let syms = n("SELECT count(*) FROM entities WHERE kind = 'symbol'");
    let tri = n("SELECT count(*) FROM search_tri");
    let rows = n("SELECT count(*) FROM search_rowid");
    assert_eq!(rows - tri, syms);
    assert_eq!(
        n(
            "SELECT count(*) FROM embed_queue q JOIN entities e ON e.id = q.entity_id WHERE e.kind = 'symbol'"
        ),
        0
    );
    // A person can still ask for all of it.
    crate::roots::add_project(&ix, &root.join("vendor_tree"), "full", "").unwrap();
    crawl(
        &mut ix,
        &CrawlOptions {
            large_project: 30,
            ..Default::default()
        },
    )
    .unwrap();
    let c = &ix.conn;
    let syms: i64 = c
        .query_row(
            "SELECT count(*) FROM entities WHERE project = 'vendor_tree' AND kind = 'symbol'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(syms, 40);
}

#[test]
fn a_search_reaches_entities_by_index_and_times_its_stages() {
    let ix = Index::open_in_memory().unwrap();
    let c = &ix.conn;
    // Every join a ranked list makes is an index lookup, never a scan of the
    // map or the entities.
    for (table, bm25) in [
        ("search", "bm25(search)"),
        ("search_tri", "bm25(search_tri)"),
    ] {
        let sql = crate::query::fts_sql(table, bm25, 300);
        let mut st = c.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let plan: Vec<String> = st
            .query_map(rusqlite::params!["x", "", "", "", ""], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(
            plan.iter().any(|l| l.contains("search_rowid_rid")),
            "{table}: {plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|l| l.starts_with("SCAN m") || l.starts_with("SCAN e")),
            "{table}: {plan:?}"
        );
    }
    let r = search(
        c,
        &SearchOptions {
            query: "anything".into(),
            ..Default::default()
        },
        &NoSemantic,
    )
    .unwrap();
    for stage in [
        "labels",
        "exact names",
        "cards",
        "trigrams",
        "full text",
        "results",
    ] {
        assert!(r.ms.contains_key(stage), "{:?}", r.ms);
    }
}

#[test]
fn a_local_candidate_past_the_global_cut_still_ranks() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    // Another project with 350 entities that match better than the local one.
    let z = root.join("zeta");
    for i in 0..350 {
        write(
            &z,
            &format!("w/wobble_{i}.md"),
            &format!("# Wobble {i}\n\nWobble wobble.\n"),
        );
    }
    let a = root.join("alpha");
    write(
        &a,
        "src/spin.rs",
        "//! Keeps the wheel from a wobble at speed.\npub fn spin() {}\n",
    );
    let mut ix = Index::open(&root.parent().unwrap().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let run = |caller: &str| {
        search(
            &ix.conn,
            &SearchOptions {
                query: "wobble".into(),
                caller_project: caller.into(),
                limit: 10,
                ..Default::default()
            },
            &NoSemantic,
        )
        .unwrap()
    };
    // Globally it is past the top 300 of every list.
    assert!(!run("").hits.iter().any(|h| h.project == "alpha"));
    let r = run("alpha");
    assert_eq!(r.hits[0].id, "alpha:src/spin.rs", "{:#?}", r.hits);
    assert!(
        r.hits[0]
            .explain
            .iter()
            .any(|e| e.starts_with("lexical here #1"))
    );
}

#[test]
fn rust_byte_strings_with_r_hash_dont_hide_the_rest_of_a_file() {
    let text = "fn a() {\n    if name.starts_with(b\"r#\") {}\n}\n/// The parser's part.\npub fn parse_part() {}\nconst S: &str = r#\"\nfn not_a_symbol() {}\n\"#;\nfn after_raw() {}\nconst T: &str = br##\"fn inline_raw() {}\"##;\nfn last() {}\n";
    let x = extract("p.rs", "rust", text, true);
    let names: Vec<&str> = x.symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a", "parse_part", "S", "after_raw", "T", "last"],
        "{names:?}"
    );
}

#[test]
fn a_rare_identifier_finds_the_file_that_holds_it() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    let a = root.join("alpha");
    write(
        &a,
        "src/use_it.rs",
        "fn run() {\n    let v = frob_cache_zq.get();\n}\n",
    );
    write(&a, "src/frob.rs", "//! Frobs caches.\npub fn frob() {}\n");
    for i in 0..5 {
        write(
            &a,
            &format!("src/common_{i}.rs"),
            "fn f() { let shared_word_zq = 1; }\n",
        );
    }
    let mut ix = Index::open(&root.parent().unwrap().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let run = |q: &str| {
        search(
            &ix.conn,
            &SearchOptions {
                query: q.into(),
                caller_project: "alpha".into(),
                limit: 10,
                ..Default::default()
            },
            &NoSemantic,
        )
        .unwrap()
    };
    let r = run("frob_cache_zq");
    assert_eq!(r.hits[0].id, "alpha:src/use_it.rs", "{:#?}", r.hits);
    assert!(r.hits[0].explain[0].starts_with("one of 1 files holding"));
    // Held by five files: not rare, no pin.
    let r = run("shared_word_zq");
    assert!(
        !r.hits
            .iter()
            .any(|h| h.explain.iter().any(|e| e.contains("files holding")))
    );
}

#[test]
fn repos_in_a_plain_container_are_projects_and_a_cap_cuts_depth_not_siblings() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    let repo = |p: &std::path::Path, files: &[(&str, &str)]| {
        std::fs::create_dir_all(p).unwrap();
        git(p, &["init", "-q", "-b", "main"]);
        for (rel, text) in files {
            write(p, rel, text);
        }
        commit(p, "first");
    };
    // A shelf of clones (kept out of the way: shallow), and a plain dir at
    // full depth holding a repo; both hold a repo named `tool`.
    repo(
        &root.join("_shelf/tool"),
        &[
            ("README.md", "# Tool\n\nA shelved tool.\n"),
            ("src/t.rs", "pub fn shelved() {}\n"),
        ],
    );
    repo(
        &root.join("_shelf/procs"),
        &[("src/p.rs", "pub fn procs() {}\n")],
    );
    write(&root.join("_shelf"), "notes.md", "# Shelf\n");
    repo(
        &root.join("work/tool"),
        &[("src/t.rs", "pub fn worked() {}\n")],
    );
    write(&root.join("work"), "plan.md", "# Plan\n");
    let mut ix = Index::open(&root.parent().unwrap().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let c = &ix.conn;
    let proj = |name: &str| -> Option<(String, String)> {
        c.query_row(
            "SELECT path, depth FROM projects WHERE name = ?",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok()
    };
    let (p, d) = proj("tool").unwrap();
    assert!(p.ends_with("/_shelf/tool") && d == "shallow", "{p} {d}");
    assert_eq!(proj("procs").unwrap().1, "shallow");
    let (p, d) = proj("work-tool").unwrap();
    assert!(p.ends_with("/work/tool") && d == "full", "{p} {d}");
    let n = |sql: &str| -> i64 { c.query_row(sql, [], |r| r.get(0)).unwrap() };
    // Found as their own projects, not inside the container's crawl.
    assert!(crate::model::get(c, "tool").unwrap().is_some());
    assert_eq!(crate::model::get(c, "tool").unwrap().unwrap().title, "Tool");
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE id = 'work-tool:src/t.rs#worked'"),
        1
    );
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE project = 'work' AND rel LIKE 'tool/%'"),
        0
    );
    assert_eq!(
        n("SELECT count(*) FROM entities WHERE id = 'work:plan.md'"),
        1
    );

    // A walk that hits its cap drops the deepest level, not a later sibling.
    let w = root.parent().unwrap().join("wide");
    for i in 0..30 {
        write(&w, &format!("a/deep/x/f{i}.txt"), "x\n");
    }
    write(&w, "a/top.txt", "a\n");
    write(&w, "z/top.txt", "z\n");
    let got = crate::walk::walk_capped(&w, &[], 0, 10);
    let rels: Vec<&str> = got.files.iter().map(|f| f.rel.as_str()).collect();
    assert!(got.truncated);
    assert!(
        rels.contains(&"a/top.txt") && rels.contains(&"z/top.txt"),
        "{rels:?}"
    );
    // A shallow walk stops at the top directories.
    let got = crate::walk::walk(&w, &[], 2);
    assert!(got.files.iter().all(|f| f.rel.matches('/').count() <= 1));
}

#[test]
fn kind_file_gives_one_row_per_file() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    let a = root.join("alpha");
    write(
        &a,
        "src/frob.rs",
        "pub fn frob_one() {}\npub fn frob_two() {}\npub fn frob_three() {}\npub struct Frob;\n",
    );
    write(&a, "src/other.rs", "// calls frob\nfn x() {}\n");
    write(
        &a,
        "docs/FROB.md",
        "# Frob\n\nAbout frobbing.\n\n## Frob tuning\n\nTune the frob.\n",
    );
    for i in 0..12 {
        write(
            &a,
            &format!("src/f{i}.rs"),
            &format!("pub fn frob_{i}() {{}}\n"),
        );
    }
    let mut ix = Index::open(&root.parent().unwrap().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let run = |kind: &str| {
        search(
            &ix.conn,
            &SearchOptions {
                query: "frob".into(),
                kind: kind.into(),
                limit: 10,
                ..Default::default()
            },
            &NoSemantic,
        )
        .unwrap()
    };
    let r = run("file");
    assert_eq!(r.hits.len(), 10, "{:#?}", r.hits);
    let ids: Vec<&str> = r.hits.iter().map(|h| h.id.as_str()).collect();
    assert!(r.hits.iter().all(|h| h.kind == "file"), "{ids:?}");
    let distinct: std::collections::HashSet<&&str> = ids.iter().collect();
    assert_eq!(distinct.len(), 10, "{ids:?}");
    assert!(ids.contains(&"alpha:src/frob.rs"), "{ids:?}");
    // A symbol's file says which symbol brought it.
    let f = r.hits.iter().find(|h| h.id == "alpha:src/f3.rs").unwrap();
    assert!(
        f.explain[0] == "by alpha:src/f3.rs#frob_3" && f.line == 1,
        "{f:?}"
    );
    // A file kind maps sections to their doc.
    let r = run("doc");
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].id, "alpha:docs/FROB.md");
}
