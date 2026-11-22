//! `agent revive` (no Go file: new in the Rust build; docs/FLEET.md, "Revive").
//! Resume plus the handover: it continues an ended agent's own conversation
//! (resume.rs's resume_agent), or, when a person already started its CLI
//! again in the agent's pane (friction #51), takes that running CLI as the
//! agent's (agent_service::revive), and then sends the re-brief. It kills
//! nothing it hasn't identified: a pane running something else is refused.
use std::path::{Path, PathBuf};
use std::time::Duration;

use agent_base::gostd::strconv::quote;
use agent_bus::Context;
use agent_fleet::handoff::{self, NotADoc};
use agent_service::UnsubmittedError;
use agent_service::revive::{PaneCli, PaneProcess, ReviveCase};
use agent_store::{Agent, StateEntry, Time};

use super::cleanup::Teams;
use super::command::{ArgRule, Def, Doc};
use super::remote::parse_target;
use super::resume::{print_resumed, resume_agent};
use super::root::Ctx;
use super::send::{mark_sender, print_sent};
use super::spawn::{Readiness, spawn_readiness};
use super::status::status_words;

// `agent revive`

pub const DOC: Doc = Doc {
    use_line: "revive <name>",
    short: "Bring an ended agent back on its own conversation and re-brief it from its handoff doc",
    long: r#"Revive is resume plus the handover. It brings back an agent whose CLI has
ended ("agent list" shows "ended (...)") on its own conversation, keeping
its name, backend, parent, model, effort and state, and then sends it the
re-brief: where its handoff doc is, and that it should read it and carry
on.

How it comes back depends on its pane:
  - Nothing runs there (its session is gone, or its shell sits at its
    prompt with nothing running under it): revive continues its last
    session exactly as agent resume does (claude and agy, with resume's
    refusals), in a new session of gomuxai's own.
  - A CLI of its backend runs there again, because a person started one in
    the agent's own pane (say, after it quit to log in again): revive takes
    that running CLI as the agent's, launching nothing, so status reads it
    again and agent send reaches it. Only a CLI revive identifies counts:
    run under the pane's shell by you, the backend's CLI on PATH (resolved,
    not by name), in an interactive session, not a one-shot command (agy
    changelog, claude -p); for claude, a session Claude Code's session
    registry proves. An agy started by hand runs without the hooks agent
    wires at launch: its status comes from its screen, and messages held
    for it go in once it is at its prompt, until it is restarted through
    gomuxai (quit it, then run agent revive <name> once more).
  - Anything else runs there, or the pane can't be read: revive refuses,
    naming the process, and kills nothing. Look with agent capture <name>,
    quit it, and revive again; agent resume <name> replaces it knowingly.

The handoff doc is, in order: --handoff FILE (a relative path is taken
from where you run revive; stored as the agent's state key handoff, so
the next revive uses it too), the state key handoff (set by agent spawn
--handoff, or agent state set <name> handoff <file>; a relative path is
taken from the agent's directory), or docs/lanes/<name>.md in the agent's
directory when that file exists. It must be a regular file; a state key
naming one that isn't there is an error, checked before anything happens.
With none, the re-brief says only that it was revived. The re-brief names
the doc's path, never its content. It waits up to 15s for the agent to be
at its prompt with an empty input line, then goes in as agent send sends
(after any messages held for it); otherwise it is held, and goes in once
it is (an agent at its workspace trust prompt gets it once the prompt is
approved). --no-brief sends none.

An agent that hasn't ended is left as it is: revive says what it is doing,
and agent send re-briefs it as it runs. An adopted agent is refused, as
agent resume refuses it. Inside an agent (AGENT_NAME set), you can revive
only your own helpers and theirs. Revive works on this host's agents: run
it on another host for that host's."#,
    example: r#"  agent revive helper
  agent revive helper --handoff docs/lanes/helper.md
  agent revive helper --no-brief"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent revive`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "handoff",
        value_name = "string",
        default_value = "",
        help = "Handoff doc to re-brief it from; stored as its state key handoff",
    )]
    pub handoff: String,
    #[arg(long = "no-brief", help = "Bring it back without sending the re-brief")]
    pub no_brief: bool,
    #[arg(
        long = "skip-permissions",
        help = "Pass --dangerously-skip-permissions to Claude or Agy, if it is launched (use with caution)",
    )]
    pub skip_permissions: bool,
    #[arg(
        long = "trust-workspace",
        help = "Confirm the CLI's workspace trust prompt, if it is launched (use with caution)",
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
    let (r, remote) = parse_target(ctx, &args.args[0])?;
    if remote {
        anyhow::bail!(
            "revive works on this host's agents: run agent revive {} on {} (agent resume {r} resumes it from here, without the re-brief)",
            r.name,
            r.host
        );
    }
    let name = r.name;
    let handoff_arg = if args.handoff.is_empty() {
        None
    } else {
        Some(handoff_file(&args.handoff)?)
    };

    let svc = ctx.service()?;
    let agent = svc.agent(&name)?;
    let caller = ctx.non_interactive_caller_agent();
    if !caller.is_empty() {
        let list = svc.list()?;
        if !Teams::new(list).ancestors(&name).contains(&caller) {
            let by = if agent.spawned_by.is_empty() {
                "a person".to_string()
            } else {
                quote(&agent.spawned_by)
            };
            anyhow::bail!(
                "can't revive {name}: it was spawned by {by}, not by you ({}) or your helpers",
                quote(&caller)
            );
        }
    }
    if agent.adopted {
        return Err(svc.adopted_resume_error(&agent));
    }
    // The re-brief's doc, checked before anything happens (and found again
    // once the agent is back: a resume may give it the directory its
    // session ran in, where none was on record).
    if !args.no_brief && handoff_arg.is_none() {
        let stated = state_value(ctx, &name, handoff::STATE_KEY_HANDOFF);
        if let Err(e) = handoff::resolve(&agent.cwd, &name, Some(stated.as_str()))
            && e != NotADoc::NoBase
        {
            return Err(anyhow::anyhow!("{}; nothing was changed", doc_error(&name, &stated, &e)));
        }
    }

    match svc.revive_case(&name)? {
        ReviveCase::Running(info) => {
            anyhow::bail!(
                "agent {name} is {}, not ended: revive brings back an agent whose CLI has ended (agent status {name} -v says what it is doing; agent send {name} \"...\" re-briefs it as it runs)",
                status_words(&info)
            );
        }
        ReviveCase::Refuse(p) => return Err(refusal(&agent, &p)),
        ReviveCase::Resume(_) => {
            let resp = resume_agent(
                ctx,
                &Context::background(),
                &name,
                &caller,
                args.skip_permissions,
                args.trust_workspace,
            )?;
            print_resumed(ctx, &name, &resp)?;
        }
        ReviveCase::TakeOver(mut cli) => {
            cli.transcript = session_transcript(ctx, &cli.session_id);
            let revived = svc.take_over_pane(&name, &cli)?;
            print_took_over(ctx, &revived, &cli)?;
            if args.skip_permissions || args.trust_workspace {
                ctx.hint(
                    "--skip-permissions and --trust-workspace apply to a CLI revive launches, and it launched none",
                );
            }
        }
    }

    if let Some(path) = &handoff_arg {
        ctx.store()?.set_state(&StateEntry {
            agent_name: name.clone(),
            key: handoff::STATE_KEY_HANDOFF.to_string(),
            value: path.clone(),
            updated_at: Time::now(),
        })?;
    }
    if args.no_brief {
        return ctx.write_out("Sent no re-brief (--no-brief).\n");
    }
    let agent = svc.agent(&name)?;
    let doc = rebrief_doc(ctx, &agent, handoff_arg.as_deref())
        .map_err(|e| e.context(format!("{name} is back, but its re-brief wasn't sent")))?;
    send_rebrief(ctx, &agent, &caller, &doc)
}

