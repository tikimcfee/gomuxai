//! Regression tests for the independent review of v1 (findings 1 to 12),
//! each on a made-up tree in a temp dir under /var/tmp.

use std::path::{Path, PathBuf};

use rusqlite::params;

use crate::crawl::{CrawlOptions, crawl};
use crate::db::Index;
use crate::index_test::{commit, git, temp, write};
use crate::query::{NoSemantic, SearchOptions, search};
use crate::roots;
use crate::view;

struct T {
    dir: tempfile::TempDir,
    root: PathBuf,
    ix: Index,
}

/// An empty root `dev` (and a sibling `outside` dir that no project holds).
fn tree() -> T {
    let dir = temp();
    let base = std::fs::canonicalize(dir.path()).unwrap();
    let root = base.join("dev");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(base.join("outside")).unwrap();
    let ix = Index::open(&base.join("index.db")).unwrap();
    roots::add_root(&ix, &root, "full").unwrap();
    T { dir, root, ix }
}

impl T {
    fn outside(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path())
            .unwrap()
            .join("outside")
    }
    fn repo(&self, name: &str) -> PathBuf {
        let p = self.root.join(name);
        std::fs::create_dir_all(&p).unwrap();
        git(&p, &["init", "-q", "-b", "main"]);
        p
    }
    fn crawl(&mut self) -> crate::CrawlReport {
        crawl(&mut self.ix, &CrawlOptions::default()).unwrap()
    }
    fn rebuild(&mut self) -> crate::CrawlReport {
        crawl(
            &mut self.ix,
            &CrawlOptions {
                rebuild: true,
                ..Default::default()
            },
        )
        .unwrap()
    }
    fn get(&self, id: &str) -> Option<crate::Entity> {
        crate::model::get(&self.ix.conn, id).unwrap()
    }
    fn key(&self, id: &str) -> String {
        self.get(id).map(|e| e.content_key).unwrap_or_default()
    }
    fn search(&self, q: &str) -> crate::SearchResult {
        search(
            &self.ix.conn,
            &SearchOptions {
                query: q.into(),
                limit: 10,
                ..Default::default()
            },
            &NoSemantic,
        )
        .unwrap()
    }
    fn top(&self, q: &str) -> String {
        let r = self.search(q);
        r.hits.first().map(|h| h.id.clone()).unwrap_or_default()
    }
    fn count(&self, sql: &str) -> i64 {
        self.ix.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }
    /// Every search row's text, to look for leaked content.
    fn all_text(&self) -> String {
        let mut st = self
            .ix
            .conn
            .prepare("SELECT title || ' ' || summary || ' ' || labels FROM search")
            .unwrap();
        let v: Vec<String> = st
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        let mut st = self.ix.conn.prepare("SELECT text FROM summaries").unwrap();
        let s: Vec<String> = st
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        format!("{} {}", v.join(" "), s.join(" "))
    }
}

// 1: a shallow project's README that is a symlink out of the project.
#[test]
fn shallow_readme_symlink_is_not_read() {
    let mut t = tree();
    write(
        &t.outside(),
        "secret.txt",
        "# Secret Host\n\nOUTSIDE_PAYLOAD_999 is leaked.\n",
    );
    let p = t.root.join("p");
    std::fs::create_dir_all(&p).unwrap();
    std::os::unix::fs::symlink(t.outside().join("secret.txt"), p.join("README.md")).unwrap();
    write(&p, "sub/README.md", "# Sub\n\nFine.\n");
    roots::add_project(&t.ix, &p, "shallow", "").unwrap();
    t.crawl();
    assert!(!t.all_text().contains("OUTSIDE_PAYLOAD_999"));
    assert!(t.search("OUTSIDE_PAYLOAD_999").hits.is_empty());
    assert_eq!(t.get("p").unwrap().title, "p");
}

// 2: a symlinked directory in a git work tree.
#[test]
fn symlinked_dir_in_a_repo_is_not_followed() {
    let mut t = tree();
    write(
        &t.outside(),
        "od/notes.md",
        "# Leaked\n\nINTERMEDIATE_LEAK_777.\n",
    );
    let r = t.repo("repo");
    write(&r, "sub/notes.md", "# Notes\n\nInside.\n");
    commit(&r, "first");
    std::fs::remove_dir_all(r.join("sub")).unwrap();
    std::os::unix::fs::symlink(t.outside().join("od"), r.join("sub")).unwrap();
    t.crawl();
    assert!(!t.all_text().contains("INTERMEDIATE_LEAK_777"));
    if let Some(e) = t.get("repo:sub/notes.md") {
        assert!(e.no_content.contains("outside"), "{e:?}");
    }
}

