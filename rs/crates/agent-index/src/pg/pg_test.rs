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
    let cfg = crate::config::Chunks::default();
    let md = "intro\n# One\na\n```\n# not a heading\n```\n## Two\nb\n";
    let c = chunk_text(md, true, &cfg);
    let heads: Vec<&str> = c.iter().map(|c| c.heading.as_str()).collect();
    assert_eq!(heads, vec!["", "One", "Two"]);
    assert_eq!((c[1].line_start, c[1].line_end), (2, 6));
    let code: String = (0..1000).map(|i| format!("line {i}\n")).collect();
    let c = chunk_text(&code, false, &cfg);
    assert_eq!(c.len(), 3);
    assert_eq!((c[0].line_start, c[0].line_end), (1, 400));
    assert_eq!(c[2].line_end, 1000);
    // A long line is cut, never one chunk past the cap.
    let long = "x".repeat(100_000);
    let c = chunk_text(&long, false, &cfg);
    assert!(c.iter().all(|c| c.text.len() <= cfg.max_bytes));
    assert_eq!(c.iter().map(|c| c.text.len()).sum::<usize>(), 100_000);
    // The cut is the configuration's.
    let small = crate::config::Chunks {
        max_lines: 100,
        ..cfg.clone()
    };
    assert_eq!(chunk_text(&code, false, &small).len(), 10);
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
    let r =
        super::search::search(pg, &opts("parse_config"), None, Default::default(), None).unwrap();
    assert_eq!(
        r.hits[0].id, "beta:src/lib.rs#parse_config",
        "{:#?}",
        r.hits
    );
    // A camelCase name by its words (terms split it).
    let r =
        super::search::search(pg, &opts("wall layout"), None, Default::default(), None).unwrap();
    assert!(
        r.hits
            .iter()
            .take(3)
            .any(|h| h.id.contains("WallLayout") || h.id.contains("word-wall")),
        "{:#?}",
        r.hits
    );
    // Words only in a file's body: the full-text list over chunks.
    let r = super::search::search(pg, &opts("zebraword"), None, Default::default(), None).unwrap();
    assert_eq!(r.hits[0].id, "beta:src/lib.rs", "{:#?}", r.hits);
    assert!(
        r.hits[0]
            .explain
            .iter()
            .any(|e| e.contains("full text") || e.contains("files holding"))
    );
    // A camelCase identifier inside a body, by its words.
    let r =
        super::search::search(pg, &opts("render glyphs"), None, Default::default(), None).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id == "alpha:src/lib.rs"),
        "{:#?}",
        r.hits
    );
    // A typo, by trigrams.
    let r = super::search::search(pg, &opts("delivr"), None, Default::default(), None).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id.starts_with("alpha:src/deliver")),
        "{:#?}",
        r.hits
    );
    assert_eq!(r.absent, vec!["delivr".to_string()]);
    // A label (the package name, the README title) pins its entity.
    let r = super::search::search(pg, &opts("Alpha"), None, Default::default(), None).unwrap();
    assert!(
        r.mapping.iter().any(|m| m.entity == "alpha" && m.exact),
        "{r:#?}"
    );
    assert_eq!(r.hits[0].id, "alpha");
    // --kind file maps a symbol to its file; --project and --under filter.
    let mut o = opts("deliver held");
    o.kind = "file".into();
    let r = super::search::search(pg, &o, None, Default::default(), None).unwrap();
    assert!(r.hits.iter().all(|h| h.kind == "file"), "{:#?}", r.hits);
    let mut o = opts("parse config");
    o.project = "alpha".into();
    let r = super::search::search(pg, &o, None, Default::default(), None).unwrap();
    assert!(r.hits.iter().all(|h| h.project == "alpha"));
    // The caller's project first, and its dependency next.
    let mut o = opts("lib");
    o.caller_project = "alpha".into();
    let r = super::search::search(pg, &o, None, Default::default(), None).unwrap();
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

/// A search's results without its timings, to compare two runs.
fn results(pg: &Pg, o: &SearchOptions) -> serde_json::Value {
    let mut v =
        serde_json::to_value(super::search::search(pg, o, None, Default::default(), None).unwrap())
            .unwrap();
    v.as_object_mut().unwrap().remove("ms");
    v
}

