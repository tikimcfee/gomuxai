//! `agent resume` (Go: internal/cli/resume.go).
use agent_base::gostd::strconv::quote;
use agent_bus::{Context, ResumeResponse};
use agent_discovery::Session;
use agent_service::{AgentStatus, ResumeOptions};
use agent_store::Agent;
use anyhow::Context as _;
use clap::{ArgMatches, FromArgMatches as _};

use super::attach::attach_session;
use super::cleanup::Teams;
use super::command::{ArgRule, Cmd, Def, Doc};
use super::remote::parse_target;
use super::remote_ops::run_remote_resume;
use super::resume_all::run_resume_all;
use super::root::Ctx;
use super::spawn::{Readiness, check_resumable, spawn_readiness};
use super::transcript::resolve_transcript;

// `agent resume`

pub const DOC: Doc = Doc {
    use_line: "resume <name> | --all [--dry-run]",
    short: "Start an ended agent again, continuing its last session; --all: every one a reboot ended",
    long: r#"Resume starts an agent whose session has ended again, on that same
session: after a reboot, a stopped g3d-termd daemon, or a CLI that quit. It keeps
the agent's name, backend and parent (so its helpers stay its helpers),
the model spawn --model started it on, its own state keys and its last
_result, and delivers the messages held
for it when it ended. It starts in the directory the session ran in.

Only an ended claude or agy agent can be resumed ("agent list" shows
"ended (...)"). A failed resume leaves it ended, to resume again or clean
up.

An adopted agent (agent adopt) is refused: its session is your own, so
resume neither types a launch into your shell nor creates a session to
replace it. Start the CLI there yourself (claude -c continues its last
conversation) and adopt keeps tracking it. If that session is gone,
agent cleanup <name>, then agent spawn <name> --continue (or --resume
<session>) starts it in a session of gomuxai's own.

Inside an agent (AGENT_NAME set), you can resume only your own helpers and
theirs.

--all resumes every agent on this host that a reboot or a stopped g3d-termd
ended: status ended (gone), as agent status resolves it, and
resumable (claude or agy, not adopted, with a session). Agents that ended
any other way (quit, finished, closed, replaced, parent) were ended on
purpose, so --all leaves them, listing each as skipped with how it ended;
agent resume <name> still resumes one. Gone agents it can't resume (aider,
adopted, no session) are listed as skipped with why. Parents come back
before their helpers: one depth of the agent tree at a time, up to 4 at
once within it, each launch half a second after the one before. It prints
one line per ended agent (resumed, skipped, failed), parents first and
then by name, then a total, and exits 1 if any resume failed. --dry-run
says what it would resume (and checks each session as resume would) and
skip, and why, and changes nothing. Inside an agent, --all covers only
your own helpers and theirs. --skip-permissions and --trust-workspace
apply to every agent it resumes; --attach, which opens one session, is
refused. --all is this host's: run agent resume --all on another host to
resume its agents.

