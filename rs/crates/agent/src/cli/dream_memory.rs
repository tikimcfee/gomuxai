//! `agent dream memory`: a dream's reads and guarded writes of a memory dir
//! (docs/DREAMING.md; the rules are in agent_dream::memory). New in the Rust
//! build; wired under `agent dream` by its group's def().

use std::io::Read as _;
use std::path::{Path, PathBuf};

use agent_base::gostd::filepath;
use agent_dream::memory::{self, PutOutcome};
use agent_dream::stale::{AgentCommand, AgentCommands, Repos};
use anyhow::Context as _;

use agent_store::Dream;

use super::command::{ArgRule, Def, Doc, Run};
use super::dream::{own_open_dream, record_memory};
use super::root::Ctx;

// `agent dream memory`

pub const DOC: Doc = Doc {
    use_line: "memory",
    short: "Read, write and check a memory directory, under a dream's guard",
    long: r#"Memory reads and writes a memory directory: Claude Code's per-project
memory/, one fact per .md file, with MEMORY.md as its index. --dir names
the directory; without it, the memory directory of the project this
directory belongs to.

get prints a file's version, then its content. put and remove take the
version you read (--expect; "new" for a file that doesn't exist yet) and
refuse if the file changed since. put refuses a file without valid
frontmatter (name, description, metadata.type), a new or changed fact with
no [source: ...; added: YYYY-MM-DD] line (a change that only points
[[links]] at memories that exist needs none), a rewrite that loses a source
citation the file had, and a MEMORY.md that drops more than a quarter of
its lines. remove refuses a file whose citations no other memory carries.
Both save the prior file under the dream's directory first, so they run
only inside an open dream.

check reports, changing nothing: exact and near duplicate entries, [[links]]
naming no memory, index lines naming no file, and files the index doesn't
name; with --repo, also paths, make targets and agent commands those repos
no longer have."#,
    example: r#"  agent dream memory get feedback_tests.md
  agent dream memory put feedback_tests.md --expect 3f9a0c1d2e4b5a69 --from draft.md
  agent dream memory put MEMORY.md --expect new --from index.md
  agent dream memory remove old_fact.md --expect 0a1b2c3d4e5f6071
  agent dream memory check --json"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(&DOC, vec![get_def(), put_def(), remove_def(), check_def()])
}

/// --dir, shared by every subcommand.
#[derive(clap::Args, Debug, Clone)]
pub struct DirArg {
    #[arg(
        long = "dir",
        value_name = "string",
        default_value = "",
        help = "The memory directory (default: this project's)"
    )]
    pub dir: String,
}

// `agent dream memory get`

pub const GET: Doc = Doc {
    use_line: "get <file>",
    short: "Print a memory file's version, then its content",
    long: r#"Get prints the file's version on the first line, then its content. A file
that doesn't exist prints the version "new" and nothing else: put it with
--expect new to create it."#,
    example: "  agent dream memory get feedback_tests.md\n  agent dream memory get MEMORY.md --dir ~/.claude/projects/-srv-app/memory",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct GetArgs {
    #[command(flatten)]
    pub dir: DirArg,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn get_def() -> Def {
    leaf!(GET, GetArgs, run_get)
}

pub fn run_get(ctx: &Ctx, args: &GetArgs) -> anyhow::Result<()> {
    let dir = memory_dir(ctx, &args.dir.dir)?;
    match memory::get(&dir, &args.args[0])? {
        Some((content, v)) => ctx.write_out(&format!("{v}\n{content}")),
        None => ctx.write_out(&format!("{}\n", memory::NEW)),
    }
}

// `agent dream memory put`

pub const PUT: Doc = Doc {
    use_line: "put <file> --expect VERSION --from PATH",
    short: "Write a memory file, if it hasn't changed since you read it",
    long: r#"Put writes the content of --from ("-" for stdin) to the file, if the file is
still at --expect (the version get printed; "new" to create one). It
refuses content without valid frontmatter, a new or changed fact with no
source line, a rewrite that loses a citation the file had, and a MEMORY.md
that drops more than a quarter of its lines. The prior file is saved under
the dream's directory first; the write is atomic. It prints the new
version."#,
    example: "  agent dream memory put feedback_tests.md --expect 3f9a0c1d2e4b5a69 --from draft.md",
    args: ArgRule::Exact(1),
    required: &["expect", "from"],
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct PutArgs {
    #[arg(
        long = "expect",
        value_name = "string",
        default_value = "",
        help = "The version you read (\"new\" for a new file)"
    )]
    pub expect: String,
    #[arg(
        long = "from",
        value_name = "string",
        default_value = "",
        help = "Read the content from a file (\"-\" for stdin)"
    )]
    pub from: String,
    #[command(flatten)]
    pub dir: DirArg,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn put_def() -> Def {
    leaf!(PUT, PutArgs, run_put)
}

