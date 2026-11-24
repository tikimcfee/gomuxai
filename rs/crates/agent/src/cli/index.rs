//! `agent index` and its subcommands (no Go file: new in the Rust build;
//! docs/INDEX.md). The index is agent_index's; this file parses flags and
//! prints. It lives in `index.db`, beside this binary's `gomuxai.db`.
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use agent_index::pg::Pg;
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
            sync_def(),
            summarize_def(),
            config_def(),
            serve_def(),
            embed_def(),
            search_def(),
            show_def(),
            tree_def(),
            related_def(),
            stale_def(),
            usage_def(),
            alias_def(),
            lexicon_def(),
        ],
    )
}

// `agent index usage`

pub const USAGE: Doc = Doc {
    use_line: "usage",
    short: "Count the logged searches, shows, trees and relateds, by agent and by day",
    long: r#"Usage reads the query log (one JSON line per search, show, tree and
related: when, which agent from AGENT_NAME, the command, query and flags,
the top five paths, the time taken and the configuration's fingerprint),
and counts the calls by agent and command and by day, with each
command's median and 90th percentile time. The log is
~/.local/share/gomuxai/index-queries.jsonl ($XDG_DATA_HOME), or [log].path
in the index configuration; [log].enabled = false stops it."#,
    example: r#"  agent index usage
  agent index usage --since 2026-11-24 --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct UsageArgs {
    #[arg(
        long = "since",
        value_name = "string",
        default_value = "",
        help = "Only calls at or after this time (UTC, RFC 3339 or a prefix: 2026-11-24)"
    )]
    pub since: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print the counts as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn usage_def() -> Def {
    leaf!(USAGE, UsageArgs, run_usage)
}

