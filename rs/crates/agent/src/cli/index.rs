//! `agent index` and its subcommands (no Go file: new in the Rust build;
//! docs/INDEX.md). The index is agent_index's; this file parses flags and
//! prints. It lives in `index.db`, beside this binary's `gomuxai.db`.
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_index::view::{self, Resolved};
use agent_index::{CrawlOptions, Index, NoSemantic, SearchOptions};
use anyhow::Context as _;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::root::Ctx;

// `agent index`

pub const DOC: Doc = Doc {
    use_line: "index",
    short: "Find things across the dev tree: search, show and map its projects",
    long: r#"Index is one queryable map of the trees you add: every project, directory,
file, symbol and doc section gets a stable id (alpha:src/lib.rs,
alpha:src/lib.rs#parse, alpha:docs/X.md#a-heading), its place in the tree,
edges to other entities (depends-on, documented-by, tests, co-changes),
and a short summary. Ask it before grep and find: search ranks entities
and gives their paths, show . says where you are and what is here, and
tree PROJECT --depth 1 prints a project's map.

Opt-in: agent index root add DIR adds a tree, and every directory directly
under it becomes a project, a repo or not; agent index crawl reads them.
A crawl is cheap and incremental: it re-reads only files whose git blob
(or, outside git, content hash) changed, and carries renamed files'
summaries and labels over, keeping the old id as an alias. It never
indexes .env*, *.pem, *.key, id_* or *.p12 files, reads no file over 1 MB
or binary file (an entity, no content), and never follows a symlink out
of its project.

Summaries in this version are seeds: what the code itself says (a crate's
description and //! doc, a Go package comment, a docstring, a doc's first
paragraph, a README's first paragraph for its directory, doc comments).
Freshness reads seed (only a seed exists), fresh, stale or none.

The index lives in index.db beside this binary's gomuxai.db; everything
but labels a person or a glossary added is rebuilt by crawl --rebuild."#,
    example: r#"  agent index root add ~/dev
  agent index crawl
  agent index search "word wall"
  agent index show .
  agent index tree alpha --depth 1"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            root_def(),
            project_def(),
            crawl_def(),
            embed_def(),
            search_def(),
            show_def(),
            tree_def(),
            related_def(),
            stale_def(),
            alias_def(),
            lexicon_def(),
        ],
    )
}

/// This binary's index, beside its gomuxai.db.
fn open(ctx: &Ctx) -> anyhow::Result<Index> {
    let path = agent_index::path_beside(&ctx.cfg().database_path);
    Index::open(&path)
}

fn cwd() -> anyhow::Result<PathBuf> {
    std::env::current_dir().context("reading the current directory")
}

/// A path argument against the current directory, canonical.
fn abs_dir(arg: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(arg);
    let p = if p.is_absolute() { p.to_path_buf() } else { cwd()?.join(p) };
    std::fs::canonicalize(&p).with_context(|| p.display().to_string())
}

/// The project the caller works in: the deepest whose directory holds the
/// current directory.
fn caller_project(ix: &Index) -> String {
    let Ok(cwd) = cwd() else { return String::new() };
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
    let cwd = cwd.to_string_lossy().into_owned();
    agent_index::roots::list_projects(&ix.conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|p| cwd == p.path || cwd.starts_with(&format!("{}/", p.path)))
        .max_by_key(|p| p.path.len())
        .map(|p| p.name)
        .unwrap_or_default()
}

/// Refuses to read an index nothing has been added to, saying how to start.
fn require_projects(ix: &Index) -> anyhow::Result<()> {
    let n: i64 = ix
        .conn
        .query_row("SELECT count(*) FROM entities WHERE kind = 'project'", [], |r| r.get(0))?;
    if n == 0 {
        anyhow::bail!(
            "nothing is indexed yet: agent index root add DIR adds a tree, then agent index crawl reads it"
        );
    }
    Ok(())
}

fn resolve(ix: &Index, arg: &str) -> anyhow::Result<Resolved> {
    view::resolve(&ix.conn, arg, &cwd()?)
}

fn int_flag(v: i64, name: &str) -> anyhow::Result<usize> {
    if v < 0 {
        anyhow::bail!("--{name} can't be negative: {v}");
    }
    Ok(v as usize)
}

// `agent index root`

