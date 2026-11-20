//! `agent task`, `agent task set`, `agent task get`, `agent task list`, `agent task clear` (Go: internal/cli/task.go).
use std::fmt::Write as _;

use agent_base::gostd::os;
use agent_base::gostd::strconv::quote;
use agent_base::util;
use agent_store::{Task, Time};
use anyhow::Context as _;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::output::{plural, usage_error};
use super::root::{Ctx, command};

// `agent task`

pub const DOC: Doc = Doc {
    use_line: "task",
    short: "Save reusable tasks, to spawn agents with",
    long: r#"Task manages reusable task definitions, stored in this host's database.

Tasks exist independently of agents: several agents can be spawned with the
same task ("agent spawn <name> --task <task>" sends its content as the
agent's first message)."#,
    example: r#"  agent task set review-auth -f tasks/review-auth.md
  agent spawn reviewer --backend claude --task review-auth"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            task_set_def(),
            task_get_def(),
            task_list_def(),
            task_clear_def(),
        ],
    )
}

// `agent task set`

pub const TASK_SET: Doc = Doc {
    use_line: "set <name> [content]",
    short: "Create or update a task",
    long: r#"Set creates or updates a task in the repository.

Content can be provided directly as an argument or read from a file using -f.
When using -f, the content is stored in the database (not just the path)."#,
    example: r#"  agent task set research-auth "Analyze the authentication patterns in internal/auth"
  agent task set research-auth -f tasks/research-auth.md"#,
    args: ArgRule::Range(1, 2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent task set`.
#[derive(clap::Args, Debug, Clone)]
pub struct TaskSetArgs {
    #[arg(
        long = "file",
        short = 'f',
        value_name = "string",
        default_value = "",
        help = "Read content from file",
    )]
    pub file: String,
    /// The positional arguments, checked against TASK_SET.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn task_set_def() -> Def {
    leaf!(TASK_SET, TaskSetArgs, run_task_set)
}

/// A task as --json prints it.
#[derive(Serialize)]
struct TaskJsonEntry<'a> {
    name: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    content: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    source_file: &'a str,
    created_at: Time,
}

pub fn run_task_set(ctx: &Ctx, args: &TaskSetArgs) -> anyhow::Result<()> {
    let a = &args.args;
    let name = &a[0];
    let (content, source_file) = if !args.file.is_empty() && a.len() > 1 {
        return Err(usage_error(
            &command("task set"),
            anyhow::anyhow!("give the content or -f <file>, not both"),
        ));
    } else if !args.file.is_empty() {
        let data = os::read_file(&args.file).context("reading file")?;
        (
            String::from_utf8_lossy(&data).into_owned(),
            args.file.clone(),
        )
    } else if a.len() > 1 {
        (a[1].clone(), String::new())
    } else {
        return Err(usage_error(
            &command("task set"),
            anyhow::anyhow!("task {name} needs content: as an argument, or from a file with -f"),
        ));
    };

    let store = ctx.store()?;
    let existing = store.get_task(name)?;
    let task = Task {
        name: name.clone(),
        content,
        source_file,
        created_at: Time::now(),
    };
    store.set_task(&task)?;

    let verb = if existing.is_some() { "Updated" } else { "Saved" };
    let from = if task.source_file.is_empty() {
        "inline".to_string()
    } else {
        format!("from {}", task.source_file)
    };
    ctx.write_out(&format!(
        "{verb} task {name} ({}, {from}).\n",
        plural(task.content.matches('\n').count() + 1, "line")
    ))
}

/// The named task, or an error that suggests close names.
pub(crate) fn get_task(ctx: &Ctx, name: &str) -> anyhow::Result<Task> {
    let store = ctx.store()?;
    if let Some(task) = store.get_task(name)? {
        return Ok(task);
    }
    let Ok(tasks) = store.list_tasks() else {
        anyhow::bail!("no task {}", quote(name));
    };
    let names: Vec<&str> = tasks.iter().map(|t| t.name.as_str()).collect();
    let similar = util::similar(name, &names);
    if !similar.is_empty() {
        anyhow::bail!(
            "no task {}; did you mean {}?",
            quote(name),
            util::or_list(&similar, None)
        );
    }
    if tasks.is_empty() {
        anyhow::bail!(
            "no task {}: there are no tasks yet (agent task set <name> -f <file> adds one)",
            quote(name)
        );
    }
    anyhow::bail!(
        "no task {} ({}: agent task list)",
        quote(name),
        plural(tasks.len(), "task")
    )
}

// `agent task get`

pub const TASK_GET: Doc = Doc {
    use_line: "get <name>",
    short: "Print a task's content",
    long: "Get prints a task's content. It exits 1 if there is no such task.",
    example: r#"  agent task get research-auth
  agent task get research-auth --json"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent task get`.
#[derive(clap::Args, Debug, Clone)]
pub struct TaskGetArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    /// The positional arguments, checked against TASK_GET.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn task_get_def() -> Def {
    leaf!(TASK_GET, TaskGetArgs, run_task_get)
}

pub fn run_task_get(ctx: &Ctx, args: &TaskGetArgs) -> anyhow::Result<()> {
    let task = get_task(ctx, &args.args[0])?;
    if args.json {
        return ctx.print_json(&TaskJsonEntry {
            name: &task.name,
            content: &task.content,
            source_file: &task.source_file,
            created_at: task.created_at,
        });
    }
    let mut out = task.content;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    ctx.write_out(&out)
}

// `agent task list`

pub const TASK_LIST: Doc = Doc {
    use_line: "list",
    short: "List all tasks",
    example: "  agent task list",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent task list`.
#[derive(clap::Args, Debug, Clone)]
pub struct TaskListArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    /// The positional arguments, checked against TASK_LIST.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn task_list_def() -> Def {
    leaf!(TASK_LIST, TaskListArgs, run_task_list)
}

pub fn run_task_list(ctx: &Ctx, args: &TaskListArgs) -> anyhow::Result<()> {
    let tasks = ctx.store()?.list_tasks()?;

    if args.json {
        let out: Vec<TaskJsonEntry> = tasks
            .iter()
            .map(|t| TaskJsonEntry {
                name: &t.name,
                content: "",
                source_file: &t.source_file,
                created_at: t.created_at,
            })
            .collect();
        return ctx.print_json(&out);
    }

    if tasks.is_empty() {
        return ctx.write_out("No tasks yet (agent task set <name> -f <file> adds one).\n");
    }

    let mut out = format!("{:<25} {:<30} {}\n", "NAME", "SOURCE", "CREATED");
    out.push_str(&"-".repeat(80));
    out.push('\n');
    for task in &tasks {
        // Go cuts the source by bytes: the last 27 of a long one.
        let source = if task.source_file.is_empty() {
            "(inline)".to_string()
        } else if task.source_file.len() > 30 {
            let tail = &task.source_file.as_bytes()[task.source_file.len() - 27..];
            format!("...{}", String::from_utf8_lossy(tail))
        } else {
            task.source_file.clone()
        };
        let _ = writeln!(
            out,
            "{:<25} {:<30} {}",
            task.name,
            source,
            task.created_at.format_rfc3339()
        );
    }
    ctx.write_out(&out)
}

// `agent task clear`

pub const TASK_CLEAR: Doc = Doc {
    use_line: "clear <name>",
    short: "Remove a task",
    long: r#"Clear removes a task from the repository. It exits 1 if there is no such
task."#,
    example: "  agent task clear research-auth",
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent task clear`.
#[derive(clap::Args, Debug, Clone)]
pub struct TaskClearArgs {
    /// The positional arguments, checked against TASK_CLEAR.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn task_clear_def() -> Def {
    leaf!(TASK_CLEAR, TaskClearArgs, run_task_clear)
}

pub fn run_task_clear(ctx: &Ctx, args: &TaskClearArgs) -> anyhow::Result<()> {
    let name = &args.args[0];
    get_task(ctx, name)?;
    ctx.store()?.delete_task(name)?;
    ctx.write_out(&format!("Removed task {name}.\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::testenv::TestEnv;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn set_get_list_clear() {
        let env = TestEnv::new();
        let ctx = &env.ctx;
        let set = |file: &str, a: &[&str]| {
            run_task_set(ctx, &TaskSetArgs { file: file.to_string(), args: args(a) })
        };
        let list = |json: bool| {
            run_task_list(ctx, &TaskListArgs { json, args: vec![] }).unwrap();
            env.take_out()
        };
        let get = |name: &str, json: bool| {
            run_task_get(ctx, &TaskGetArgs { json, args: args(&[name]) }).map(|_| env.take_out())
        };

        assert_eq!(list(false), "No tasks yet (agent task set <name> -f <file> adds one).\n");
        assert_eq!(list(true), "[]\n");
        let err = get("x", false).unwrap_err();
        assert_eq!(
            err.to_string(),
            r#"no task "x": there are no tasks yet (agent task set <name> -f <file> adds one)"#
        );

        set("", &["review", "Review internal/auth"]).unwrap();
        assert_eq!(env.take_out(), "Saved task review (1 line, inline).\n");
        assert_eq!(get("review", false).unwrap(), "Review internal/auth\n");

        let file = env.dir.path().join("a-long-directory-name-for-tasks-review.md");
        std::fs::write(&file, "line one\nline two\n").unwrap();
        let path = file.to_string_lossy().into_owned();
        set(&path, &["review"]).unwrap();
        assert_eq!(env.take_out(), format!("Updated task review (3 lines, from {path}).\n"));
        assert_eq!(get("review", false).unwrap(), "line one\nline two\n");
        let json: serde_json::Value = serde_json::from_str(&get("review", true).unwrap()).unwrap();
        assert_eq!(json["content"], "line one\nline two\n");
        assert_eq!(json["source_file"], path.as_str());

        set("", &["deploy", "ship it"]).unwrap();
        env.take_out();
        let out = list(false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], format!("{:<25} {:<30} CREATED", "NAME", "SOURCE"));
        assert_eq!(lines[1], "-".repeat(80));
        assert!(lines[2].starts_with(&format!("{:<25} {:<30} ", "deploy", "(inline)")), "{out}");
        let tail = &path[path.len() - 27..];
        assert!(lines[3].starts_with(&format!("{:<25} ...{tail} ", "review")), "{out}");
        let json: serde_json::Value = serde_json::from_str(&list(true)).unwrap();
        assert_eq!(json[0]["name"], "deploy");
        assert!(json[1].get("content").is_none(), "list --json leaves content out: {json}");

        let err = set("x.md", &["review", "both"]).unwrap_err();
        assert!(
            err.to_string().starts_with(
                "give the content or -f <file>, not both\nUsage: agent task set <name> [content] [flags]"
            ),
            "{err}"
        );
        let err = set("", &["empty"]).unwrap_err();
        assert!(
            err.to_string().starts_with("task empty needs content: as an argument, or from a file with -f\nUsage:"),
            "{err}"
        );
        let err = set("/nonexistent/t.md", &["x"]).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "reading file: open /nonexistent/t.md: no such file or directory"
        );

        run_task_clear(ctx, &TaskClearArgs { args: args(&["deploy"]) }).unwrap();
        assert_eq!(env.take_out(), "Removed task deploy.\n");
        let err = get("deploy", false).unwrap_err();
        assert_eq!(err.to_string(), r#"no task "deploy" (1 task: agent task list)"#);
    }
}