fn run_usage(ctx: &Ctx, a: &UsageArgs) -> anyhow::Result<()> {
    let cfg = load_config(&a.config)?.config;
    let mut log = cfg.clone();
    log.log.enabled = true;
    let Some(path) = log.log_path() else {
        anyhow::bail!("no query log: HOME isn't set and [log].path is empty");
    };
    let since = (!a.since.is_empty()).then_some(a.since.as_str());
    let u = agent_index::qlog::usage(&path, since)
        .map_err(|e| anyhow::anyhow!("reading the query log {}: {e}", path.display()))?;
    if a.json {
        return ctx.print_json(&serde_json::json!({"log": path, "enabled": cfg.log.enabled, "usage": u}));
    }
    let mut o = String::new();
    let counts = |m: &std::collections::BTreeMap<String, usize>| {
        m.iter()
            .map(|(k, n)| format!("{k} {n}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(
        o,
        "{} logged calls{} in {}{}.",
        u.entries,
        if u.entries > 0 {
            format!(" from {} to {}", u.first, u.last)
        } else {
            String::new()
        },
        path.display(),
        if u.errors > 0 {
            format!(", {} of them failed", u.errors)
        } else {
            String::new()
        }
    );
    if !cfg.log.enabled {
        let _ = writeln!(o, "The log is off ([log].enabled = false).");
    }
    if !u.by_agent.is_empty() {
        let w = u.by_agent.keys().map(String::len).max().unwrap_or(0);
        let _ = writeln!(o, "By agent:");
        for (agent, m) in &u.by_agent {
            let _ = writeln!(o, "  {agent:<w$}  {}", counts(m));
        }
        let _ = writeln!(o, "By day (UTC):");
        for (day, m) in &u.by_day {
            let _ = writeln!(o, "  {day}  {}", counts(m));
        }
        let lat: Vec<String> = u
            .latency
            .iter()
            .map(|(c, (p50, p90))| format!("{c} {p50:.0} / {p90:.0}"))
            .collect();
        let _ = writeln!(o, "Time, median / 90th percentile ms: {}.", lat.join("; "));
    }
    if u.unreadable > 0 {
        let _ = writeln!(o, "{} lines weren't entries (skipped).", u.unreadable);
    }
    ctx.write_out(&o)
}

/// The index (the pg store's crawl ledger): the one the configuration
/// names (`[ledger].path`, the configuration as Config::load finds it),
/// else index.db beside this binary's gomuxai.db.
fn open(ctx: &Ctx) -> anyhow::Result<Index> {
    open_with(ctx, "")
}

/// [`open`], with `--config`.
fn open_with(ctx: &Ctx, config: &str) -> anyhow::Result<Index> {
    open_cfg(ctx, &load_config(config)?.config)
}

/// [`open`], with a configuration in hand.
fn open_cfg(ctx: &Ctx, cfg: &agent_index::config::Config) -> anyhow::Result<Index> {
    Index::open(&cfg.ledger_path(agent_index::path_beside(&ctx.cfg().database_path)))
}

/// The pg store's URL and schema (AGENT_INDEX_PG, else `[store].url`), or
/// an error saying how to give one.
fn pg_target(cfg: &agent_index::config::Config) -> anyhow::Result<(String, String)> {
    cfg.pg_target().ok_or_else(|| {
        anyhow::anyhow!(
            "the pg store needs its database URL (postgresql:///DB?host=SOCKET_DIR): AGENT_INDEX_PG, or [store].url in the index configuration"
        )
    })
}

/// The Postgres store, when the command uses it: `--store pg`, else
/// `AGENT_INDEX_STORE=pg`, else `[store].kind = "pg"` in the configuration.
/// Its URL is `AGENT_INDEX_PG`, else `[store].url`; its schema
/// `AGENT_INDEX_PG_SCHEMA`, else `[store].schema`, else idx. None for the
/// SQLite store (the default). With Postgres, index.db is the crawl's
/// ledger only: every query reads Postgres. Its tunables come from the
/// configuration file (`--config`, else AGENT_INDEX_CONFIG, else
/// ~/.config/gomuxai/index.toml; none there means the defaults).
fn pg_store(store: &str, config: &str) -> anyhow::Result<Option<Pg>> {
    let cfg = load_config(config)?.config;
    if !cfg.wants_pg(store)? {
        return Ok(None);
    }
    let (url, schema) = pg_target(&cfg)?;
    Ok(Some(Pg::connect_with(&url, &schema, cfg)?))
}

/// The configuration: `--config` when given, else as Config::load finds it.
fn load_config(config: &str) -> anyhow::Result<agent_index::config::Loaded> {
    let given = (!config.is_empty()).then(|| Path::new(config));
    agent_index::config::Config::load(given)
}

/// After a command changed the ledger outside a crawl (a label, a root
/// removed): with the pg store chosen by AGENT_INDEX_STORE, syncs it.
fn resync(ctx: &Ctx, ix: &Index) -> anyhow::Result<()> {
    if let Some(pg) = pg_store("", "")? {
        sync_pg(ctx, ix, &pg, false)?;
    }
    Ok(())
}

/// Mirrors the ledger into Postgres, with the memory and friction corpora,
/// saying so on stderr.
fn sync_pg(
    ctx: &Ctx,
    ix: &Index,
    pg: &Pg,
    rebuild: bool,
) -> anyhow::Result<agent_index::pg::sync::SyncReport> {
    let t0 = std::time::Instant::now();
    let (side, sessions) = side_notes(ctx, ix, pg)?;
    let read_ms = t0.elapsed().as_millis();
    let mut r = agent_index::pg::sync::sync_with(&ix.conn, pg, rebuild, Some(&side))?;
    r.phases.insert("read side corpora".into(), read_ms);
    ctx.notice(format!(
        "Synced to Postgres ({}, schema {}): {} entities ({} memory notes, friction entries and sessions, {} edges from them), {} written, {} removed, {} files cut into {} chunks, {} queued for embedding, in {}.",
        pg.url_shown,
        pg.schema,
        r.entities,
        r.side,
        r.side_edges,
        r.written,
        r.deleted,
        r.chunked_files,
        r.chunks,
        r.queued,
        secs(r.ms + read_ms)
    ));
    if let Some(s) = &sessions {
        ctx.notice(format!(
            "Sessions: {} listed, {} read, {} unchanged, {} left out (scripted {}, subagents {}, too large {}), {} unreadable; {} turns in {} chunks; scrubbed {} pasted and {} inserted blocks and {} credential lines ({} of {} prose lines kept).",
            s.listed,
            s.read,
            s.kept,
            s.scripted + s.subagents + s.too_large,
            s.scripted,
            s.subagents,
            s.too_large,
            s.failed,
            s.turns,
            s.chunks,
            s.pasted_blocks,
            s.inserted_blocks,
            s.credential_lines,
            s.lines_kept,
            s.lines_in
        ));
    }
    r.sessions = sessions;
    Ok(r)
}

/// The memory notes ([memory].dirs), this host's friction entries (the
/// agent store, or [friction].store opened read only) and its Claude and
/// agy sessions (the directories agent sessions scans), as the sync's side
/// notes; a session unchanged since the last sync is kept, not read.
fn side_notes(
    ctx: &Ctx,
    ix: &Index,
    pg: &Pg,
) -> anyhow::Result<(
    Vec<agent_index::pg::sync::SideNote>,
    Option<agent_index::pg::sessions::SessionsReport>,
)> {
    use agent_index::pg::side;
    let cfg = &pg.cfg;
    let projects: Vec<(String, String)> = agent_index::roots::list_projects(&ix.conn)?
        .into_iter()
        .map(|p| (p.name, p.path))
        .collect();
    let data = agent_index::path_beside(&ctx.cfg().database_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_default();
    let dirs = side::expand_dirs(&cfg.memory.dirs, &data);
    let mut notes = side::memory_notes(&dirs, &data, &projects);
    if !cfg.friction.off {
        let filter = agent_store::models::FrictionFilter::default();
        let entries = if cfg.friction.store.is_empty() {
            ctx.store()?.list_friction(&filter)?
        } else {
            agent_store::Store::open_read_only(&cfg.friction.store)?.list_friction(&filter)?
        };
        for f in entries {
            let e = side::FrictionEntry {
                uid: f.uid,
                kind: f.kind,
                status: f.status,
                summary: f.summary,
                detail: f.detail,
                resolution: f.resolution,
                accept_note: f.accept_note,
                cwd: f.cwd,
                filed_by: f.filed_by,
                created_at: f.created_at.to_string(),
                error: f.command.map(|c| c.error).unwrap_or_default(),
            };
            notes.push(side::friction_note(&e, &projects));
        }
    }
    let mut report = None;
    if !cfg.sessions.off {
        use agent_index::pg::sessions;
        let listed = agent_discovery::scan(&super::transcript::session_dirs(ctx));
        let refs: Vec<sessions::SessionRef> = listed
            .into_iter()
            .map(|s| sessions::SessionRef {
                harness: s.harness,
                session_id: s.session_id,
                path: s.path,
                cwd: s.cwd,
                title: s.title,
                size: s.size,
                mtime: s.mtime.map(|t| t.to_rfc3339()).unwrap_or_default(),
                parent: s.parent_session.map(|p| p.session_id).unwrap_or_default(),
            })
            .collect();
        let known = sessions::known_keys(pg)?;
        let (more, rep) = sessions::session_notes(&refs, &known, &projects, &cfg.sessions);
        notes.extend(more);
        report = Some(rep);
    }
    Ok((notes, report))
}

fn cwd() -> anyhow::Result<PathBuf> {
    std::env::current_dir().context("reading the current directory")
}

/// A path argument against the current directory, canonical.
fn abs_dir(arg: &str) -> anyhow::Result<PathBuf> {
    let p = Path::new(arg);
    let p = if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd()?.join(p)
    };
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
    let n: i64 = ix.conn.query_row(
        "SELECT count(*) FROM entities WHERE kind = 'project'",
        [],
        |r| r.get(0),
    )?;
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
    Def::group(
        &ROOT,
        vec![root_add_def(), root_remove_def(), root_list_def()],
    )
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
    #[arg(
        long = "depth",
        value_name = "string",
        default_value = "full",
        help = "How deep its projects go: full, files or shallow"
    )]
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
    resync(ctx, &ix)?;
    let list = if removed.is_empty() {
        "none".to_string()
    } else {
        removed.join(", ")
    };
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
        let _ = writeln!(
            o,
            "root {}  {}  depth {}, {} projects",
            r.id, r.path, r.depth, r.projects
        );
    }
    for p in &projects {
        let crawled = if p.last_crawled_at.is_empty() {
            "never crawled".to_string()
        } else {
            format!("crawled {}", p.last_crawled_at)
        };
        let kind = if p.last_crawled_at.is_empty() {
            ""
        } else if p.is_git {
            "git, "
        } else {
            "not git, "
        };
        let added = if p.explicit {
            ", added by project add"
        } else {
            ""
        };
        let _ = writeln!(
            o,
            "  {}  {}  {}{}, {crawled}{added}",
            p.name, p.path, kind, p.depth
        );
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
    #[arg(
        long = "depth",
        value_name = "string",
        default_value = "full",
        help = "full, files or shallow"
    )]
    pub depth: String,
    #[arg(
        long = "name",
        value_name = "string",
        default_value = "",
        help = "The project's name (default: the directory's)"
    )]
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
    #[arg(
        long = "limit",
        value_name = "int",
        default_value_t = 0,
        help = "Stop after this many entities (0: all)"
    )]
    pub limit: i64,
    #[arg(
        long = "model-dir",
        value_name = "string",
        default_value = "",
        help = "The model's directory (default: the Hugging Face cache's bge-small-en-v1.5)"
    )]
    pub model_dir: String,
    #[arg(
        long = "store",
        value_name = "string",
        default_value = "",
        help = "sqlite or pg (default: AGENT_INDEX_STORE, else [store].kind in the index configuration, else sqlite)"
    )]
    pub store: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
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
    let pg = pg_store(&a.store, &a.config)?;
    let mut ix = open_with(ctx, &a.config)?;
    match &pg {
        Some(pg) => {
            if agent_index::pg::view::project_count(pg)? == 0 {
                anyhow::bail!(
                    "nothing is in the pg store yet: agent index crawl --store pg fills it"
                );
            }
        }
        None => require_projects(&ix)?,
    }
    // --model-dir, else the pg store's configured one, else the cache's.
    let mut given = (!a.model_dir.is_empty()).then(|| PathBuf::from(&a.model_dir));
    if let Some(pg) = &pg {
        check_embedder(&pg.cfg)?;
        if given.is_none() && !pg.cfg.embedder.model_dir.is_empty() {
            given = Some(PathBuf::from(&pg.cfg.embedder.model_dir));
        }
    }
    let dir = embed::model_dir(given.as_deref())?;
    let emb = embed::Embedder::load(&dir)?;
    let mut last = 0;
    let progress = |done: usize, total: usize| {
        if done >= last + 1000 && done < total {
            last = done;
            eprintln!("embedded {done} of {total}");
        }
    };
    let r = match &pg {
        Some(pg) => {
            agent_index::pg::embed_pg::drain(pg, &emb, int_flag(a.limit, "limit")?, progress)?
        }
        None => embed::drain(&mut ix.conn, &emb, int_flag(a.limit, "limit")?, progress)?,
    };
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
        let _ = writeln!(
            o,
            "{} left: agent index embed goes on from there.",
            r.remaining
        );
    }
    ctx.write_out(&o)
}

#[cfg(not(feature = "embed"))]
fn run_embed(_: &Ctx, _: &EmbedArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "this agent was built without the embedder: build it with the cargo feature embed (cargo build --features embed)"
    )
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
                ctx.hint(format!(
                    "searching by words only: the embedding model isn't here ({e:#})"
                ));
            }
            return None;
        }
    };
    match embed::VectorSemantic::open(&ix.conn, &dir) {
        Ok(Some(s)) => Some(Box::new(s)),
        Ok(None) => None,
        Err(e) => {
            ctx.hint(format!(
                "searching by words only: the vectors couldn't be used ({e:#})"
            ));
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
    #[arg(
        long = "project",
        value_name = "string",
        default_value = "",
        help = "Only this project"
    )]
    pub project: String,
    #[arg(
        long = "rebuild",
        help = "Read every file again, not only what changed"
    )]
    pub rebuild: bool,
    #[arg(
        long = "dry-run",
        help = "Work out what would change, and write nothing"
    )]
    pub dry_run: bool,
    #[arg(
        long = "store",
        value_name = "string",
        default_value = "",
        help = "sqlite or pg (default: AGENT_INDEX_STORE, else [store].kind in the index configuration, else sqlite); pg syncs the crawl into Postgres"
    )]
    pub store: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print the crawl's report as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn crawl_def() -> Def {
    leaf!(CRAWL, CrawlArgs, run_crawl)
}

