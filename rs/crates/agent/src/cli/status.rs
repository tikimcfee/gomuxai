//! `agent status` (Go: internal/cli/status.go).
use std::collections::BTreeMap;

use agent_base::gostd::json as gojson;
use agent_base::gostd::time as gotime;
use agent_base::types::{END_REASONS, EndReason, STATUSES, meaning_of};
use agent_bus::AgentRef;
use agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_UNKNOWN,
    AGENT_STATUS_WAITING, AgentStatus, Context, STATUS_SOURCE_SCRAPE, StatusInfo, one_line,
};
use agent_store::Time;
use serde::Serialize;
use serde_json::value::RawValue;

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::{call_remote, parse_target};
use super::root::Ctx;

// `agent status`

pub const DOC: Doc = Doc {
    use_line: "status <name>[@host]",
    short: "Show whether an agent is idle, processing, waiting, ...",
    long: r#"Status reports whether an agent is idle, processing, waiting (blocked on a
human, e.g. a permission prompt), error (its last turn failed; it takes input
again), ended (no CLI runs its session any more, with an end reason: quit,
gone, replaced, ...), or unknown. "agent statuses" says what each means.

-v adds what the status means, where it came from, the last event, its
detail (for a permission prompt, the full tool call: what approve would
allow), the tool,
and how many messages are held for the agent while it's busy (see
"agent send"). --json has the same fields.

An "unsent:" line, with or without -v, means a message typed into a Claude
agent's input box is still there because Enter didn't submit it: "agent key
agent-<name> Enter" does. An "unchecked:" line means a message was typed in
without that check (its input box couldn't be read) and no turn has started
since: "agent capture <name>" shows whether it sits there.

Sources, most authoritative first:
  native  the backend CLI's own session status (Claude Code's session registry)
  report  the last status reported by backend hooks or "agent report"
  scrape  terminal output: for backends without hooks, and for an agy turn
          that Esc or Ctrl+C ended, which fires no hook ("interrupted")

When a Claude agent's session on record isn't running, status asks what its
pane runs: a CLI a person started anew there becomes the agent's session
(event "adopted"). If it runs none, a session last reported running is
ended (gone).

A Claude or agy agent started in a folder its CLI hasn't trusted waits at
the CLI's workspace trust prompt, which runs no hook: status reads it off
the screen as waiting, tool "trust", detail "trust: <folder>". agent approve
trusts the folder; agent deny declines it, and the CLI exits.

agy shows its permission and ask_question dialogs with no hook of their own,
so a dialog on screen after the tool's PreToolUse makes it waiting, with
"permission: ..." or "question: ..." as the detail.

