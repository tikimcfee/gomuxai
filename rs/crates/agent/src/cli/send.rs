//! `agent send` (Go: internal/cli/send.go).
use agent_bus::SendRequest;
use agent_service::{
    AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING, AgentStatus, Context, SendOptions, SendResult,
    dialog_title, is_slash_command, one_line,
};

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::{call_remote, parse_target};
use super::root::{Ctx, stdin_is_terminal};

// `agent send`

pub const DOC: Doc = Doc {
    use_line: "send <name>[@host] <message>",
    short: "Send a message to an agent: typed in now, or held while it's busy",
    long: r#"Send delivers a message to an agent, and says what it did with it and what
status it found the agent in.

An agent at its prompt gets the message typed into its terminal. A Claude or
agy agent that is busy (processing, or waiting on a prompt or question)
holds it instead (a Claude agent once "agent hook" has reported its current
session: a CLI started without the hook gets messages typed in), and a hook hands over every message held for it, coalesced oldest
first with newest last:
  claude  when its next tool batch finishes (PostToolBatch) or when its turn
          ends (Stop): Claude receives all held messages coalesced oldest-first
          as hook context and continues in the same turn.
  agy     when its current step ends (a model call and its tools), from its
          PostInvocation hook: all held messages arrive coalesced into one
          prompt, and the model answers them in the same turn.
If the turn ends without that hook (Esc, a deny, a failed turn), the held
messages are typed in once it has sat at its prompt for 10s, or ahead of the
next message sent to it. Use --later to hold until the entire turn ends,
--replace to supersede your earlier pending messages (scoped to --key when
given; keeping superseded text as context), --key <topic> to auto-supersede
your earlier pending messages with the same topic key, --supersede <id> to
supersede a specific pending message ID (with a note if it was already
delivered), or --interrupt (people and spawning ancestors) to block the
agent's next tool call via PreToolUse and deliver urgently. Messages are never
typed into an agent's input line while it has text in it (a person typing
there): they're held until it's empty. "agent status <name> -v" shows how many
are held and what they wait for, and "agent status <name> --receipt <id>"
inspects a message's delivery receipt.

--now types the message in even while the agent is busy (sweeping any held
messages ahead of it when the input line is empty; Claude takes it at its
next tool boundary, agy at its next step), as send always does for other
backends. Typing is refused while the agent is at a dialog (a
permission, question, or workspace trust prompt), where the Enter would
answer the dialog instead; answer permission prompts with approve/deny, and
questions with agent key. Send is also refused once the agent's session has
ended, since the message would run in the shell left behind. --force types the message in
without these checks, for a local agent, and only works for a person at a
terminal. An agent on another host is addressed as name@host (needs a
running bus); its host holds or types the message the same way.

A message sent by an agent (AGENT_NAME set) starts with "[from name@host]",
so the recipient knows where to reply. One sent without AGENT_NAME starts
with "[from a person on host]", or "[from a Claude session on host]" from an
AI CLI a person started (AI_AGENT or CLAUDECODE=1 set): neither has an agent
record to reply to. A slash command ("/compact", "/model claude-fable-5-1")
is left unmarked whoever sends it, so the CLI runs it (agent history
still records who sent it), and is never held: a hook can't run it, so it
is typed in now. A path ("/tmp/out.txt is ready") isn't a slash command.
Claude's /model also saves the model as the default for every new session
in the user's settings; to run one agent on another model, spawn it with
--model.

"Sent" means the message was typed into the agent's terminal, not that the
agent has read it: right after a send, the agent still looks idle until it
picks the message up. To wait for the turn it starts (or, for a held
message, until the agent is done with it too), use
"agent wait <name> --for idle,waiting --next".

Typing into Claude waits for its input box to show the message before
pressing Enter (a busy Claude reads an Enter that arrives with the text as
a newline), then checks that the box emptied, pressing Enter again while it
still shows the message, 3 times in all. If it is still there, send fails
saying so: don't send it again; "agent key agent-<name> Enter" submits it,
and "agent status <name>" shows it as unsent until then. If the input box
can't be read (not on screen), send types the message and presses Enter
without checking, and says so; status shows it as unchecked until a turn
starts.