// `agent index config`

pub const CONFIG: Doc = Doc {
    use_line: "config",
    short: "Print the index configuration in effect, and its fingerprint",
    long: r#"Config prints the configuration the pg store's commands run with, every
key resolved (a key the file leaves out has its default), as TOML, with
where it came from and its fingerprint (a hash of the values: search
--json reports it, and syncs, embeds and logged queries record it).

The file is --config FILE, else AGENT_INDEX_CONFIG, else
~/.config/gomuxai/index.toml ($XDG_CONFIG_HOME/gomuxai/index.toml); with
no file there, the built-in defaults apply. A file named by --config or
the environment must exist, and an unknown key is an error. Print the
defaults into a file to start one: agent index config > index.toml. See
docs/INDEX-V2.md, "Configuration", for what each section tunes."#,
    example: r#"  agent index config
  agent index config --config tune/rrf30.toml --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ConfigArgs {
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print {\"path\", \"fingerprint\", \"config\"}")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn config_def() -> Def {
    leaf!(CONFIG, ConfigArgs, run_config)
}

fn run_config(ctx: &Ctx, a: &ConfigArgs) -> anyhow::Result<()> {
    let l = load_config(&a.config)?;
    let path = l.path.as_ref().map(|p| p.display().to_string());
    let fp = l.config.fingerprint();
    if a.json {
        return ctx.print_json(&serde_json::json!({
            "path": path,
            "fingerprint": fp,
            "config": l.config,
        }));
    }
    let from = match &path {
        Some(p) => format!("from {p}"),
        None => "the built-in defaults (no configuration file)".to_string(),
    };
    ctx.write_out(&format!(
        "# The index configuration in effect: {from}.\n# Fingerprint {fp}.\n\n{}",
        l.config.to_toml()
    ))
}

// `agent index sync`

pub const SYNC: Doc = Doc {
    use_line: "sync",
    short: "Mirror the crawl's index.db into Postgres (the pg store)",
    long: r#"Sync writes what the last crawl found into the Postgres store
(AGENT_INDEX_PG; schema AGENT_INDEX_PG_SCHEMA, default idx): every entity
whose row changed, its labels, the chunks of files whose content changed
(read from disk again), edges and projects, in one transaction. Entities
whose card text changed are queued for agent index embed. crawl --store pg
runs it after crawling; run it alone after a crawl with the sqlite store.

--rebuild empties the schema's tables and writes everything again (card
vectors whose text is unchanged are kept); the rows are the same an
incremental sync reaches."#,
    example: r#"  AGENT_INDEX_PG='postgresql:///devindex?host=/run/pg' agent index sync
  agent index sync --rebuild --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct SyncArgs {
    #[arg(
        long = "rebuild",
        help = "Empty the Postgres tables and write everything again"
    )]
    pub rebuild: bool,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print the sync's report as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn sync_def() -> Def {
    leaf!(SYNC, SyncArgs, run_sync)
}

fn run_sync(ctx: &Ctx, a: &SyncArgs) -> anyhow::Result<()> {
    let ix = open_with(ctx, &a.config)?;
    let Some(pg) = pg_store("pg", &a.config)? else {
        unreachable!("pg asked")
    };
    let r = sync_pg(ctx, &ix, &pg, a.rebuild)?;
    if a.json {
        return ctx.print_json(&r);
    }
    let phases: Vec<String> = r
        .phases
        .iter()
        .map(|(k, v)| format!("{k} {}", secs(*v)))
        .collect();
    ctx.write_out(&format!(
        "Synced {} entities into {} (schema {}) in {}: {} written, {} removed, {} labels, {} files cut into {} chunks, {} queued for embedding{}{}.\nPhases: {}.\n",
        r.entities,
        pg.url_shown,
        pg.schema,
        secs(r.ms),
        r.written,
        r.deleted,
        r.labels,
        r.chunked_files,
        r.chunks,
        r.queued,
        if r.edges_replaced { format!("; edges rewritten ({})", r.edges) } else { String::new() },
        if r.tables_replaced.is_empty() { String::new() } else { format!("; {} rewritten", r.tables_replaced.join(", ")) },
        phases.join(", ")
    ))
}

// `agent index summarize`

pub const SUMMARIZE: Doc = Doc {
    use_line: "summarize",
    short: "Write file summaries and asks with a local model (the pg store)",
    long: r#"Summarize asks a local OpenAI-compatible model server ([llm.summaries]
endpoint in the index configuration; agent never starts one) for a summary,
a why and three to five plain asks for each file of the full-depth projects
that gets one, checks each answer in code, and keeps it in Postgres keyed
by the file's content key and the run's fingerprint (the model and its
snapshot, the server, the prompt, sampling, clipping, filters, checks and
this build). A sync then shows a summary over the file's seed while the
file is unchanged and queues the changed cards for agent index embed.

Skipped, before any read: sensitive names, files with no content, fixture,
data and asset kinds, vendored code, program output directories (outputs,
results, snapshots, runs, logs), directories of 50+ numbered files, lock and
checksum files, licenses and ignore files, JSON that isn't a manifest or a
small hand-written file; after reading: trivial files and generated ones.
--coverage prints what would be summarized and what is skipped and why, by
project and kind, and the 20 directories with the most skipped, without
calling the model.

A run resumes: files already summarized (or failed) under the same
fingerprint are skipped; files the server didn't answer in time (5xx,
timeouts, past --file-timeout) get no row, so a resume asks again. It
stops cleanly and exits 1 when the server stops answering (8 connection
errors in a row, or /health failing), when no file finished for --stall,
after --max-time, or when over --max-error-rate of the last --error-window
files failed; its stats (done, remaining, skipped, errors, rate, ETA) are
saved in its runs row and --stats FILE as it goes. --skip-project leaves a
project out; --own-files PROJECT=FILE keeps only a fork's own files."#,
    example: r#"  agent index summarize --coverage --project gomuxai
  agent index summarize --project gomuxai --limit 100 --stats run.json
  agent index summarize --prefix 'lab:research/' --limit 50
  agent index summarize --skip-project vendor-fork --max-time 4h --stats status.json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct SummarizeArgs {
    #[arg(
        long = "project",
        value_name = "stringArray",
        help = "Only this project (repeatable); default every full-depth project"
    )]
    pub project: Vec<String>,
    #[arg(
        long = "corpus",
        value_name = "stringArray",
        help = "Files of this corpus (code, prose; repeatable); default code"
    )]
    pub corpus: Vec<String>,
    #[arg(
        long = "prefix",
        value_name = "stringArray",
        help = "Only files whose id starts so, as project:dir/ (repeatable)"
    )]
    pub prefix: Vec<String>,
    #[arg(
        long = "limit",
        value_name = "int",
        default_value_t = 0,
        help = "At most this many files per project (or prefix), sampled evenly; 0 is all"
    )]
    pub limit: i64,
    #[arg(
        long = "coverage",
        help = "Print what would be summarized and what is skipped, and why; call no model"
    )]
    pub coverage: bool,
    #[arg(
        long = "dirs",
        help = "The bottom-up pass: summarize the directories holding the selected files, deepest first, then their projects, from their README, listing and children's summaries (run it after the files); --limit N samples N of them"
    )]
    pub dirs: bool,
    #[arg(
        long = "out",
        value_name = "string",
        default_value = "",
        help = "Write the coverage report (or the run's stats) as JSON here"
    )]
    pub out: String,
    #[arg(
        long = "stats",
        value_name = "string",
        default_value = "",
        help = "Rewrite the run's stats here as it goes (with --dirs: the whole pass's status)"
    )]
    pub stats: String,
    #[arg(
        long = "skip-project",
        value_name = "stringArray",
        help = "Leave this project out (repeatable); its files count as skipped"
    )]
    pub skip_project: Vec<String>,
    #[arg(
        long = "skip-prefix",
        value_name = "stringArray",
        help = "Leave files whose id starts so out, as project:dir/ (repeatable); they count as skipped"
    )]
    pub skip_prefix: Vec<String>,
    #[arg(
        long = "own-files",
        value_name = "stringArray",
        help = "PROJECT=FILE: summarize only the files FILE lists (paths in the project, one a line) of that fork; its others count as upstream (repeatable)"
    )]
    pub own_files: Vec<String>,
    #[arg(
        long = "file-timeout",
        value_name = "duration",
        default_value = "10m",
        help = "One file's deadline, all its requests and retries together; past it the file gets no row, so a resume asks again"
    )]
    pub file_timeout: String,
    #[arg(
        long = "stall",
        value_name = "duration",
        default_value = "10m",
        help = "Stop the run when no file finished for this long (0: never; with --dirs, over the whole pass)"
    )]
    pub stall: String,
    #[arg(
        long = "max-time",
        value_name = "duration",
        default_value = "0",
        help = "Stop the run cleanly after this long (0: no cap; with --dirs, counted from the pass's start)"
    )]
    pub max_time: String,
    #[arg(
        long = "error-window",
        value_name = "int",
        default_value_t = 200,
        help = "Stop the run when over --max-error-rate of this many latest files failed (0: never)"
    )]
    pub error_window: i64,
    #[arg(
        long = "max-error-rate",
        value_name = "string",
        default_value = "0.2",
        help = "The failed share of --error-window files that stops the run"
    )]
    pub max_error_rate: String,
    #[arg(
        long = "retry-failed",
        help = "Ask again for files that failed under this fingerprint"
    )]
    pub retry_failed: bool,
    #[arg(
        long = "no-sync",
        help = "Don't sync afterwards (the summaries show after the next sync)"
    )]
    pub no_sync: bool,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(
        long = "json",
        help = "Print the coverage report or the run's stats as JSON"
    )]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn summarize_def() -> Def {
    leaf!(SUMMARIZE, SummarizeArgs, run_summarize)
}

