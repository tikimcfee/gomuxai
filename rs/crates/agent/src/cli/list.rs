//! `agent list` (Go: internal/cli/list.go).
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use agent_bus::{AgentInfo, Context, Handler as _};
use agent_service::AGENT_STATUS_WAITING;
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::{call_remote, canonical_host, is_remote_host};
use super::root::Ctx;

// `agent list`

pub const DOC: Doc = Doc {
    use_line: "list",
    short: "List the agents on this host (or another)",
    long: r#"List shows the agents recorded on this host as a nested list, with each
agent's status, backend, parent, session, and creation time. --host lists
another host's agents through the bus.

Status is the same live status "agent status" shows (idle, processing,
waiting, error, ended, unknown); "agent status <name> -v" says more, and
"agent statuses" what each means. A host running an older agent sends none,
shown as "-".

Each agent's helpers (spawned with "agent spawn") are nested right under it,
so a team reads as one tree. An agent can clean up only its own helpers and
theirs."#,
    example: r#"  agent list
  agent list --host hub
  agent list --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent list`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Host whose agents to list (default: this host)",
    )]
    pub host: String,
    #[arg(long = "json", help = "Print the agents as JSON")]
    pub json: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// An agent as list --json prints it (Go: agentJSON).
#[derive(Serialize)]
struct AgentJson {
    name: String,
    host: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    status: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    end_reason: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    parent: String,
    session: String,
    backend: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    adopted: bool,
    created_at: agent_bus::Time,
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let list_host = canonical_host(ctx, &args.host);
    let agents = list_agents(ctx, &list_host)?;

    if args.json {
        let host = if is_remote_host(ctx, &list_host) {
            list_host.clone()
        } else {
            ctx.cfg().host.clone()
        };
        let out: Vec<AgentJson> = team_order(&agents)
            .into_iter()
            .map(|a| AgentJson {
                name: a.name,
                host: host.clone(),
                status: a.status,
                end_reason: a.end_reason,
                parent: a.spawned_by,
                session: a.session,
                backend: a.backend,
                adopted: a.adopted,
                created_at: a.created_at,
            })
            .collect();
        return ctx.print_json(&out);
    }

    if agents.is_empty() {
        if is_remote_host(ctx, &list_host) {
            ctx.write_out(&format!("No agents on host {list_host}.\n"))?;
        } else {
            ctx.write_out(&format!(
                "No agents on host {} (agent spawn <name> --backend claude starts one).\n",
                ctx.cfg().host
            ))?;
        }
        return Ok(());
    }

    let mut waiting = 0;
    let mut out = String::new();
    for item in team_tree(&agents) {
        let agent = item.agent;
        let mut status = agent.status.clone();
        if status.is_empty() {
            status = "-".to_string(); // a host that doesn't send status
        }
        if !agent.end_reason.is_empty() {
            status += &format!(" ({})", agent.end_reason);
        }
        if status == AGENT_STATUS_WAITING.as_str() {
            waiting += 1;
        }

        let (head_indent, meta_indent) = if item.depth > 0 {
            (
                format!("{}└ ", "  ".repeat(item.depth)),
                "  ".repeat(item.depth + 2),
            )
        } else {
            (String::new(), "  ".to_string())
        };

        let mut backend_label = agent.backend.clone();
        if agent.adopted {
            backend_label += ", adopted";
        }
        out += &format!("{head_indent}{}: {status} ({backend_label})\n", agent.name);
        if !agent.spawned_by.is_empty() {
            out += &format!("{meta_indent}parent:  {}\n", agent.spawned_by);
        }
        out += &format!("{meta_indent}session: {}\n", agent.session);
        out += &format!(
            "{meta_indent}created: {}\n",
            agent.created_at.0.format("%Y-%m-%d %H:%M:%S")
        );
    }
    ctx.write_out(&out)?;
    if waiting > 0 {
        ctx.hint(format!(
            "{} waiting on a person (a permission prompt, a question, or a trust prompt): agent status <name> -v shows what it's asking",
            plural(waiting, "agent")
        ));
    }
    Ok(())
}

/// An agent and its nesting depth under its parent (Go: teamNode).
pub struct TeamNode {
    pub agent: AgentInfo,
    pub depth: usize,
}

/// Orders each agent's helpers right after it, transitively, with their
/// nesting depth under their parent (Go: teamTree).
pub fn team_tree(agents: &[AgentInfo]) -> Vec<TeamNode> {
    let listed: HashSet<&str> = agents.iter().map(|a| a.name.as_str()).collect();
    let mut helpers: HashMap<&str, Vec<&AgentInfo>> = HashMap::new();
    for a in agents {
        if listed.contains(a.spawned_by.as_str()) {
            helpers.entry(a.spawned_by.as_str()).or_default().push(a);
        }
    }

    fn place<'a>(
        a: &'a AgentInfo,
        depth: usize,
        helpers: &HashMap<&str, Vec<&'a AgentInfo>>,
        placed: &mut HashSet<&'a str>,
        out: &mut Vec<TeamNode>,
    ) {
        if !placed.insert(a.name.as_str()) {
            return;
        }
        out.push(TeamNode {
            agent: a.clone(),
            depth,
        });
        for h in helpers.get(a.name.as_str()).into_iter().flatten() {
            place(h, depth + 1, helpers, placed, out);
        }
    }

    let mut out = Vec::with_capacity(agents.len());
    let mut placed = HashSet::new();
    for a in agents {
        if !listed.contains(a.spawned_by.as_str()) {
            place(a, 0, &helpers, &mut placed, &mut out);
        }
    }
    // A parent's name can be reused after cleanup, so recorded parents can
    // form a loop with no agent at its top. Place those agents too.
    for a in agents {
        place(a, 0, &helpers, &mut placed, &mut out);
    }
    out
}

/// Lists each agent's helpers right after it, transitively, and otherwise
/// keeps the agents' order (Go: teamOrder).
pub fn team_order(agents: &[AgentInfo]) -> Vec<AgentInfo> {
    team_tree(agents).into_iter().map(|n| n.agent).collect()
}

/// Returns host's agents, in the form the bus serves them for both local and
/// remote hosts, so both print the same (Go: listAgents).
pub fn list_agents(ctx: &Ctx, host: &str) -> anyhow::Result<Vec<AgentInfo>> {
    if !is_remote_host(ctx, host) {
        let handler = agent_service::new_rpc_handler(Arc::clone(ctx.service()?));
        return Ok(handler.list(&Context::background())?.agents);
    }
    agent_bus::validate_host(host)?;
    call_remote(ctx, host, |c| c.list(&Context::background(), host))
}