// 3: a tracked file replaced by a symlink is never hashed through it, and one
// path git can't hash doesn't blank the keys after it.
#[test]
fn hash_object_never_follows_links_or_loses_later_keys() {
    let mut t = tree();
    write(&t.outside(), "target.txt", "outside content\n");
    let r = t.repo("repo");
    write(&r, "f.txt", "inside\n");
    write(&r, "m.rs", "fn m() {}\n");
    commit(&r, "first");
    std::fs::remove_file(r.join("f.txt")).unwrap();
    std::os::unix::fs::symlink(t.outside().join("target.txt"), r.join("f.txt")).unwrap();
    // An untracked nested repo, and untracked files sorting after it.
    let sub = r.join("a_subrepo");
    std::fs::create_dir_all(&sub).unwrap();
    git(&sub, &["init", "-q"]);
    write(&sub, "x.txt", "x\n");
    write(&r, "z_new.rs", "fn z() {}\n");
    write(&r, "m.rs", "fn m() { changed(); }\n");
    t.crawl();
    let outside_blob =
        crate::git::run(&t.outside(), &["hash-object", "--no-filters", "target.txt"])
            .unwrap_or_default();
    let fkey = t.key("repo:f.txt");
    assert!(!fkey.is_empty() && fkey != outside_blob.trim());
    assert!(t.get("repo:f.txt").unwrap().no_content.contains("symlink"));
    assert!(!t.key("repo:z_new.rs").is_empty());
    assert!(!t.key("repo:m.rs").is_empty());
    assert!(
        t.get("repo:a_subrepo/").is_none()
            && t.get("repo:a_subrepo").is_none_or(|e| e.kind != "file")
    );
    let again = t.crawl();
    assert_eq!(again.projects[0].changed, 0, "{:?}", again.projects[0]);
}

// 4: outside git a file over 1 MB is keyed without reading it.
#[test]
fn large_files_outside_git_are_not_read() {
    let mut t = tree();
    let p = t.root.join("plain");
    let big = vec![b'a'; 2 << 20];
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(p.join("big.bin"), &big).unwrap();
    t.crawl();
    let e = t.get("plain:big.bin").unwrap();
    assert_eq!(e.no_content, "over 1 MB");
    assert!(!e.content_key.is_empty());
    assert_ne!(e.content_key, crate::crawl::hash(&[&big]));
}

// 5: no-op crawls write nothing; prefix lookups use the primary key.
#[test]
fn a_crawl_with_nothing_changed_rewrites_nothing() {
    let mut t = tree();
    let r = t.repo("repo");
    for d in ["a", "b", "c/d"] {
        write(&r, &format!("{d}/x.rs"), "/// X.\npub fn x() {}\n");
    }
    write(&r, "README.md", "# Repo\n\nA repo.\n");
    commit(&r, "first");
    t.crawl();
    let again = t.crawl();
    assert_eq!(again.search_rows, 0, "{again:?}");
    assert_eq!(again.queued, 0);
    let plan: String =
        t.ix.conn
            .query_row(
                &format!("EXPLAIN QUERY PLAN {}", crate::crawl::CHILDREN_SQL),
                params!["repo:a/x.rs#", "repo:a/x.rs$"],
                |r| r.get(3),
            )
            .unwrap();
    assert!(plan.contains("SEARCH"), "{plan}");
}