fn coverage_text(c: &agent_index::pg::summ::Coverage) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "{} files in full-depth projects: {} to summarize, {} skipped ({} selected after --limit).",
        c.files, c.summarize, c.skipped, c.selected
    );
    let _ = writeln!(
        s,
        "A bottom-up pass over them: {} directories, {} projects.",
        c.bottom_up_dirs, c.bottom_up_projects
    );
    let mut reasons: Vec<_> = c.by_reason.iter().collect();
    reasons.sort_by(|a, b| b.1.cmp(a.1));
    let _ = writeln!(s, "\nSkipped, by reason:");
    for (r, n) in reasons {
        let _ = writeln!(s, "  {n:>7}  {r}");
    }
    for (title, groups) in [("project", &c.by_project), ("kind", &c.by_kind)] {
        let _ = writeln!(s, "\nBy {title} (summarize / skipped: top reasons):");
        for (k, g) in groups {
            let mut rs: Vec<_> = g.skipped.iter().collect();
            rs.sort_by(|a, b| b.1.cmp(a.1));
            let top: Vec<String> = rs.iter().take(4).map(|(r, n)| format!("{r} {n}")).collect();
            let _ = writeln!(
                s,
                "  {k:<28} {:>6} / {:>6}  {}",
                g.summarize,
                g.skipped.values().sum::<usize>(),
                top.join(", ")
            );
        }
    }
    if c.copied_files > 0 {
        let _ = writeln!(
            s,
            "\nIn crates copied from elsewhere (their manifest names another repository; summarized, marked): {} files in {} crates:",
            c.copied_files,
            c.copied.len()
        );
        let mut cs: Vec<_> = c.copied.iter().collect();
        cs.sort_by(|a, b| b.1.files.cmp(&a.1.files).then(a.0.cmp(b.0)));
        for (d, x) in cs.iter().take(25) {
            let _ = writeln!(s, "  {:>6}  {d}  ({})", x.files, x.repository);
        }
    }
    let _ = writeln!(s, "\nDirectories with the most skipped files:");
    for (d, n, r) in &c.top_skipped_dirs {
        let _ = writeln!(s, "  {n:>6}  {d}  ({r})");
    }
    s
}

/// `--own-files PROJECT=FILE`: each project's list of its own files.
fn own_files(
    args: &[String],
) -> anyhow::Result<std::collections::BTreeMap<String, std::collections::HashSet<String>>> {
    let mut out = std::collections::BTreeMap::new();
    for a in args {
        let Some((project, file)) = a.split_once('=') else {
            anyhow::bail!("--own-files takes PROJECT=FILE, not {a:?}");
        };
        let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
        let set: std::collections::HashSet<String> = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect();
        out.insert(project.to_string(), set);
    }
    Ok(out)
}

/// A duration flag; "0" or empty is none.
fn dur(s: &str, name: &str) -> anyhow::Result<Option<std::time::Duration>> {
    if s.is_empty() || s == "0" {
        return Ok(None);
    }
    let d = super::flags::parse_duration(s).map_err(|e| anyhow::anyhow!("--{name}: {e}"))?;
    Ok((!d.is_zero()).then_some(d))
}

/// `summarize --dirs`: the bottom-up pass over the directories holding
/// `files` (the selection after the filters), or its coverage.
fn run_dirs(
    ctx: &Ctx,
    a: &SummarizeArgs,
    pg: &agent_index::pg::Pg,
    o: &agent_index::pg::summ::Options,
    files: Vec<agent_index::summarize::Candidate>,
) -> anyhow::Result<()> {
    use agent_index::pg::dirsum;
    // The scope is the whole selection's, a sample's too: a sampled
    // parent whose children weren't sampled is written provisional.
    let scope = dirsum::scope(&files);
    let units = dirsum::sample(dirsum::units(pg, &files, o)?, o.limit);
    if units.is_empty() {
        anyhow::bail!(
            "no directories hold a selected file (check --project, --prefix and --skip-prefix)"
        );
    }
    if a.coverage {
        let cov = dirsum::coverage(pg, &units, &scope, o)?;
        if !a.out.is_empty() {
            let v = serde_json::json!({"coverage": cov, "units": units});
            std::fs::write(&a.out, serde_json::to_string_pretty(&v)?)
                .with_context(|| format!("writing {}", a.out))?;
        }
        if a.json {
            return ctx.print_json(&cov);
        }
        let depths: Vec<String> = cov
            .by_depth
            .iter()
            .rev()
            .map(|(d, n)| format!("{d}: {n}"))
            .collect();
        return ctx.write_out(&format!(
            "{} units ({} projects) over {} selected files; by depth (0: projects) {}.\n{} large enough to be cut into parts ({} parts); {} with no child summarized yet (seeds only); composed input {:.1}M characters.\n",
            cov.units,
            cov.projects,
            files.len(),
            depths.join(", "),
            cov.large,
            cov.parts,
            cov.seeds_only,
            cov.chars as f64 / 1e6
        ));
    }
    let cfg = &pg.cfg.llm.summaries;
    ctx.notice(format!(
        "Summarizing {} directories and projects, deepest first, with {} at {}, {} at a time.",
        units.len(),
        if cfg.model.is_empty() {
            "the server's model"
        } else {
            cfg.model.as_str()
        },
        cfg.endpoint,
        cfg.concurrency
    ));
    let res = dirsum::run(pg, units, &scope, o, &mut |label, s| {
        let failed: usize = s.failed.values().sum();
        ctx.notice(format!(
            "  {label}: {} written, {} failed, {} not answered of {} in {:.0}s: {:.2} units/s, {:.0} prefill tok/s",
            s.written, failed, s.not_answered, s.todo, s.wall_s, s.files_per_s, s.prefill_tok_s
        ));
    });
    if !a.no_sync {
        let ix = open_cfg(ctx, &pg.cfg)?;
        sync_pg(ctx, &ix, pg, false)?;
    }
    let runs = res?;
    if !a.out.is_empty() {
        let v: Vec<_> = runs
            .iter()
            .map(|(l, s)| serde_json::json!({"wave": l, "stats": s}))
            .collect();
        std::fs::write(&a.out, serde_json::to_string_pretty(&v)?)
            .with_context(|| format!("writing {}", a.out))?;
    }
    if a.json {
        let v: Vec<_> = runs
            .iter()
            .map(|(l, s)| serde_json::json!({"wave": l, "stats": s}))
            .collect();
        return ctx.print_json(&v);
    }
    let mut out = String::new();
    let (mut w, mut f, mut d) = (0, 0, 0);
    for (l, s) in &runs {
        let failed: usize = s.failed.values().sum();
        w += s.written;
        f += failed;
        d += s.done_before;
        out.push_str(&format!(
            "  {l}: run {}, {} selected, {} done before, {} written, {} failed, {:.0}s\n",
            s.run_id, s.selected, s.done_before, s.written, failed, s.wall_s
        ));
    }
    ctx.write_out(&format!(
        "Bottom-up pass: {} runs, {w} written, {f} failed, {d} done before.\n{out}",
        runs.len()
    ))
}

