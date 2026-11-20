//! `agent` with no arguments: a short orientation (Go: internal/cli/orient.go).

use std::time::Duration;

use agent_bus::AgentInfo;
use agent_service::AGENT_STATUS_UNKNOWN;
use agent_store::Agent;

use super::bus::{bus_set_up, query_bus};
use super::cleanup::Teams;
use super::list::team_order;
use super::output::plural;
use super::root::{Ctx, RootArgs};
use super::watch::watcher_running;

/// How many agents the orientation names (Go: orientMaxAgents).
const ORIENT_MAX_AGENTS: usize = 8;

/// Bounds the wait for the bus's status answer: the orientation must stay
/// fast (Go: orientBusTimeout).
const ORIENT_BUS_TIMEOUT: Duration = Duration::from_millis(500);

/// Prints a short orientation for whoever runs "agent" alone (Go: runOrient).
pub fn run_orient(ctx: &Ctx, _args: &RootArgs) -> anyhow::Result<()> {
    let text = orient(ctx, &ctx.non_interactive_caller_agent())?;
    ctx.write_out(&text)
}

/// The orientation for me (an agent's name, or "" for a person): who you
/// are, the agents that matter to you, the bus, your queued messages, and
/// where to go next. Statuses are resolved as by agent status (Go: orient).
pub fn orient(ctx: &Ctx, me: &str) -> anyhow::Result<String> {
    let svc = ctx.service()?;
    let agents = svc.list()?;
    let t = Teams::new(agents.clone());
    let host = &ctx.cfg().host;
    let mut w = String::new();

    // Who you are.
    let self_ = t.agents.get(me);
    if me.is_empty() {
        w += &format!("You are a person at a terminal, on host {host}.\n");
    } else {
        match self_ {
            None => {
                w += &format!(
                    "You are agent {me} on host {host}, with no agent record here (so agent state set {me} fails).\n"
                )
            }
            Some(a) if !a.spawned_by.is_empty() => {
                w += &format!(
                    "You are agent {me} on host {host}, a helper of {}.\n",
                    a.spawned_by
                )
            }
            Some(_) => w += &format!("You are agent {me} on host {host}, spawned by a person.\n"),
        }
    }

    // The agents that matter to you: your helpers, or everyone, in the
    // order agent list shows them.
    let infos: Vec<AgentInfo> = agents
        .iter()
        .map(|a| AgentInfo {
            name: a.name.clone(),
            spawned_by: a.spawned_by.clone(),
            ..Default::default()
        })
        .collect();
    let mut shown: Vec<&Agent> = team_order(&infos)
        .iter()
        .filter_map(|a| t.agents.get(&a.name))
        .collect();
    if !me.is_empty() {
        shown = t
            .helpers(me)
            .iter()
            .filter_map(|n| t.agents.get(n))
            .collect();
    }
    if agents.is_empty() {
        w += "Agents here: none yet.\n";
    } else if !me.is_empty() && shown.is_empty() {
        w += &format!(
            "Your helpers: none ({} here: agent list).\n",
            plural(agents.len(), "agent")
        );
    } else {
        let label = if me.is_empty() {
            format!("Agents here ({})", agents.len())
        } else {
            format!(
                "Your helpers ({} of {} here)",
                shown.len(),
                plural(agents.len(), "agent")
            )
        };
        w += &format!("{label}: {}\n", agent_statuses(ctx, &shown)?);
    }

    w += &format!("Bus: {}\n", bus_summary(ctx));

    if !me.is_empty()
        && t.agents.contains_key(me)
        && let Ok(pending) = ctx.store()?.get_pending_messages(me, 100)
        && !pending.is_empty()
    {
        let mut line = format!(
            "Queued for you: {}, delivered when you're idle",
            plural(pending.len(), "message")
        );
        if svc.holds_messages_named(me) {
            line = format!(
                "Held for you: {}, delivered when this turn ends",
                plural(pending.len(), "message")
            );
        } else if let (running, true) = watcher_running(ctx)
            && !running
        {
            line += ", but no watcher is running to deliver (agent watch)";
        }
        w += &format!("{line}.\n");
    }

    if agents.is_empty() {
        w += "Start with: agent spawn <name> --backend claude, then agent send <name> \"<task>\". All commands: agent help\n";
    } else {
        w += "Next: agent status <name> -v (live status), agent send <name> \"<message>\", agent wait <name> --for idle,waiting --next. All commands: agent help\n";
    }
    Ok(w)
}

/// Lists agents with their status, e.g. "helper idle, builder processing
/// (1 held)", up to ORIENT_MAX_AGENTS of them (Go: agentStatuses).
fn agent_statuses(ctx: &Ctx, agents: &[&Agent]) -> anyhow::Result<String> {
    let svc = ctx.service()?;
    let mut parts = Vec::new();
    for (i, a) in agents.iter().enumerate() {
        if i == ORIENT_MAX_AGENTS {
            parts.push(format!("and {} more (agent list)", agents.len() - i));
            break;
        }
        let (mut status, mut held) = (AGENT_STATUS_UNKNOWN.to_string(), String::new());
        if let Ok(info) = svc.status_info(&a.name) {
            status = info.status.to_string();
            if info.held > 0 {
                held = format!(" ({} held)", info.held);
            }
        }
        parts.push(format!("{} {status}{held}", a.name));
    }
    Ok(parts.join(", "))
}

/// Describes this host's bus in a few words (Go: busSummary).
fn bus_summary(ctx: &Ctx) -> String {
    let info = match query_bus(ctx, ORIENT_BUS_TIMEOUT) {
        Err(e) if agent_bus::is_no_bus(&e) && !bus_set_up(ctx) => {
            return "not set up (only needed to reach other hosts: agent bus --help)".to_string();
        }
        Err(e) if agent_bus::is_no_bus(&e) => return "not running (agent bus start)".to_string(),
        Err(e) => return format!("can't tell ({e:#})"),
        Ok(info) => info,
    };
    let mut s = format!(
        "running, {} of {} linked",
        info.linked_peers,
        plural(info.trusted.len(), "trusted peer")
    );
    if !info.trusted.is_empty() {
        s += &format!(" ({})", info.trusted.join(", "));
    }
    if !info.leaf_addr.is_empty() {
        s += "; this host is the hub";
    }
    s
}
