//! Tests of the Postgres store. The pure parts (chunking, terms, the card)
//! always run; the rest run against a throwaway schema in the database
//! `AGENT_INDEX_PG` names, dropped afterwards, and are skipped (passing)
//! when it isn't set.

use std::path::Path;

use super::sync::{camel_terms, chunk_text, path_terms};
use super::{Pg, check_schema};
use crate::crawl::{CrawlOptions, crawl};
use crate::db::Index;
use crate::query::SearchOptions;

#[test]
fn chunks_by_heading_and_window() {
    let md = "intro\n# One\na\n```\n# not a heading\n```\n## Two\nb\n";
    let c = chunk_text(md, true);
    let heads: Vec<&str> = c.iter().map(|c| c.heading.as_str()).collect();
    assert_eq!(heads, vec!["", "One", "Two"]);
    assert_eq!((c[1].line_start, c[1].line_end), (2, 6));
    let code: String = (0..1000).map(|i| format!("line {i}\n")).collect();
    let c = chunk_text(&code, false);
    assert_eq!(c.len(), 3);
    assert_eq!((c[0].line_start, c[0].line_end), (1, 400));
    assert_eq!(c[2].line_end, 1000);
    // A long line is cut, never one chunk past the cap.
    let long = "x".repeat(100_000);
    let c = chunk_text(&long, false);
    assert!(c.iter().all(|c| c.text.len() <= super::sync::CHUNK_BYTES));
    assert_eq!(c.iter().map(|c| c.text.len()).sum::<usize>(), 100_000);
}

#[test]
fn terms_split_identifiers() {
    assert_eq!(
        camel_terms("let x = gatherGlobal(HTTPServer); snake_case"),
        "gather global http server"
    );
    let p = path_terms("rs/crates/agent-index/src/WallLayout.rs");
    for w in [
        "rs",
        "crates",
        "agent",
        "index",
        "walllayout",
        "wall",
        "layout",
    ] {
        assert!(p.split(' ').any(|t| t == w), "{w} in {p}");
    }
    assert!(check_schema("idx_test_1").is_ok());
    assert!(check_schema("Idx; drop").is_err());
}

/// A throwaway schema, dropped when the test ends.
struct Throwaway(Pg);

impl Drop for Throwaway {
    fn drop(&mut self) {
        let _ = self.0.drop_schema();
    }
}