fn run_summarize(ctx: &Ctx, a: &SummarizeArgs) -> anyhow::Result<()> {
    let Some(pg) = pg_store("pg", &a.config)? else {
        unreachable!("pg asked")
    };
    let o = agent_index::pg::summ::Options {
        projects: a.project.clone(),
        corpora: a.corpus.clone(),
        prefixes: a.prefix.clone(),
        limit: int_flag(a.limit, "limit")?,
        retry_failed: a.retry_failed,
        code: agent_base::buildinfo::id(),
        stats_path: (!a.stats.is_empty()).then(|| PathBuf::from(&a.stats)),
        skip_projects: a.skip_project.clone(),
        skip_prefixes: a.skip_prefix.clone(),
        dirs: false,
        own_files: own_files(&a.own_files)?,
        file_timeout: dur(&a.file_timeout, "file-timeout")?,
        stall: dur(&a.stall, "stall")?,
        max_wall: dur(&a.max_time, "max-time")?,
        error_window: int_flag(a.error_window, "error-window")?,
        max_error_rate: a
            .max_error_rate
            .parse::<f64>()
            .ok()
            .filter(|r| (0.0..=1.0).contains(r))
            .context("--max-error-rate takes a number from 0 to 1")?,
    };
    let cfg = pg.cfg.llm.summaries.clone();
    let cands = agent_index::pg::summ::candidates(&pg, &o)?;
    if cands.is_empty() {
        anyhow::bail!(
            "no files of a full-depth project match (check --project and --prefix, and that a crawl and sync ran)"
        );
    }
    let (files, mut cov) = agent_index::pg::summ::select_with(&cands, &cfg, &o);
    if a.dirs {
        return run_dirs(ctx, a, &pg, &o, files);
    }
    let files = agent_index::pg::summ::sample(files, &o);
    cov.selected = files.len();
    (cov.bottom_up_dirs, cov.bottom_up_projects) = agent_index::pg::summ::bottom_up(&files);
    if a.coverage {
        if !a.out.is_empty() {
            let mut cov = cov.clone();
            cov.kept = files.iter().map(|c| c.id.clone()).collect();
            std::fs::write(&a.out, serde_json::to_string_pretty(&cov)?)
                .with_context(|| format!("writing {}", a.out))?;
        }
        if a.json {
            return ctx.print_json(&cov);
        }
        return ctx.write_out(&coverage_text(&cov));
    }
    ctx.notice(format!(
        "Summarizing {} files ({} skipped by the filters) with {} at {}, {} at a time.",
        files.len(),
        cov.skipped,
        if cfg.model.is_empty() {
            "the server's model"
        } else {
            cfg.model.as_str()
        },
        cfg.endpoint,
        cfg.concurrency
    ));
    let res = agent_index::pg::summ::run(&pg, files, &o, &cov, &mut |s| {
        let failed: usize = s.failed.values().sum();
        ctx.notice(format!(
            "  {} written, {} failed, {} not answered of {} in {:.0}s: {:.2} files/s, {:.0} prefill tok/s, ETA {:.0} min",
            s.written, failed, s.not_answered, s.todo, s.wall_s, s.files_per_s, s.prefill_tok_s, s.eta_s / 60.0
        ));
    });
    // What was written shows after a sync, whether or not the run finished.
    if !a.no_sync {
        let ix = open_cfg(ctx, &pg.cfg)?;
        sync_pg(ctx, &ix, &pg, false)?;
    }
    let stats = res?;
    if !a.out.is_empty() {
        std::fs::write(&a.out, serde_json::to_string_pretty(&stats)?)
            .with_context(|| format!("writing {}", a.out))?;
    }
    if a.json {
        return ctx.print_json(&stats);
    }
    let failed: Vec<String> = stats
        .failed
        .iter()
        .map(|(r, n)| format!("{r} {n}"))
        .collect();
    ctx.write_out(&format!(
        "Run {} (fingerprint {}, {}): {} selected, {} done before, {} copied, {} written, {} failed{}; {} clipped, {} salvaged, {} second attempts, {} asks stripped, {} repeated asks dropped; {:.0}s, {:.2} files/s, {:.0} prefill tok/s.\n",
        stats.run_id,
        stats.fingerprint,
        stats.writer,
        stats.selected,
        stats.done_before,
        stats.copied,
        stats.written,
        stats.failed.values().sum::<usize>(),
        if failed.is_empty() { String::new() } else { format!(" ({})", failed.join(", ")) },
        stats.clipped,
        stats.salvaged,
        stats.second_attempts,
        stats.asks_stripped,
        stats.asks_repeated_dropped,
        stats.wall_s,
        stats.files_per_s,
        stats.prefill_tok_s
    ))
}

fn secs(ms: u128) -> String {
    format!("{:.2}s", ms as f64 / 1000.0)
}