pub fn run_put(ctx: &Ctx, args: &PutArgs) -> anyhow::Result<()> {
    let dir = memory_dir(ctx, &args.dir.dir)?;
    let file = &args.args[0];
    memory::check_file_name(file)?;
    let content = read_from(&args.from)?;
    let (d, snap) = snapshot_dir(ctx, &dir, "memory put")?;
    let outcome = memory::put(&dir, file, &content, &args.expect, &snap)?;
    record_memory(ctx, &d, &dir)?;
    match outcome {
        PutOutcome::Written(v) => ctx.write_out(&format!("Wrote {file} (version {v}).\n")),
        PutOutcome::Unchanged(v) => ctx.write_out(&format!(
            "{file} already holds that (version {v}); nothing written.\n"
        )),
    }
}

fn read_from(from: &str) -> anyhow::Result<String> {
    if from == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading stdin")?;
        return Ok(s);
    }
    std::fs::read_to_string(from).with_context(|| format!("reading {from}"))
}

// `agent dream memory remove`

pub const REMOVE: Doc = Doc {
    use_line: "remove <file> --expect VERSION",
    short: "Move a memory file out, into the dream's directory",
    long: r#"Remove moves the file into the dream's directory, if it is still at
--expect. It refuses MEMORY.md, and a file whose source citations no other
memory carries: merge them into the memory that keeps the fact first. Put
MEMORY.md again without its line afterwards."#,
    example: "  agent dream memory remove old_fact.md --expect 0a1b2c3d4e5f6071",
    args: ArgRule::Exact(1),
    required: &["expect"],
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct RemoveArgs {
    #[arg(
        long = "expect",
        value_name = "string",
        default_value = "",
        help = "The version you read"
    )]
    pub expect: String,
    #[command(flatten)]
    pub dir: DirArg,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn remove_def() -> Def {
    leaf!(REMOVE, RemoveArgs, run_remove)
}

pub fn run_remove(ctx: &Ctx, args: &RemoveArgs) -> anyhow::Result<()> {
    let dir = memory_dir(ctx, &args.dir.dir)?;
    let file = &args.args[0];
    memory::check_file_name(file)?;
    let (d, snap) = snapshot_dir(ctx, &dir, "memory remove")?;
    memory::remove(&dir, file, &args.expect, &snap)?;
    record_memory(ctx, &d, &dir)?;
    ctx.write_out(&format!(
        "Removed {file} (kept in {}).\n",
        snap.join("history").display()
    ))
}

// `agent dream memory check`

pub const CHECK: Doc = Doc {
    use_line: "check",
    short: "Report duplicates, broken links and index gaps in a memory directory",
    long: r#"Check reads the memory directory and reports, changing nothing: files whose
frontmatter or source lines don't parse, exact duplicates and near ones
(Dice similarity of 0.8 or more on the words of entries 5 words or longer;
titles and source lines aside), [[links]] naming no memory, MEMORY.md lines
naming no file, and files MEMORY.md doesn't name.

With --repo (one per repo the memory is about), it also reports what the
memory says that those repos no longer bear out, by file and line, from
backticked text outside code fences:

  stale-path   a path (it has a / or a file extension) that exists in no
               repo: relative to a repo's root, or a tail of a path in
               one; absolute only under a repo; bare names only for
               source files; ~ paths, URLs, placeholders (<name>, ..., *),
               paths a repo's git ignores, and paths no repo's top level
               starts (origin/main) are not checked
  stale-make   make <target> no repo's Makefile defines (unchecked when a
               target is named by a variable, or with -C/-f)
  stale-agent  agent <sub> this binary has no command for, and --flags
               the command doesn't take: in its backticks, or in their own
               on a line naming just that one command

