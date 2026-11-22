//! `agent spawn` (Go: internal/cli/spawn.go).
use agent_backend::ErrNotFound;
use agent_base::gostd::filepath;
use agent_base::gostd::strconv::quote;
use agent_bus::{AgentRef, Context, RemoteError, SpawnRequest, SpawnResponse};
use agent_discovery::{HARNESS_AGY, HARNESS_CLAUDE, STATUS_ENDED, Session};
use agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING, AGENT_STATUS_WAITING,
    AgentStatus, SpawnOptions, dialog_title, one_line, trust_backend_name, trust_prompt_folder,
    validate_effort, validate_model,
};
use agent_fleet::handoff;
use agent_store::{Agent, StateEntry, Time};

use super::attach::attach_session;
use super::revive::handoff_file;
use super::command::{ArgRule, Def, Doc};
use super::friction::age;
use super::logs::abs_path;
use super::remote::{call_remote, canonical_host, is_remote_host, parse_target};
use super::root::Ctx;
use super::spawn_pick;
use super::task::get_task;
use super::transcript::{live_for, resolve_transcript, session_at, session_index};

// `agent spawn`

pub const DOC: Doc = Doc {
    use_line: "spawn <name>[@host] [task-file]",
    short: "Start a new agent in its own g3d-termd session",
    long: r#"Spawn creates a new agent in a g3d-termd session, running the backend's CLI
(built in: claude, aider, agy, shell).

Initial work, if given, is sent as the agent's first message:
  - --task <name>  : the task's content, from the task repository (DB)
  - [task-file]    : a request to read that file (claude/agy: "Please read and
                     execute the task in <file>", aider: "/read <file>")
Without either, send the first message yourself with "agent send".

When an agent runs spawn (AGENT_NAME set), the new agent is recorded as its
helper: "agent list" shows it under PARENT, and only that agent (or its own
parent, and so on up) or a person can clean it up. It starts in the
directory spawn runs from, so it gets that project's permission settings
(for Claude, its .claude/ settings on top of the user's); --skip-permissions
bypasses them.

A Claude agent reports its status, takes held messages, and has its safe
agent commands approved through the hook Claude Code runs for every session,
so spawn refuses one until "agent hooks install" has registered this
binary's hook.

Spawn returns once the CLI has had its startup time, and says the status it
found the agent in: "idle" means it is ready for a message.

In a folder the CLI hasn't trusted yet, Claude and agy first ask whether to
trust it. Spawn then says so ("It is waiting at Claude's workspace trust
prompt for <folder>: agent approve <name> trusts it, or respawn with
--trust-workspace."), and the agent's status is waiting until someone
answers: agent approve trusts the folder, agent deny declines it (the CLI
exits). A Claude agent's first message waits for it, held rather than typed
into the prompt, and goes in once the CLI is at its prompt.
--trust-workspace answers the prompt at launch instead (trusting a folder
also enables its own hooks and settings, so only for folders you trust).

A Claude agent is launched with --settings '{"skillOverrides":
{"auto-mode-setup":"off"}}', for its session only: that turns off
/auto-mode-setup there, whose offer comes up when a turn ends in auto mode
and would take the keys of the next message. Another dialog Claude shows
on its own (at startup or between turns) makes the agent waiting, tool
"dialog"; its first message is held until the dialog is gone.

--resume continues a Claude or agy session that already exists, as this
agent, instead of starting a new one: a session you ran in a terminal joins
the workgroup, and agent send reaches it from then on. It takes what agent
transcript takes (a session id, the start of one, a transcript's path, or a
workgroup agent's name for its last session) and the --backend of the
session's harness. The session must have ended (agent sessions shows its
status): quit the CLI running it first, since two CLIs on one session would both write its transcript. A
subagent's session can't be resumed on its own. The agent starts in the
directory the session ran in, whatever directory spawn runs from.

--continue (-c) is --resume with the session the CLI's own --continue
would pick, in the directory the agent would start in (--cwd, or spawn's
directory), so the agent knows which session it is on:
  - claude: the most recent conversation in that directory, as claude -c
    picks it: the session recorded there whose transcript was written last.
  - agy: the most recently active conversation whose workspace is that
    directory, or, if there is none, one in a parent or child directory of
    it, as agy -c falls back to.
Subagents' sessions are never picked. Spawn says which session it picked
(its id, title, and when it was last active), and then goes on exactly as
--resume does with that id: a session still running is refused. It needs
--backend claude or agy, and doesn't go with --resume.

--model runs the CLI on that model (its --model flag): claude ("opus",
"claude-fable-5-1"), agy ("gemini-3.8-flash-high"; agy models lists them)
and aider take one; other backends refuse it. The agent remembers it:
agent resume relaunches it on the same model, and agent status -v shows it.
A model switched inside the session (Claude's /model, which agent send
types in as a command) lasts for that run only.

--effort runs the CLI at that reasoning effort (its --effort): low, medium,
high, xhigh or max, the roster's efforts. claude and agy take one; other
backends refuse it. agy names each effort of a model as its own id, so
with --model it launches on <model>-<effort> (--model gemini-4-carbon
--effort high runs gemini-4-carbon-high, the id its transcripts record),
and a model that already ends in an effort is refused with --effort. Like the model, the agent remembers it: agent resume
and agent revive relaunch at it, and agent status -v and agent fleet show
it.

--pick KIND lets the roster choose instead of --backend, --model and
--effort: it ranks this host's and every linked host's entries for that
task kind exactly as agent roster pick --task KIND does, says what it
chose and why in a line ("Picked claude/claude-haiku-5-5@low for edit:
tasks include edit; notes on edit: 2 good from 2 sessions; fast"), and
spawns that. NAME@HOST (or --host) picks from that host's roster only. A
pick on another host spawns there as NAME@HOST does, so that host's bus
must serve spawn; if it doesn't, spawn says so. --saved NAME[@VERSION]
spawns a pick kept with agent roster pick --save NAME, on its own host, if
its entry is still available there. Both refuse --backend,
--backend-config, --model and --effort, and when nothing in the roster can
be picked, the refusal lists each entry's availability (as agent roster
list shows it) and how to add one.

--attach opens the new agent's session in this terminal once it
starts, as agent attach does (Ctrl-\ twice detaches; the agent keeps running).

--cwd starts the agent in that directory instead of the one spawn runs
from (a relative task file is then taken from spawn's directory).

A custom backend (--backend-config <file>) is only known through that file:
pass it again to status, wait, and watch for that agent.

On another host: name@host (or --host) starts the agent on that host
through the agent bus, as status, send and wait reach it there. That host's
bus must allow it: spawn is off for other hosts unless its --remote-ops lists
it (agent bus start --remote-ops list,status,capture,send,respond,friction,logs,fleet,template,manage,spawn).
Paths are that host's: --cwd must be absolute there, and without it the
agent starts in the home directory of the user its bus runs as; a task file
is a path there, relative to --cwd. --task is read from this host's tasks
and sent as the first message. --resume, --attach, --skip-permissions,
--trust-workspace, --handoff and --backend-config only work on the agent's
own host: run spawn there for those (so --cwd must be a directory the CLI
already trusts there, or a person there confirms its trust prompt). An agent
spawned by agent lead on host laptop is recorded on the other host as a
helper of lead@laptop, so it can reply with agent send lead@laptop; the
other host takes that name on trust. cleanup doesn't reach other hosts:
clean the agent up on its host.

--handoff names the agent's handoff doc, the file that says where its work
stands: spawn stores its absolute path as the agent's state key handoff,
and agent revive re-briefs the agent from it (without one, revive looks
for docs/lanes/<name>.md in the agent's directory)."#,
    example: r#"  agent spawn researcher --backend claude
  agent spawn researcher --backend claude --task research-auth
  agent spawn builder --backend claude tasks/build.md
  agent spawn helper --backend aider
  agent spawn auto-worker --backend claude --skip-permissions
  agent spawn reviewer --backend agy --trust-workspace
  agent spawn quick --backend claude --model claude-fable-5-1
  agent spawn deep --backend claude --model claude-opus-5-5 --effort xhigh
  agent spawn fixer --pick edit tasks/fix.md
  agent spawn reviewer --saved quick-review@2
  agent spawn lead --backend claude --resume 3f34adff --attach
  agent spawn lead --backend claude --continue
  agent spawn builder@hub --backend claude --cwd /srv/repo tasks/build.md
  agent spawn builder --host hub --backend claude --task build"#,
    args: ArgRule::Range(1, 2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent spawn`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "attach", help = "Open the agent's session in this terminal once it starts")]
    pub attach: bool,
    #[arg(
        long = "continue",
        short = 'c',
        help = "Continue the session the CLI's own --continue would pick here (claude, agy): its most recent one in this directory, or --cwd's",
    )]
    pub continue_: bool,
    #[arg(
        long = "cwd",
        value_name = "string",
        default_value = "",
        help = "Directory the agent starts in (on another host: an absolute path there)",
    )]
    pub cwd: String,
    #[arg(
        long = "effort",
        value_name = "string",
        default_value = "",
        help = "Reasoning effort the CLI runs at, passed as its --effort (claude, agy): low, medium, high, xhigh or max; resume keeps it",
    )]
    pub effort: String,
    #[arg(
        long = "handoff",
        value_name = "string",
        default_value = "",
        help = "The agent's handoff doc, stored as its state key handoff (agent revive re-briefs it from there)",
    )]
    pub handoff: String,
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Start the agent on this host, through the agent bus (same as name@host)",
    )]
    pub host: String,
    #[arg(
        long = "model",
        value_name = "string",
        default_value = "",
        help = "Model the CLI runs on, passed as its --model (claude, agy, aider); resume keeps it",
    )]
    pub model: String,
    // The help names TASK_KINDS (a test checks).
    #[arg(
        long = "pick",
        value_name = "string",
        default_value = "",
        help = "Let the roster choose backend, model, effort and host for this task kind (edit, bugfix, feature, long-horizon, review, research, design, test, kernel, chat), as agent roster pick does",
    )]
    pub pick: String,
    #[arg(
        long = "resume",
        value_name = "string",
        default_value = "",
        help = "Continue this ended Claude or agy session as the agent (an id, the start of one, or a transcript's path)",
    )]
    pub resume: String,
    #[arg(
        long = "saved",
        value_name = "string",
        default_value = "",
        help = "Spawn a saved pick (agent roster pick --save): NAME[@VERSION], its newest by default",
    )]
    pub saved: String,
    #[arg(
        long = "skip-permissions",
        help = "Pass --dangerously-skip-permissions to Claude or Agy (use with caution)",
    )]
    pub skip_permissions: bool,
    #[arg(
        long = "task",
        value_name = "string",
        default_value = "",
        help = "Reference a task from the repository by name",
    )]
    pub task: String,
    #[arg(
        long = "trust-workspace",
        help = "Confirm the CLI's workspace trust prompt on launch (use with caution)",
    )]
    pub trust_workspace: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let Some(c) = spawn_pick::choose(ctx, args)? else {
        return spawn_as(ctx, args, &ctx.cfg().backend);
    };
    ctx.write_out(&c.said)?;
    let (r, _) = spawn_target(ctx, &args.args[0], &args.host)?;
    let mut picked = Args {
        model: c.model.clone(),
        effort: c.effort.clone(),
        host: if c.remote { c.host.clone() } else { String::new() },
        pick: String::new(),
        saved: String::new(),
        ..args.clone()
    };
    picked.args[0] = r.name.clone();
    let res = spawn_as(ctx, &picked, &c.backend);
    if c.remote {
        // Another host starts it only if its bus serves spawn to this one.
        let local = ctx.cfg().host.clone();
        return res.map_err(|e| {
            e.context(format!(
                "the pick runs on {host}, so {host} was asked to spawn {name} and didn't: a host starts agents for others only if its bus's --remote-ops lists spawn (agent bus status there shows its list); agent spawn {name}@{local} {flag} picks from this host only",
                host = c.host,
                name = r.name,
                flag = if args.pick.trim().is_empty() {
                    "--pick KIND".to_string()
                } else {
                    format!("--pick {}", args.pick.trim())
                },
            ))
        });
    }
    res
}

/// Spawns as args say, on backend (the --backend flag's, or the one --pick
/// or --saved chose).
fn spawn_as(ctx: &Ctx, args: &Args, backend: &str) -> anyhow::Result<()> {
    let (r, remote) = spawn_target(ctx, &args.args[0], &args.host)?;
    if remote {
        return run_remote_spawn(ctx, args, backend, &r, &args.args[1..]);
    }
    let name = r.name;
    let mut task_file = String::new();
    let mut task_content = String::new();
    let backend = backend.to_string();
    // Checked before anything starts; stored once the agent's record is.
    let handoff_doc = if args.handoff.is_empty() {
        String::new()
    } else {
        handoff_file(&args.handoff)?
    };

    check_task_args(&backend, &args.task, args.args.get(1))?;
    if !args.task.is_empty() {
        // Fetch task from DB
        let task = get_task(ctx, &args.task)?;
        task_content = task.content;
        // Use source file if available for reference
        if !task.source_file.is_empty() {
            task_file = task.source_file;
        }
    } else if args.args.len() > 1 {
        check_task_file(ctx, &name, &args.args[1])?;
        task_file = args.args[1].clone();
    }

    if args.continue_ && !args.resume.is_empty() {
        anyhow::bail!(
            "--continue and --resume don't go together: --continue picks the session, --resume names it"
        );
    }
    let mut cwd = String::new();
    if !args.cwd.is_empty() {
        if !args.resume.is_empty() {
            anyhow::bail!(
                "--cwd and --resume don't go together: a resumed agent starts where its session ran"
            );
        }
        cwd = abs_path(&args.cwd);
        if !task_file.is_empty() {
            task_file = abs_path(&task_file); // the agent reads it from cwd
        }
    }

    let mut from: Option<Session> = None;
    if args.continue_ {
        let dir = if cwd.is_empty() { abs_path(".") } else { cwd.clone() };
        let s = continue_session(ctx, &dir, &[backend.as_str()])?;
        ctx.write_out(&continue_choice(&s, &dir))?;
        from = Some(s);
        cwd = String::new();
    } else if !args.resume.is_empty() {
        from = Some(resumable_session(ctx, &args.resume, &backend)?);
    }
    let mut resume = String::new();
    let mut session_dir = cwd.clone();
    if let Some(from) = &from {
        // The agent starts where the session ran: its termd session opens
        // there, and the CLI finds the session from there. (The Go enters
        // that directory; this passes it as the spawn's directory, which
        // does the same without moving the whole process.)
        if !task_file.is_empty() {
            task_file = abs_path(&task_file);
        }
        if let Err(e) = check_chdir(&from.cwd) {
            anyhow::bail!(
                "entering {}, where session {} ran: chdir {}: {}",
                from.cwd,
                from.session_id,
                from.cwd,
                agent_base::gostd::os::errno_text(&e)
            );
        }
        session_dir = from.cwd.clone();
        resume = from.session_id.clone();
    }

    let opts = SpawnOptions {
        name: name.clone(),
        backend: backend.clone(),
        task_file: task_file.clone(),
        task_content,
        skip_permissions: args.skip_permissions,
        trust_workspace: args.trust_workspace,
        spawned_by: ctx.non_interactive_caller_agent(),
        resume: resume.clone(),
        cwd: session_dir,
        model: args.model.clone(),
        effort: args.effort.clone(),
        ..Default::default()
    };

    let svc = ctx.service()?;
    let agent = match svc.spawn(&Context::background(), opts) {
        Err(e) if e.chain().any(|c| c.downcast_ref::<ErrNotFound>().is_some()) => {
            return Err(ctx.with_backend_config_err(e));
        }
        r => r?,
    };

    let where_ = if cwd.is_empty() {
        String::new()
    } else {
        format!("in {cwd}")
    };
    let mut out = spawn_summary(&agent, &where_, &args.task, &task_file, &resume);
    out += &runs_on(&args.model, &args.effort);
    if !handoff_doc.is_empty() {
        ctx.store()?.set_state(&StateEntry {
            agent_name: name.clone(),
            key: handoff::STATE_KEY_HANDOFF.to_string(),
            value: handoff_doc.clone(),
            updated_at: Time::now(),
        })?;
        out += &format!(
            "Its handoff doc is {handoff_doc} (state key handoff; agent revive re-briefs it from there).\n"
        );
    }
    if let Ok(info) = svc.status_info(&name) {
        out += &spawn_readiness(
            &name,
            &agent.backend,
            &info.status,
            &info.detail,
            Readiness {
                task: !args.task.is_empty() || !task_file.is_empty(),
                remote: false,
            },
        );
        out.push('\n');
    }
    ctx.write_out(&out)?;
    if args.attach {
        return attach_session(ctx, &agent.session_name);
    }
    Ok(())
}

/// Refuses --task with a positional [task-file], and either for the shell
/// backend, whose agent would take a task as commands.
fn check_task_args(backend: &str, task: &str, task_file: Option<&String>) -> anyhow::Result<()> {
    // Only one of --task or positional [task-file] may be passed.
    if let Some(file) = task_file
        && !task.is_empty()
    {
        anyhow::bail!(
            "cannot specify both --task {} and positional [task-file] {}",
            quote(task),
            quote(file)
        );
    }
    if backend == "shell" && (!task.is_empty() || task_file.is_some()) {
        anyhow::bail!(
            "backend \"shell\" does not take an initial --task or [task-file]; use agent send or agent keys after spawning"
        );
    }
    Ok(())
}

/// Whether chdir(2) into dir would succeed, and its error if not, without
/// moving the process: it is a directory, and this process may search it
/// (and every directory on the way). A directory that can be read but not
/// searched (mode 0600) fails here, as Go's os.Chdir does, before termd or
/// the agent's files are touched.
fn check_chdir(dir: &str) -> std::io::Result<()> {
    if !std::fs::metadata(dir)?.is_dir() {
        return Err(std::io::Error::from_raw_os_error(libc::ENOTDIR));
    }
    let c = std::ffi::CString::new(dir).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: a NUL-terminated path; faccessat only reads it.
    let rc = unsafe { libc::faccessat(libc::AT_FDCWD, c.as_ptr(), libc::X_OK, libc::AT_EACCESS) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Ensures a positional [task-file] argument is an existing file, rather than
/// a stray word or stored task name that would otherwise be sent to the agent
/// as "Please read and execute the task in <arg>" (Go: checkTaskFile).
fn check_task_file(ctx: &Ctx, agent_name: &str, path: &str) -> anyhow::Result<()> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Ok(store) = ctx.store()
                && let Ok(Some(_)) = store.get_task(path)
            {
                anyhow::bail!(
                    "task file {} not found ({} is a stored task: pass --task {path} instead)",
                    quote(path),
                    quote(path)
                );
            }
            anyhow::bail!(
                "task file {} not found (omit [task-file] to spawn {agent_name} with no initial task)",
                quote(path)
            );
        }
        Err(e) => anyhow::bail!(
            "checking task file {}: stat {path}: {}",
            quote(path),
            agent_base::gostd::os::errno_text(&e)
        ),
        Ok(m) if m.is_dir() => {
            anyhow::bail!("task file {} is a directory, not a file", quote(path))
        }
        Ok(_) => Ok(()),
    }
}

/// Reads spawn's name argument and --host: r.host is set, and remote true,
/// when either names another host. Naming this host is a local spawn; naming
/// two different hosts is an error (Go: spawnTarget).
pub fn spawn_target(ctx: &Ctx, arg: &str, host: &str) -> anyhow::Result<(AgentRef, bool)> {
    let (r, remote) = parse_target(ctx, arg)?;
    if host.is_empty() {
        return Ok((r, remote));
    }
    agent_bus::validate_host(host)?;
    let host = canonical_host(ctx, host);
    let mut arg_host = r.host.clone();
    if arg.contains('@') && arg_host.is_empty() {
        arg_host = ctx.cfg().host.clone(); // name@<this host>, which parse_target made bare
    }
    if !arg_host.is_empty() && arg_host != host {
        anyhow::bail!("{arg} names host {arg_host}, but --host says {host}: give one");
    }
    if !is_remote_host(ctx, &host) {
        return Ok((
            AgentRef {
                name: r.name,
                host: String::new(),
            },
            false,
        ));
    }
    Ok((AgentRef { name: r.name, host }, true))
}

/// Starts r.name on r.host through the agent bus. rest is spawn's arguments
/// after the name: the task file, if any (Go: runRemoteSpawn).
fn run_remote_spawn(
    ctx: &Ctx,
    args: &Args,
    backend: &str,
    r: &AgentRef,
    rest: &[String],
) -> anyhow::Result<()> {
    // The local spawn's checks on the task, which hold whatever host the
    // agent starts on. (The Go skips them for another host: a [task-file]
    // beside --task went unsent, and a shell agent got --task typed in as
    // commands.) That the file is there is checked there.
    check_task_args(backend, &args.task, rest.first())?;
    for (set, flag) in [
        (!args.resume.is_empty(), "--resume"),
        (args.continue_, "--continue"),
        (args.attach, "--attach"),
        (args.skip_permissions, "--skip-permissions"),
        (args.trust_workspace, "--trust-workspace"),
        (!args.handoff.is_empty(), "--handoff"),
        (!ctx.globals.backend_config.is_empty(), "--backend-config"),
    ] {
        if set {
            anyhow::bail!(
                "{flag} works only for an agent on this host: to spawn {} with it, run agent spawn on {}",
                r.name,
                r.host
            );
        }
    }
    if !args.cwd.is_empty() && !args.cwd.starts_with('/') {
        anyhow::bail!(
            "--cwd {}: give an absolute path on {} (a relative one would be relative to wherever its bus runs)",
            args.cwd,
            r.host
        );
    }
    if !args.model.is_empty() {
        // Checked here too, so a bad name fails before it crosses the bus.
        validate_model(&args.model)?;
    }
    if !args.effort.is_empty() {
        validate_effort(&args.effort)?;
    }
    let mut req = SpawnRequest {
        backend: backend.to_string(),
        cwd: args.cwd.clone(),
        model: args.model.clone(),
        effort: args.effort.clone(),
        ..Default::default()
    };
    let me = ctx.non_interactive_caller_agent();
    if !me.is_empty() {
        req.spawned_by = AgentRef {
            name: me,
            host: ctx.cfg().host.clone(),
        }
        .to_string();
    }
    if !args.task.is_empty() {
        let task = get_task(ctx, &args.task)?;
        req.task_content = task.content; // its source file, if any, is a path here
    } else if let Some(f) = rest.first() {
        req.task_file = f.clone();
    }

    let resp = match call_remote(ctx, &r.host, |c| c.spawn(&Context::background(), r, req)) {
        Ok(resp) => resp,
        Err(e) if e.chain().any(|c| c.downcast_ref::<RemoteError>().is_some()) => {
            return Err(e.context(format!("{} couldn't spawn {}", r.host, r.name)));
        }
        Err(e) => {
            if format!("{e:#}").contains("no bus answering") {
                ctx.hint(format!(
                    "{} serves spawn to other hosts only if its bus runs with --remote-ops listing spawn (agent bus status there shows what it allows)",
                    r.host
                ));
            }
            return Err(e);
        }
    };

    let agent = Agent {
        name: r.to_string(),
        session_name: resp.session.clone(),
        backend: resp.backend.clone(),
        spawned_by: resp.spawned_by.clone(),
        ..Default::default()
    };
    let mut out = spawn_summary(
        &agent,
        &format!("on {} in {}", r.host, resp.cwd),
        &args.task,
        &resp.task_file,
        "",
    );
    out += &runs_on(&resp.model, &resp.effort);
    // A host that predates --model or --effort ignores it.
    if resp.model.is_empty() && !args.model.is_empty() {
        out += &format!(
            "{} didn't take --model (its agent predates it): {r} runs on its CLI's default model.\n",
            r.host
        );
    }
    if resp.effort.is_empty() && !args.effort.is_empty() {
        out += &format!(
            "{} didn't take --effort (its agent predates it): {r} runs at its CLI's default effort.\n",
            r.host
        );
    }
    out += &remote_spawn_readiness(r, &resp, !args.task.is_empty() || !resp.task_file.is_empty());
    out.push('\n');
    ctx.write_out(&out)
}

/// Says the model and effort the agent runs on, if spawn named either ("" if
/// neither).
pub fn runs_on(model: &str, effort: &str) -> String {
    match (model.is_empty(), effort.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("It runs on model {model}.\n"),
        (true, false) => format!("It runs at effort {effort}.\n"),
        (false, false) => format!("It runs on model {model} at effort {effort}.\n"),
    }
}

/// spawn_readiness for an agent on another host, where logs don't reach (Go:
/// remoteSpawnReadiness).
fn remote_spawn_readiness(r: &AgentRef, resp: &SpawnResponse, task: bool) -> String {
    let status = AgentStatus::from(resp.status.as_str());
    if status == AGENT_STATUS_ENDED {
        return format!("Its session has already ended: agent capture {r} shows its screen.");
    }
    spawn_readiness(
        &r.to_string(),
        &resp.backend,
        &status,
        &resp.detail,
        Readiness { task, remote: true },
    )
}

/// Finds the session --resume names and checks that an agent of backend_name
/// can continue it (Go: resumableSession).
fn resumable_session(ctx: &Ctx, arg: &str, backend_name: &str) -> anyhow::Result<Session> {
    let t = resolve_transcript(ctx, arg)?;
    check_resumable(&t.session, backend_name)?;
    Ok(t.session)
}

/// Finds the session `<harness> --continue` would pick in dir
/// (discovery's continue), the most recent of each harness's picks, and
/// checks, as --resume does, that an agent can continue it (Go:
/// continueSession).
pub fn continue_session(ctx: &Ctx, dir: &str, harnesses: &[&str]) -> anyhow::Result<Session> {
    for h in harnesses {
        if *h != HARNESS_CLAUDE && *h != HARNESS_AGY {
            anyhow::bail!(
                "--continue continues a claude or agy session: backend {h} keeps no sessions to continue"
            );
        }
    }
    let mut index = session_index(ctx)?;
    let mut pick: Option<Session> = None;
    for h in harnesses {
        if let Some(s) = index.r#continue(h, dir)
            && pick.as_ref().is_none_or(|p| s.mtime > p.mtime)
        {
            pick = Some(s);
        }
    }
    let which = harnesses.join(" or ");
    let Some(mut pick) = pick else {
        anyhow::bail!(
            "no {which} session to continue in {dir} (agent sessions lists the ones there are; --resume <id> names one)"
        );
    };
    let resolved = index.resolve(std::slice::from_ref(&pick), &live_for(ctx, &pick.harness));
    if let Some(i) = session_at(&resolved, &pick.path) {
        pick = resolved[i].clone();
    }
    let harness = pick.harness.clone();
    check_resumable(&pick, &harness).map_err(|e| {
        anyhow::anyhow!(
            "--continue picks {}, the most recent {which} session in {dir}, but {e:#}",
            pick.session_id
        )
    })?;
    Ok(pick)
}

/// Says which session --continue picked: its id, title, and when it was last
/// active, and where, if not in dir itself (Go: continueChoice).
pub fn continue_choice(s: &Session, dir: &str) -> String {
    let mut b = format!("Continuing {} session {}", s.harness, s.session_id);
    if !s.title.is_empty() {
        b += &format!(" ({})", quote(&s.title));
    }
    if let Some(mtime) = s.mtime {
        // Go: sessionWhen(s.Mtime, "2006-01-02 15:04").
        b += &format!(
            ", last active {}",
            mtime.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M")
        );
        let a = age(chrono::Utc::now().signed_duration_since(mtime));
        if a != "now" {
            b += &format!(" ({a} ago)");
        }
    }
    if !s.cwd.is_empty() && !same_dir(&s.cwd, dir) {
        b += &format!(", in {}: the most recent near {dir}.\n", s.cwd);
    } else {
        b += &format!(": the most recent in {dir}.\n");
    }
    b
}

/// Whether a and b name one directory (Go: sameDir).
fn same_dir(a: &str, b: &str) -> bool {
    let canon = |p: &str| match std::fs::canonicalize(p) {
        Ok(r) => r.to_string_lossy().into_owned(),
        Err(_) => filepath::clean(p),
    };
    canon(a) == canon(b)
}

/// Says why an agent of backend_name can't continue s (Go: checkResumable).
pub fn check_resumable(s: &Session, backend_name: &str) -> anyhow::Result<()> {
    if s.harness != HARNESS_CLAUDE && s.harness != HARNESS_AGY {
        anyhow::bail!(
            "session {} is a {} session: --resume continues claude and agy sessions",
            s.session_id,
            s.harness
        );
    }
    if s.harness != backend_name {
        anyhow::bail!(
            "session {} is a {} session: spawn it with --backend {}",
            s.session_id,
            s.harness,
            s.harness
        );
    }
    if let Some(parent) = &s.parent_session {
        anyhow::bail!(
            "session {} is a subagent's: resume the session that started it, {}",
            s.session_id,
            parent.session_id
        );
    }
    if s.status != STATUS_ENDED {
        let owner = match &s.workgroup {
            Some(w) => format!(" (agent {} runs it)", w.name),
            None => String::new(),
        };
        anyhow::bail!(
            "session {} is {}, not ended{owner}: quit the CLI running it first, since two CLIs on one session would both write its transcript",
            s.session_id,
            s.status
        );
    }
    if s.cwd.is_empty() {
        anyhow::bail!(
            "session {} recorded no working directory to resume it in",
            s.session_id
        );
    }
    if !std::fs::metadata(&s.cwd).is_ok_and(|m| m.is_dir()) {
        anyhow::bail!(
            "session {} ran in {}, which is no longer a directory",
            s.session_id,
            s.cwd
        );
    }
    Ok(())
}

/// What spawn_readiness says beside the status (Go: readiness).
#[derive(Debug, Clone, Copy, Default)]
pub struct Readiness {
    /// The agent was given a task.
    pub task: bool,
    /// It runs on another host, where --trust-workspace can't reach.
    pub remote: bool,
}

/// Says what the status spawn found the new agent in means for sending to it
/// (Go: spawnReadiness).
pub fn spawn_readiness(
    name: &str,
    backend_name: &str,
    status: &AgentStatus,
    detail: &str,
    r: Readiness,
) -> String {
    if let Some(folder) = trust_prompt_folder(status, detail) {
        let mut line = format!(
            "It is waiting at {}'s workspace trust prompt for {folder}: agent approve {name} trusts it",
            trust_backend_name(backend_name)
        );
        if !r.remote {
            line += ", or respawn with --trust-workspace";
        }
        line += ".";
        if r.task {
            line += " Its task waits until the folder is trusted.";
        }
        return line;
    }
    if let Some(title) = dialog_title(status, detail) {
        let mut line = format!(
            "It is waiting at a dialog of {}'s ({}): agent deny {name} closes it, agent approve {name} picks its highlighted option.",
            trust_backend_name(backend_name),
            one_line(&title)
        );
        if r.task {
            line += " Its task is held until the dialog is gone.";
        }
        return line;
    }
    if *status == AGENT_STATUS_IDLE {
        return "It is idle: ready for a message.".to_string();
    }
    if *status == AGENT_STATUS_PROCESSING {
        return "It is processing (still starting, or working on its task).".to_string();
    }
    if *status == AGENT_STATUS_WAITING {
        return format!("It is already waiting on a prompt: agent status {name} -v shows it.");
    }
    if *status == AGENT_STATUS_ENDED {
        return format!("Its session has already ended: agent logs {name} shows why.");
    }
    format!(
        "Its status is {} so far: agent wait {name} --for idle --timeout 1m waits until it's ready.",
        status.as_str()
    )
}

/// Says what spawn did: the new agent, where it started (where, e.g. "on hub
/// in /srv", if not the obvious place), its parent if an agent spawned it,
/// and what it was sent first (Go: spawnSummary).
pub fn spawn_summary(agent: &Agent, where_: &str, task: &str, task_file: &str, resume: &str) -> String {
    let mut b = format!(
        "Spawned agent {} (session {}, backend {})",
        agent.name, agent.session_name, agent.backend
    );
    if !where_.is_empty() {
        b += &format!(" {where_}");
    }
    if !agent.spawned_by.is_empty() {
        b += &format!(", a helper of {}", agent.spawned_by);
    }
    b += ".\n";
    if !resume.is_empty() {
        b += &format!("It continues session {resume}.\n");
    }
    if !task.is_empty() {
        b += &format!("Sent it task {task}.\n");
    } else if !task_file.is_empty() {
        b += &format!("Asked it to read {task_file}.\n");
    } else if resume.is_empty() {
        b += &format!("It has no task yet: agent send {} \"...\"\n", agent.name);
    }
    b
}