fn run_crawl(ctx: &Ctx, a: &CrawlArgs) -> anyhow::Result<()> {
    let mut ix = open_with(ctx, &a.config)?;
    let opts = CrawlOptions {
        project: (!a.project.is_empty()).then(|| a.project.clone()),
        rebuild: a.rebuild,
        dry_run: a.dry_run,
        large_project: 0,
    };
    let pg = pg_store(&a.store, &a.config)?;
    let r = agent_index::crawl(&mut ix, &opts)?;
    let synced = match &pg {
        Some(pg) if !a.dry_run => Some(sync_pg(ctx, &ix, pg, a.rebuild)?),
        _ => None,
    };
    if a.json {
        let mut v = serde_json::to_value(&r)?;
        if let (Some(s), Some(m)) = (&synced, v.as_object_mut()) {
            m.insert("sync".into(), serde_json::to_value(s)?);
        }
        return ctx.print_json(&v);
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
        if r.dry_run {
            " (dry run: nothing written)"
        } else {
            ""
        }
    );
    for p in &r.projects {
        let git = if p.git { "git" } else { "not git" };
        let mut changes = format!(
            "{} added, {} changed, {} removed, {} renamed",
            p.added, p.changed, p.removed, p.renamed
        );
        if p.commits_since > 0 {
            changes = format!(
                "{changes}; {} commits since the last crawl",
                p.commits_since
            );
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
            notes.push(format!(
                "{} secret-looking files never indexed",
                p.secret_skipped
            ));
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
            notes.push(format!(
                "{} files without content read (large, binary or symlinks)",
                p.no_content
            ));
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

A question about two things (how project A and project B each do
something) can come in parts, on the pg store: the words cut by a lone |
("layout engine cache | build script cache"), or --query for each further
part. Each part is searched on its own over the whole tree, each gets
places among what the reranker reads, and each result is reranked against
the part that found it.

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
    #[arg(
        long = "kind",
        value_name = "string",
        default_value = "",
        help = "Only this kind: project, dir, symbol, section; or file, or a file kind (source, test, fixture, doc, config, build, data, asset, submodule), one row per file, a symbol or section counting for its file"
    )]
    pub kind: String,
    #[arg(
        long = "project",
        value_name = "string",
        default_value = "",
        help = "Only this project"
    )]
    pub project: String,
    #[arg(
        long = "under",
        value_name = "string",
        default_value = "",
        help = "Only entities at or under this path"
    )]
    pub under: String,
    #[arg(long = "limit", value_name = "int", default_value = "0", value_parser = flags::parse_int, help = "At most this many results (default 10; the pg store's search.limit)")]
    pub limit: i64,
    #[arg(long = "budget", value_name = "int", default_value = "1500", value_parser = flags::parse_int, help = "Fit the text to this many tokens (0: no limit)")]
    pub budget: i64,
    #[arg(
        long = "explain",
        help = "Say which list found each result, and the priors applied"
    )]
    pub explain: bool,
    #[arg(
        long = "store",
        value_name = "string",
        default_value = "",
        help = "sqlite or pg (default: AGENT_INDEX_STORE, else [store].kind in the index configuration, else sqlite)"
    )]
    pub store: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(
        long = "lexical",
        value_name = "string",
        default_value = "",
        help = "pg store: bm25 (pg_search), fts (native full text), both, or auto (default: AGENT_INDEX_LEXICAL, else [search].lexical, else auto: bm25 where the schema has its indexes)"
    )]
    pub lexical: String,
    #[arg(
        long = "corpus",
        value_name = "string",
        default_value = "",
        help = "pg store: only these corpora, comma-separated: code, prose, memory, friction (default: all of them, grouped; code and prose for a --kind)"
    )]
    pub corpus: String,
    #[arg(
        long = "rerank",
        help = "pg store: rerank the top candidates with the cross-encoder ([reranker] in the configuration), even when it is off there"
    )]
    pub rerank: bool,
    #[arg(
        long = "cold",
        help = "pg store: search in this process even when agent index serve is up"
    )]
    pub cold: bool,
    #[arg(
        long = "query",
        value_name = "string",
        help = "pg store: another part of the question, searched on its own and pooled with the others (repeatable; or cut the words with a lone |)"
    )]
    pub queries: Vec<String>,
    #[arg(long = "json", help = "Print the results as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn search_def() -> Def {
    leaf!(SEARCH, SearchArgs, run_search)
}

/// A query embedder: text to a unit vector, shareable between threads.
type QueryEmbedder = agent_index::pg::serve::Embed;

/// The query embedder for the pg store: the model loads on the first query
/// that needs it (only when the store has vectors). None without the
/// embedder or its model.
#[cfg(feature = "embed")]
fn query_embedder(ctx: &Ctx, cfg: &agent_index::config::Config) -> Option<QueryEmbedder> {
    use agent_index::embed;
    if let Err(e) = check_embedder(cfg) {
        ctx.hint(format!("searching without the semantic list: {e:#}"));
        return None;
    }
    let given =
        (!cfg.embedder.model_dir.is_empty()).then(|| PathBuf::from(&cfg.embedder.model_dir));
    let dir = match embed::model_dir(given.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            ctx.hint(format!(
                "searching without the semantic list: the embedding model isn't here ({e:#})"
            ));
            return None;
        }
    };
    let cell: std::sync::OnceLock<Result<embed::Embedder, String>> = std::sync::OnceLock::new();
    Some(std::sync::Arc::new(move |q: &str| {
        let emb = cell
            .get_or_init(|| embed::Embedder::load(&dir).map_err(|e| format!("{e:#}")))
            .as_ref()
            .map_err(|e| anyhow::anyhow!("loading the embedding model: {e}"))?;
        Ok(emb.embed(&[q.to_string()], true)?.remove(0))
    }))
}

#[cfg(not(feature = "embed"))]
fn query_embedder(_: &Ctx, _: &agent_index::config::Config) -> Option<QueryEmbedder> {
    None
}

/// This build embeds with bge-small-en-v1.5 only: another configured model
/// is refused rather than silently replaced.
#[cfg_attr(not(feature = "embed"), allow(dead_code))]
fn check_embedder(cfg: &agent_index::config::Config) -> anyhow::Result<()> {
    let want = "bge-small-en-v1.5";
    if cfg.embedder.model != want {
        anyhow::bail!(
            "embedder.model is {:?}, but this agent embeds with {want} only (the vectors in the store are its)",
            cfg.embedder.model
        );
    }
    Ok(())
}

/// The current directory, canonical, as a string.
fn cwd_string() -> String {
    let Ok(cwd) = cwd() else { return String::new() };
    std::fs::canonicalize(&cwd)
        .unwrap_or(cwd)
        .to_string_lossy()
        .into_owned()
}

/// Logs one read command to the query log ([log]; agent_index::qlog):
/// how long it took, its top paths or its error. Never fails.
fn log_call(
    cfg: &agent_index::config::Config,
    mut e: agent_index::qlog::Entry,
    t0: std::time::Instant,
    pg: bool,
    res: &anyhow::Result<Vec<String>>,
) {
    let Some(path) = cfg.log_path() else {
        return;
    };
    e.ms = (t0.elapsed().as_secs_f64() * 1e4).round() / 10.0;
    e.fingerprint = cfg.fingerprint();
    e.store = if pg { "pg" } else { "sqlite" }.to_string();
    match res {
        Ok(top) => e.top = top.iter().take(5).cloned().collect(),
        Err(err) => e.error = Some(format!("{err:#}")),
    }
    agent_index::qlog::append(&path, &e);
}

/// The first paths of a search's answer (its flat list).
fn top_paths(r: &agent_index::SearchResult) -> Vec<String> {
    r.hits
        .iter()
        .take(5)
        .map(|h| {
            if h.line > 0 {
                format!("{}:{}", h.path, h.line)
            } else {
                h.path.clone()
            }
        })
        .collect()
}

/// The flags every read command shares, for the log.
fn log_common(e: &mut agent_index::qlog::Entry, store: &str, config: &str, json: bool) {
    e.flag("store", store, store.is_empty());
    e.flag("config", config, config.is_empty());
    e.flag("json", json, !json);
}

/// The warm process's socket: [serve].socket, else index.sock in the data
/// directory.
fn serve_socket(ctx: &Ctx, cfg: &agent_index::config::Config) -> PathBuf {
    if cfg.serve.socket.is_empty() {
        agent_index::pg::serve::default_socket(&agent_index::path_beside(&ctx.cfg().database_path))
    } else {
        PathBuf::from(&cfg.serve.socket)
    }
}

fn run_search(ctx: &Ctx, a: &SearchArgs) -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    let cfg = load_config(&a.config)?.config;
    let pg = cfg.wants_pg(&a.store)?;
    let res = if pg {
        run_search_pg(ctx, a)
    } else {
        run_search_sqlite(ctx, a)
    };
    let mut e = agent_index::qlog::Entry::new("search", &a.args.join(" "));
    e.flag("kind", a.kind.as_str(), a.kind.is_empty());
    e.flag("project", a.project.as_str(), a.project.is_empty());
    e.flag("under", a.under.as_str(), a.under.is_empty());
    e.flag("limit", a.limit, a.limit == 0);
    e.flag("budget", a.budget, a.budget == 1500);
    e.flag("explain", a.explain, !a.explain);
    e.flag("lexical", a.lexical.as_str(), a.lexical.is_empty());
    e.flag("corpus", a.corpus.as_str(), a.corpus.is_empty());
    e.flag("rerank", a.rerank, !a.rerank);
    e.flag("cold", a.cold, !a.cold);
    log_common(&mut e, &a.store, &a.config, a.json);
    log_call(&cfg, e, t0, pg, &res);
    res.map(|_| ())
}

