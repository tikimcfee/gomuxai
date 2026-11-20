//! `agent queue` (Go: internal/cli/queue.go).
use agent_base::gostd::strconv::quote;
use agent_service::{AGENT_STATUS_ENDED, Context, SendOptions};

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::parse_target;
use super::root::Ctx;
use super::send::{mark_sender, print_sent};
use super::watch::watcher_running;

// `agent queue`

pub const DOC: Doc = Doc {
    use_line: "queue <name> <message>",
    short: "Queue a message, delivered when the agent is idle",
    long: r#"Queue adds a message to the agent's inbox, for delivery when it is ready.

For a Claude or agy agent, queue does what "agent send" does, and needs no
watcher: the message is typed in if the agent is at its prompt, and
otherwise held until a hook hands it over (Claude's Stop, agy's
PostInvocation).

For other agents, the watcher ("agent watch") delivers queued messages when
the agent's status is idle, or error (a failed turn, back at its prompt). It
holds them while the agent is processing or waiting, and fails them once
the agent's session has ended. Without a running watcher, queued messages are never
delivered: queue says so when it finds none.

The agent must be on this host: name@<this host> works, while an agent on
another host is refused, since queueing isn't served over the bus (use
"agent send name@host").

Use --from to specify the sender (defaults to AGENT_NAME env var or "user").
Messages are marked with the sender as "agent send" marks them: an agent's
address, or for "user", "[from a person on host]" (or "[from a Claude
session on host]" from an AI CLI a person started)."#,
    example: r#"  agent queue researcher "when you're free, check the test results"
  agent queue builder --from coordinator "priority: fix the auth bug""#,
    args: ArgRule::Min(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent queue`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "from",
        value_name = "string",
        default_value = "",
        help = "Sender ID (defaults to AGENT_NAME or 'user')",
    )]
    pub from: String,
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
            "queued messages are kept per host, and {r} is on {}: queueing isn't served over the bus, so use agent send {r} instead",
            quote(&r.host)
        );
    }
    let name = &r.name;
    let message = args.args[1..].join(" ");

    // Determine sender
    let mut sender = args.from.clone();
    if sender.is_empty() {
        sender = ctx.non_interactive_caller_agent();
    }
    if sender.is_empty() {
        sender = "user".to_string();
    }

    let message = if sender == "user" {
        mark_sender(ctx, "", &message)
    } else {
        mark_sender(ctx, &sender, &message)
    };
    let svc = ctx.service()?;
    if svc.holds_messages_named(name) {
        // Send already holds a message until the agent is free, and needs no
        // watcher to deliver it.
        let res = svc.send(
            &Context::background(),
            name,
            &message,
            SendOptions {
                now: false,
                from: sender.clone(),
            },
        )?;
        return print_sent(ctx, name, &res);
    }
    svc.queue(&sender, name, &message)?;

    let mut line = format!("Queued for {name} from {sender}");
    if let Ok(pending) = ctx.store()?.get_pending_messages(name, 100)
        && pending.len() > 1
    {
        line += &format!(", behind {}", plural(pending.len() - 1, "other message"));
    }
    if let Ok(info) = svc.status_info(name) {
        line += &format!(" (it is {})", info.status);
        if info.status == AGENT_STATUS_ENDED {
            line += "; its session has ended, so the watcher will fail the message";
        }
    }
    ctx.write_out(&(line + ".\n"))?;
    if let (false, true) = watcher_running(ctx) {
        ctx.hint(format!(
            "no watcher is running, so it won't be delivered until one starts: agent watch (or agent send {name} now)"
        ));
    }
    Ok(())
}