What it can't be sure of it leaves alone. The report is sorted, so the same
directory always gives the same one."#,
    example: "  agent dream memory check\n  agent dream memory check --dir ~/.claude/projects/-srv-app/memory --json\n  agent dream memory check --repo /srv/app --repo /srv/app-tools",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

#[derive(clap::Args, Debug, Clone)]
pub struct CheckArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    #[command(flatten)]
    pub dir: DirArg,
    #[arg(
        long = "repo",
        value_name = "stringArray",
        action = clap::ArgAction::Append,
        help = "Also check paths, make targets and agent commands against this repo; repeatable"
    )]
    pub repo: Vec<String>,
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn check_def() -> Def {
    leaf!(CHECK, CheckArgs, run_check)
}

pub fn run_check(ctx: &Ctx, args: &CheckArgs) -> anyhow::Result<()> {
    let dir = memory_dir(ctx, &args.dir.dir)?;
    let repos = if args.repo.is_empty() {
        None
    } else {
        let home = std::env::var("HOME").unwrap_or_default();
        let mut roots = Vec::new();
        for r in &args.repo {
            let p = PathBuf::from(expand_home(r, &home));
            if !p.is_dir() {
                anyhow::bail!("--repo {r}: no such directory");
            }
            roots.push(p.canonicalize().unwrap_or(p));
        }
        Some(Repos {
            roots,
            agent: Some(agent_commands(super::root::tree())),
        })
    };
    let findings = memory::check_with(&dir, repos.as_ref())?;
    if args.json {
        return ctx.print_json(&findings);
    }
    if findings.is_empty() {
        return ctx.write_out(&format!("Nothing to report in {}.\n", dir.display()));
    }
    let mut out = String::new();
    for f in &findings {
        out.push_str(&f.to_string());
        out.push('\n');
    }
    ctx.write_out(&out)
}

/// The commands this binary knows, for the stale check: every command by its
/// path below `agent` (aliases as paths of their own), its subcommands, whether
/// a word after it must be one, and its long flags (global ones and --help
/// included).
pub(crate) fn agent_commands(root: &Def) -> AgentCommands {
    fn add(def: &Def, path: &str, is_root: bool, out: &mut AgentCommands) {
        let parser = def.clap_command();
        let flags = parser
            .get_arguments()
            .filter(|a| !a.is_positional())
            .filter_map(|a| a.get_long().map(str::to_string))
            .collect();
        let mut subs = std::collections::BTreeSet::new();
        for s in &def.subs {
            for name in std::iter::once(s.name()).chain(s.doc.aliases.iter().copied()) {
                subs.insert(name.to_string());
                let p = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path} {name}")
                };
                add(s, &p, false, out);
            }
        }
        let needs_sub = is_root || (!def.subs.is_empty() && !matches!(def.run, Run::Leaf(_)));
        out.commands.insert(
            path.to_string(),
            AgentCommand {
                subs,
                needs_sub,
                flags,
            },
        );
    }
    let mut out = AgentCommands::default();
    add(root, "", true, &mut out);
    out
}

// Where things are.

/// The memory dir to work on: `--dir` (a leading `~` is the home dir), else the
/// memory dir of the project the current directory belongs to.
fn memory_dir(ctx: &Ctx, flag: &str) -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let dir = if !flag.is_empty() {
        expand_home(flag, &home)
    } else {
        let cwd = std::env::current_dir().context("finding the current directory")?;
        let projects = claude_projects_dir(ctx, &home)?;
        filepath::join(&[&projects, &project_folder(&cwd.to_string_lossy()), "memory"])
    };
    let p = PathBuf::from(&dir);
    if !p.is_dir() {
        anyhow::bail!("no memory directory at {dir}; name one with --dir");
    }
    Ok(p)
}