/// Why revive leaves an ended agent's pane alone, and what to do.
fn refusal(agent: &Agent, p: &PaneProcess) -> anyhow::Error {
    let name = &agent.name;
    let todo = format!(
        "agent capture {name} shows its pane; quit what runs there, then agent revive {name} again (agent resume {name} replaces it knowingly, closing the session and whatever runs in it)"
    );
    if p.pid == 0 {
        return anyhow::anyhow!(
            "can't revive {name}: {}, so revive can't tell what its session {} runs, and changed nothing: {todo}",
            p.why,
            agent.session_name
        );
    }
    anyhow::anyhow!(
        "can't revive {name}: its session {} runs {} (pid {}), which {}; revive takes over only a CLI of backend {} it identifies, and replaces only a pane whose shell sits at its prompt with nothing running, so it changed nothing: {todo}",
        agent.session_name,
        p.command,
        p.pid,
        p.why,
        agent.backend
    )
}

/// The transcript of session id, as the session index finds it ("" when it
/// doesn't, or id is ""), so a later resume continues that session.
fn session_transcript(ctx: &Ctx, id: &str) -> String {
    if id.is_empty() {
        return String::new();
    }
    super::transcript::session_index(ctx)
        .ok()
        .and_then(|index| index.sessions().into_iter().find(|s| s.session_id == id))
        .map(|s| s.path)
        .unwrap_or_default()
}

