//! agent-index on made-up trees: git repos built in a temp dir (never /tmp),
//! a plain directory, and the read side over them.

use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::params;

use crate::crawl::{CrawlOptions, crawl, tested_name};
use crate::db::Index;
use crate::extract::{self, extract, first_sentence};
use crate::files;
use crate::names;
use crate::query::{NoSemantic, SearchOptions, rrf, search};
use crate::roots;
use crate::view;

pub(crate) fn temp() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("agent-index-")
        .tempdir_in("/var/tmp")
        .unwrap()
}

pub(crate) fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

pub(crate) fn git(dir: &Path, args: &[&str]) {
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
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

pub(crate) fn commit(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// A tree with three projects: alpha (a Cargo workspace in git), beta (plain
/// notes, no git) and gamma (a small Go module in git).
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    ix: Index,
}

fn fixture() -> Fixture {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap().join("dev");
    let alpha = root.join("alpha");
    std::fs::create_dir_all(&alpha).unwrap();
    git(&alpha, &["init", "-q", "-b", "main"]);
    write(
        &alpha,
        "README.md",
        "# Alpha\n\nAlpha keeps the word wall and its parser.\n\nMore text.\n",
    );
    write(
        &alpha,
        "Cargo.toml",
        "[workspace]\nmembers = [\"crates/*\"]\n\n[workspace.dependencies]\nalpha-core = { path = \"crates/core\" }\nserde = \"1\"\n",
    );
    write(
        &alpha,
        "crates/core/Cargo.toml",
        "[package]\nname = \"alpha-core\"\ndescription = \"The core of alpha: config parsing\"\n\n[dependencies]\nserde.workspace = true\n",
    );
    write(
        &alpha,
        "crates/core/src/lib.rs",
        "//! Parses alpha's config files.\n//!\n//! Second paragraph.\n\n/// Reads a config from text. More detail here.\npub fn parse_config(text: &str) -> Config {\n    todo!()\n}\n\n/// A parsed config.\npub struct Config {\n    pub name: String,\n}\n\nimpl Default for Config {\n    fn default() -> Self { todo!() }\n}\n",
    );
    write(
        &alpha,
        "crates/core/src/lib_test.rs",
        "#[test]\nfn parses() {}\n",
    );
    write(
        &alpha,
        "crates/cli/Cargo.toml",
        "[package]\nname = \"alpha-cli\"\n\n[dependencies]\nalpha-core.workspace = true\n",
    );
    write(
        &alpha,
        "crates/cli/src/main.rs",
        "fn main() {\n    run_wall();\n}\n\nfn run_wall() {}\n",
    );
    write(
        &alpha,
        "word-wall/README.md",
        "# The word wall\n\nA wall of words that glows.\n",
    );
    write(
        &alpha,
        "word-wall/wall.ts",
        "/** Draws the word wall. */\nexport function drawWall(): void {}\nexport class WallLayout {}\n",
    );
    write(
        &alpha,
        "docs/DESIGN.md",
        "# Design\n\nThe parser lives in `crates/core/src/lib.rs` and notes in `beta/notes.md`.\nSee also `missing/gone.rs` and `lib.rs`.\n\n## Wire layout\n\nHow wires are laid out.\n\n## Wire layout\n\nAgain.\n",
    );
    write(
        &alpha,
        "_archive/old_parser.rs",
        "/// The old config parser.\npub fn parse_config_old() {}\n",
    );
    write(
        &alpha,
        "Makefile",
        "## Build everything\nbuild:\n\tcargo build\n\ntest: build ## Run the tests\n\tcargo test\n",
    );
    write(&alpha, ".env", "SECRET=1\n");
    write(&alpha, "server.key", "-----BEGIN-----\n");
    write(&alpha, "assets/logo.png", "\u{0}PNG\u{0}");
    std::fs::write(
        alpha.join("assets/big.txt"),
        vec![b'a'; (files::MAX_CONTENT + 10) as usize],
    )
    .unwrap();
    commit(&alpha, "first");
    // Co-change history: lib.rs and lib_test.rs change together.
    for i in 0..3 {
        write(
            &alpha,
            "crates/core/src/lib_test.rs",
            &format!("#[test]\nfn parses() {{ let _ = {i}; }}\n"),
        );
        let lib = std::fs::read_to_string(alpha.join("crates/core/src/lib.rs")).unwrap();
        write(&alpha, "crates/core/src/lib.rs", &format!("{lib}// {i}\n"));
        commit(&alpha, &format!("change {i}"));
    }
    // After the commits: untracked, and a symlink out of the project; and
    // build output no .gitignore covers.
    write(&alpha, "scratch.md", "# Scratch\n\nNot committed yet.\n");
    write(&alpha, "rs/target/debug/build.json", "{}\n");
    std::os::unix::fs::symlink("/etc/hostname", alpha.join("outside-link")).unwrap();

    let beta = root.join("beta");
    std::fs::create_dir_all(&beta).unwrap();
    write(
        &beta,
        "notes.md",
        "# Beta notes\n\nNotes about the glyph3d renderer.\n",
    );
    write(&beta, ".gitignore", "build-out/\n*.log\n");
    write(&beta, "build-out/x.md", "# ignored\n");
    write(&beta, "run.log", "ignored\n");
    write(
        &beta,
        "tool.py",
        "\"\"\"A small tool for beta.\"\"\"\n\ndef glyph3d_render(x):\n    \"\"\"Renders glyphs. In 3D.\"\"\"\n    return x\n",
    );

    let gamma = root.join("gamma");
    std::fs::create_dir_all(&gamma).unwrap();
    git(&gamma, &["init", "-q", "-b", "main"]);
    write(
        &gamma,
        "go.mod",
        "module example.invalid/gamma\n\ngo 1.22\n",
    );
    write(
        &gamma,
        "main.go",
        "// Package main serves gamma.\npackage main\n\n// Serve starts the server.\nfunc Serve() {}\n\ntype Server struct{}\n",
    );
    commit(&gamma, "first");

    let ix = Index::open(&dir.path().join("index.db")).unwrap();
    roots::add_root(&ix, &root, "full").unwrap();
    Fixture {
        _dir: dir,
        root,
        ix,
    }
}

fn crawl_all(f: &mut Fixture) -> crate::CrawlReport {
    crawl(&mut f.ix, &CrawlOptions::default()).unwrap()
}

fn exists(f: &Fixture, id: &str) -> bool {
    crate::model::get(&f.ix.conn, id).unwrap().is_some()
}

fn opts(q: &str) -> SearchOptions {
    SearchOptions {
        query: q.into(),
        limit: 10,
        ..Default::default()
    }
}

#[test]
fn identifiers_split_and_stay_whole() {
    assert_eq!(
        names::split_words("gather_global"),
        vec!["gather", "global"]
    );
    assert_eq!(names::split_words("glyph3d"), vec!["glyph3d"]);
    assert_eq!(names::split_words("HTTPServer"), vec!["http", "server"]);
    assert_eq!(
        names::split_words("WireLayout.swift"),
        vec!["wire", "layout", "swift"]
    );
    let w = names::whole_and_split("gather_global");
    assert!(
        w.contains("gather_global") && w.contains("gather global"),
        "{w}"
    );
    assert_eq!(names::whole_and_split("glyph3d"), "glyph3d");
    assert_eq!(
        names::query_terms("Where's gather_global?"),
        vec!["where", "s", "gather_global"]
    );
}

#[test]
fn label_variants() {
    let v = names::variants("word-wall");
    for want in [
        "word-wall",
        "word wall",
        "wordwall",
        "WordWall",
        "word_wall",
    ] {
        assert!(v.contains(&want.to_string()), "{want} missing from {v:?}");
    }
    assert_eq!(names::variants("alpha"), vec!["alpha"]);
    assert_eq!(names::norm("WordWall"), "word wall");
    assert_eq!(names::compact("word wall"), "wordwall");
    assert_eq!(
        names::slug("3. Unified `architecture`!"),
        "3-unified-architecture"
    );
}

#[test]
fn file_kinds_and_secrets() {
    assert_eq!(files::file_kind("src/lib.rs"), "source");
    assert_eq!(files::file_kind("src/lib_test.rs"), "test");
    assert_eq!(files::file_kind("tests/it.rs"), "test");
    assert_eq!(files::file_kind("web/a.spec.ts"), "test");
    assert_eq!(files::file_kind("docs/X.md"), "doc");
    assert_eq!(files::file_kind("Cargo.toml"), "build");
    assert_eq!(files::file_kind("Makefile"), "build");
    assert_eq!(files::file_kind("config/app.yaml"), "config");
    assert_eq!(files::file_kind("data/rows.csv"), "data");
    assert_eq!(files::file_kind("assets/logo.png"), "asset");
    for s in [
        ".env",
        ".env.local",
        "server.key",
        "cert.pem",
        "id_ed25519",
        "id_rsa.pub",
        "a.p12",
    ] {
        assert!(files::is_secret(s), "{s}");
    }
    assert!(!files::is_secret("keys.rs") && !files::is_secret("environment.md"));
    assert_eq!(tested_name("lib_test.rs").as_deref(), Some("lib.rs"));
    assert_eq!(tested_name("test_tool.py").as_deref(), Some("tool.py"));
    assert_eq!(tested_name("wall.test.ts").as_deref(), Some("wall.ts"));
    assert_eq!(
        tested_name("WallTests.swift").as_deref(),
        Some("Wall.swift")
    );
    assert_eq!(tested_name("lib.rs"), None);
}

#[test]
fn seeds_and_symbols_per_language() {
    let x = extract(
        "lib.rs",
        "rust",
        "//! Parses config files. Also more.\n//!\n//! Detail.\n\n/// Reads it. Then more.\n#[inline]\npub fn read_it() {}\npub(crate) struct S;\nimpl Display for S {}\n",
        true,
    );
    assert_eq!(x.seed.as_deref(), Some("Parses config files. Also more."));
    let names: Vec<&str> = x.symbols.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["read_it", "S", "impl Display for S"]);
    assert_eq!(x.symbols[0].doc, "Reads it.");
    assert_eq!(x.symbols[0].line, 7);

    let x = extract(
        "m.go",
        "go",
        "// Package srv serves things.\npackage srv\n\n// Run runs.\nfunc (s *S) Run() {}\ntype S struct{}\n",
        true,
    );
    assert_eq!(x.seed.as_deref(), Some("Package srv serves things."));
    assert_eq!(x.symbols[0].name, "Run");
    assert_eq!(x.symbols[0].doc, "Run runs.");
    assert_eq!(x.symbols[1].kind, "struct");

    let x = extract(
        "t.py",
        "python",
        "#!/usr/bin/env python\n\"\"\"Tool module.\n\nMore.\"\"\"\n\nclass A:\n    def go(self):\n        '''Goes. Far.'''\n",
        true,
    );
    assert_eq!(x.seed.as_deref(), Some("Tool module."));
    assert_eq!(x.symbols[1].name, "go");
    assert_eq!(x.symbols[1].doc, "Goes.");

    let x = extract(
        "w.ts",
        "typescript",
        "/**\n * Draws walls.\n */\n\n/** Lays out. */\nexport class Layout {}\nexport type Id = string;\n",
        true,
    );
    assert_eq!(x.seed.as_deref(), Some("Draws walls."));
    assert_eq!(x.symbols[0].name, "Layout");
    assert_eq!(x.symbols[0].doc, "Lays out.");
    assert_eq!(x.symbols[1].kind, "type");

    let x = extract(
        "V.swift",
        "swift",
        "/// A view.\npublic final class WallView {}\nextension WallView {}\n",
        true,
    );
    assert_eq!(x.symbols[0].name, "WallView");
    assert_eq!(x.symbols[0].doc, "A view.");

    let x = extract(
        "run.sh",
        "shell",
        "#!/bin/sh\n# Runs the build.\n\n# Cleans up.\ncleanup() {\n  :\n}\n",
        true,
    );
    assert_eq!(x.seed.as_deref(), Some("Runs the build."));
    assert_eq!(x.symbols[0].name, "cleanup");
    assert_eq!(x.symbols[0].doc, "Cleans up.");

    let x = extract(
        "Makefile",
        "make",
        ".PHONY: build\n# Builds it.\nbuild:\n\tcc\ntest: build ## Tests it\n",
        true,
    );
    let t: Vec<(&str, &str)> = x
        .symbols
        .iter()
        .map(|s| (s.name.as_str(), s.doc.as_str()))
        .collect();
    assert_eq!(t, vec![("build", "Builds it."), ("test", "Tests it")]);

    let x = extract(
        "D.md",
        "markdown",
        "[![badge](x)](y)\n# Title here\n\nFirst para\ncontinues.\n\n```\n# not a heading\n```\n## Sub `part`\n\nSub text.\n## Sub part\n",
        true,
    );
    assert_eq!(x.title.as_deref(), Some("Title here"));
    assert_eq!(x.seed.as_deref(), Some("First para continues."));
    let s: Vec<(&str, &str)> = x
        .sections
        .iter()
        .map(|s| (s.slug.as_str(), s.seed.as_str()))
        .collect();
    assert_eq!(
        s,
        vec![
            ("title-here", "First para continues."),
            ("sub-part", "Sub text."),
            ("sub-part-1", "")
        ]
    );

    assert_eq!(
        first_sentence("Reads it, e.g. fast. Then more."),
        "Reads it, e.g. fast."
    );
    assert_eq!(
        extract::refs_in("see `a/b.rs:12`, `x.md` and `not a path` `https://x/y`").len(),
        2
    );
}