// 6: an exact identifier finds its definition first, across kinds.
#[test]
fn exact_identifiers_rank_their_definition_first() {
    let mut t = tree();
    let r = t.repo("alpha");
    write(
        &r,
        "src/deliver.rs",
        "//! Delivers.\n\n/// Delivers held messages.\npub fn deliver() {}\n",
    );
    write(&r, "src/other.rs", "fn other() { deliver(); }\n");
    write(&r, "word-wall/README.md", "# The word wall\n\nWords.\n");
    write(
        &r,
        "src/parser.rs",
        "pub fn word_wall() {}\npub fn parse_config() {}\npub fn _parse_config() {}\npub struct Config;\n",
    );
    write(
        &r,
        "web/render.ts",
        "export class Renderer {\n  renderGlyphs(): void {}\n  private static layoutLine(a: number) {\n  }\n}\nconst drawAll = (x: number) => x;\n",
    );
    write(
        &r,
        "go/main.go",
        "package main\n\nconst (\n\tMaxRetries = 3\n\tminWait = 1\n)\n\ntype (\n\tWallItem struct{}\n)\n",
    );
    write(&r, "py/tool.py", "def load_rows(path):\n    pass\n");
    write(&r, "Makefile", "# Builds it.\nbuild:\n\tcc\n");
    write(&r, "src/id_map.rs", "pub fn lookup_id() {}\n");
    write(
        &r,
        "lib/thing.c",
        "static int count_glyphs(const char *s) {\n  return 0;\n}\nstruct glyph_info { int x; };\n",
    );
    write(
        &r,
        "lib/Thing.java",
        "public class ThingMaker {\n  public void makeThing() {\n  }\n}\n",
    );
    commit(&r, "first");
    // co-change makes deliver.rs important, which mustn't beat its own symbol.
    for i in 0..3 {
        write(
            &r,
            "src/deliver.rs",
            &format!(
                "//! Delivers.\n\n/// Delivers held messages.\npub fn deliver() {{}}\n// {i}\n"
            ),
        );
        write(
            &r,
            "src/other.rs",
            &format!("fn other() {{ deliver(); }}\n// {i}\n"),
        );
        commit(&r, &format!("c{i}"));
    }
    t.crawl();
    for (q, want) in [
        ("deliver", "alpha:src/deliver.rs#deliver"),
        ("word_wall", "alpha:src/parser.rs#word_wall"),
        ("parse_config", "alpha:src/parser.rs#parse_config"),
        ("_parse_config", "alpha:src/parser.rs#_parse_config"),
        ("Config", "alpha:src/parser.rs#Config"),
        ("renderGlyphs", "alpha:web/render.ts#renderGlyphs"),
        ("layoutLine", "alpha:web/render.ts#layoutLine"),
        ("drawAll", "alpha:web/render.ts#drawAll"),
        ("Renderer", "alpha:web/render.ts#Renderer"),
        ("MaxRetries", "alpha:go/main.go#MaxRetries"),
        ("WallItem", "alpha:go/main.go#WallItem"),
        ("load_rows", "alpha:py/tool.py#load_rows"),
        ("lookup_id", "alpha:src/id_map.rs#lookup_id"),
        ("count_glyphs", "alpha:lib/thing.c#count_glyphs"),
        ("makeThing", "alpha:lib/Thing.java#makeThing"),
        ("deliver.rs", "alpha:src/deliver.rs"),
        ("word-wall", "alpha:word-wall"),
    ] {
        assert_eq!(
            t.top(q),
            want,
            "query {q}: {:#?}",
            t.search(q).hits.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
    }
    // Make targets are definitions too.
    assert_eq!(t.top("build"), "alpha:Makefile#build");
}

// 7: documented-by raises the documented entity, not the doc.
#[test]
fn documented_by_counts_toward_what_is_documented() {
    let mut t = tree();
    let r = t.repo("alpha");
    write(&r, "src/parser.rs", "fn p() {}\n");
    write(&r, "src/lexer.rs", "fn l() {}\n");
    write(
        &r,
        "docs/CATALOG.md",
        "# Catalog\n\n`src/parser.rs` and `src/lexer.rs`.\n",
    );
    commit(&r, "first");
    t.crawl();
    assert!(t.get("alpha:src/parser.rs").unwrap().importance >= 1.0);
    assert_eq!(t.get("alpha:docs/CATALOG.md").unwrap().importance, 0.0);
}

// 8: keys follow deletions, shallow edits outside git, and empty projects.
#[test]
fn keys_follow_deletions_shallow_edits_and_empty_projects() {
    let mut t = tree();
    let r = t.repo("repo");
    write(&r, "sub/a.rs", "fn a() {}\n");
    write(&r, "sub/b.rs", "fn b() {}\n");
    write(&r, "keep/c.rs", "fn c() {}\n");
    commit(&r, "first");
    t.crawl();
    let before = t.key("repo:sub");
    std::fs::remove_file(r.join("sub/b.rs")).unwrap();
    t.crawl();
    let unstaged = t.key("repo:sub");
    assert_ne!(unstaged, before);
    git(&r, &["rm", "-q", "--cached", "keep/c.rs"]);
    std::fs::remove_file(r.join("keep/c.rs")).ok();
    write(&r, "keep/d.rs", "fn d() {}\n");
    git(&r, &["add", "keep/d.rs"]);
    let k = t.key("repo:keep");
    t.crawl();
    assert_ne!(t.key("repo:keep"), k);

    let n = t.root.join("notes");
    write(&n, "README.md", "# Notes\n\nOne.\n");
    roots::add_project(&t.ix, &n, "shallow", "").unwrap();
    t.crawl();
    let k1 = t.key("notes");
    write(&n, "README.md", "# Notes\n\nTwo, and longer now.\n");
    t.crawl();
    assert_ne!(t.key("notes"), k1);

    let e = t.root.join("empty");
    write(&e, ".env", "X=1\n");
    t.crawl();
    assert_eq!(t.get("empty").map(|e| e.kind), Some("project".to_string()));
}

// 9: depth changes, rebuilds and uncommitted renames keep what a person said.
#[test]
fn renames_and_depth_changes_survive_any_crawl() {
    let mut t = tree();
    let r = t.repo("repo");
    write(
        &r,
        "a.rs",
        "/// Sym.\npub fn sym_a() {}\n/// Other.\npub fn beta_fn() {}\n",
    );
    write(&r, "m.rs", "pub fn m() {}\n");
    commit(&r, "first");
    t.crawl();
    let c = &t.ix.conn;
    crate::labels::alias_add(c, "repo:a.rs", "the a file").unwrap();
    crate::labels::alias_add(c, "repo:a.rs#sym_a", "the sym").unwrap();
    crate::labels::alias_add(c, "repo:m.rs", "the m file").unwrap();
    // A rename crawled with --rebuild.
    git(&r, &["mv", "a.rs", "a2.rs"]);
    commit(&r, "rename");
    t.rebuild();
    assert_eq!(t.top("the a file"), "repo:a2.rs");
    assert_eq!(t.top("the sym"), "repo:a2.rs#sym_a");
    // An old id is a label search answers to.
    assert_eq!(t.top("repo:a.rs"), "repo:a2.rs");
    // A rename crawled before it is committed.
    git(&r, &["mv", "m.rs", "m2.rs"]);
    t.crawl();
    commit(&r, "rename m");
    t.crawl();
    assert_eq!(t.top("the m file"), "repo:m2.rs");
    let res = view::resolve(&t.ix.conn, "repo:m.rs", Path::new("/")).unwrap();
    assert_eq!(res.id, "repo:m2.rs");

    // Editing one function leaves the other's vector and queue alone.
    t.ix.conn
        .execute(
            "INSERT INTO vectors (entity_id, chunk, model, text_key, dim, vec) VALUES ('repo:a2.rs#beta_fn', 0, 'm', 'k', 1, x'0000')",
            [],
        )
        .unwrap();
    t.ix.conn.execute("DELETE FROM embed_queue", []).unwrap();
    write(
        &r,
        "a2.rs",
        "/// Sym.\npub fn sym_a() { body(); }\n/// Other.\npub fn beta_fn() {}\n",
    );
    t.crawl();
    assert_eq!(
        t.count("SELECT count(*) FROM vectors WHERE entity_id = 'repo:a2.rs#beta_fn'"),
        1
    );
    assert_eq!(
        t.count("SELECT count(*) FROM embed_queue WHERE entity_id = 'repo:a2.rs#beta_fn'"),
        0
    );

    // Depth down to shallow drops files; back to full brings symbols back.
    roots::add_project(&t.ix, &r, "shallow", "").unwrap();
    t.crawl();
    assert_eq!(t.count("SELECT count(*) FROM entities WHERE project = 'repo' AND kind IN ('file', 'symbol', 'section')"), 0);
    roots::add_project(&t.ix, &r, "files", "").unwrap();
    t.crawl();
    assert_eq!(
        t.count("SELECT count(*) FROM entities WHERE project = 'repo' AND kind = 'symbol'"),
        0
    );
    roots::add_project(&t.ix, &r, "full", "").unwrap();
    t.crawl();
    assert!(
        t.count("SELECT count(*) FROM entities WHERE project = 'repo' AND kind = 'symbol'") >= 2
    );
}

// 10: absent terms honor filters; labels, matched lines and priors.
#[test]
fn absent_terms_labels_matches_and_priors() {
    let mut t = tree();
    let a = t.repo("alpha");
    write(
        &a,
        "src/deliver.rs",
        "pub fn deliver() {}\npub fn parse_int() {}\n",
    );
    write(&a, "src/old/legacy.rs", "pub fn deliver_old() {}\n");
    commit(&a, "first");
    write(&t.root.join("beta"), "notes.md", "# Beta\n\nNotes.\n");
    t.crawl();
    let c = &t.ix.conn;
    let r = search(
        c,
        &SearchOptions {
            query: "deliver".into(),
            project: "beta".into(),
            limit: 10,
            ..Default::default()
        },
        &NoSemantic,
    )
    .unwrap();
    assert_eq!(r.absent, vec!["deliver"]);
    crate::labels::alias_add(c, "beta:notes.md", "shiny glow").unwrap();
    let r = t.search("ShinyGlow");
    assert_eq!(r.hits[0].id, "beta:notes.md");
    assert!(r.absent.is_empty(), "{:?}", r.absent);
    crate::labels::alias_add(
        c,
        "alpha:src/deliver.rs",
        "the part that types into busy agents",
    )
    .unwrap();
    assert_eq!(
        t.top("the part that types into busy agents"),
        "alpha:src/deliver.rs"
    );
    let r = t.search("parse_config int");
    for h in &r.hits {
        if h.id.ends_with("#parse_int") {
            assert!(
                !h.matched.iter().any(|m| m.contains("parse_config")),
                "{:?}",
                h.matched
            );
        }
    }
    // --under a path whose ancestors are named like kept-out-of-the-way dirs.
    let r = search(
        c,
        &SearchOptions {
            query: "deliver".into(),
            under: a.to_string_lossy().into_owned(),
            limit: 10,
            ..Default::default()
        },
        &NoSemantic,
    )
    .unwrap();
    let pos = |id: &str| r.hits.iter().position(|h| h.id == id).unwrap_or(99);
    assert!(pos("alpha:src/old/legacy.rs#deliver_old") > pos("alpha:src/deliver.rs#deliver"));
    assert!(
        r.hits[0].explain.is_empty()
            || !r.hits.iter().any(|h| h.id.contains("/old/")
                && !h.explain.iter().any(|e| e.contains("out-of-the-way")))
    );
}

// 11: budgets cut the tail, keep the hierarchy, and search shows why.
#[test]
fn budgets_keep_hierarchy_and_search_shows_why() {
    let mut t = tree();
    let r = t.repo("p");
    write(
        &r,
        "doc.md",
        "# Doc\n\n`aaa/x.rs` and `zzz-long-directory-name/y.rs`.\n",
    );
    write(&r, "aaa/x.rs", "fn x() {}\n");
    write(&r, "zzz-long-directory-name/y.rs", "fn y() {}\n");
    commit(&r, "first");
    t.crawl();
    let c = &t.ix.conn;
    for budget in 1..40 {
        let tr = view::tree(c, "p", 2, budget).unwrap();
        // A line at depth 2 only ever follows its own dir.
        for (i, l) in tr.lines.iter().enumerate() {
            if l.depth == 2 {
                let parent = &tr.lines[..i]
                    .iter()
                    .rev()
                    .find(|x| x.depth == 1)
                    .unwrap()
                    .id;
                assert!(
                    l.id.starts_with(&format!("{parent}/")),
                    "budget {budget}: {:#?}",
                    tr.lines
                );
            }
        }
    }
    let rel = view::related(c, "p:doc.md").unwrap();
    let (text, left) = view::render_related(&rel, 1);
    assert!(text.is_empty() && left > 0, "{text}");
    c.execute(
        "INSERT INTO summaries (entity, content_key, text, why, writer, status, written_at) VALUES ('p:aaa/x.rs', 'k', 'X things.', 'Without it nothing works.', 'person', 'live', 'now')",
        [],
    )
    .unwrap();
    crate::crawl::refresh_search(
        c,
        &std::collections::BTreeSet::from(["p:aaa/x.rs".to_string()]),
    )
    .unwrap();
    let (text, _) = view::render_search(&t.search("aaa x"), 0, false);
    assert!(text.contains("Why: Without it nothing works."), "{text}");
}

// 12: ignore rules, manifests, references, migration, remotes, root removal.
#[test]
fn ignore_manifests_refs_migration_and_more() {
    let mut t = tree();
    let p = t.root.join("plain");
    write(&p, ".gitignore", "cache/\n");
    write(&p, "sub/cache", "a file named cache\n");
    write(&p, "cache/x.txt", "ignored\n");
    write(
        &p,
        "pyproject.toml",
        "[project]\nname = \"plain\"\ndependencies = [\n  \"uvicorn[standard]>=0.20\",\n  \"beta-pkg\",\n]\n",
    );
    write(
        &p,
        "Cargo.toml",
        "[package] # the crate\nname = \"plain-crate\"\n",
    );
    let a = t.repo("alpha");
    write(&a, "src/deliver.rs", "pub fn deliver() {}\n");
    write(
        &a,
        "docs/X.md",
        "# X\n\n`alpha:src/deliver.rs#deliver`, `src/deliver.rs:1:1` and `src/lib.rs`.\n",
    );
    commit(&a, "first");
    let g = t.repo("gamma");
    write(&g, "src/lib.rs", "fn l() {}\n");
    commit(&g, "first");
    t.crawl();
    assert!(t.get("plain:sub/cache").is_some());
    assert!(t.get("plain:cache/x.txt").is_none());
    let m = crate::manifest::parse(
        "pyproject.toml",
        &std::fs::read_to_string(p.join("pyproject.toml")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        m.deps.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
        vec!["uvicorn", "beta-pkg"]
    );
    let m = crate::manifest::parse(
        "Cargo.toml",
        "[package] # the crate\nname = \"plain-crate\"\n",
    )
    .unwrap();
    assert_eq!(m.name, "plain-crate");
    let edge = |s: &str| {
        t.count(&format!("SELECT count(*) FROM edges WHERE kind = 'documented-by' AND source = '{s}' AND target = 'alpha:docs/X.md'"))
    };
    assert_eq!(edge("alpha:src/deliver.rs#deliver"), 1);
    assert_eq!(edge("alpha:src/deliver.rs"), 1);
    // A broken relative path doesn't bind to another project's file of that path.
    assert_eq!(edge("gamma:src/lib.rs"), 0);
    assert_eq!(
        t.count("SELECT count(*) FROM dangling WHERE ref = 'src/lib.rs'"),
        1
    );
    assert_eq!(
        t.count("SELECT count(*) FROM dangling WHERE ref LIKE 'src/deliver.rs%'"),
        0
    );

    assert_eq!(
        crate::git::normalize_remote("git@github.com:1password/cli.git"),
        "github.com/1password/cli"
    );
    assert_eq!(
        crate::git::normalize_remote("ssh://git@host:22/a/b.git"),
        "host/a/b"
    );

    // A v1 file without search_rowid: opening it rebuilds the search rows once.
    let path = t.ix.path.clone();
    let n_before = t.count("SELECT count(*) FROM search");
    t.ix.conn
        .execute_batch("DROP TABLE search_rowid; PRAGMA user_version = 1;")
        .unwrap();
    drop(std::mem::replace(
        &mut t.ix,
        Index::open_in_memory().unwrap(),
    ));
    t.ix = Index::open(&path).unwrap();
    assert_eq!(t.count("SELECT count(*) FROM search"), n_before);
    assert_eq!(t.count("SELECT count(*) FROM search_rowid"), n_before);
    write(&a, "src/deliver.rs", "pub fn deliver() { now(); }\n");
    t.crawl();
    let dup =
        t.count("SELECT count(*) FROM (SELECT id FROM search GROUP BY id HAVING count(*) > 1)");
    assert_eq!(dup, 0);

    let (_, _) = roots::remove_root(&t.ix, "@dev").unwrap();
    assert_eq!(t.count("SELECT count(*) FROM search WHERE id = '@dev'"), 0);
    assert_eq!(
        t.count("SELECT count(*) FROM search_rowid WHERE id = '@dev'"),
        0
    );
}