pub const ROOT: Doc = Doc {
    use_line: "root",
    short: "Add, remove and list the trees the index covers",
    long: r#"Root holds the trees the index covers. Every directory directly under a
root is a project, a repo or not (hidden directories and build output
aside); a git worktree is skipped, since it is its main repo's. A project
name is its directory's name and must be unique: a clash is skipped and
said, and agent index project add DIR --name OTHER adds it under another
name."#,
    example: r#"  agent index root add ~/dev
  agent index root list
  agent index root remove ~/dev"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

fn root_def() -> Def {
    Def::group(&ROOT, vec![root_add_def(), root_remove_def(), root_list_def()])
}

pub const ROOT_ADD: Doc = Doc {
    use_line: "add DIR",
    short: "Add a tree: every directory directly under it becomes a project",
    long: r#"Add makes DIR a root. Nothing is read until agent index crawl.
--depth sets how deep its projects go: full (dirs, files, symbols,
sections; the default), files (no symbols), or shallow (the project and
its top directories, from READMEs). agent index project add changes one
project's depth."#,
    example: r#"  agent index root add ~/dev
  agent index root add ~/src/vendor --depth shallow"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct RootAddArgs {
    #[arg(long = "depth", value_name = "string", default_value = "full", help = "How deep its projects go: full, files or shallow")]
    pub depth: String,
    #[arg(long = "json", help = "Print the root as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn root_add_def() -> Def {
    leaf!(ROOT_ADD, RootAddArgs, run_root_add)
}

fn run_root_add(ctx: &Ctx, a: &RootAddArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    let r = agent_index::roots::add_root(&ix, &abs_dir(&a.args[0])?, &a.depth)?;
    if a.json {
        return ctx.print_json(&r);
    }
    ctx.write_out(&format!(
        "Added root {} ({}), its projects at depth {}.\nNext: agent index crawl reads it.\n",
        r.id, r.path, r.depth
    ))
}

pub const ROOT_REMOVE: Doc = Doc {
    use_line: "remove DIR|ID",
    short: "Remove a root and the projects found under it",
    long: r#"Remove drops a root and every project found under it from the index (not
the files: only the index's rows). Projects added with agent index
project add stay. Labels a person or a glossary added stay too, keyed by
id, for when the tree comes back."#,
    example: r#"  agent index root remove ~/dev
  agent index root remove @dev"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct RootRemoveArgs {
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn root_remove_def() -> Def {
    leaf!(ROOT_REMOVE, RootRemoveArgs, run_root_remove)
}

fn run_root_remove(ctx: &Ctx, a: &RootRemoveArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    let (r, removed) = agent_index::roots::remove_root(&ix, &a.args[0])?;
    let list = if removed.is_empty() { "none".to_string() } else { removed.join(", ") };
    ctx.write_out(&format!(
        "Removed root {} ({}) and {} projects found under it: {list}.\n",
        r.id,
        r.path,
        removed.len()
    ))
}

pub const ROOT_LIST: Doc = Doc {
    use_line: "list",
    short: "List the roots and their projects",
    long: r#"List prints each root, its default depth and how many projects it has,
then every project: its depth, whether it is a git repo, and when it was
last crawled."#,
    example: r#"  agent index root list
  agent index root list --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct RootListArgs {
    #[arg(long = "json", help = "Print {\"roots\": [...], \"projects\": [...]}")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn root_list_def() -> Def {
    leaf!(ROOT_LIST, RootListArgs, run_root_list)
}

fn run_root_list(ctx: &Ctx, a: &RootListArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    let roots = agent_index::roots::list_roots(&ix.conn)?;
    let projects = agent_index::roots::list_projects(&ix.conn)?;
    if a.json {
        #[derive(Serialize)]
        struct Out<'a> {
            index: String,
            roots: &'a [agent_index::roots::Root],
            projects: &'a [agent_index::roots::Project],
        }
        return ctx.print_json(&Out {
            index: ix.path.display().to_string(),
            roots: &roots,
            projects: &projects,
        });
    }
    let mut o = String::new();
    if roots.is_empty() && projects.is_empty() {
        o.push_str("The index covers nothing yet.\nNext: agent index root add DIR adds a tree.\n");
        return ctx.write_out(&o);
    }
    let _ = writeln!(o, "Index {}:", ix.path.display());
    for r in &roots {
        let _ = writeln!(o, "root {}  {}  depth {}, {} projects", r.id, r.path, r.depth, r.projects);
    }
    for p in &projects {
        let crawled = if p.last_crawled_at.is_empty() { "never crawled".to_string() } else { format!("crawled {}", p.last_crawled_at) };
        let kind = if p.last_crawled_at.is_empty() { "" } else if p.is_git { "git, " } else { "not git, " };
        let added = if p.explicit { ", added by project add" } else { "" };
        let _ = writeln!(o, "  {}  {}  {}{}, {crawled}{added}", p.name, p.path, kind, p.depth);
    }
    ctx.write_out(&o)
}

// `agent index project`

pub const PROJECT: Doc = Doc {
    use_line: "project",
    short: "Make a deeper directory a project, or change a project's depth",
    long: r#"Project add makes any directory a project of its own (a deeper one than
a root's children; a root's own children are projects already), or
changes an existing project's depth."#,
    example: r#"  agent index project add ~/dev/big/vendor/lib --depth files"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

fn project_def() -> Def {
    Def::group(&PROJECT, vec![project_add_def()])
}

pub const PROJECT_ADD: Doc = Doc {
    use_line: "add DIR",
    short: "Add a directory as a project, or set a project's depth",
    long: r#"Add makes DIR a project (named after it, or --name), or sets the depth
of the project DIR already is: full (dirs, files, symbols, sections),
files (no symbols) or shallow (the project and its top directories). A
project inside another is left out of the outer one's crawl."#,
    example: r#"  agent index project add ~/dev/alpha/vendor/lib --depth files
  agent index project add ~/notes --name notes"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ProjectAddArgs {
    #[arg(long = "depth", value_name = "string", default_value = "full", help = "full, files or shallow")]
    pub depth: String,
    #[arg(long = "name", value_name = "string", default_value = "", help = "The project's name (default: the directory's)")]
    pub name: String,
    #[arg(long = "json", help = "Print what was done as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn project_add_def() -> Def {
    leaf!(PROJECT_ADD, ProjectAddArgs, run_project_add)
}

fn run_project_add(ctx: &Ctx, a: &ProjectAddArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    let r = agent_index::roots::add_project(&ix, &abs_dir(&a.args[0])?, &a.depth, &a.name)?;
    if a.json {
        return ctx.print_json(&r);
    }
    let what = match r.outcome.as_str() {
        "added" => "Added project",
        "updated" => "Updated project",
        _ => "Project",
    };
    ctx.write_out(&format!(
        "{what} {} ({}), depth {}.\nNext: agent index crawl --project {} reads it.\n",
        r.name, r.path, r.depth, r.name
    ))
}

// `agent index embed`

pub const EMBED: Doc = Doc {
    use_line: "embed",
    short: "Embed the index's entities, so search ranks by meaning as well as words",
    long: r#"Embed gives every project, directory, file, doc section and concept a
vector of its card: its path, title, summary or seed, why, and symbol
names. Search then adds a third ranked list, the entities whose cards are
nearest the query's meaning, to the two word lists (names and summaries,
and files' full text), fused by rank. Symbols and fixture files get no
vector.

The model is bge-small-en-v1.5, run on the CPU (no GPU, no server), from
the Hugging Face cache (~/.cache/huggingface/hub) or --model-dir, which
holds config.json, tokenizer.json and model.safetensors. Only entities
whose card text changed since their vector was made are embedded, so after
the first run it takes seconds. The first run over a large tree takes a
while (about 60 cards a second on 16 cores); --limit N stops after N, and
the next run goes on from there.

This agent embeds only when built with the cargo feature embed; without
it, search ranks by words alone."#,
    example: r#"  agent index crawl
  agent index embed
  agent index embed --limit 2000"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct EmbedArgs {
    #[arg(long = "limit", value_name = "int", default_value_t = 0, help = "Stop after this many entities (0: all)")]
    pub limit: i64,
    #[arg(long = "model-dir", value_name = "string", default_value = "", help = "The model's directory (default: the Hugging Face cache's bge-small-en-v1.5)")]
    pub model_dir: String,
    #[arg(long = "json", help = "Print the report as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn embed_def() -> Def {
    leaf!(EMBED, EmbedArgs, run_embed)
}

#[cfg(feature = "embed")]
fn run_embed(ctx: &Ctx, a: &EmbedArgs) -> anyhow::Result<()> {
    use agent_index::embed;
    // SAFETY: first thing in the command, before anything starts a thread
    // (main goes straight to the command); the workers each run their matrix
    // products on one thread (see embed::single_threaded_products).
    unsafe { embed::single_threaded_products() };
    let mut ix = open(ctx)?;
    require_projects(&ix)?;
    let given = (!a.model_dir.is_empty()).then(|| PathBuf::from(&a.model_dir));
    let dir = embed::model_dir(given.as_deref())?;
    let emb = embed::Embedder::load(&dir)?;
    let mut last = 0;
    let r = embed::drain(&mut ix.conn, &emb, int_flag(a.limit, "limit")?, |done, total| {
        if done >= last + 1000 && done < total {
            last = done;
            eprintln!("embedded {done} of {total}");
        }
    })?;
    if a.json {
        return ctx.print_json(&serde_json::json!({
            "model": embed::MODEL_ID,
            "embedded": r.embedded,
            "removed": r.removed,
            "settled": r.settled,
            "remaining": r.remaining,
            "secs": r.secs,
        }));
    }
    let mut o = format!(
        "Embedded {} entities in {:.1}s with {}",
        r.embedded,
        r.secs,
        embed::MODEL_ID
    );
    if r.removed > 0 {
        let _ = write!(o, "; dropped {} vectors of entities gone", r.removed);
    }
    o.push_str(".\n");
    if r.remaining > 0 {
        let _ = writeln!(o, "{} left: agent index embed goes on from there.", r.remaining);
    }
    ctx.write_out(&o)
}

#[cfg(not(feature = "embed"))]
fn run_embed(_: &Ctx, _: &EmbedArgs) -> anyhow::Result<()> {
    anyhow::bail!("this agent was built without the embedder: build it with the cargo feature embed (cargo build --features embed)")
}

/// The semantic list, when this agent has the embedder and the index has
/// vectors. A broken store or a missing model is said, and search goes on
/// with the word lists alone.
#[cfg(feature = "embed")]
fn semantic(ctx: &Ctx, ix: &Index) -> Option<Box<dyn agent_index::Semantic>> {
    use agent_index::embed;
    let dir = match embed::model_dir(None) {
        Ok(d) => d,
        Err(e) => {
            let has: bool = ix
                .conn
                .query_row("SELECT EXISTS (SELECT 1 FROM vectors)", [], |r| r.get(0))
                .unwrap_or(false);
            if has {
                ctx.hint(format!("searching by words only: the embedding model isn't here ({e:#})"));
            }
            return None;
        }
    };
    match embed::VectorSemantic::open(&ix.conn, &dir) {
        Ok(Some(s)) => Some(Box::new(s)),
        Ok(None) => None,
        Err(e) => {
            ctx.hint(format!("searching by words only: the vectors couldn't be used ({e:#})"));
            None
        }
    }
}

#[cfg(not(feature = "embed"))]
fn semantic(_: &Ctx, _: &Index) -> Option<Box<dyn agent_index::Semantic>> {
    None
}

// `agent index crawl`

pub const CRAWL: Doc = Doc {
    use_line: "crawl",
    short: "Bring the index up to date with the files on disk",
    long: r#"Crawl reads every project (or --project's): in a git repo the tracked
files and the untracked ones git doesn't ignore, keyed by blob id; outside
git a walk that honors .gitignore files, keyed by a content hash. Only
files whose key changed are read again: their kind and language, seed
summary, symbols (Rust, Go, TS/JS, Python, Swift, shell, Make), Markdown
sections and backticked references. A file renamed since the last crawl
keeps its summaries and labels, and its old id answers as an alias.

Then, across every project: depends-on edges from Cargo.toml, go.mod,
package.json and pyproject.toml; documented-by edges from backticked paths
in Markdown (a reference that resolves nowhere is listed by agent index
stale); tests edges by file naming; co-changes from git history (every
100 commits or a week); and importance (fan-in and co-change degree).
Changed entities are queued for embedding.

--rebuild reads every file again; --dry-run works it all out and writes
nothing."#,
    example: r#"  agent index crawl
  agent index crawl --project alpha
  agent index crawl --rebuild --dry-run"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct CrawlArgs {
    #[arg(long = "project", value_name = "string", default_value = "", help = "Only this project")]
    pub project: String,
    #[arg(long = "rebuild", help = "Read every file again, not only what changed")]
    pub rebuild: bool,
    #[arg(long = "dry-run", help = "Work out what would change, and write nothing")]
    pub dry_run: bool,
    #[arg(long = "json", help = "Print the crawl's report as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn crawl_def() -> Def {
    leaf!(CRAWL, CrawlArgs, run_crawl)
}

fn secs(ms: u128) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

fn run_crawl(ctx: &Ctx, a: &CrawlArgs) -> anyhow::Result<()> {
    let mut ix = open(ctx)?;
    let opts = CrawlOptions {
        project: (!a.project.is_empty()).then(|| a.project.clone()),
        rebuild: a.rebuild,
        dry_run: a.dry_run,
        large_project: 0,
    };
    let r = agent_index::crawl(&mut ix, &opts)?;
    if a.json {
        return ctx.print_json(&r);
    }
    let mut o = String::new();
    if r.projects.is_empty() {
        o.push_str("The index covers nothing yet.\nNext: agent index root add DIR adds a tree.\n");
        return ctx.write_out(&o);
    }
    let _ = writeln!(
        o,
        "Crawled {} projects in {}{}",
        r.projects.len(),
        secs(r.ms),
        if r.dry_run { " (dry run: nothing written)" } else { "" }
    );
    for p in &r.projects {
        let git = if p.git { "git" } else { "not git" };
        let mut changes = format!(
            "{} added, {} changed, {} removed, {} renamed",
            p.added, p.changed, p.removed, p.renamed
        );
        if p.commits_since > 0 {
            changes = format!("{changes}; {} commits since the last crawl", p.commits_since);
        }
        let _ = writeln!(
            o,
            "  {}  {git}, {}: {} files, {} dirs ({changes}), {} symbols and {} sections read, {}",
            p.name,
            p.depth,
            p.files,
            p.dirs,
            p.symbols,
            p.sections,
            secs(p.ms)
        );
        let mut notes = Vec::new();
        if p.secret_skipped > 0 {
            notes.push(format!("{} secret-looking files never indexed", p.secret_skipped));
        }
        if p.truncated {
            notes.push(format!(
                "its walk stopped at {} files: the deepest directories are left out",
                agent_index::walk::MAX_FILES
            ));
        }
        if p.capped {
            notes.push(format!(
                "over {} files, so crawled shallow: its top directories only (agent index project add {} --depth files|full asks for more)",
                agent_index::crawl::LARGE_PROJECT,
                p.path
            ));
        }
        if p.no_content > 0 {
            notes.push(format!("{} files without content read (large, binary or symlinks)", p.no_content));
        }
        if p.cochange {
            notes.push("co-change recomputed".to_string());
        }
        if !notes.is_empty() {
            let _ = writeln!(o, "    {}", notes.join("; "));
        }
    }
    let edges: Vec<String> = r.edges.iter().map(|(k, n)| format!("{k} {n}")).collect();
    let _ = writeln!(o, "Edges: {}.", edges.join(", "));
    if r.dangling > 0 {
        let _ = writeln!(
            o,
            "{} backticked references in docs resolve nowhere (agent index stale lists them).",
            r.dangling
        );
    }
    let _ = writeln!(
        o,
        "Search rows rewritten: {}; queued for embedding: {}.",
        r.search_rows, r.queued
    );
    for s in &r.sync.added {
        let _ = writeln!(o, "New project: {s}.");
    }
    for s in &r.sync.removed {
        let _ = writeln!(o, "Removed project (its directory is gone): {s}.");
    }
    for s in &r.sync.skipped {
        let _ = writeln!(o, "Skipped {s}.");
    }
    ctx.write_out(&o)
}

// `agent index search`

pub const SEARCH: Doc = Doc {
    use_line: "search QUERY",
    short: "Search the index: ranked entities with their paths and summaries",
    long: r#"Search finds entities by their names, paths, titles, summaries, symbols
and labels. First the query, or a run of its words, is matched against
labels (names in all their spellings, package names, README titles, old
ids, and what people and glossaries added): an exact match goes to the
top. Then bm25 over the words (identifiers whole and split: gather_global
matches "gather global") and over trigrams of names and paths (substrings
and typos), fused by reciprocal rank; then your own project first, files
and symbols over doc sections, important entities up, and directories
kept out of the way (_x, archive, old) down unless --under is in one.
At most two results come from one file.

Each result: its id, kind and freshness; the absolute path to open (with
a line for a symbol or section); its summary; and what matched. The
labels used are printed first, and any word no entity mentions is named.

--budget fits the text to that many tokens (four characters a token,
default 1500): summaries are cut to a sentence, then the tail dropped,
and it says how many were left out. --json prints every result, unfitted:
{"query", "terms", "hits": [{"rank", "id", "kind", "file_kind",
"project", "freshness", "path", "line", "title", "summary", "why",
"untracked", "matched", "score", "explain"}], "mapping", "absent",
"candidates", "ms" (milliseconds per stage)}."#,
    example: r#"  agent index search "held message delivery"
  agent index search deliver --kind symbol --project alpha
  agent index search "wire layout" --under ~/dev/beta --budget 800
  agent index search "word wall" --explain"#,
    args: ArgRule::Min(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct SearchArgs {
    #[arg(long = "kind", value_name = "string", default_value = "", help = "Only this kind: project, dir, file, symbol, section, or a file kind (source, test, fixture, doc, config, build, data, asset, submodule)")]
    pub kind: String,
    #[arg(long = "project", value_name = "string", default_value = "", help = "Only this project")]
    pub project: String,
    #[arg(long = "under", value_name = "string", default_value = "", help = "Only entities at or under this path")]
    pub under: String,
    #[arg(long = "limit", value_name = "int", default_value = "10", value_parser = flags::parse_int, help = "At most this many results")]
    pub limit: i64,
    #[arg(long = "budget", value_name = "int", default_value = "1500", value_parser = flags::parse_int, help = "Fit the text to this many tokens (0: no limit)")]
    pub budget: i64,
    #[arg(long = "explain", help = "Say which list found each result, and the priors applied")]
    pub explain: bool,
    #[arg(long = "json", help = "Print the results as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn search_def() -> Def {
    leaf!(SEARCH, SearchArgs, run_search)
}

fn run_search(ctx: &Ctx, a: &SearchArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let under = if a.under.is_empty() {
        String::new()
    } else {
        abs_dir(&a.under)?.to_string_lossy().into_owned()
    };
    let caller = caller_project(&ix);
    let o = SearchOptions {
        query: a.args.join(" "),
        kind: a.kind.clone(),
        project: a.project.clone(),
        under,
        // 0 means the default, as search takes it, so "more matched" is said.
        limit: match int_flag(a.limit, "limit")? {
            0 => 10,
            n => n,
        },
        caller_project: caller.clone(),
        scope: String::new(),
    };
    let r = match semantic(ctx, &ix) {
        Some(sem) => agent_index::search(&ix.conn, &o, sem.as_ref())?,
        None => agent_index::search(&ix.conn, &o, &NoSemantic)?,
    };
    if a.json {
        return ctx.print_json(&r);
    }
    let (text, left) = view::render_search(&r, int_flag(a.budget, "budget")?, a.explain);
    let mut out = text;
    if left > 0 {
        let _ = writeln!(
            out,
            "{left} more left out to fit --budget {}: a larger --budget, or --json, shows them.",
            a.budget
        );
    } else if r.hits.len() == o.limit && r.candidates > o.limit {
        let _ = writeln!(
            out,
            "{} more matched: --limit N shows more.",
            r.candidates - o.limit
        );
    }
    ctx.write_out(&out)?;
    // The project searched, else the one you are in.
    let project = if a.project.is_empty() { &caller } else { &a.project };
    behind_hint(ctx, &ix, project);
    Ok(())
}

/// Says when the caller's project moved since its last crawl.
fn behind_hint(ctx: &Ctx, ix: &Index, project: &str) {
    if project.is_empty() {
        return;
    }
    let Ok(Some(p)) = agent_index::roots::get_project(&ix.conn, project) else { return };
    if !p.is_git || p.last_commit.is_empty() {
        return;
    }
    let head = agent_index::git::head(Path::new(&p.path));
    if !head.is_empty() && head != p.last_commit {
        let n = agent_index::git::commits_between(Path::new(&p.path), &p.last_commit, &head);
        ctx.hint(format!(
            "{project} is {n} commits ahead of its last crawl: agent index crawl --project {project}"
        ));
    }
}

// `agent index show`

pub const SHOW: Doc = Doc {
    use_line: "show [ID|PATH]",
    short: "Show one entity: where it sits, its summary, children, edges and symbols",
    long: r#"Show prints an entity (an id, an old id, or a path; default ".", where you
are): its id, kind and freshness, its absolute path, a breadcrumb up to
the root with each ancestor's title and summary, its summary, its labels,
a file's symbols and sections, its children by importance, and its edges
by kind (depends on, used by, documented by, documents, tests, tested by,
changes with). A path is canonicalized: one that leads out of every
indexed project (a symlink, say) is refused."#,
    example: r#"  agent index show .
  agent index show alpha:src/lib.rs
  agent index show src/lib.rs --json"#,
    args: ArgRule::Max(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ShowArgs {
    #[arg(long = "json", help = "Print the entity and its neighborhood as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn show_def() -> Def {
    leaf!(SHOW, ShowArgs, run_show)
}

fn run_show(ctx: &Ctx, a: &ShowArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let arg = a.args.first().map(String::as_str).unwrap_or(".");
    let s = view::show(&ix.conn, resolve(&ix, arg)?)?;
    if a.json {
        return ctx.print_json(&s);
    }
    ctx.write_out(&view::render_show(&s))
}

// `agent index tree`

pub const TREE: Doc = Doc {
    use_line: "tree [ID|PATH]",
    short: "Print a map of a project or directory, with one-line summaries",
    long: r#"Tree prints the entities under an id or path (default: the project you
are in, or every root and project when you are in none) to --depth, one
line each with its title and summary's first sentence, the most
important first, and a directory's count of children in brackets. It is
fitted to --budget tokens (default 1500), saying how many were left out."#,
    example: r#"  agent index tree
  agent index tree alpha --depth 1
  agent index tree . --depth 3 --budget 3000"#,
    args: ArgRule::Max(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct TreeArgs {
    #[arg(long = "depth", value_name = "int", default_value = "2", value_parser = flags::parse_int, help = "How many levels down")]
    pub depth: i64,
    #[arg(long = "budget", value_name = "int", default_value = "1500", value_parser = flags::parse_int, help = "Fit to this many tokens (0: no limit)")]
    pub budget: i64,
    #[arg(long = "json", help = "Print the tree as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn tree_def() -> Def {
    leaf!(TREE, TreeArgs, run_tree)
}

fn run_tree(ctx: &Ctx, a: &TreeArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let id = match a.args.first() {
        Some(arg) => resolve(&ix, arg)?.id,
        None => caller_project(&ix),
    };
    let t = view::tree(&ix.conn, &id, int_flag(a.depth, "depth")?, int_flag(a.budget, "budget")?)?;
    if a.json {
        return ctx.print_json(&t);
    }
    let mut o = String::new();
    if !id.is_empty()
        && let Some(e) = agent_index::model::get(&ix.conn, &id)?
    {
        let s = agent_index::model::summary(&ix.conn, &id)?;
        let mut head = format!("{}  {}", e.id, e.display_title());
        if let Some(s) = s {
            head = format!("{head}: {}", agent_index::extract::first_sentence(&s.text));
        }
        let _ = writeln!(o, "{}", agent_index::extract::clip(&head, 160));
    }
    o.push_str(&view::render_tree(&t));
    if t.left_out > 0 {
        let _ = writeln!(
            o,
            "{} more entries left out to fit --budget {}: a larger --budget or a smaller --depth.",
            t.left_out, a.budget
        );
    }
    ctx.write_out(&o)
}

// `agent index related`

pub const RELATED: Doc = Doc {
    use_line: "related ID|PATH",
    short: "List an entity's neighbors by edge: dependencies, docs, tests, co-changes",
    long: r#"Related lists what an entity is tied to, grouped by how the edge reads from
it (depends on, used by, documented by, documents, tests, tested by,
changes with), each group by weight, with each neighbor's summary."#,
    example: r#"  agent index related alpha:src/lib.rs
  agent index related src/lib.rs --json"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct RelatedArgs {
    #[arg(long = "budget", value_name = "int", default_value = "1500", value_parser = flags::parse_int, help = "Fit to this many tokens (0: no limit)")]
    pub budget: i64,
    #[arg(long = "json", help = "Print the neighbors as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn related_def() -> Def {
    leaf!(RELATED, RelatedArgs, run_related)
}

fn run_related(ctx: &Ctx, a: &RelatedArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let res = resolve(&ix, &a.args[0])?;
    let r = view::related(&ix.conn, &res.id)?;
    if a.json {
        return ctx.print_json(&r);
    }
    let (text, left) = view::render_related(&r, int_flag(a.budget, "budget")?);
    let mut o = format!("Related to {}:\n", r.id);
    if r.groups.is_empty() {
        o.push_str("No edges yet: nothing depends on it, documents it, tests it or changes with it.\n");
    }
    o.push_str(&text);
    if left > 0 {
        let _ = writeln!(o, "{left} more left out to fit --budget {}.", a.budget);
    }
    ctx.write_out(&o)
}

// `agent index stale`

pub const STALE: Doc = Doc {
    use_line: "stale",
    short: "List what needs a summary, by importance, and doc references that resolve nowhere",
    long: r#"Stale counts the projects, directories and files of the index (or of
--project) by freshness (fresh, seed, stale, none), lists those that are
stale or have no summary at all, most important first, and the
backticked references in docs that resolve to no indexed path (doc
drift)."#,
    example: r#"  agent index stale
  agent index stale --project alpha --limit 20"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct StaleArgs {
    #[arg(long = "project", value_name = "string", default_value = "", help = "Only this project")]
    pub project: String,
    #[arg(long = "limit", value_name = "int", default_value = "50", value_parser = flags::parse_int, help = "At most this many of each list")]
    pub limit: i64,
    #[arg(long = "json", help = "Print the counts and lists as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn stale_def() -> Def {
    leaf!(STALE, StaleArgs, run_stale)
}

fn run_stale(ctx: &Ctx, a: &StaleArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let s = view::stale(&ix.conn, &a.project, int_flag(a.limit, "limit")?)?;
    if a.json {
        return ctx.print_json(&s);
    }
    let mut o = String::new();
    let counts: Vec<String> = s.counts.iter().map(|(k, n)| format!("{k} {n}")).collect();
    let scope = if a.project.is_empty() { "the index".to_string() } else { a.project.clone() };
    let _ = writeln!(o, "Freshness in {scope} (projects, dirs and files): {}.", counts.join(", "));
    if s.entities.is_empty() {
        let _ = writeln!(o, "Nothing is stale or without a summary.");
    } else {
        let _ = writeln!(o, "Stale or without a summary, most important first:");
        for b in &s.entities {
            let _ = writeln!(o, "  {}  {}  {}", b.id, b.kind, b.freshness);
        }
    }
    if !s.dangling.is_empty() {
        let _ = writeln!(o, "References in docs that resolve nowhere:");
        for d in &s.dangling {
            let _ = writeln!(o, "  {} line {}: `{}`", d.doc, d.line, d.reference);
        }
    }
    ctx.write_out(&o)
}

// `agent index alias`

pub const ALIAS: Doc = Doc {
    use_line: "alias",
    short: "Add a person's label to an entity",
    long: r#"Alias add gives an entity a name people use for it ("the word wall"), so
search finds it by that name first. Labels survive crawls and renames."#,
    example: r#"  agent index alias add alpha:word-wall "the word wall""#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

fn alias_def() -> Def {
    Def::group(&ALIAS, vec![alias_add_def()])
}

pub const ALIAS_ADD: Doc = Doc {
    use_line: "add ID|PATH LABEL",
    short: "Give an entity a label search answers to",
    long: r#"Add labels an entity (an id, an old id or a path) with LABEL, as a
person's alternative label: a search whose words are the label, or
contain it, puts the entity first and prints the mapping."#,
    example: r#"  agent index alias add alpha:word-wall "the word wall"
  agent index alias add src/deliver.rs "the part that types into busy agents""#,
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct AliasAddArgs {
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn alias_add_def() -> Def {
    leaf!(ALIAS_ADD, AliasAddArgs, run_alias_add)
}

fn run_alias_add(ctx: &Ctx, a: &AliasAddArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let res = resolve(&ix, &a.args[0])?;
    let id = agent_index::labels::alias_add(&ix.conn, &res.id, &a.args[1])?;
    ctx.write_out(&format!(
        "Labeled {id} \"{}\".\nNext: agent index search {:?} finds it first.\n",
        a.args[1].trim(),
        a.args[1].trim()
    ))
}

// `agent index lexicon`

pub const LEXICON: Doc = Doc {
    use_line: "lexicon",
    short: "Import an existing glossary as labels",
    long: r#"Lexicon import reads a project's glossary into labels, so search answers
to the words people use."#,
    example: r#"  agent index lexicon import docs/lexicon.jsonl"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

fn lexicon_def() -> Def {
    Def::group(&LEXICON, vec![lexicon_import_def()])
}

pub const LEXICON_IMPORT: Doc = Doc {
    use_line: "import FILE",
    short: "Import a JSONL glossary: lay phrasings, canonical names, provenance",
    long: r#"Import reads FILE, one JSON object a line: {"term", "canonical",
"definition", "confusables", "provenance": ["path:line", ...],
"lay_phrasings": [...]}. Each row's provenance names the entities it is
about (a path relative to the file's directory or a project, an absolute
path, or an id; the line is dropped); a row without one goes to the
entities its canonical name already labels. Its lay phrasings become ask
labels and its term and canonical name alternative labels, with the
definition and confusables kept as the labels' evidence. Rows that
resolve to no entity are skipped and counted."#,
    example: r#"  agent index lexicon import docs/lexicon.jsonl
  agent index lexicon import lexicon.jsonl --json"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct LexiconImportArgs {
    #[arg(long = "json", help = "Print the import's report as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn lexicon_import_def() -> Def {
    leaf!(LEXICON_IMPORT, LexiconImportArgs, run_lexicon_import)
}

fn run_lexicon_import(ctx: &Ctx, a: &LexiconImportArgs) -> anyhow::Result<()> {
    let ix = open(ctx)?;
    require_projects(&ix)?;
    let file = {
        let p = Path::new(&a.args[0]);
        if p.is_absolute() { p.to_path_buf() } else { cwd()?.join(p) }
    };
    let r = agent_index::labels::lexicon_import(&ix.conn, &file)?;
    if a.json {
        return ctx.print_json(&r);
    }
    let mut o = format!(
        "Imported {} of {} rows from {}: {} labels.\n",
        r.imported, r.rows, r.file, r.labels
    );
    if r.unresolved > 0 {
        let terms: BTreeSet<&String> = r.unresolved_terms.iter().collect();
        let _ = writeln!(
            o,
            "Skipped {} rows that resolve to no entity (their provenance names no indexed path): {}.",
            r.unresolved,
            terms.into_iter().map(|t| format!("{t:?}")).collect::<Vec<_>>().join(", ")
        );
    }
    if r.unreadable > 0 {
        let _ = writeln!(o, "Skipped {} lines that aren't JSON objects.", r.unreadable);
    }
    ctx.write_out(&o)
}

#[cfg(test)]
#[path = "index_test.rs"]
mod index_test;