An agent on another host is addressed as name@host (needs a running bus)."#,
    example: r#"  agent status researcher
  agent status builder -v   # also show source, last event, and detail
  agent status builder@hub  # an agent on host "hub"
  agent status builder --json"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent status`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "json", help = "Print the status and its details as JSON")]
    pub json: bool,
    #[arg(long = "verbose", short = 'v', help = "Show source, last event, and detail")]
    pub verbose: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let name = &args.args[0];
    let (r, remote) = parse_target(ctx, name)?;

    let info = if remote {
        remote_status(ctx, &r)?
    } else {
        ctx.service()?.status_info(&r.name)?
    };

    // A message Enter failed to submit sits in its input box: say so even
    // without -v, since nothing reaches it until someone presses Enter.
    let (mut unsubmitted, mut unchecked) = (None, None);
    if !remote {
        let svc = ctx.service()?;
        unsubmitted = svc.unsubmitted(&r.name);
        // Likewise one typed without checking, until a turn starts.
        unchecked = svc.unchecked(&r.name).filter(|(why, _)| !why.is_empty());
    }

    if args.json {
        let host = if remote {
            r.host.clone()
        } else {
            ctx.cfg().host.clone()
        };
        let out = StatusJsonOut {
            agent: r.name.clone(),
            host,
            status: info.status.to_string(),
            end_reason: info.end_reason.as_str().to_string(),
            source: info.source.clone(),
            native_err: info.native_err.clone(),
            event: info.event.clone(),
            detail: info.detail.clone(),
            tool: info.tool.clone(),
            // Go: json.RawMessage when json.Valid: the text as it is (big
            // numbers, escapes and all), indented with the rest.
            tool_input: gojson::from_str::<Box<RawValue>>(&info.tool_input).ok(),
            updated_at: (!info.updated_at.is_zero()).then_some(info.updated_at),
            held: info.held,
            model: info.model.clone(),
            effort: info.effort.clone(),
            unsubmitted_at: unsubmitted.as_ref().map(|(at, _)| *at),
            unchecked: unchecked
                .as_ref()
                .map(|(why, _)| why.clone())
                .unwrap_or_default(),
            unchecked_at: unchecked.as_ref().map(|(_, at)| *at),
        };
        return ctx.print_json(&out);
    }

    let mut out = format!("{name}: {}\n", status_words(&info));
    if args.verbose {
        let wrap = |s: &str| wrap_indented(s, STATUS_WRAP_WIDTH, STATUS_INDENT);
        out += &format!("  means:  {}\n", wrap(&status_meaning(&info)));
        out += &format!("  source: {}\n", info.source);
        if !info.model.is_empty() {
            out += &format!("  model:  {}\n", info.model);
        }
        if !info.effort.is_empty() {
            out += &format!("  effort: {}\n", info.effort);
        }
        if !info.native_err.is_empty() {
            out += &format!("  native: unread: {}\n", wrap(&info.native_err));
        }
        if !info.event.is_empty() {
            out += &format!("  event:  {} ({} ago)\n", info.event, since_rounded(&info.updated_at));
        }
        // At a prompt, the input says in full what an answer allows; the
        // detail then only needs to name it.
        let input = if info.status == AGENT_STATUS_WAITING {
            format_tool_input(&info.tool_input)
        } else {
            String::new()
        };
        if !info.detail.is_empty() && !input.is_empty() {
            out += &format!("  detail: {}\n", one_line(&info.detail));
        } else if !info.detail.is_empty() {
            out += &format!("  detail: {}\n", wrap(&info.detail));
        }
        if !info.tool.is_empty() {
            out += &format!("  tool:   {}\n", info.tool);
        }
        if !input.is_empty() {
            out += &format!("  input:  {input}\n");
        }
        if info.held > 0 {
            out += &held_line(ctx, &r.name, remote, &info)?;
            out.push('\n');
        }
    }
    if let Some((at, session)) = &unsubmitted {
        out += &format!(
            "  unsent: a message typed into its input box at {} wasn't submitted; it sits there: agent key {session} Enter submits it\n",
            at.0.format("%H:%M:%S")
        );
    }
    if let Some((why, at)) = &unchecked {
        out += &format!(
            "  unchecked: a message typed in at {} wasn't checked for submission ({why}), and no turn has started since: agent capture {name} shows whether it sits in its input box\n",
            at.0.format("%H:%M:%S")
        );
    }
    ctx.write_out(&out)?;
    let n = &r.name;
    if info.status == AGENT_STATUS_ENDED {
        ctx.hint(format!(
            "agent logs {n} shows how it ended; agent cleanup {n} removes it; agent statuses says what each status means"
        ));
    } else if info.status == AGENT_STATUS_UNKNOWN && info.source == STATUS_SOURCE_SCRAPE {
        ctx.hint(format!(
            "its backend doesn't report status and its screen matched no known prompt: agent capture {name} shows the screen"
        ));
    } else if info.is_trust_prompt() {
        ctx.hint(format!(
            "agent approve {name} trusts the folder; agent deny {name} declines it, and its CLI exits"
        ));
    } else if info.is_dialog() {
        ctx.hint(format!(
            "a dialog of its CLI's own, which takes what is typed: agent deny {name} closes it (Esc); agent approve {name} picks its highlighted option (Enter); agent capture {name} shows it"
        ));
    } else if info.status == AGENT_STATUS_WAITING && !args.verbose {
        ctx.hint(format!("agent status {name} -v shows what it's waiting on"));
    }
    Ok(())
}