/// The configuration file moves values out of the code without moving
/// results: a file holding every default searches as no file does, and a
/// changed value changes both the fingerprint and the results.
#[test]
fn config_defaults_search_the_same() {
    let Some(t) = throwaway("config") else { return };
    let pg = &t.0;
    let (_dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let (url, _) = Pg::from_env().unwrap();
    let full = crate::config::Config::parse(&crate::config::Config::default().to_toml()).unwrap();
    let pg_file = Pg::connect_with(&url, &pg.schema, full).unwrap();
    let queries = [
        "parse_config",
        "wall layout",
        "zebraword",
        "delivr",
        "Alpha",
        "deliver held",
    ];
    for q in queries {
        for caller in ["", "alpha"] {
            for kind in ["", "file"] {
                let o = SearchOptions {
                    caller_project: caller.into(),
                    kind: kind.into(),
                    ..opts(q)
                };
                assert_eq!(
                    results(pg, &o),
                    results(&pg_file, &o),
                    "{q} from {caller:?} --kind {kind:?}"
                );
            }
        }
    }
    // One file per result: a different list, and a different fingerprint.
    let one = crate::config::Config::parse("[search]\nper_file = 1\n").unwrap();
    let pg_one = Pg::connect_with(&url, &pg.schema, one).unwrap();
    let o = opts("deliver held");
    let (a, b) = (results(pg, &o), results(&pg_one, &o));
    assert_ne!(a["config"], b["config"]);
    let files = |v: &serde_json::Value| -> Vec<String> {
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| crate::model::file_of(h["id"].as_str().unwrap()).to_string())
            .collect()
    };
    let fb = files(&b);
    let mut dedup = fb.clone();
    dedup.dedup();
    assert_eq!(fb, dedup, "one result a file: {b:#}");
    assert!(files(&a).len() > fb.len(), "{a:#}");
}

/// The semantic list: card vectors on HNSW, the query vector sent as text.
#[test]
fn search_semantic_list() {
    let Some(t) = throwaway("dense") else { return };
    let pg = &t.0;
    let (_dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    // A stand-in vector for every entity that wants one, one direction for
    // beta's guide so the query finds it nearest.
    let one = |x: f32| super::vec_text(&[x; super::DIM]);
    let mut other = vec![0.0f32; super::DIM];
    other[0] = 1.0;
    pg.execute(
        "UPDATE entities SET card_vec = $1::text::halfvec(384), vec_key = text_key WHERE embed",
        &[&one(0.05)],
    )
    .unwrap();
    pg.execute(
        "UPDATE entities SET card_vec = $1::text::halfvec(384) WHERE id = 'beta:docs/GUIDE.md'",
        &[&super::vec_text(&other)],
    )
    .unwrap();
    pg.batch("INSERT INTO meta (key, value) VALUES ('vectors', '1')")
        .unwrap();
    let embed = |_: &str| -> anyhow::Result<Vec<f32>> { Ok(other.clone()) };
    let r =
        super::search::search(pg, &opts("guide"), Some(&embed), Default::default(), None).unwrap();
    let hit = r
        .hits
        .iter()
        .find(|h| h.id == "beta:docs/GUIDE.md")
        .expect("the guide");
    assert!(
        hit.explain.iter().any(|e| e.starts_with("semantic #1")),
        "{:#?}",
        r.hits
    );
}

/// The A/B's BM25 lists (pg_search), where the database has the extension.
#[test]
fn search_bm25_lists() {
    use super::search::Lexical;
    let Some(t) = throwaway("bm25") else { return };
    let pg = &t.0;
    if !super::schema::has_bm25(pg).unwrap() {
        // No pg_search here: asking for BM25 says so, and names the way out.
        let e = super::search::search(pg, &opts("deliver"), None, Lexical::Bm25, None).unwrap_err();
        assert!(format!("{e:#}").contains("pg_search"), "{e:#}");
        return;
    }
    let (_dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let lists = |r: &crate::query::SearchResult| -> Vec<String> {
        r.hits.iter().flat_map(|h| h.explain.clone()).collect()
    };
    // Cards: a snake_case name by its words, cut by the tokenizer.
    let r = super::search::search(pg, &opts("deliver held"), None, Lexical::Bm25, None).unwrap();
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
    let r = super::search::search(pg, &opts("zebraword"), None, Lexical::Bm25, None).unwrap();
    assert_eq!(r.hits[0].id, "beta:src/lib.rs", "{:#?}", r.hits);
    assert!(
        lists(&r).iter().any(|e| e.starts_with("bm25 full text #")),
        "{:#?}",
        r.hits
    );
    let r = super::search::search(pg, &opts("render glyphs"), None, Lexical::Bm25, None).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id == "alpha:src/lib.rs"),
        "{:#?}",
        r.hits
    );
    // Filters and the local lists.
    let mut o = opts("parse config");
    o.project = "alpha".into();
    let r = super::search::search(pg, &o, None, Lexical::Bm25, None).unwrap();
    assert!(r.hits.iter().all(|h| h.project == "alpha"));
    let mut o = opts("deliver held");
    o.caller_project = "alpha".into();
    let r = super::search::search(pg, &o, None, Lexical::Both, None).unwrap();
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

/// A stand-in reranker: answers `/score` with each document's index as its
/// score, so the last candidate sent comes first. Its address.
fn fake_reranker() -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut w = s;
                loop {
                    let mut len = 0usize;
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    loop {
                        let mut h = String::new();
                        r.read_line(&mut h).unwrap();
                        if h.trim().is_empty() {
                            break;
                        }
                        if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                            len = v.trim().parse().unwrap();
                        }
                    }
                    let mut body = vec![0; len];
                    r.read_exact(&mut body).unwrap();
                    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    let n = v["documents"].as_array().unwrap().len();
                    let data: Vec<_> = (0..n)
                        .map(|i| serde_json::json!({"index": i, "score": i as f64}))
                        .collect();
                    let out = serde_json::json!({"data": data}).to_string();
                    write!(
                        w,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{out}",
                        out.len()
                    )
                    .unwrap();
                }
            });
        }
    });
    format!("http://{addr}")
}

