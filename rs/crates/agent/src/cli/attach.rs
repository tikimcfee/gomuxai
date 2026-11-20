//! `agent attach` (Go: internal/cli/attach.go).
use agent_service::AGENT_STATUS_ENDED;
use anyhow::Context as _;

use super::command::{ArgRule, Def, Doc};
use super::exec::exec_into;
use super::remote::parse_target;
use super::root::Ctx;
use super::ssh::ssh_attach;

// `agent attach`

pub const DOC: Doc = Doc {
    use_line: "attach <name>",
    short: "Open an agent's session in this terminal",
    long: r#"Attach opens an agent's g3d-termd session in this terminal via "g3d attach
--take": you see its screen and can type into it. Ctrl-\ twice detaches; the
agent keeps running.

agent attach name@host opens an agent on another host. The bus can't carry
a terminal, so agent runs ssh: ssh -t <destination> <its agent binary>
attach <name>, where the destination is the host's name unless agent bus
ssh <host> <destination> set another, and the host says through the bus
where its binary is. ssh is a separate trust path from the bus (your ssh
keys and ~/.ssh/config, not the bus's pinned keys), so it needs "ssh
<destination>" to work from here; if ssh fails, its error is what you see.

Either way this process becomes g3d (or ssh): a signal to it reaches them
directly, and nothing is left behind.

An agent whose session is gone (it ended: a reboot, say) has nothing to
attach to, and attach says so before running g3d or ssh; agent resume
<name> --attach (or name@host) starts a claude or agy agent again and
opens it.

agent spawn --attach spawns an agent and attaches to it in one step."#,
    example: r#"  agent attach helper
  agent attach helper@hub
  agent spawn helper --backend claude --attach"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent attach`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
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
        return ssh_attach(ctx, &r, "attach", &[]);
    }
    let svc = ctx.service()?;
    let agent = svc.agent(&r.name)?;
    if let Ok(false) = ctx.termd()?.session_exists(&agent.session_name) {
        let mut reason = String::new();
        if let Ok(info) = svc.status_info(&agent.name)
            && info.status == AGENT_STATUS_ENDED
        {
            reason = info.end_reason.to_string();
        }
        let resumable = agent_base::types::resumable(
            &agent.backend,
            agent.adopted,
            &svc.last_session_id(&agent.name),
            true,
        );
        return Err(gone_attach_error(&agent.name, &agent.session_name, &reason, resumable));
    }
    attach_session(ctx, &agent.session_name)
}

/// attach's refusal for an agent whose session is gone (a reboot, a stopped
/// g3d-termd, or the CLI exited): there is nothing to open. r names the agent
/// as the person typed it (name or name@host); reason is how it ended, if
/// known (Go: goneAttachError).
pub fn gone_attach_error(r: &str, session: &str, reason: &str, resumable: bool) -> anyhow::Error {
    let mut ended = "has ended".to_string();
    if !reason.is_empty() {
        ended += &format!(" ({reason})");
    }
    if resumable {
        return anyhow::anyhow!(
            "{r} {ended}: its session {session} is gone, so there is nothing to attach to. agent resume {r} --attach starts it again on its last session and opens it"
        );
    }
    anyhow::anyhow!(
        "{r} {ended}: its session {session} is gone, so there is nothing to attach to (agent logs {r} shows its last output)"
    )
}

/// Hands this terminal to g3d, showing session, until the person detaches:
/// this process becomes g3d (exec_into) (Go: attachSession).
pub fn attach_session(ctx: &Ctx, session: &str) -> anyhow::Result<()> {
    let (bin, argv) = ctx
        .termd()?
        .attach_command(session)
        .with_context(|| format!("attaching to session {session}"))?;
    exec_into(ctx, &bin, &argv).with_context(|| format!("attaching to session {session}"))
}