/// Where Claude Code keeps its per-project folders: the app config's
/// claude_projects_dir, else `$CLAUDE_CONFIG_DIR/projects`, else
/// `~/.claude/projects`.
fn claude_projects_dir(ctx: &Ctx, home: &str) -> anyhow::Result<String> {
    if let Some(p) = &ctx.cfg().claude_projects_dir {
        if p.is_empty() {
            anyhow::bail!(
                "claude_projects_dir is turned off in the app config; name a memory directory with --dir"
            );
        }
        return Ok(expand_home(p, home));
    }
    let cfg_dir = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    if !cfg_dir.is_empty() {
        return Ok(filepath::join(&[&cfg_dir, "projects"]));
    }
    if home.is_empty() {
        anyhow::bail!(
            "no home directory to find Claude's projects in; name a memory directory with --dir"
        );
    }
    Ok(filepath::join(&[home, ".claude", "projects"]))
}

/// The folder Claude Code names a project by: its path with every character
/// other than a letter or digit made `-`.
pub(crate) fn project_folder(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn expand_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        return home.to_string();
    }
    match path.strip_prefix("~/") {
        Some(rest) => filepath::join(&[home, rest]),
        None => path.to_string(),
    }
}

/// The caller's open dream, and where put and remove save what they replace
/// in `dir`: `dream-snapshots/<id>/<project folder>/` under this host's data
/// dir, outside the dream's own folder (which its agent may edit, and so
/// could change its baseline), holding the dir as the dream found it
/// (taken now if the dream had no snapshot of it yet). Refuses a caller with
/// no open dream.
fn snapshot_dir(ctx: &Ctx, dir: &Path, verb: &str) -> anyhow::Result<(Dream, PathBuf)> {
    let d = own_open_dream(ctx, verb)?;
    record_memory(ctx, &d, dir)?;
    let snap = snapshot_dir_for(&ctx.data_dir(), &d.id.to_string(), dir);
    Ok((d, snap))
}

