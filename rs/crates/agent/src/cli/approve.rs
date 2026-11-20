//! `agent approve`, `agent deny` (Go: internal/cli/approve.go).
use agent_base::types::PermissionResponse;
use agent_bus::{AgentRef, RespondRequest};
use agent_service::{Context, PermissionOptions, one_line, permission_response_name};

use super::command::{ArgRule, Def, Doc};
use super::remote::{call_remote, parse_target};
use super::root::Ctx;

// `agent approve`

pub const DOC: Doc = Doc {
    use_line: "approve <name>[@host]",
    short: "Answer yes to an agent's permission prompt",
    long: r#"Approve answers "yes" to the permission prompt an agent is waiting on, and
says which prompt it answered.

Check what it's asking first: "agent status <name> -v" shows the pending
tool call in full. Refuses unless the agent's status is "waiting", and refuses
dialogs that are questions for the user rather than permission prompts (e.g.
Claude's AskUserQuestion: answer those with "agent keys"; a send to a waiting
Claude agent is held for the end of its turn). --force overrides both.

Use --expect with the tool you inspected, so a prompt that replaced it in the
meantime isn't approved blind. An agent on another host is addressed as
name@host (needs a running bus); its host runs the same checks.

At the CLI's workspace trust prompt (Claude or agy in a folder it hasn't
trusted; status shows tool "trust", detail "trust: <folder>"), approve
picks "Yes, I trust this folder", which trusts it for good (so there is no
--always), and the CLI starts. --expect trust names that prompt. A message
spawn held while the prompt was up goes in once the CLI is at its prompt.

At a dialog Claude shows on its own (an offer or notice at startup or
after a turn; status shows tool "dialog", detail "dialog: <title>; Enter
picks: <option>"), approve presses Enter, picking the option the detail
names. --expect dialog names it. Held messages go in once the dialog is
gone and the agent has been at its prompt for 10s."#,
    example: r#"  agent status helper -v && agent approve helper --expect Bash
  agent approve helper --always   # Claude's "yes, don't ask again" option
  agent approve helper --expect trust   # trust the folder at its trust prompt
  agent approve helper --expect dialog  # pick a dialog's highlighted option
  agent approve helper@hub --expect Bash"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent approve`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "always", help = "Approve and stop asking for this kind of action")]
    pub always: bool,
    #[arg(
        long = "expect",
        value_name = "string",
        default_value = "",
        help = "Refuse unless the pending prompt is for this tool (see status -v)",
    )]
    pub expect: String,
    #[arg(long = "force", help = "Send even if the agent isn't waiting on a permission prompt")]
    pub force: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (resp, verb) = if args.always {
        (PermissionResponse::ApproveAlways, "Always approved")
    } else {
        (PermissionResponse::Approve, "Approved")
    };
    respond_permission(ctx, &args.args[0], resp, verb, args.force, &args.expect)
}

// `agent deny`

pub const DENY: Doc = Doc {
    use_line: "deny <name>[@host]",
    short: "Answer no to an agent's permission prompt",
    long: r#"Deny rejects the permission prompt an agent is waiting on. For Claude this
cancels the current turn; follow up with "agent send" to redirect it.

Refuses unless the agent's status is "waiting" and the prompt is still on
screen (override with --force): for Claude the deny key is Escape, which
would cancel the turn if the prompt was answered in the meantime.

At the CLI's workspace trust prompt, deny picks "No, exit": the CLI exits,
and the agent is ended (quit).

At a dialog Claude shows on its own (status shows tool "dialog"), deny
presses Escape, which closes it (an offer takes it for "not now"); no turn
is cancelled. Held messages go in once the agent has been at its prompt
for 10s."#,
    example: r#"  agent deny helper
  agent deny helper --expect Bash
  agent deny helper --expect trust   # decline its workspace trust prompt
  agent deny helper --expect dialog  # close a dialog Claude showed on its own
  agent deny helper@hub"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent deny`.
#[derive(clap::Args, Debug, Clone)]
pub struct DenyArgs {
    #[arg(
        long = "expect",
        value_name = "string",
        default_value = "",
        help = "Refuse unless the pending prompt is for this tool (see status -v)",
    )]
    pub expect: String,
    #[arg(long = "force", help = "Send even if the agent isn't waiting on a permission prompt")]
    pub force: bool,
    /// The positional arguments, checked against DENY.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn deny_def() -> Def {
    leaf!(DENY, DenyArgs, run_deny)
}

pub fn run_deny(ctx: &Ctx, args: &DenyArgs) -> anyhow::Result<()> {
    respond_permission(
        ctx,
        &args.args[0],
        PermissionResponse::Deny,
        "Denied",
        args.force,
        &args.expect,
    )
}

fn respond_permission(
    ctx: &Ctx,
    name: &str,
    resp: PermissionResponse,
    verb: &str,
    force: bool,
    expect: &str,
) -> anyhow::Result<()> {
    let (r, remote) = parse_target(ctx, name)?;
    let info = if remote {
        remote_respond(ctx, &r, resp, force, expect)?;
        None
    } else {
        Some(ctx.service()?.respond_permission(
            &r.name,
            resp,
            PermissionOptions {
                force,
                expect_tool: expect.to_string(),
            },
        )?)
    };
    let prompt = match &info {
        Some(i) if !i.tool.is_empty() => format!("{} prompt", i.tool),
        _ => "prompt".to_string(),
    };
    let mut line = format!("{verb} {name}'s {prompt}");
    if let Some(i) = &info
        && !i.detail.is_empty()
    {
        line += &format!(" ({})", one_line(&i.detail));
    }
    ctx.write_out(&(line + ".\n"))?;
    if resp == PermissionResponse::Deny
        && let Some(i) = &info
        && !i.is_trust_prompt()
        && !i.is_dialog()
        && let Ok(a) = ctx.service()?.agent(&r.name)
        && a.backend == "claude"
    {
        ctx.hint(format!(
            "denying cancels Claude's turn: tell it what to do instead with agent send {name} \"...\""
        ));
    }
    Ok(())
}

fn remote_respond(
    ctx: &Ctx,
    r: &AgentRef,
    resp: PermissionResponse,
    force: bool,
    expect: &str,
) -> anyhow::Result<()> {
    let response = permission_response_name(resp)?;
    let req = RespondRequest {
        agent: String::new(),
        response,
        force,
        expect_tool: expect.to_string(),
    };
    call_remote(ctx, &r.host, |c| c.respond(&Context::background(), r, req))
}

#[cfg(test)]
#[path = "approve_test.rs"]
mod approve_test;