/// Says what a revive that took over the CLI in the agent's pane did.
fn print_took_over(ctx: &Ctx, agent: &Agent, cli: &PaneCli) -> anyhow::Result<()> {
    let svc = ctx.service()?;
    let name = &agent.name;
    let mut out = format!(
        "Revived agent {name} (session {}, backend {}): took over the {} already running in its session (pid {}), launching nothing.\n",
        agent.session_name, agent.backend, agent.backend, cli.pid
    );
    if !cli.session_id.is_empty() {
        out += &format!("It runs session {}.\n", cli.session_id);
    }
    let spawn_hooked = svc
        .backends()
        .create_provider(&agent.backend, Default::default())
        .is_ok_and(|p| p.as_spawn_hooker().is_some());
    if spawn_hooked && !cli.hooked {
        out += &format!(
            "It runs without the hooks agent wires at launch (it wasn't started through agent): its status comes from its screen, and messages held for it go in once it is at its prompt with an empty input line, until it is restarted through agent (quit it, then agent revive {name}).\n"
        );
    }
    let held = svc.held_count(name);
    if held > 0 {
        out += &format!("{held} held message(s) go to it once it is at its prompt.\n");
    }
    if let Ok(info) = svc.status_info(name) {
        out += &spawn_readiness(
            name,
            &agent.backend,
            &info.status,
            &info.detail,
            Readiness::default(),
        );
        out.push('\n');
    }
    ctx.write_out(&out)
}

/// The doc a re-brief names: Some(path, where it came from), or None.
type RebriefDoc = Option<(PathBuf, String)>;

/// Finds the re-brief's handoff doc: --handoff (already checked, absolute),
/// else the agent's state key handoff, else docs/lanes/<name>.md in its
/// directory. A state key naming something that isn't a regular file is an
/// error that names the key and the path, and how to change or clear it.
fn rebrief_doc(ctx: &Ctx, agent: &Agent, handoff_arg: Option<&str>) -> anyhow::Result<RebriefDoc> {
    let name = &agent.name;
    let key = handoff::STATE_KEY_HANDOFF;
    if let Some(path) = handoff_arg {
        return Ok(Some((PathBuf::from(path), format!("its state key {key}"))));
    }
    let stated = state_value(ctx, name, key);
    match handoff::resolve(&agent.cwd, name, Some(stated.as_str())) {
        Ok(Some(path)) => {
            let from = if stated.is_empty() {
                format!("docs/lanes/{name}.md in its directory")
            } else {
                format!("its state key {key}")
            };
            Ok(Some((path, from)))
        }
        Ok(None) => Ok(None),
        Err(e) => Err(doc_error(name, &stated, &e)),
    }
}

/// The error for an agent whose state key handoff (stated) names something
/// that isn't a regular file (e), naming the key and the path, and how to
/// change or clear it.
fn doc_error(name: &str, stated: &str, e: &NotADoc) -> anyhow::Error {
    let key = handoff::STATE_KEY_HANDOFF;
    let shown = match e {
        NotADoc::Stat { path, .. } if path != stated => format!("{} ({path})", quote(stated)),
        _ => quote(stated),
    };
    anyhow::anyhow!(
        "can't re-brief {name}: its state key {key} names {shown}, which{}: agent state set {name} {key} <file> names another, agent state clear {name} {key} drops it (then docs/lanes/{name}.md in its directory is used, if there), or --no-brief revives it without a re-brief",
        not_a_doc_words(e)
    )
}