/// The snapshot dir of dream `id` for memory dir `dir`: one folder per memory dir,
/// named for the path it is at.
pub(crate) fn snapshot_dir_for(data_dir: &str, id: &str, dir: &Path) -> PathBuf {
    let abs = std::path::absolute(dir).unwrap_or_else(|_| dir.to_path_buf());
    PathBuf::from(data_dir)
        .join("dream-snapshots")
        .join(id)
        .join(project_folder(&abs.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::super::testenv::TestEnv;
    use super::*;

    const FACT: &str = "---\nname: alpha\ndescription: d\nmetadata:\n  type: user\n---\nRun the tests.\n[source: commit:abc123; added: 2026-11-01]\n";

    fn mem_dir(env: &TestEnv) -> PathBuf {
        let d = env.dir.path().join("memory");
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn get_prints_the_version_then_the_content() {
        let env = TestEnv::new();
        let d = mem_dir(&env);
        let dir = DirArg {
            dir: d.to_string_lossy().into_owned(),
        };
        let get = |f: &str| GetArgs {
            dir: dir.clone(),
            args: vec![f.into()],
        };
        run_get(&env.ctx, &get("a.md")).unwrap();
        assert_eq!(env.take_out(), "new\n");
        std::fs::write(d.join("a.md"), FACT).unwrap();
        run_get(&env.ctx, &get("a.md")).unwrap();
        assert_eq!(
            env.take_out(),
            format!("{}\n{FACT}", memory::version(FACT.as_bytes()))
        );
        assert!(run_get(&env.ctx, &get("../a.md")).is_err());
    }

    #[test]
    fn check_reports_and_prints_json() {
        let env = TestEnv::new();
        let d = mem_dir(&env);
        let check = |json| CheckArgs {
            json,
            dir: DirArg {
                dir: d.to_string_lossy().into_owned(),
            },
            repo: vec![],
            args: vec![],
        };
        run_check(&env.ctx, &check(false)).unwrap();
        assert!(env.take_out().starts_with("Nothing to report in "));
        std::fs::write(d.join("a.md"), FACT).unwrap();
        run_check(&env.ctx, &check(false)).unwrap();
        assert_eq!(env.take_out(), "a.md: not in MEMORY.md\n");
        run_check(&env.ctx, &check(true)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&env.take_out()).unwrap();
        assert_eq!(v[0]["kind"], "unindexed");
        let missing = CheckArgs {
            json: false,
            dir: DirArg {
                dir: d.join("nope").to_string_lossy().into_owned(),
            },
            repo: vec![],
            args: vec![],
        };
        assert!(
            run_check(&env.ctx, &missing)
                .unwrap_err()
                .to_string()
                .contains("no memory directory")
        );
    }

    #[test]
    fn put_and_remove_need_an_open_dream() {
        let env = TestEnv::new();
        let d = mem_dir(&env);
        let from = env.dir.path().join("draft.md");
        std::fs::write(&from, FACT).unwrap();
        let dir = DirArg {
            dir: d.to_string_lossy().into_owned(),
        };
        let e = run_put(
            &env.ctx,
            &PutArgs {
                expect: memory::NEW.into(),
                from: from.to_string_lossy().into_owned(),
                dir: dir.clone(),
                args: vec!["a.md".into()],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("agent dream memory put is for a dream's own agent"),
            "{e}"
        );
        let e = run_remove(
            &env.ctx,
            &RemoveArgs {
                expect: "x".into(),
                dir,
                args: vec!["a.md".into()],
            },
        )
        .unwrap_err()
        .to_string();
        assert!(
            e.contains("agent dream memory remove is for a dream's own agent"),
            "{e}"
        );
        assert!(!d.join("a.md").exists());
    }

    #[test]
    fn project_folder_matches_claude_code() {
        assert_eq!(project_folder("/srv/my_app.v2"), "-srv-my-app-v2");
    }

    #[test]
    fn snapshot_dir_is_per_memory_dir() {
        let p = snapshot_dir_for("/data", "d7", Path::new("/p/-srv-app/memory"));
        assert_eq!(
            p,
            PathBuf::from("/data/dream-snapshots/d7/-p--srv-app-memory")
        );
    }

    #[test]
    fn expand_home_handles_tilde() {
        assert_eq!(expand_home("~/x", "/h"), "/h/x");
        assert_eq!(expand_home("~", "/h"), "/h");
        assert_eq!(expand_home("/a/~", "/h"), "/a/~");
    }

    #[test]
    fn agent_commands_walk_the_real_tree() {
        let c = agent_commands(super::super::root::tree());
        let root = &c.commands[""];
        assert!(root.needs_sub);
        assert!(root.subs.contains("send") && root.subs.contains("dream"));
        let check = &c.commands["dream memory check"];
        assert!(check.flags.contains("repo") && check.flags.contains("dir"));
        assert!(check.flags.contains("help"));
        assert!(!check.needs_sub);
        assert!(c.commands["dream memory"].needs_sub);
        assert!(c.commands["send"].flags.contains("now"));
    }

    #[test]
    fn check_with_repo_reports_stale_mentions() {
        let env = TestEnv::new();
        let d = mem_dir(&env);
        let repo = env.dir.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/main.rs"), "").unwrap();
        std::fs::write(
            d.join("a.md"),
            format!(
                "{FACT}See `src/main.rs`, not `src/old.rs`; `agent send x --now`, `agent sned x`, `agent dream memory check --repo .`.\n"
            ),
        )
        .unwrap();
        std::fs::write(d.join("MEMORY.md"), "- [A](a.md) — a\n").unwrap();
        let args = CheckArgs {
            json: false,
            dir: DirArg {
                dir: d.to_string_lossy().into_owned(),
            },
            repo: vec![repo.to_string_lossy().into_owned()],
            args: vec![],
        };
        run_check(&env.ctx, &args).unwrap();
        assert_eq!(
            env.take_out(),
            "a.md:9: `agent sned x`: agent has no subcommand \"sned\"\n\
             a.md:9: `src/old.rs`: not in any repo\n"
        );
        let bad = CheckArgs {
            repo: vec![repo.join("nope").to_string_lossy().into_owned()],
            ..args
        };
        assert!(run_check(&env.ctx, &bad).is_err());
    }
}
