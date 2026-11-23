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