/// How long ago t was, rounded to the second, as Go prints a duration
/// (time.Since(t).Round(time.Second)).
fn since_rounded(t: &Time) -> String {
    gotime::format_duration_nanos(round_duration(since(t), SEC))
}

const SEC: i64 = 1_000_000_000;

/// Go's time.Since(t): nanoseconds from t to now, saturated at the bounds of
/// a Duration as Time.Sub saturates (a zero time, from a peer that sent none,
/// is about 2562047h ago).
fn since(t: &Time) -> i64 {
    let d = Time::now().0.signed_duration_since(t.0);
    d.num_nanoseconds()
        .unwrap_or(if d > chrono::TimeDelta::zero() { i64::MAX } else { i64::MIN })
}

/// Go's Duration.Round(m): to the nearest multiple of m, halfway values away
/// from zero, and the largest (or smallest) Duration where that overflows.
fn round_duration(d: i64, m: i64) -> i64 {
    if m <= 0 {
        return d;
    }
    // Go's lessThanHalf, without overflow.
    let less_than_half = |x: i64| (x as u64) + (x as u64) < m as u64;
    let r = d % m;
    if d < 0 {
        let r = -r;
        if less_than_half(r) {
            return d + r;
        }
        return i64::try_from(i128::from(d) - i128::from(m) + i128::from(r)).unwrap_or(i64::MIN);
    }
    if less_than_half(r) {
        return d - r;
    }
    d.checked_add(m - r).unwrap_or(i64::MAX)
}

/// A status as a line shows it: "ended (quit)" for an end (Go: statusWords).
pub fn status_words(info: &StatusInfo) -> String {
    if info.status == AGENT_STATUS_ENDED && !info.end_reason.is_empty() {
        return format!("{} ({})", info.status, info.end_reason.as_str());
    }
    info.status.to_string()
}

/// What a status (and its end reason) means.
fn status_meaning(info: &StatusInfo) -> String {
    let mut means = meaning_of(STATUSES, info.status.as_str()).to_string();
    if info.status == AGENT_STATUS_ENDED && !info.end_reason.is_empty() {
        let r = meaning_of(END_REASONS, info.end_reason.as_str());
        if !r.is_empty() {
            means = format!("{}: {r}", info.end_reason.as_str());
        }
    }
    means
}

/// status -v's line about the messages waiting for an agent.
fn held_line(ctx: &Ctx, name: &str, remote: bool, info: &StatusInfo) -> anyhow::Result<String> {
    let n = plural(info.held, "message");
    let mut line = if remote {
        format!("  held:   {n}")
    } else if info.is_trust_prompt() {
        format!(
            "  held:   {n}, typed in once the folder is trusted (agent approve {name}) and it is at its prompt"
        )
    } else if info.is_dialog() {
        format!(
            "  held:   {n}, typed in once the dialog is gone (agent deny {name} closes it) and it has been at its prompt for 10s"
        )
    } else {
        let svc = ctx.service()?;
        if svc.holds_messages_named(name)
            && (info.status == AGENT_STATUS_IDLE || info.status == AGENT_STATUS_ERROR)
        {
            // The turn ended without its Stop hook: Esc, a deny, a failure.
            let why = svc.input_blocked(name);
            if !why.is_empty() {
                format!("  held:   {n}, waiting for an empty input line ({why})")
            } else {
                format!(
                    "  held:   {n}, typed in once it has been at its prompt for 10s (agent deliver {name} types it now)"
                )
            }
        } else if svc.holds_messages_named(name) {
            format!("  held:   {n}, delivered when {}", svc.delivery_point(name))
        } else {
            format!("  queued: {n}, delivered by agent watch when it's idle")
        }
    };
    if info.status == AGENT_STATUS_ENDED {
        line += "; its session has ended, so they won't be";
    }
    Ok(line)
}