fn throwaway(tag: &str) -> Option<Throwaway> {
    let (url, _) = Pg::from_env()?;
    let schema = format!(
        "idx_test_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    );
    Some(Throwaway(Pg::connect(&url, &schema).expect("connect")))
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A small tree: two projects, one depending on the other.
fn fixture() -> (tempfile::TempDir, Index) {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().join("dev");
    write(
        &root,
        "alpha/Cargo.toml",
        "[package]\nname = \"alpha\"\ndescription = \"The alpha tool\"\n\n[dependencies]\nbeta = { path = \"../beta\" }\n",
    );
    write(
        &root,
        "alpha/README.md",
        "# Alpha\n\nAlpha delivers held messages to busy agents.\n\n## Word wall\n\nThe word wall lays glyphs out.\n",
    );
    write(
        &root,
        "alpha/src/lib.rs",
        "//! The alpha library.\n\n/// Delivers a held message.\npub fn deliver_held() {}\n\npub struct WallLayout;\n\nfn gatherGlobal() { let renderGlyphs = 1; }\n",
    );
    write(
        &root,
        "alpha/src/deliver.rs",
        "//! Held-message delivery.\npub fn wake() { deliver_held(); }\n",
    );
    write(
        &root,
        "beta/Cargo.toml",
        "[package]\nname = \"beta\"\ndescription = \"Beta parses configs\"\n",
    );
    write(
        &root,
        "beta/src/lib.rs",
        "//! Beta.\npub fn parse_config() {}\n// zebraword appears here only\n",
    );
    write(
        &root,
        "beta/docs/GUIDE.md",
        "# Guide\n\nSee `src/lib.rs` for parse_config.\n",
    );
    let mut ix = Index::open(&t.path().join("index.db")).unwrap();
    crate::roots::add_root(&ix, &root, "full").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    (t, ix)
}

/// Every row the sync writes, as text, to compare two syncs.
fn dump(pg: &Pg) -> Vec<String> {
    let mut out = Vec::new();
    for (table, cols, order) in [
        (
            "entities",
            "id, kind, project, parent, rel, name, path, file_kind, size, lines, line, content_key, title, importance::text, seed, summary, why, terms, labels_text, symbols, tsv::text, tri, embed::text, text_key, chunk_key, sync_key",
            "id",
        ),
        (
            "chunks",
            "entity_id, ord, line_start, line_end, heading, text, terms, tsv_en::text, tsv_simple::text",
            "entity_id, ord",
        ),
        (
            "labels",
            "entity_id, label, norm, compact, kind, origin, weight::text, evidence",
            "entity_id, label, kind",
        ),
        (
            "edges",
            "src, dst, kind, weight::text, origin, evidence",
            "src, dst, kind",
        ),
        ("aliases", "old_id, new_id", "old_id"),
        ("dangling", "doc, ref, line", "doc, ref"),
        (
            "projects",
            "name, path, depth, is_git::text, crawled_depth",
            "name",
        ),
        ("roots", "id, path, depth", "id"),
        ("embed_queue", "entity_id, text_key", "entity_id"),
    ] {
        for r in pg
            .query(
                &format!("SELECT concat_ws(' | ', {cols}) FROM {table} ORDER BY {order}"),
                &[],
            )
            .unwrap()
        {
            out.push(format!("{table}: {}", r.get::<_, String>(0)));
        }
    }
    out
}

#[test]
fn sync_incremental_equals_rebuild() {
    let Some(t) = throwaway("sync") else { return };
    let pg = &t.0;
    let (dir, mut ix) = fixture();
    let r = super::sync::sync(&ix.conn, pg, false).unwrap();
    assert!(r.written > 10, "{r:?}");
    assert!(r.chunks > 0 && r.queued > 0, "{r:?}");
    // A second sync with nothing changed writes nothing.
    let r = super::sync::sync(&ix.conn, pg, false).unwrap();
    assert_eq!((r.written, r.deleted, r.chunks), (0, 0, 0), "{r:?}");
    assert!(!r.edges_replaced && r.tables_replaced.is_empty(), "{r:?}");
    // Change a file, remove one, rename one; crawl and sync incrementally.
    let root = dir.path().join("dev");
    write(
        &root,
        "alpha/src/lib.rs",
        "//! The alpha library, changed.\npub fn deliver_held() {}\npub fn quorumSense() {}\n",
    );
    std::fs::remove_file(root.join("beta/docs/GUIDE.md")).unwrap();
    std::fs::rename(
        root.join("alpha/src/deliver.rs"),
        root.join("alpha/src/delivery.rs"),
    )
    .unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let r = super::sync::sync(&ix.conn, pg, false).unwrap();
    assert!(r.written > 0 && r.deleted > 0, "{r:?}");
    let incremental = dump(pg);
    assert!(incremental.iter().any(|l| l.contains("quorumsense")));
    assert!(
        !incremental
            .iter()
            .any(|l| l.starts_with("entities: beta:docs/GUIDE.md"))
    );
    // A rebuild from scratch gives the same rows.
    super::sync::sync(&ix.conn, pg, true).unwrap();
    let rebuilt = dump(pg);
    let a: std::collections::BTreeSet<_> = incremental.iter().collect();
    let b: std::collections::BTreeSet<_> = rebuilt.iter().collect();
    let only_inc: Vec<_> = a.difference(&b).collect();
    let only_reb: Vec<_> = b.difference(&a).collect();
    assert!(
        only_inc.is_empty() && only_reb.is_empty(),
        "incremental only: {only_inc:#?}\nrebuild only: {only_reb:#?}"
    );
}

fn opts(q: &str) -> SearchOptions {
    SearchOptions {
        query: q.to_string(),
        limit: 10,
        ..Default::default()
    }
}

#[test]
fn search_show_tree_related() {
    let Some(t) = throwaway("search") else { return };
    let pg = &t.0;
    let (dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    // An identifier's definition first.
    let r = super::search::search(pg, &opts("parse_config"), None, Default::default()).unwrap();
    assert_eq!(
        r.hits[0].id, "beta:src/lib.rs#parse_config",
        "{:#?}",
        r.hits
    );
    // A camelCase name by its words (terms split it).
    let r = super::search::search(pg, &opts("wall layout"), None, Default::default()).unwrap();
    assert!(
        r.hits
            .iter()
            .take(3)
            .any(|h| h.id.contains("WallLayout") || h.id.contains("word-wall")),
        "{:#?}",
        r.hits
    );
    // Words only in a file's body: the full-text list over chunks.
    let r = super::search::search(pg, &opts("zebraword"), None, Default::default()).unwrap();
    assert_eq!(r.hits[0].id, "beta:src/lib.rs", "{:#?}", r.hits);
    assert!(
        r.hits[0]
            .explain
            .iter()
            .any(|e| e.contains("full text") || e.contains("files holding"))
    );
    // A camelCase identifier inside a body, by its words.
    let r = super::search::search(pg, &opts("render glyphs"), None, Default::default()).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id == "alpha:src/lib.rs"),
        "{:#?}",
        r.hits
    );
    // A typo, by trigrams.
    let r = super::search::search(pg, &opts("delivr"), None, Default::default()).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id.starts_with("alpha:src/deliver")),
        "{:#?}",
        r.hits
    );
    assert_eq!(r.absent, vec!["delivr".to_string()]);
    // A label (the package name, the README title) pins its entity.
    let r = super::search::search(pg, &opts("Alpha"), None, Default::default()).unwrap();
    assert!(
        r.mapping.iter().any(|m| m.entity == "alpha" && m.exact),
        "{r:#?}"
    );
    assert_eq!(r.hits[0].id, "alpha");
    // --kind file maps a symbol to its file; --project and --under filter.
    let mut o = opts("deliver held");
    o.kind = "file".into();
    let r = super::search::search(pg, &o, None, Default::default()).unwrap();
    assert!(r.hits.iter().all(|h| h.kind == "file"), "{:#?}", r.hits);
    let mut o = opts("parse config");
    o.project = "alpha".into();
    let r = super::search::search(pg, &o, None, Default::default()).unwrap();
    assert!(r.hits.iter().all(|h| h.project == "alpha"));
    // The caller's project first, and its dependency next.
    let mut o = opts("lib");
    o.caller_project = "alpha".into();
    let r = super::search::search(pg, &o, None, Default::default()).unwrap();
    assert_eq!(r.hits[0].project, "alpha", "{:#?}", r.hits);
    assert!(
        r.hits
            .iter()
            .flat_map(|h| &h.explain)
            .any(|e| e.contains("own project"))
    );

    // show, tree, related.
    let cwd = dir.path().join("dev/alpha");
    let res = super::view::resolve(pg, "src/lib.rs", &cwd).unwrap();
    assert_eq!(res.id, "alpha:src/lib.rs");
    let s = super::view::show(pg, res).unwrap();
    assert_eq!(
        s.breadcrumb
            .iter()
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["@dev", "alpha", "alpha:src"]
    );
    assert!(s.symbols.iter().any(|b| b.name == "deliver_held"));
    let v1 = crate::view::show(
        &ix.conn,
        crate::view::resolve(&ix.conn, "src/lib.rs", &cwd).unwrap(),
    )
    .unwrap();
    assert_eq!(
        serde_json::to_value(&s).unwrap(),
        serde_json::to_value(&v1).unwrap()
    );
    let tr = super::view::tree(pg, "alpha", 3, 0).unwrap();
    let v1t = crate::view::tree(&ix.conn, "alpha", 3, 0).unwrap();
    assert_eq!(
        crate::view::render_tree(&tr),
        crate::view::render_tree(&v1t)
    );
    let tr = super::view::tree(pg, "", 2, 0).unwrap();
    let v1t = crate::view::tree(&ix.conn, "", 2, 0).unwrap();
    assert_eq!(
        crate::view::render_tree(&tr),
        crate::view::render_tree(&v1t)
    );
    let rel = super::view::related(pg, "alpha").unwrap();
    let v1r = crate::view::related(&ix.conn, "alpha").unwrap();
    assert_eq!(
        serde_json::to_value(&rel).unwrap(),
        serde_json::to_value(&v1r).unwrap()
    );
    assert!(rel.groups.contains_key("depends on"), "{rel:#?}");
}