#[test]
fn crawl_makes_entities_keys_and_skips_secrets() {
    let mut f = fixture();
    let r = crawl_all(&mut f);
    let names: Vec<&str> = r.projects.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta", "gamma"]);
    let alpha = &r.projects[0];
    assert!(alpha.git && alpha.secret_skipped == 2, "{alpha:?}");
    assert!(exists(&f, "@dev"));
    for id in [
        "alpha",
        "alpha:crates/core",
        "alpha:crates/core/src/lib.rs",
        "alpha:crates/core/src/lib.rs#parse_config",
        "alpha:crates/core/src/lib.rs#impl-Default-for-Config",
        "alpha:docs/DESIGN.md#wire-layout",
        "alpha:docs/DESIGN.md#wire-layout-1",
        "alpha:Makefile#build",
        "beta:notes.md",
        "beta:tool.py#glyph3d_render",
        "gamma:main.go#Serve",
    ] {
        assert!(exists(&f, id), "{id} missing");
    }
    assert!(!exists(&f, "alpha:.env") && !exists(&f, "alpha:server.key"));
    // .gitignore outside git.
    assert!(!exists(&f, "beta:build-out/x.md") && !exists(&f, "beta:run.log"));

    let c = &f.ix.conn;
    let get = |id: &str| crate::model::get(c, id).unwrap().unwrap();
    // Keys: a git blob id for a file, a tree id for a clean dir.
    let lib = get("alpha:crates/core/src/lib.rs");
    assert_eq!(lib.content_key.len(), 40);
    assert_eq!(lib.file_kind, "source");
    assert_eq!(lib.lang, "rust");
    let tree_id =
        crate::git::run(&f.root.join("alpha"), &["rev-parse", "HEAD:crates/core"]).unwrap();
    assert_eq!(get("alpha:crates/core").content_key, tree_id.trim());
    assert_eq!(get("alpha:assets/big.txt").no_content, "over 1 MB");
    assert_eq!(get("alpha:assets/logo.png").no_content, "binary");
    assert_eq!(
        get("alpha:outside-link").no_content,
        "symlink outside the project"
    );
    assert!(get("alpha:scratch.md").untracked);
    let build: i64 = c
        .query_row(
            "SELECT count(*) FROM entities WHERE id LIKE 'alpha:rs/target%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(build, 0, "build output git doesn't ignore isn't indexed");
    // The top dir has untracked content: a listing hash, not HEAD's tree.
    let top = crate::git::run(&f.root.join("alpha"), &["rev-parse", "HEAD^{tree}"]).unwrap();
    assert_ne!(get("alpha").content_key, top.trim());
    assert_eq!(get("beta:notes.md").content_key.len(), 20);

    // Seeds.
    let seed = |id: &str| {
        crate::model::summary(c, id)
            .unwrap()
            .map(|s| s.text)
            .unwrap_or_default()
    };
    assert_eq!(
        seed("alpha:crates/core/src/lib.rs"),
        "Parses alpha's config files."
    );
    assert_eq!(
        seed("alpha:crates/core/src/lib.rs#parse_config"),
        "Reads a config from text."
    );
    assert!(seed("alpha:crates/core").starts_with("The core of alpha: config parsing"));
    assert_eq!(seed("alpha"), "Alpha keeps the word wall and its parser.");
    assert_eq!(get("alpha").title, "Alpha");
    assert_eq!(get("alpha:word-wall").title, "The word wall");
    assert_eq!(seed("gamma:main.go"), "Package main serves gamma.");
    assert_eq!(seed("beta:tool.py"), "A small tool for beta.");
    assert_eq!(seed("alpha:Makefile#test"), "Run the tests");

    // Edges.
    let edge = |s: &str, t: &str, k: &str| -> bool {
        c.query_row(
            "SELECT count(*) FROM edges WHERE source = ? AND target = ? AND kind = ?",
            params![s, t, k],
            |r| r.get::<_, i64>(0),
        )
        .unwrap()
            > 0
    };
    assert!(edge("alpha:crates/cli", "alpha:crates/core", "depends-on"));
    assert!(edge(
        "alpha:crates/core/src/lib.rs",
        "alpha:docs/DESIGN.md",
        "documented-by"
    ));
    assert!(
        edge("beta:notes.md", "alpha:docs/DESIGN.md", "documented-by"),
        "cross-project reference"
    );
    assert!(edge(
        "alpha:crates/core/src/lib_test.rs",
        "alpha:crates/core/src/lib.rs",
        "tests"
    ));
    assert!(edge(
        "alpha:crates/core/src/lib.rs",
        "alpha:crates/core/src/lib_test.rs",
        "co-changes"
    ));
    let dangling: Vec<String> = c
        .prepare("SELECT ref FROM dangling")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(dangling, vec!["missing/gone.rs"]);
    assert!(get("alpha:crates/core/src/lib.rs").importance > 0.0);

    // Labels from code and the embed queue.
    let labels: Vec<String> = c
        .prepare("SELECT label FROM labels WHERE entity = 'alpha:word-wall'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    for want in ["word wall", "wordwall", "WordWall", "The word wall"] {
        assert!(
            labels.contains(&want.to_string()),
            "{want} not in {labels:?}"
        );
    }
    let has_pkg: i64 = c
        .query_row("SELECT count(*) FROM labels WHERE entity = 'alpha:crates/core' AND label = 'alpha-core'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(has_pkg, 1);
    let queued: i64 = c
        .query_row("SELECT count(*) FROM embed_queue", [], |r| r.get(0))
        .unwrap();
    assert!(
        queued > 20 && r.queued as i64 == queued,
        "{queued} vs {}",
        r.queued
    );

    // A second crawl changes nothing and queues nothing.
    let r2 = crawl_all(&mut f);
    assert_eq!(
        r2.projects[0].changed + r2.projects[0].added,
        0,
        "{:?}",
        r2.projects[0]
    );
    assert_eq!(r2.queued, 0);
}

#[test]
fn dry_run_writes_nothing() {
    let mut f = fixture();
    let r = crawl(
        &mut f.ix,
        &CrawlOptions {
            dry_run: true,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(r.projects[0].added > 5);
    assert!(!exists(&f, "alpha:README.md"));
}

#[test]
fn rename_carries_summaries_and_keeps_an_alias() {
    let mut f = fixture();
    crawl_all(&mut f);
    let alpha = f.root.join("alpha");
    // A person's summary and label on the file, which a rename must carry.
    f.ix.conn
        .execute(
            "INSERT INTO summaries (entity, content_key, text, writer, status, written_at) VALUES ('alpha:crates/core/src/lib.rs', 'k', 'Where config is parsed.', 'person', 'live', 'now')",
            [],
        )
        .unwrap();
    crate::labels::alias_add(
        &f.ix.conn,
        "alpha:crates/core/src/lib.rs",
        "the config reader",
    )
    .unwrap();
    git(
        &alpha,
        &["mv", "crates/core/src/lib.rs", "crates/core/src/parse.rs"],
    );
    commit(&alpha, "rename");
    let r = crawl_all(&mut f);
    assert_eq!(r.projects[0].renamed, 1, "{:?}", r.projects[0]);
    assert!(!exists(&f, "alpha:crates/core/src/lib.rs"));
    let c = &f.ix.conn;
    let s = crate::model::summary(c, "alpha:crates/core/src/parse.rs")
        .unwrap()
        .unwrap();
    assert_eq!(s.text, "Where config is parsed.");
    let res = view::resolve(c, "alpha:crates/core/src/lib.rs", Path::new("/")).unwrap();
    assert_eq!(res.id, "alpha:crates/core/src/parse.rs");
    assert_eq!(
        res.alias_of.as_deref(),
        Some("alpha:crates/core/src/lib.rs")
    );
    let shown = view::render_show(&view::show(c, res).unwrap());
    assert!(
        shown.starts_with(
            "alpha:crates/core/src/lib.rs was renamed: it is alpha:crates/core/src/parse.rs now."
        ),
        "{shown}"
    );
    assert!(
        shown.contains("\"the config reader\" (alt, person)"),
        "{shown}"
    );
    let hits = search(c, &opts("config reader"), &NoSemantic).unwrap();
    assert_eq!(hits.hits[0].id, "alpha:crates/core/src/parse.rs");
}

#[test]
fn symlink_out_of_a_project_resolves_nowhere() {
    let mut f = fixture();
    crawl_all(&mut f);
    let c = &f.ix.conn;
    let e = view::resolve(c, "outside-link", &f.root.join("alpha")).unwrap_err();
    assert!(
        format!("{e:#}").contains("outside every indexed project"),
        "{e:#}"
    );
    // The link's own entity says it wasn't read; nothing of its target is.
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM entities WHERE path = '/etc/hostname'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0);
    let res = view::resolve(c, ".", &f.root.join("alpha/crates/core/src")).unwrap();
    assert_eq!(res.id, "alpha:crates/core/src");
}

#[test]
fn search_ranks_labels_names_and_says_what_is_absent() {
    let mut f = fixture();
    crawl_all(&mut f);
    let c = &f.ix.conn;
    let r = search(c, &opts("I want to work on the word wall"), &NoSemantic).unwrap();
    assert_eq!(r.hits[0].id, "alpha:word-wall", "{:#?}", r.hits);
    assert!(
        r.mapping
            .iter()
            .any(|m| m.phrase.ends_with("word wall") && m.entity == "alpha:word-wall"),
        "{:#?}",
        r.mapping
    );
    let (text, _) = view::render_search(&r, 0, false);
    assert!(
        text.starts_with("Labels: \"the word wall\" → `alpha:word-wall`"),
        "{text}"
    );
    let r = search(c, &opts("word wall"), &NoSemantic).unwrap();
    assert!(r.mapping.iter().all(|m| m.exact) && r.hits[0].id == "alpha:word-wall");

    let r = search(c, &opts("parse_config"), &NoSemantic).unwrap();
    assert_eq!(
        r.hits[0].id, "alpha:crates/core/src/lib.rs#parse_config",
        "{:#?}",
        r.hits
    );
    // The archived copy ranks below the live one.
    let pos = |id: &str| r.hits.iter().position(|h| h.id.starts_with(id));
    assert!(pos("alpha:_archive").unwrap_or(99) > pos("alpha:crates/core/src/lib.rs").unwrap());
    assert!(
        r.hits[0].matched.iter().any(|m| m.starts_with("name")),
        "{:?}",
        r.hits[0].matched
    );

    // Whole identifiers: glyph3d isn't split into a bare "3d".
    let r = search(c, &opts("glyph3d"), &NoSemantic).unwrap();
    assert!(
        r.hits.iter().any(|h| h.id == "beta:tool.py#glyph3d_render"),
        "{:#?}",
        r.hits
    );

    let r = search(c, &opts("wall zzqxv"), &NoSemantic).unwrap();
    assert_eq!(r.absent, vec!["zzqxv"]);
    let (text, _) = view::render_search(&r, 0, false);
    assert!(text.contains("No entity mentions `zzqxv`."), "{text}");

    // Filters.
    let mut o = opts("config");
    o.kind = "symbol".into();
    let r = search(c, &o, &NoSemantic).unwrap();
    assert!(!r.hits.is_empty() && r.hits.iter().all(|h| h.kind == "symbol"));
    let mut o = opts("server");
    o.project = "gamma".into();
    let r = search(c, &o, &NoSemantic).unwrap();
    assert!(r.hits.iter().all(|h| h.project == "gamma") && !r.hits.is_empty());
    let mut o = opts("parse_config");
    o.under = f.root.join("alpha/_archive").to_string_lossy().into_owned();
    let r = search(c, &o, &NoSemantic).unwrap();
    assert_eq!(
        r.hits[0].id, "alpha:_archive/old_parser.rs#parse_config_old",
        "{:#?}",
        r.hits
    );
    // A typo finds the file through trigrams (symbols have no trigram rows:
    // there are millions in a large tree).
    let r = search(c, &opts("WallLayuot"), &NoSemantic).unwrap();
    assert!(
        r.hits
            .iter()
            .take(5)
            .any(|h| h.id.starts_with("alpha:word-wall/wall.ts")),
        "{:#?}",
        r.hits
    );
}

#[test]
fn at_most_two_results_per_file() {
    let mut f = fixture();
    crawl_all(&mut f);
    let r = search(&f.ix.conn, &opts("wire layout design"), &NoSemantic).unwrap();
    let n = r
        .hits
        .iter()
        .filter(|h| h.id.starts_with("alpha:docs/DESIGN.md"))
        .count();
    assert!(n <= 2 && n > 0, "{:#?}", r.hits);
}

#[test]
fn rrf_fuses_by_rank() {
    let a = vec!["x".to_string(), "y".into(), "z".into()];
    let b = vec!["y".to_string(), "w".into()];
    let v = rrf(&[a, b]);
    assert_eq!(v[0].0, "y");
    assert!((v[0].1 - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-12);
    assert_eq!(v[1].0, "x");
    let ids: Vec<&str> = v.iter().map(|x| x.0.as_str()).collect();
    assert_eq!(ids, vec!["y", "x", "w", "z"]);
}

#[test]
fn budget_cuts_summaries_then_the_tail() {
    let mut f = fixture();
    crawl_all(&mut f);
    let r = search(&f.ix.conn, &opts("config"), &NoSemantic).unwrap();
    assert!(r.hits.len() >= 4);
    let (full, left) = view::render_search(&r, 0, false);
    assert_eq!(left, 0);
    let (small, left) = view::render_search(&r, view::tokens(&full) / 3, false);
    assert!(
        left > 0 && view::tokens(&small) <= view::tokens(&full) / 3,
        "{small}"
    );
    // Fitting cuts summaries to a sentence first.
    let (_, left_mid) = view::render_search(&r, view::tokens(&full) - 5, false);
    assert!(left_mid <= 1);
}

#[test]
fn lexicon_import_adds_asks_and_skips_what_resolves_nowhere() {
    let mut f = fixture();
    crawl_all(&mut f);
    let lex = f.root.join("alpha/lexicon.jsonl");
    std::fs::write(
        &lex,
        concat!(
            "{\"term\":\"config reader\",\"canonical\":\"parse_config\",\"definition\":\"reads config\",\"confusables\":[],\"provenance\":[\"crates/core/src/lib.rs:6\"],\"lay_phrasings\":[\"the thing that reads settings\"]}\n",
            "{\"term\":\"glow\",\"canonical\":\"word wall\",\"provenance\":[],\"lay_phrasings\":[\"shiny words\"]}\n",
            "{\"term\":\"nowhere\",\"canonical\":\"nothing-here\",\"provenance\":[\"no/such/file.rs:1\"]}\n",
            "not json\n"
        ),
    )
    .unwrap();
    let rep = crate::labels::lexicon_import(&f.ix.conn, &lex).unwrap();
    assert_eq!(
        (rep.rows, rep.imported, rep.unresolved, rep.unreadable),
        (4, 2, 1, 1),
        "{rep:?}"
    );
    let c = &f.ix.conn;
    let kind: String = c
        .query_row(
            "SELECT kind FROM labels WHERE entity = 'alpha:crates/core/src/lib.rs' AND label = 'the thing that reads settings'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kind, "ask");
    let r = search(c, &opts("the thing that reads settings"), &NoSemantic).unwrap();
    assert_eq!(r.hits[0].id, "alpha:crates/core/src/lib.rs");
    let r = search(c, &opts("shiny words"), &NoSemantic).unwrap();
    assert_eq!(r.hits[0].id, "alpha:word-wall");
}

#[test]
fn show_tree_related_stale() {
    let mut f = fixture();
    crawl_all(&mut f);
    let c = &f.ix.conn;
    let res = view::resolve(c, "crates/core/src/lib.rs", &f.root.join("alpha")).unwrap();
    let s = view::show(c, res).unwrap();
    let text = view::render_show(&s);
    assert!(
        text.starts_with("alpha:crates/core/src/lib.rs   file (source, rust)  seed\n"),
        "{text}"
    );
    assert!(
        text.contains("  alpha  Alpha: Alpha keeps the word wall and its parser."),
        "{text}"
    );
    assert!(
        text.contains("fn       parse_config: Reads a config from text."),
        "{text}"
    );
    assert!(
        text.contains("documented by: alpha:docs/DESIGN.md"),
        "{text}"
    );
    assert!(
        text.contains("tested by: alpha:crates/core/src/lib_test.rs"),
        "{text}"
    );

    let t = view::tree(c, "alpha", 1, 0).unwrap();
    let out = view::render_tree(&t);
    assert!(
        out.contains("word-wall/  The word wall: A wall of words that glows."),
        "{out}"
    );
    assert!(!out.contains("lib.rs"), "depth 1: {out}");
    let t = view::tree(c, "alpha", 3, 40).unwrap();
    assert!(t.left_out > 0);

    let r = view::related(c, "alpha:crates/core/src/lib.rs").unwrap();
    assert!(r.groups.contains_key("changes with") && r.groups.contains_key("documented by"));

    let st = view::stale(c, "alpha", 50).unwrap();
    assert!(st.counts.get("seed").copied().unwrap_or(0) > 3);
    assert_eq!(st.dangling.len(), 1);
    assert_eq!(st.dangling[0].reference, "missing/gone.rs");
}

#[test]
fn project_add_and_root_remove() {
    let mut f = fixture();
    crawl_all(&mut f);
    // A deeper dir as a project of its own: the outer crawl leaves it out.
    let added =
        roots::add_project(&f.ix, &f.root.join("alpha/word-wall"), "files", "wall").unwrap();
    assert_eq!(added.outcome, "added");
    crawl_all(&mut f);
    assert!(exists(&f, "wall:wall.ts"));
    assert!(!exists(&f, "alpha:word-wall/wall.ts"));
    assert!(
        !exists(&f, "wall:wall.ts#drawWall"),
        "files depth has no symbols"
    );
    let e = roots::add_project(&f.ix, &f.root.join("beta"), "full", "").unwrap();
    assert_eq!(e.outcome, "updated");
    assert!(roots::add_project(&f.ix, &f.root.join("gamma"), "full", "beta").is_err());
    let (_, removed) = roots::remove_root(&f.ix, f.root.to_str().unwrap()).unwrap();
    assert_eq!(removed, vec!["alpha", "gamma"]);
    assert!(!exists(&f, "alpha:README.md") && exists(&f, "beta:notes.md"));
}

#[test]
fn shallow_depth_keeps_top_dirs_only() {
    let dir = temp();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    write(&root, "p/README.md", "# P\n\nThe p project.\n");
    write(&root, "p/sub/README.md", "# Sub\n\nThe sub dir.\n");
    write(&root, "p/sub/deep/x.rs", "fn x() {}\n");
    let mut ix = Index::open(&root.join("index.db")).unwrap();
    roots::add_project(&ix, &root.join("p"), "shallow", "").unwrap();
    crawl(&mut ix, &CrawlOptions::default()).unwrap();
    let c = &ix.conn;
    assert_eq!(
        crate::model::summary(c, "p:sub").unwrap().unwrap().text,
        "The sub dir."
    );
    assert!(crate::model::get(c, "p:sub/deep").unwrap().is_none());
    assert!(crate::model::get(c, "p:sub/deep/x.rs").unwrap().is_none());
}