To continue a session that no agent owns (one you ran in a terminal, or a
cleaned-up agent's), use agent spawn <name> --resume <session>.

agent resume name@host resumes an agent on another host through the agent
bus: that host runs its own resume, so the refusals are its own (not
claude or agy, no session, not ended, adopted). Its bus must list resume
in --remote-ops: resume is off for other hosts by default, since it starts
a process as that host's user. Inside an agent, the other host lets you
resume only your own helpers there. --skip-permissions and
--trust-workspace are refused for another host's agent, as for agent spawn
name@host: run agent resume on that host for those.

agent resume name@host --attach runs ssh to that host instead, and its own
agent resume <name> --attach there, as agent attach name@host does (see
agent help attach): as you on that host, not through its resume op, and
it needs ssh to work from here."#,
    example: r#"  agent resume lead
  agent resume --all --dry-run   # after a reboot: what --all would resume and skip
  agent resume --all
  agent resume lead --attach
  agent resume lead@hub
  agent resume lead@hub --attach"#,
    args: ArgRule::Custom(resume_args),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent resume`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "all",
        help = "Resume every agent here that a reboot or stopped g3d-termd ended (yours only, inside an agent)",
    )]
    pub all: bool,
    #[arg(long = "attach", help = "Open the agent's session in this terminal once it starts")]
    pub attach: bool,
    #[arg(
        long = "dry-run",
        help = "With --all: list what it would resume and skip, and why, changing nothing",
    )]
    pub dry_run: bool,
    #[arg(
        long = "skip-permissions",
        help = "Pass --dangerously-skip-permissions to Claude or Agy (use with caution)",
    )]
    pub skip_permissions: bool,
    #[arg(
        long = "trust-workspace",
        help = "Confirm the CLI's workspace trust prompt on launch (use with caution)",
    )]
    pub trust_workspace: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

/// resume's argument rule: one agent, or --all and none; --dry-run only with
/// --all, and --attach only without.
fn resume_args(_cmd: &Cmd, m: &ArgMatches, args: &[String]) -> anyhow::Result<()> {
    let flags = Args::from_arg_matches(m)?;
    if !flags.all {
        if flags.dry_run {
            anyhow::bail!("--dry-run goes with --all: agent resume --all --dry-run");
        }
        if args.len() != 1 {
            anyhow::bail!("accepts 1 arg(s), received {}", args.len());
        }
        return Ok(());
    }
    if let Some(first) = args.first() {
        // Checked by shape: arguments are checked before the config that names
        // this host is loaded.
        if let Some((_, host)) = first.split_once('@') {
            anyhow::bail!(
                "resume --all resumes this host's agents: run agent resume --all on {host} to resume its own, or agent resume {first} for that one"
            );
        }
        anyhow::bail!("--all picks the agents itself: name one agent, or pass --all, not both");
    }
    if flags.attach {
        anyhow::bail!(
            "--attach opens one agent's session, and --all resumes several: agent attach <name> opens one once they're back"
        );
    }
    Ok(())
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    if args.all {
        return run_resume_all(
            ctx,
            &Context::background(),
            args.dry_run,
            &ctx.non_interactive_caller_agent(),
            args.skip_permissions,
            args.trust_workspace,
        );
    }
    let (r, remote) = parse_target(ctx, &args.args[0])?;
    if remote {
        return run_remote_resume(
            ctx,
            &r,
            args.skip_permissions,
            args.trust_workspace,
            args.attach,
        );
    }
    let resp = resume_agent(
        ctx,
        &Context::background(),
        &r.name,
        &ctx.non_interactive_caller_agent(),
        args.skip_permissions,
        args.trust_workspace,
    )?;
    print_resumed(ctx, &r.name, &resp)?;
    if args.attach {
        return attach_session(ctx, &resp.session);
    }
    Ok(())
}

/// Says what a resume did, for the agent named name (name@host for another
/// host's) (Go: printResumed).
pub fn print_resumed(ctx: &Ctx, name: &str, resp: &ResumeResponse) -> anyhow::Result<()> {
    let mut out = format!(
        "Resumed agent {name} (session {}, backend {}) on session {}, in {}.\n",
        resp.session, resp.backend, resp.session_id, resp.cwd
    );
    if resp.held > 0 {
        out += &format!(
            "{} held message(s) go to it once it is at its prompt.\n",
            resp.held
        );
    }
    if !resp.status.is_empty() {
        out += &spawn_readiness(
            name,
            &resp.backend,
            &AgentStatus::from(resp.status.as_str()),
            &resp.detail,
            Readiness::default(),
        );
        out.push('\n');
    }
    ctx.write_out(&out)
}

/// Finds the session resume would continue agent on, and checks that it can
/// (Go: resumeSession).
pub fn resume_session(ctx: &Ctx, agent: &Agent) -> anyhow::Result<Session> {
    let t = resolve_transcript(ctx, &agent.name)
        .with_context(|| format!("finding agent {}'s last session", agent.name))?;
    let mut s = t.session;
    if s.cwd.is_empty() && !agent.cwd.is_empty() {
        // agy ties a conversation to its workspace in history.jsonl only
        // in some lines (its prompts' carry no conversation id), so a
        // conversation can have none recorded yet. The agent's record has
        // where its session ran: where its termd session started.
        s.cwd = agent.cwd.clone();
    }
    check_resumable(&s, &agent.backend)?;
    Ok(s)
}

/// Resumes the agent name on this host, as agent resume does, for agent
/// resume and the bus's resume op. caller is the agent asking: "" for a
/// person, a name here, or name@host for another host's agent, which may
/// resume only its own helpers and theirs (Go: resumeAgent).
pub fn resume_agent(
    ctx: &Ctx,
    c: &Context,
    name: &str,
    caller: &str,
    skip_permissions: bool,
    trust_workspace: bool,
) -> anyhow::Result<ResumeResponse> {
    let svc = ctx.service()?;
    let agent = svc.agent(name)?;
    if !caller.is_empty() {
        let list = svc.list()?;
        if !Teams::new(list).ancestors(name).iter().any(|a| a == caller) {
            let by = if agent.spawned_by.is_empty() {
                "a person".to_string()
            } else {
                quote(&agent.spawned_by)
            };
            anyhow::bail!(
                "can't resume {name}: it was spawned by {by}, not by you ({}) or your helpers",
                quote(caller)
            );
        }
    }

    if agent.adopted {
        // Its termd session is the person's: nothing is typed or created.
        return Err(svc.adopted_resume_error(&agent));
    }

    let s = resume_session(ctx, &agent)?;

    // The agent starts where the session ran: its termd session opens in
    // that directory, and the CLI finds the session from there.
    let resumed = svc.resume(
        c,
        ResumeOptions {
            name: name.to_string(),
            session_id: s.session_id.clone(),
            cwd: s.cwd.clone(),
            skip_permissions,
            trust_workspace,
        },
    )?;
    let mut resp = ResumeResponse {
        name: resumed.name,
        session: resumed.session_name,
        backend: resumed.backend,
        session_id: s.session_id,
        cwd: s.cwd,
        held: svc.held_count(name) as i64,
        ..Default::default()
    };
    if let Ok(info) = svc.status_info(name) {
        resp.status = info.status.to_string();
        resp.end_reason = info.end_reason.to_string();
        resp.detail = info.detail;
    }
    Ok(resp)
}