/// NotADoc's words after "which".
fn not_a_doc_words(e: &NotADoc) -> String {
    match e {
        NotADoc::Stat { err, .. } => format!(" can't be read ({err})"),
        NotADoc::LineBreak => " has a line break in it".to_string(),
        NotADoc::NoBase => " is relative, and it has no directory on record to take it from".to_string(),
        other => other.to_string(),
    }
}

/// Sends the re-brief (agent_fleet::handoff::rebrief) once the agent can
/// take it (AgentService::send_rebrief), and says what it sent and what
/// became of it.
fn send_rebrief(ctx: &Ctx, agent: &Agent, caller: &str, doc: &RebriefDoc) -> anyhow::Result<()> {
    let svc = ctx.service()?;
    let name = &agent.name;
    let line = match doc {
        Some((path, from)) => format!("Re-brief: handoff doc {} (from {from}).\n", path.display()),
        None => format!(
            "Re-brief: no handoff doc (no state key {}, no docs/lanes/{name}.md in {}), so it says only that it was revived.\n",
            handoff::STATE_KEY_HANDOFF,
            agent.cwd
        ),
    };
    ctx.write_out(&line)?;
    let path = doc.as_ref().map(|(p, _)| p.as_path());
    let message = mark_sender(ctx, caller, &handoff::rebrief(name, path));
    let from = if caller.is_empty() { "user" } else { caller };
    let wait = REBRIEF_WAIT.with(|w| w.get());
    let res = svc
        .send_rebrief(&Context::background(), name, &message, from, wait)
        .map_err(|e| {
            if e.downcast_ref::<UnsubmittedError>().is_some() {
                // It was typed: another would go in twice.
                return e.context(format!("{name} is back, and its re-brief was typed"));
            }
            e.context(format!(
                "{name} is back, but its re-brief wasn't sent (agent send {name} \"...\" sends one once it can take it)"
            ))
        })?;
    print_sent(ctx, name, &res)?;
    if res.held && res.held_until == agent_service::revive::HELD_UNTIL_TRUST {
        ctx.write_out(&format!(
            "agent approve {name} trusts the folder, and the re-brief goes in once its CLI has started (agent deny {name} declines, and its CLI exits).\n"
        ))?;
    } else if res.held {
        ctx.write_out(&format!(
            "It wasn't at its prompt with an empty input line within {}s, so the re-brief waits: agent status {name} -v says what it is doing, agent capture {name} shows it.\n",
            wait.as_secs_f64()
        ))?;
    }
    Ok(())
}

thread_local! {
    /// How long the re-brief waits for the agent to sit at an idle prompt
    /// with an empty input line before it is held instead (tests shorten it).
    static REBRIEF_WAIT: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::from_secs(15)) };
}

/// Sets how long the re-brief waits on this thread (tests).
#[cfg(test)]
pub(crate) fn set_rebrief_wait(d: Duration) {
    REBRIEF_WAIT.with(|w| w.set(d));
}

/// The agent's state value for key, "" if unset.
fn state_value(ctx: &Ctx, name: &str, key: &str) -> String {
    ctx.store()
        .ok()
        .and_then(|s| s.get_state(name, key).ok().flatten())
        .map(|e| e.value)
        .unwrap_or_default()
}

/// Checks a --handoff FILE is a regular file, taking a relative one from the
/// working directory revive (or spawn) runs in, and returns its absolute
/// path, resolved with the filesystem, as the state key handoff stores it.
pub(crate) fn handoff_file(arg: &str) -> anyhow::Result<String> {
    let cwd = std::env::current_dir().unwrap_or_default();
    match handoff::regular_file(Path::new(&cwd), arg) {
        Ok(p) => Ok(p.to_string_lossy().into_owned()),
        Err(e) => anyhow::bail!("--handoff {arg}{e}"),
    }
}
