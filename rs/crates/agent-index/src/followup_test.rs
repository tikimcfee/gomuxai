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
            ("src/lib.rs", "pub fn helper() {}\n"),
        ],
    );
    // Another project with many exact matches, whose ids sort first.
    mk(
        "aaa",
        &[
            (
                "src/a.rs",
                "pub fn effort() {}\npub fn helper() {}\npub fn spin() {}\n",
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
    // With no good match at home, the exact match elsewhere still leads.
    let r = top("spin", "alpha");
    assert_eq!(r[0], "aaa:src/a.rs#spin", "{r:?}");
    // From inside aaa, its own come first; with no caller, by name.
    let r = top("effort", "aaa");
    assert!(r[..3].iter().all(|id| id.starts_with("aaa:")), "{r:?}");
    let r = top("effort", "");
    assert_eq!(r[0], "aaa:src/a.rs#effort", "{r:?}");
}