/// The reranker reorders the top candidates (pins kept above), and a
/// reranker that doesn't answer leaves the fused order, saying so.
#[test]
fn rerank_reorders_and_falls_back() {
    let Some(t) = throwaway("rerank") else { return };
    let pg = &t.0;
    let (_dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let o = opts("deliver held message");
    let plain = super::search::search(pg, &o, None, Default::default(), None).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.reranker.endpoint = fake_reranker();
    cfg.reranker.depth.insert("code".into(), 4);
    let rc = super::rerank::Client::new(&cfg.reranker);
    let (url, _) = Pg::from_env().unwrap();
    let pg2 = Pg::connect_with(&url, &pg.schema, cfg.clone()).unwrap();
    let r = super::search::search(&pg2, &o, None, Default::default(), Some(&rc)).unwrap();
    let info = r.rerank.as_ref().unwrap();
    assert!(info.ok && info.scored > 0, "{info:?}");
    assert!(r.ms.contains_key("rerank"));
    // The stand-in reverses what it was sent; nothing past the depth moves.
    let ids =
        |r: &crate::query::SearchResult| r.hits.iter().map(|h| h.id.clone()).collect::<Vec<_>>();
    let (a, b) = (ids(&plain), ids(&r));
    assert_ne!(a, b);
    assert!(
        r.hits.iter().any(|h| h.explain[0].starts_with("rerank #1")),
        "{:#?}",
        r.hits
    );
    // Nothing answers: the fused order, and why.
    cfg.reranker.endpoint = "http://127.0.0.1:9".into();
    cfg.reranker.timeout_ms = 500;
    let rc = super::rerank::Client::new(&cfg.reranker);
    let r = super::search::search(&pg2, &o, None, Default::default(), Some(&rc)).unwrap();
    let info = r.rerank.as_ref().unwrap();
    assert!(!info.ok && info.error.is_some(), "{info:?}");
    assert_eq!(ids(&r), a);
    assert!(
        r.hits
            .iter()
            .any(|h| h.explain.iter().any(|e| e.contains("fused order kept")))
    );
}

/// The warm process answers what a cold search answers.
#[test]
fn serve_answers_as_cold() {
    let Some(t) = throwaway("serve") else { return };
    let pg = &t.0;
    let (dir, ix) = fixture();
    super::sync::sync(&ix.conn, pg, false).unwrap();
    let (url, _) = Pg::from_env().unwrap();
    let sock = dir.path().join("index.sock");
    let listener = std::os::unix::net::UnixListener::bind(&sock).unwrap();
    let srv = super::serve::Server {
        url: url.clone(),
        schema: pg.schema.clone(),
        config: crate::config::Config::default(),
        embed: None,
    };
    std::thread::spawn(move || super::serve::serve_on(srv, listener, 2));
    let cwd = dir.path().join("dev/alpha").to_string_lossy().into_owned();
    let strip = |r: &super::serve::Response| {
        let mut v = serde_json::to_value(r).unwrap();
        v["result"].as_object_mut().unwrap().remove("ms");
        v
    };
    for (q, kind) in [
        ("parse_config", ""),
        ("deliver held", "file"),
        ("wall layout", ""),
        ("delivr", ""),
    ] {
        let req = super::serve::Request {
            query: q.into(),
            kind: kind.into(),
            cwd: cwd.clone(),
            fingerprint: pg.cfg.fingerprint(),
            build: super::serve::build_id(),
            ..Default::default()
        };
        let warm = super::serve::ask(&sock, &req, std::time::Duration::from_secs(30)).unwrap();
        let cold = super::serve::answer(pg, &req, None, None).unwrap();
        assert_eq!(strip(&warm), strip(&cold), "{q}");
        assert_eq!(warm.caller, "alpha");
    }
    // Another configuration is refused (the client then runs cold).
    let req = super::serve::Request {
        query: "lib".into(),
        fingerprint: "other".into(),
        build: super::serve::build_id(),
        ..Default::default()
    };
    assert!(super::serve::ask(&sock, &req, std::time::Duration::from_secs(30)).is_err());
}