/// The A/B's BM25 lists (pg_search), where the database has the extension.
#[test]
fn search_bm25_lists() {
    use super::search::Lexical;
    let Some(t) = throwaway("bm25") else { return };
    let pg = &t.0;
    if !super::schema::has_bm25(pg).unwrap() {
        // No pg_search here: asking for BM25 says so, and names the way out.
        let e = super::search::search(pg, &opts("deliver"), None, Lexical::Bm25).unwrap_err();
        assert!(format!("{e:#}").contains("pg_search"), "{e:#}");
        return;
    }
    let (_dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let lists = |r: &crate::query::SearchResult| -> Vec<String> {
        r.hits.iter().flat_map(|h| h.explain.clone()).collect()
    };
    // Cards: a snake_case name by its words, cut by the tokenizer.
    let r = super::search::search(pg, &opts("deliver held"), None, Lexical::Bm25).unwrap();
    assert!(
        r.hits
            .iter()
            .take(3)
            .any(|h| h.id.contains("deliver_held") || h.id.starts_with("alpha:src/deliver")),
        "{:#?}",
        r.hits
    );
    let l = lists(&r);
    assert!(l.iter().any(|e| e.starts_with("bm25 #")), "{l:?}");
    assert!(
        !l.iter()
            .any(|e| e.starts_with("lexical #") || e.starts_with("full text #")),
        "{l:?}"
    );
    // Chunks: a word only in a body, and a camelCase identifier by its words.
    let r = super::search::search(pg, &opts("zebraword"), None, Lexical::Bm25).unwrap();
    assert_eq!(r.hits[0].id, "beta:src/lib.rs", "{:#?}", r.hits);
    assert!(
        lists(&r).iter().any(|e| e.starts_with("bm25 full text #")),
        "{:#?}",
        r.hits
    );
    let r = super::search::search(pg, &opts("render glyphs"), None, Lexical::Bm25).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id == "alpha:src/lib.rs"),
        "{:#?}",
        r.hits
    );
    // Filters and the local lists.
    let mut o = opts("parse config");
    o.project = "alpha".into();
    let r = super::search::search(pg, &o, None, Lexical::Bm25).unwrap();
    assert!(r.hits.iter().all(|h| h.project == "alpha"));
    let mut o = opts("deliver held");
    o.caller_project = "alpha".into();
    let r = super::search::search(pg, &o, None, Lexical::Both).unwrap();
    let l = lists(&r);
    for name in [
        "bm25 here #",
        "bm25 full text here #",
        "lexical #",
        "full text #",
    ] {
        assert!(l.iter().any(|e| e.starts_with(name)), "{name}: {l:?}");
    }
}