/// Search on the sqlite store; the answer's top paths.
fn run_search_sqlite(ctx: &Ctx, a: &SearchArgs) -> anyhow::Result<Vec<String>> {
    let ix = open_with(ctx, &a.config)?;
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
        corpora: Vec::new(),
        // The sqlite store searches the parts as one query.
        queries: Vec::new(),
    };
    let o = SearchOptions {
        query: agent_index::pg::serve::split_query(&o.query, &a.queries).0,
        ..o
    };
    let r = match semantic(ctx, &ix) {
        Some(sem) => agent_index::search(&ix.conn, &o, sem.as_ref())?,
        None => agent_index::search(&ix.conn, &o, &NoSemantic)?,
    };
    let top = top_paths(&r);
    if a.json {
        ctx.print_json(&r)?;
        return Ok(top);
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
    let project = if a.project.is_empty() {
        &caller
    } else {
        &a.project
    };
    behind_hint(ctx, &ix, project);
    Ok(top)
}

/// Search on the pg store: the same options, output and `--json` shape as
/// the sqlite store's, every list read from Postgres. The warm process
/// answers when it is up (and runs this build under this configuration);
/// otherwise the search runs cold, through the same code. The answer's top
/// paths.
fn run_search_pg(ctx: &Ctx, a: &SearchArgs) -> anyhow::Result<Vec<String>> {
    use agent_index::pg::serve;
    let loaded = load_config(&a.config)?;
    let lexical = if a.lexical.is_empty() {
        std::env::var("AGENT_INDEX_LEXICAL").unwrap_or_default()
    } else {
        a.lexical.clone()
    };
    let under = if a.under.is_empty() {
        String::new()
    } else {
        abs_dir(&a.under)?.to_string_lossy().into_owned()
    };
    let req = serve::Request {
        query: a.args.join(" "),
        queries: a.queries.clone(),
        kind: a.kind.clone(),
        project: a.project.clone(),
        under,
        limit: int_flag(a.limit, "limit")?,
        cwd: cwd_string(),
        lexical,
        corpus: a.corpus.clone(),
        rerank: a.rerank,
        who: std::env::var("AGENT_NAME").unwrap_or_default(),
        fingerprint: loaded.config.fingerprint(),
        build: serve::build_id(),
    };
    let socket = serve_socket(ctx, &loaded.config);
    let resp = match (!a.cold && socket.exists())
        .then(|| serve::ask(&socket, &req, std::time::Duration::from_secs(120)))
    {
        Some(Ok(r)) => r,
        other => {
            if let Some(Err(e)) = other {
                ctx.hint(format!(
                    "searched cold: the index server didn't answer ({e:#})"
                ));
            }
            let (url, schema) = pg_target(&loaded.config)?;
            let pg = Pg::connect_with(&url, &schema, loaded.config.clone())?;
            if agent_index::pg::view::project_count(&pg)? == 0 {
                anyhow::bail!(
                    "nothing is in the pg store yet: agent index root add DIR, then agent index crawl --store pg"
                );
            }
            let emb = query_embedder(ctx, &pg.cfg);
            serve::answer(&pg, &req, emb.as_ref(), None)?
        }
    };
    let Some(r) = resp.result else {
        anyhow::bail!("the search gave no result");
    };
    let top = top_paths(&r);
    if a.json {
        ctx.print_json(&r)?;
        return Ok(top);
    }
    let budget = int_flag(a.budget, "budget")?;
    if let Some(groups) = &r.groups {
        ctx.write_out(&render_groups(&r, groups, budget, a.explain))?;
        if let Some(h) = resp.hint {
            ctx.hint(h);
        }
        return Ok(top);
    }
    let (text, left) = view::render_search(&r, budget, a.explain);
    let mut out = text;
    if let Some(ri) = &r.rerank {
        match &ri.error {
            Some(e) => {
                let _ = writeln!(
                    out,
                    "The reranker didn't answer ({e}): this is the fused order."
                );
            }
            None if a.explain => {
                let _ = writeln!(
                    out,
                    "Reranked {} candidates with {} in {:.0} ms.",
                    ri.scored, ri.model, ri.ms
                );
            }
            None => {}
        }
    }
    if left > 0 {
        let _ = writeln!(
            out,
            "{left} more left out to fit --budget {}: a larger --budget, or --json, shows them.",
            a.budget
        );
    } else if r.hits.len() == resp.limit && r.candidates > resp.limit {
        let _ = writeln!(
            out,
            "{} more matched: --limit N shows more.",
            r.candidates - resp.limit
        );
    }
    ctx.write_out(&out)?;
    if let Some(h) = resp.hint {
        ctx.hint(h);
    }
    Ok(top)
}

/// A grouped answer as text: each corpus under its name, fitted to its
/// share of `--budget` (the shares scale with it; 0 is no limit), the
/// labels and absent terms once at the top, and one line for a corpus with
/// nothing.
fn render_groups(
    r: &agent_index::SearchResult,
    groups: &[agent_index::query::Group],
    budget: usize,
    explain: bool,
) -> String {
    let mut out = String::new();
    let total: usize = groups.iter().map(|g| g.budget).sum::<usize>().max(1);
    for (i, g) in groups.iter().enumerate() {
        let name = {
            let mut c = g.corpus.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        };
        if g.hits.is_empty() {
            let _ = writeln!(
                out,
                "{name}: {}.",
                g.note.as_deref().unwrap_or("nothing good here")
            );
            continue;
        }
        let _ = writeln!(out, "{name} ({} of {}):", g.hits.len(), g.candidates);
        let sub = agent_index::SearchResult {
            hits: g.hits.clone(),
            // Labels and absent terms once, above the first group.
            mapping: if i == 0 {
                r.mapping.clone()
            } else {
                Vec::new()
            },
            absent: if i == 0 { r.absent.clone() } else { Vec::new() },
            groups: None,
            ..r.clone()
        };
        let share = if budget == 0 {
            0
        } else {
            (budget * g.budget / total).max(1)
        };
        let (text, left) = view::render_search(&sub, share, explain);
        out.push_str(&text);
        if left > 0 {
            let _ = writeln!(
                out,
                "   ({left} more {} left out to fit the budget.)",
                g.corpus
            );
        }
    }
    if let Some(ri) = &r.rerank
        && let Some(e) = &ri.error
    {
        let _ = writeln!(
            out,
            "The reranker didn't answer ({e}): this is the fused order."
        );
    }
    out
}

// `agent index serve`

pub const SERVE: Doc = Doc {
    use_line: "serve",
    short: "Keep the pg store's search warm: a process on a Unix socket",
    long: r#"Serve answers agent index search for the pg store from a warm process:
it holds [serve].workers database connections, the query embedder (its
model loaded once) and the reranker's client, on a Unix socket
([serve].socket, else index.sock beside index.db). A search uses it when
it answers and runs cold otherwise, through the same code, so the answer
is the same; it runs cold too when the server is another build of agent
or runs another configuration (its fingerprint), and says why. search
--cold skips it. Serve runs until it is stopped (Ctrl-C); run it in a
session of its own."#,
    example: r#"  agent index serve
  agent index serve --config tune/rerank50.toml"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct ServeArgs {
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn serve_def() -> Def {
    leaf!(SERVE, ServeArgs, run_serve)
}

fn run_serve(ctx: &Ctx, a: &ServeArgs) -> anyhow::Result<()> {
    use agent_index::pg::serve;
    let loaded = load_config(&a.config)?;
    let (url, schema) = pg_target(&loaded.config)?;
    let socket = serve_socket(ctx, &loaded.config);
    // The embedder loaded now, not at the first search.
    let embed = query_embedder(ctx, &loaded.config);
    if let Some(e) = &embed
        && let Err(err) = e("warm up")
    {
        ctx.hint(format!("serving without the semantic list: {err:#}"));
    }
    let workers = loaded.config.serve.workers;
    ctx.notice(format!(
        "Serving the index (schema {schema}, configuration {}) on {} with {workers} workers; Ctrl-C stops it.",
        loaded.config.fingerprint(),
        socket.display()
    ));
    let srv = serve::Server {
        url,
        schema,
        config: loaded.config,
        embed,
    };
    serve::serve(srv, &socket, workers)
}

/// Says when the caller's project moved since its last crawl.
fn behind_hint(ctx: &Ctx, ix: &Index, project: &str) {
    if project.is_empty() {
        return;
    }
    let Ok(Some(p)) = agent_index::roots::get_project(&ix.conn, project) else {
        return;
    };
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
    #[arg(
        long = "store",
        value_name = "string",
        default_value = "",
        help = "sqlite or pg (default: AGENT_INDEX_STORE, else [store].kind in the index configuration, else sqlite)"
    )]
    pub store: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print the entity and its neighborhood as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn show_def() -> Def {
    leaf!(SHOW, ShowArgs, run_show)
}