/// A status as --json prints it.
#[derive(Serialize)]
struct StatusJsonOut {
    agent: String,
    host: String,
    status: String,
    /// how the session ended, with a status of ended
    #[serde(skip_serializing_if = "String::is_empty")]
    end_reason: String,
    source: String,
    /// why the backend CLI's own status couldn't be read
    #[serde(rename = "native_error", skip_serializing_if = "String::is_empty")]
    native_err: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    event: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    detail: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    tool: String,
    /// The full input of the last tool call that may need permission: at a
    /// prompt, what approving it allows. Go keeps it as raw JSON when it is
    /// valid, which its encoder then indents with the rest.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_input: Option<Box<RawValue>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<Time>,
    /// How many messages wait for the agent: held for the end of its turn,
    /// or queued for agent watch.
    #[serde(skip_serializing_if = "is_zero")]
    held: usize,
    /// the model spawn --model started it on; absent for the CLI's default
    #[serde(skip_serializing_if = "String::is_empty")]
    model: String,
    /// the effort spawn --effort started it at; absent for the CLI's default
    #[serde(skip_serializing_if = "String::is_empty")]
    effort: String,
    /// when a message was typed into the agent's input box that Enter failed
    /// to submit, if it still sits there
    #[serde(skip_serializing_if = "Option::is_none")]
    unsubmitted_at: Option<Time>,
    /// why a message typed in at unchecked_at wasn't checked for submission,
    /// while no turn has started since
    #[serde(skip_serializing_if = "String::is_empty")]
    unchecked: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    unchecked_at: Option<Time>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// The width status -v wraps a detail to, and the indent that lines up its
/// continuation lines under the first.
const STATUS_WRAP_WIDTH: usize = 100;
const STATUS_INDENT: &str = "          ";

/// Renders a tool call's input (a JSON object) for status -v: one "key:
/// value" per field, in order, with string values as they are, line breaks
/// included, so a command reads as it would run. It returns "" for an empty or
/// malformed input.
fn format_tool_input(input: &str) -> String {
    if input.is_empty() {
        return String::new();
    }
    // Decoded as Go's json.Unmarshal does (half a surrogate pair is U+FFFD,
    // a null is no fields), each value's text as it came.
    let Ok(fields) = gojson::from_str::<BTreeMap<String, Box<RawValue>>>(input) else {
        return String::new();
    };
    let lines: Vec<String> = fields
        .iter()
        .map(|(k, raw)| {
            let value = gojson::from_str::<String>(raw.get())
                .unwrap_or_else(|_| raw.get().to_string());
            format!(
                "{k}: {}",
                wrap_indented(&value, STATUS_WRAP_WIDTH, &format!("{STATUS_INDENT}  "))
            )
        })
        .collect();
    lines.join(&format!("\n{STATUS_INDENT}"))
}

/// Wraps s to width columns, keeping its own line breaks, and indents every
/// line after the first with indent.
pub(crate) fn wrap_indented(s: &str, width: usize, indent: &str) -> String {
    let mut lines = Vec::new();
    for para in s.trim_end_matches('\n').split('\n') {
        let mut runes: Vec<char> = para.chars().collect();
        while runes.len() > width {
            let mut cut = width;
            let mut i = width;
            while i > width / 2 {
                if runes[i] == ' ' {
                    cut = i;
                    break;
                }
                i -= 1;
            }
            let head: String = runes[..cut].iter().collect();
            lines.push(head.trim_end_matches(' ').to_string());
            let rest: String = runes[cut..].iter().collect();
            runes = rest.trim_start_matches(' ').chars().collect();
        }
        lines.push(runes.iter().collect());
    }
    lines.join(&format!("\n{indent}"))
}

#[cfg(test)]
#[path = "status_test.rs"]
mod status_test;

/// An agent's status from its host, over the bus.
fn remote_status(ctx: &Ctx, r: &AgentRef) -> anyhow::Result<StatusInfo> {
    let resp = call_remote(ctx, &r.host, |c| c.status(&Context::background(), r))?;
    let mut info = StatusInfo::default();
    info.status = AgentStatus::from(resp.status);
    info.end_reason = EndReason::from(resp.end_reason);
    info.source = resp.source;
    info.event = resp.event;
    info.detail = resp.detail;
    info.tool = resp.tool;
    info.tool_input = resp.tool_input;
    info.updated_at = Time(resp.updated_at.0);
    info.held = usize::try_from(resp.held).unwrap_or(0);
    info.model = resp.model;
    info.effort = resp.effort;
    Ok(info)
}