A message is typed as a bracketed paste when the CLI asks for one (Claude
does), so it arrives whole however the terminal splits it. Claude takes a
typed message over 800 characters, or with 3 or more line breaks, as a
paste, which it gives the model as pasted material rather than as the
request. So such a message to a Claude agent whose hook reaches it is typed
as one short line instead (its first line, cut short, and "[gomuxai message
#N, ...]"), and the hook the prompt runs (UserPromptSubmit) hands the model
the whole message with it, as it does held messages. Up to 10,000
characters, the model gets it inline; beyond that, Claude saves it to a file
and gives the model a preview and the file's path. If the hook doesn't take
it within 3 SubmitWaits (9s), send says the agent got only the line. agy
takes long typed messages as the request itself, and gets them typed whole.
A held message arrives whole, however long (agy gets one over 64 KB in
labeled parts, since it cuts longer ones; Claude, one over 10,000 characters
as a file, as above)."#,
    example: r#"  agent send researcher "analyze the codebase structure"
  agent send builder "run the tests" && agent wait builder --for idle,waiting --next
  agent send builder --now "stop: the tests are already fixed"
  agent send builder@hub "run the tests""#,
    args: ArgRule::Min(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent send`.
#[derive(clap::Args, Debug, Clone, Default)]
pub struct Args {
    #[arg(
        long = "force",
        help = "Send even if the agent is at a dialog or its session has ended (local agents, people only)",
    )]
    pub force: bool,
    #[arg(
        long = "interrupt",
        help = "Block the next tool call via PreToolUse and deliver urgently (people or spawning ancestors)",
    )]
    pub interrupt: bool,
    #[arg(long = "json", help = "Print the send result and receipt as JSON")]
    pub json: bool,
    #[arg(
        long = "key",
        value_name = "string",
        default_value = "",
        help = "Topic key for coalescing and auto-superseding earlier pending messages with the same key",
    )]
    pub key: String,
    #[arg(
        long = "later",
        help = "Hold until the current turn ends completely, even on backends that deliver between steps",
    )]
    pub later: bool,
    #[arg(
        long = "now",
        help = "Type it in now even if the agent is busy, sweeping held messages first when the input line is empty",
    )]
    pub now: bool,
    #[arg(
        long = "replace",
        help = "Supersede earlier pending held messages from you to <name> (scoped to --key if set)",
    )]
    pub replace: bool,
    #[arg(
        long = "soon",
        help = "Deliver at the next tool batch or step boundary (default)",
    )]
    pub soon: bool,
    #[arg(
        long = "supersede",
        value_name = "int",
        default_value_t = 0,
        help = "Mark pending message ID superseded by this message (warns if already delivered)",
    )]
    pub supersede: i64,
    #[arg(
        long = "urgency",
        value_name = "string",
        default_value = "",
        help = "Delivery urgency: later, soon, now, or interrupt",
    )]
    pub urgency: String,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let name = &args.args[0];
    let message = args.args[1..].join(" ");
    let urgency = resolve_urgency(args)?;
    let now = args.now || urgency == "now";

    let (r, remote) = parse_target(ctx, name)?;
    let message = from_agent(ctx, &message);
    if args.force && (!env("AGENT_NAME").is_empty() || !stdin_is_terminal()) {
        // Agents may send without a prompt; --force would let one press Enter
        // on another agent's permission dialog, which is a person's call. An
        // agent's commands never have a terminal on stdin, even if it clears
        // AGENT_NAME.
        anyhow::bail!(
            "--force needs a person at a terminal; agents answer dialogs through a person (agent approve/deny)"
        );
    }
    if remote && args.force {
        anyhow::bail!("--force only applies to local agents");
    }
    if remote {
        let local_sender = sender_id(ctx);
        let remote_from = if local_sender == "user" {
            "user".to_string()
        } else {
            format!("{local_sender}@{}", ctx.cfg().host)
        };
        let req = SendRequest {
            agent: String::new(),
            message,
            now,
            from: remote_from,
            urgency,
            replace: args.replace,
            supersede: args.supersede,
            topic: args.key.clone(),
        };
        let resp = call_remote(ctx, &r.host, |c| {
            c.send_message(&Context::background(), &r, req)
        })?;
        let Some(resp) = resp else {
            // a host that predates holding: it typed the message
            return ctx.write_out(&format!("Sent to {name}.\n"));
        };
        let res = SendResult {
            status: AgentStatus::from(resp.status),
            detail: resp.detail,
            held: resp.held,
            held_count: usize::try_from(resp.held_count).unwrap_or(0),
            held_until: resp.held_until,
            typed: usize::try_from(resp.typed).unwrap_or(0),
            input_blocked: resp.input_blocked,
            unchecked: resp.unchecked,
            message_id: resp.message_id,
            superseded_ids: resp.superseded_ids,
            already_delivered_id: resp.already_delivered_id,
            already_delivered_via: resp.already_delivered_via,
            interrupted: resp.interrupted,
        };
        if args.json {
            return print_sent_json(ctx, name, &res);
        }
        return print_sent(ctx, name, &res);
    }
    let svc = ctx.service()?;
    if args.force {
        let unchecked = svc.send_unchecked(&Context::background(), &r.name, &message)?;
        ctx.write_out(&format!("Sent to {name} (unchecked: --force).\n"))?;
        return print_unchecked(ctx, name, &unchecked);
    }
    let res = svc.send(
        &Context::background(),
        &r.name,
        &message,
        SendOptions {
            now,
            from: sender_id(ctx),
            urgency,
            replace: args.replace,
            supersede: args.supersede,
            topic: args.key.clone(),
        },
    )?;
    if args.json {
        return print_sent_json(ctx, name, &res);
    }
    print_sent(ctx, name, &res)
}

fn resolve_urgency(args: &Args) -> anyhow::Result<String> {
    let shorthand_count = usize::from(args.later)
        + usize::from(args.soon)
        + usize::from(args.now)
        + usize::from(args.interrupt);
    if shorthand_count > 1 {
        anyhow::bail!("choose at most one of --later, --soon, --now, and --interrupt");
    }
    let raw = args.urgency.trim().to_ascii_lowercase();
    let explicit = match raw.as_str() {
        "" => "",
        "normal" | "soon" => "soon",
        "later" => "later",
        "now" => "now",
        "interrupt" => "interrupt",
        _ => {
            anyhow::bail!(
                "invalid --urgency {:?}: want later, soon, now, or interrupt",
                args.urgency
            );
        }
    };
    let shorthand = if args.later {
        "later"
    } else if args.soon {
        "soon"
    } else if args.now {
        "now"
    } else if args.interrupt {
        "interrupt"
    } else {
        ""
    };
    if !explicit.is_empty() && !shorthand.is_empty() && explicit != shorthand {
        anyhow::bail!(
            "cannot combine --{shorthand} with --urgency {}",
            args.urgency.trim()
        );
    }
    if !shorthand.is_empty() {
        Ok(shorthand.to_string())
    } else if !explicit.is_empty() {
        Ok(explicit.to_string())
    } else {
        Ok("soon".to_string())
    }
}

/// An environment variable, "" if unset (Go: os.Getenv): one that isn't UTF-8
/// is still set.
fn env(key: &str) -> String {
    super::root::getenv(key)
}

#[derive(serde::Serialize)]
struct SendJsonOut<'a> {
    agent: &'a str,
    status: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    detail: &'a str,
    #[serde(skip_serializing_if = "is_zero_i64")]
    id: i64,
    held: bool,
    #[serde(skip_serializing_if = "is_zero_usize")]
    held_count: usize,
    #[serde(skip_serializing_if = "str::is_empty")]
    held_until: &'a str,
    #[serde(skip_serializing_if = "is_zero_usize")]
    typed: usize,
    #[serde(skip_serializing_if = "str::is_empty")]
    input_blocked: &'a str,
    #[serde(skip_serializing_if = "str::is_empty")]
    unchecked: &'a str,
    #[serde(skip_serializing_if = "<[i64]>::is_empty")]
    superseded: &'a [i64],
    #[serde(skip_serializing_if = "is_zero_i64")]
    already_delivered_id: i64,
    #[serde(skip_serializing_if = "str::is_empty")]
    already_delivered_via: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    interrupted: bool,
}

fn is_zero_i64(n: &i64) -> bool {
    *n == 0
}

fn is_zero_usize(n: &usize) -> bool {
    *n == 0
}

fn print_sent_json(ctx: &Ctx, name: &str, res: &SendResult) -> anyhow::Result<()> {
    ctx.print_json(&SendJsonOut {
        agent: name,
        status: res.status.as_str(),
        detail: &res.detail,
        id: res.message_id,
        held: res.held,
        held_count: res.held_count,
        held_until: &res.held_until,
        typed: res.typed,
        input_blocked: &res.input_blocked,
        unchecked: &res.unchecked,
        superseded: &res.superseded_ids,
        already_delivered_id: res.already_delivered_id,
        already_delivered_via: &res.already_delivered_via,
        interrupted: res.interrupted,
    })
}

/// Says what send did with a message, and what it found the agent doing.
pub(crate) fn print_sent(ctx: &Ctx, name: &str, res: &SendResult) -> anyhow::Result<()> {
    let mut found = format!("it was {}", res.status);
    if res.status == AGENT_STATUS_UNKNOWN {
        found = "its status is unknown".to_string();
    }
    let mut meta = String::new();
    if res.message_id > 0 {
        meta.push_str(&format!("; #{}", res.message_id));
        if !res.superseded_ids.is_empty() {
            let ids: Vec<String> = res.superseded_ids.iter().map(|id| format!("#{id}")).collect();
            meta.push_str(&format!(", superseded {}", ids.join(", ")));
        }
    }
    if res.held {
        let mut behind = String::new();
        if res.held_count > 1 {
            behind = format!(", behind {}", plural(res.held_count - 1, "other message"));
        }
        if res.status == AGENT_STATUS_WAITING && !res.detail.is_empty() {
            found += &format!(" on {}", one_line(&res.detail));
        }
        if !res.input_blocked.is_empty() {
            // At its prompt, but its input line isn't empty: a person may be
            // typing there.
            ctx.write_out(&format!(
                "Held for {name} until its input line is empty{behind} ({found}; its input line: {}{meta}).\n",
                res.input_blocked
            ))?;
            return print_already_delivered(ctx, res);
        }
        let until = if res.held_until.is_empty() {
            "its turn ends" // a host that predates HeldUntil
        } else {
            &res.held_until
        };
        ctx.write_out(&format!("Held for {name} until {until}{behind} ({found}{meta}).\n"))?;
        print_already_delivered(ctx, res)?;
        if dialog_title(&res.status, &res.detail).is_some() {
            ctx.hint(format!(
                "agent deny {name} closes the dialog (Esc); agent approve {name} picks its highlighted option (Enter)"
            ));
        } else if res.status == AGENT_STATUS_WAITING {
            ctx.hint(format!(
                "its turn goes on once the prompt is answered: agent status {name} -v shows it"
            ));
        }
    } else if res.typed > 1 {
        let sup_meta = if res.superseded_ids.is_empty() {
            ""
        } else {
            meta.as_str()
        };
        ctx.write_out(&format!(
            "Sent to {name}, after {} held for it ({found}{sup_meta}).\n",
            plural(res.typed - 1, "message")
        ))?;
        print_already_delivered(ctx, res)?;
    } else {
        let sup_meta = if res.superseded_ids.is_empty() {
            ""
        } else {
            meta.as_str()
        };
        ctx.write_out(&format!("Sent to {name} ({found}{sup_meta}).\n"))?;
        print_already_delivered(ctx, res)?;
    }
    print_unchecked(ctx, name, &res.unchecked)
}

fn print_already_delivered(ctx: &Ctx, res: &SendResult) -> anyhow::Result<()> {
    if res.already_delivered_id <= 0 {
        return Ok(());
    }
    if res.already_delivered_via.is_empty() {
        ctx.write_out(&format!(
            "Note: message #{} was already delivered; this message was sent as a follow-up.\n",
            res.already_delivered_id
        ))
    } else {
        ctx.write_out(&format!(
            "Note: message #{} was already delivered (via {}); this message was sent as a follow-up.\n",
            res.already_delivered_id, res.already_delivered_via
        ))
    }
}

/// Says, when why is set, that a message was typed into a Claude agent without
/// checking that its Enter submitted it.
fn print_unchecked(ctx: &Ctx, name: &str, why: &str) -> anyhow::Result<()> {
    if why.is_empty() {
        return Ok(());
    }
    ctx.write_out(&format!(
        "Typed into {name} without checking it was submitted: {why}. agent capture {name} shows whether it sits in its input box.\n"
    ))
}

/// Who a message is from, as held messages record it: the sending agent, or
/// "user".
fn sender_id(ctx: &Ctx) -> String {
    let name = ctx.non_interactive_caller_agent();
    if name.is_empty() {
        return "user".to_string();
    }
    name
}

/// Marks a message with who sent it: AGENT_NAME (or an adopted session's
/// agent), or a person.
fn from_agent(ctx: &Ctx, message: &str) -> String {
    mark_sender(ctx, &ctx.non_interactive_caller_agent(), message)
}

/// Prefixes message with its sender's address on this host, so the recipient
/// knows who to reply to. A sender without an agent record (sender "") has no
/// address to reply to with agent send, so the tag only says what it is:
/// "[from a person on host]", or "[from a Claude session on host]" for an AI
/// CLI a person started (see unrecorded_sender). A slash command ("/compact",
/// "/model claude-fable-5-1") stays unmarked, whoever sends it, since a prefix
/// would turn it into a plain message; the command log (agent history) still
/// records who sent it.
pub(crate) fn mark_sender(ctx: &Ctx, sender: &str, message: &str) -> String {
    let host = &ctx.cfg().host;
    if is_slash_command(message) {
        return message.to_string();
    }
    if sender.is_empty() {
        return format!("[from {} on {host}] {message}", unrecorded_sender());
    }
    format!("[from {sender}@{host}] {message}")
}

/// Whether tool is a usable tool name: the part of AI_AGENT before its first
/// "_" (e.g. "claude-code" in "claude-code_2-1-310_agent"); Go's regexp
/// ^[A-Za-z][A-Za-z0-9-]{0,23}$.
fn ai_tool_name(tool: &str) -> bool {
    let b = tool.as_bytes();
    !b.is_empty()
        && b.len() <= 24
        && b[0].is_ascii_alphabetic()
        && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'-')
}

/// Describes a sender without AGENT_NAME: an AI CLI if its environment says so
/// (AI_AGENT names the tool; Claude Code also sets CLAUDECODE=1), else a
/// person.
fn unrecorded_sender() -> String {
    let v = env("AI_AGENT");
    if !v.is_empty() {
        let tool = v.split('_').next().unwrap_or_default();
        if tool.to_lowercase().starts_with("claude") {
            return "a Claude session".to_string();
        }
        if ai_tool_name(tool) {
            return format!("a {tool} session");
        }
        return "an AI session".to_string();
    }
    if env("CLAUDECODE") == "1" {
        return "a Claude session".to_string();
    }
    "a person".to_string()
}

#[cfg(test)]
#[path = "send_test.rs"]
mod send_test;