/// The pg store with something in it, or None for the sqlite store.
fn pg_read(store: &str, config: &str) -> anyhow::Result<Option<Pg>> {
    let Some(pg) = pg_store(store, config)? else {
        return Ok(None);
    };
    if agent_index::pg::view::project_count(&pg)? == 0 {
        anyhow::bail!(
            "nothing is in the pg store yet: agent index root add DIR, then agent index crawl --store pg"
        );
    }
    Ok(Some(pg))
}

fn run_show(ctx: &Ctx, a: &ShowArgs) -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    let cfg = load_config(&a.config)?.config;
    let arg = a.args.first().map(String::as_str).unwrap_or(".");
    let res = show(ctx, a, arg);
    let mut e = agent_index::qlog::Entry::new("show", arg);
    log_common(&mut e, &a.store, &a.config, a.json);
    log_call(&cfg, e, t0, cfg.wants_pg(&a.store)?, &res);
    res.map(|_| ())
}

/// Show; the entity's path.
fn show(ctx: &Ctx, a: &ShowArgs, arg: &str) -> anyhow::Result<Vec<String>> {
    let s = if let Some(pg) = pg_read(&a.store, &a.config)? {
        agent_index::pg::view::show(&pg, agent_index::pg::view::resolve(&pg, arg, &cwd()?)?)?
    } else {
        let ix = open_with(ctx, &a.config)?;
        require_projects(&ix)?;
        view::show(&ix.conn, resolve(&ix, arg)?)?
    };
    if a.json {
        ctx.print_json(&s)?;
    } else {
        ctx.write_out(&view::render_show(&s))?;
    }
    Ok(vec![s.entity.path.clone()])
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
    #[arg(
        long = "store",
        value_name = "string",
        default_value = "",
        help = "sqlite or pg (default: AGENT_INDEX_STORE, else [store].kind in the index configuration, else sqlite)"
    )]
    pub store: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print the tree as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn tree_def() -> Def {
    leaf!(TREE, TreeArgs, run_tree)
}

fn run_tree(ctx: &Ctx, a: &TreeArgs) -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    let cfg = load_config(&a.config)?.config;
    let res = tree(ctx, a);
    let mut e =
        agent_index::qlog::Entry::new("tree", a.args.first().map(String::as_str).unwrap_or(""));
    e.flag("depth", a.depth, a.depth == 2);
    e.flag("budget", a.budget, a.budget == 1500);
    log_common(&mut e, &a.store, &a.config, a.json);
    log_call(&cfg, e, t0, cfg.wants_pg(&a.store)?, &res);
    res.map(|_| ())
}

/// Tree; the path it maps.
fn tree(ctx: &Ctx, a: &TreeArgs) -> anyhow::Result<Vec<String>> {
    let (depth, budget) = (int_flag(a.depth, "depth")?, int_flag(a.budget, "budget")?);
    let (t, head) = if let Some(pg) = pg_read(&a.store, &a.config)? {
        use agent_index::pg::view as pgview;
        let id = match a.args.first() {
            Some(arg) => pgview::resolve(&pg, arg, &cwd()?)?.id,
            None => pgview::project_at(&pg, &cwd_string())?,
        };
        let t = pgview::tree(&pg, &id, depth, budget)?;
        let head = if id.is_empty() {
            None
        } else {
            pgview::get(&pg, &id)?
        };
        (t, head)
    } else {
        let ix = open_with(ctx, &a.config)?;
        require_projects(&ix)?;
        let id = match a.args.first() {
            Some(arg) => resolve(&ix, arg)?.id,
            None => caller_project(&ix),
        };
        let t = view::tree(&ix.conn, &id, depth, budget)?;
        let head = match agent_index::model::get(&ix.conn, &id)? {
            Some(e) if !id.is_empty() => Some((e, agent_index::model::summary(&ix.conn, &id)?)),
            _ => None,
        };
        (t, head)
    };
    let top: Vec<String> = head.iter().map(|(e, _)| e.path.clone()).collect();
    if a.json {
        ctx.print_json(&t)?;
        return Ok(top);
    }
    let mut o = String::new();
    if let Some((e, s)) = head {
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
    ctx.write_out(&o)?;
    Ok(top)
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
    #[arg(
        long = "store",
        value_name = "string",
        default_value = "",
        help = "sqlite or pg (default: AGENT_INDEX_STORE, else [store].kind in the index configuration, else sqlite)"
    )]
    pub store: String,
    #[arg(
        long = "config",
        value_name = "string",
        default_value = "",
        help = "The index configuration (TOML; default: AGENT_INDEX_CONFIG, else ~/.config/gomuxai/index.toml, else built-in defaults)"
    )]
    pub config: String,
    #[arg(long = "json", help = "Print the neighbors as JSON")]
    pub json: bool,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

fn related_def() -> Def {
    leaf!(RELATED, RelatedArgs, run_related)
}

fn run_related(ctx: &Ctx, a: &RelatedArgs) -> anyhow::Result<()> {
    let t0 = std::time::Instant::now();
    let cfg = load_config(&a.config)?.config;
    let res = related(ctx, a);
    let mut e = agent_index::qlog::Entry::new("related", &a.args[0]);
    e.flag("budget", a.budget, a.budget == 1500);
    log_common(&mut e, &a.store, &a.config, a.json);
    log_call(&cfg, e, t0, cfg.wants_pg(&a.store)?, &res);
    res.map(|_| ())
}

/// Related; its neighbors' ids, heaviest edges first.
fn related(ctx: &Ctx, a: &RelatedArgs) -> anyhow::Result<Vec<String>> {
    let r = if let Some(pg) = pg_read(&a.store, &a.config)? {
        let res = agent_index::pg::view::resolve(&pg, &a.args[0], &cwd()?)?;
        agent_index::pg::view::related(&pg, &res.id)?
    } else {
        let ix = open_with(ctx, &a.config)?;
        require_projects(&ix)?;
        let res = resolve(&ix, &a.args[0])?;
        view::related(&ix.conn, &res.id)?
    };
    let mut near: Vec<(f64, String)> = r
        .groups
        .values()
        .flatten()
        .map(|(edge, b)| (edge.weight, b.id.clone()))
        .collect();
    near.sort_by(|x, y| y.0.total_cmp(&x.0));
    let top: Vec<String> = near.into_iter().map(|(_, p)| p).take(5).collect();
    if a.json {
        ctx.print_json(&r)?;
        return Ok(top);
    }
    let (text, left) = view::render_related(&r, int_flag(a.budget, "budget")?);
    let mut o = format!("Related to {}:\n", r.id);
    if r.groups.is_empty() {
        o.push_str(
            "No edges yet: nothing depends on it, documents it, tests it or changes with it.\n",
        );
    }
    o.push_str(&text);
    if left > 0 {
        let _ = writeln!(o, "{left} more left out to fit --budget {}.", a.budget);
    }
    ctx.write_out(&o)?;
    Ok(top)
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
    #[arg(
        long = "project",
        value_name = "string",
        default_value = "",
        help = "Only this project"
    )]
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
    let scope = if a.project.is_empty() {
        "the index".to_string()
    } else {
        a.project.clone()
    };
    let _ = writeln!(
        o,
        "Freshness in {scope} (projects, dirs and files): {}.",
        counts.join(", ")
    );
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
    resync(ctx, &ix)?;
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
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd()?.join(p)
        }
    };
    let r = agent_index::labels::lexicon_import(&ix.conn, &file)?;
    resync(ctx, &ix)?;
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
            terms
                .into_iter()
                .map(|t| format!("{t:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if r.unreadable > 0 {
        let _ = writeln!(
            o,
            "Skipped {} lines that aren't JSON objects.",
            r.unreadable
        );
    }
    ctx.write_out(&o)
}

#[cfg(test)]
#[path = "index_test.rs"]
mod index_test;
