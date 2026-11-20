//! `agent state`, `agent state set`, `agent state get`, `agent state list`, `agent state clear` (Go: internal/cli/state.go).
use agent_base::gostd::strconv::quote;
use agent_base::util;
use agent_service::AgentNotFoundError;
use agent_store::{StateEntry, Time};
use serde::Serialize;

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::parse_target;
use super::root::Ctx;

// `agent state`

pub const DOC: Doc = Doc {
    use_line: "state",
    short: "Read and write agents' key-value state",
    long: r#"State is a key-value store per agent, for coordination: an agent writes its
progress or results, others read them.

State is stored in this host's database, and belongs to an agent record:
only agents spawned here have state, and cleanup deletes it. Keys starting
with "_" are written by status reports; e.g. "_result" holds a Claude
agent's latest reply (empty while a turn is running)."#,
    example: r#"  agent state set helper progress "3/5 files done"
  agent state get helper _result"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            state_set_def(),
            state_get_def(),
            state_list_def(),
            state_clear_def(),
        ],
    )
}

// `agent state set`

pub const STATE_SET: Doc = Doc {
    use_line: "set <name> <key> <value>",
    short: "Set a state entry for an agent",
    long: r#"Set creates or updates a state entry for an agent spawned on this host.

A coordinator that wasn't spawned with "agent spawn" has no agent record, so
it can't have state: have it pass results through a helper's state or
messages instead."#,
    example: r#"  agent state set researcher status working
  agent state set researcher progress "50%""#,
    args: ArgRule::Exact(3),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent state set`.
#[derive(clap::Args, Debug, Clone)]
pub struct StateSetArgs {
    /// The positional arguments, checked against STATE_SET.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn state_set_def() -> Def {
    leaf!(STATE_SET, StateSetArgs, run_state_set)
}

/// A state entry as --json prints it.
#[derive(Serialize)]
struct StateJsonEntry<'a> {
    agent: &'a str,
    key: &'a str,
    value: &'a str,
    updated_at: Time,
}

fn to_state_json(e: &StateEntry) -> StateJsonEntry<'_> {
    StateJsonEntry {
        agent: &e.agent_name,
        key: &e.key,
        value: &e.value,
        updated_at: e.updated_at,
    }
}

/// Reads a state command's agent argument, which must name an agent on this
/// host: state rows belong to an agent record. State lives in each host's own
/// database and isn't served over the bus, so "name@<other host>" is refused
/// rather than read as a local agent that has no state (Go: stateAgent).
fn state_agent(ctx: &Ctx, arg: &str) -> anyhow::Result<String> {
    let (r, remote) = parse_target(ctx, arg)?;
    if remote {
        anyhow::bail!(
            "state is kept per host, and {r}'s is on {}: read it there, or have the agent send it to you (agent capture {r} shows its screen)",
            quote(&r.host)
        );
    }
    ctx.service()?
        .agent(&r.name)
        .map_err(|e| e.context("state belongs to an agent record"))?;
    Ok(r.name)
}

pub fn run_state_set(ctx: &Ctx, args: &StateSetArgs) -> anyhow::Result<()> {
    let name = match state_agent(ctx, &args.args[0]) {
        Ok(name) => name,
        Err(err) => {
            if let Some(nf) = err.downcast_ref::<AgentNotFoundError>()
                && nf.session_of.is_empty()
                && nf.similar.is_empty()
            {
                anyhow::bail!(
                    "{err:#}; an agent not spawned on this host (e.g. a coordinator started by hand) has no record, so it can't have state: pass results with agent send, or through a helper's state"
                );
            }
            return Err(err);
        }
    };
    let key = &args.args[1];
    let value = &args.args[2];

    ctx.store()?.set_state(&StateEntry {
        agent_name: name.clone(),
        key: key.clone(),
        value: value.clone(),
        updated_at: Time::now(),
    })?;

    ctx.write_out(&format!("Set {name}.{key} = {}.\n", quote(value)))
}

// `agent state get`

pub const STATE_GET: Doc = Doc {
    use_line: "get <name> <key>",
    short: "Print a state value",
    long: r#"Get prints a state value. It exits 1 if the key isn't set, printing nothing
on stdout; a key that is set but empty (e.g. "_result" during a turn) prints
an empty line and exits 0."#,
    example: r#"  agent state get researcher status
  agent state get researcher _result --json"#,
    args: ArgRule::Exact(2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent state get`.
#[derive(clap::Args, Debug, Clone)]
pub struct StateGetArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    /// The positional arguments, checked against STATE_GET.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn state_get_def() -> Def {
    leaf!(STATE_GET, StateGetArgs, run_state_get)
}

pub fn run_state_get(ctx: &Ctx, args: &StateGetArgs) -> anyhow::Result<()> {
    let name = state_agent(ctx, &args.args[0])?;
    let key = &args.args[1];

    let Some(entry) = ctx.store()?.get_state(&name, key)? else {
        return Err(missing_key(ctx, &name, key));
    };

    if args.json {
        return ctx.print_json(&to_state_json(&entry));
    }
    ctx.write_out(&format!("{}\n", entry.value))
}

/// The error for a state key that isn't set: it names close keys, or the keys
/// that are set.
fn missing_key(ctx: &Ctx, name: &str, key: &str) -> anyhow::Error {
    let keys: Vec<String> = ctx
        .store()
        .ok()
        .and_then(|s| s.list_state(name).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|e| e.key)
        .collect();
    let msg = format!("agent {name} has no state {}", quote(key));
    let similar = util::similar(key, &keys);
    if !similar.is_empty() {
        return anyhow::anyhow!("{msg}; did you mean {}?", util::or_list(&similar, None));
    }
    if keys.is_empty() {
        return anyhow::anyhow!("{msg} (it has no state yet)");
    }
    anyhow::anyhow!("{msg} (keys set: {})", keys.join(", "))
}

// `agent state list`

pub const STATE_LIST: Doc = Doc {
    use_line: "list <name>",
    short: "List all state entries for an agent",
    long: r#"List shows an agent's state entries, values cut to 30 characters (--json
has them whole)."#,
    example: r#"  agent state list researcher
  agent state list researcher --json"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent state list`.
#[derive(clap::Args, Debug, Clone)]
pub struct StateListArgs {
    #[arg(long = "json", help = "Print as JSON")]
    pub json: bool,
    /// The positional arguments, checked against STATE_LIST.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn state_list_def() -> Def {
    leaf!(STATE_LIST, StateListArgs, run_state_list)
}

pub fn run_state_list(ctx: &Ctx, args: &StateListArgs) -> anyhow::Result<()> {
    let name = state_agent(ctx, &args.args[0])?;

    let entries = ctx.store()?.list_state(&name)?;

    if args.json {
        let out: Vec<StateJsonEntry> = entries.iter().map(to_state_json).collect();
        return ctx.print_json(&out);
    }

    if entries.is_empty() {
        return ctx.write_out(&format!(
            "No state for agent {name} yet (agent state set {name} <key> <value> adds some).\n"
        ));
    }

    let mut out = format!("{:<20} {:<30} {}\n", "KEY", "VALUE", "UPDATED");
    out.push_str(&"-".repeat(72));
    out.push('\n');
    for entry in &entries {
        let mut value = entry.value.split_whitespace().collect::<Vec<_>>().join(" ");
        if value.chars().count() > 30 {
            value = value.chars().take(27).collect::<String>() + "...";
        }
        out.push_str(&format!(
            "{:<20} {:<30} {}\n",
            entry.key,
            value,
            entry.updated_at.format_rfc3339()
        ));
    }
    ctx.write_out(&out)
}

// `agent state clear`

pub const STATE_CLEAR: Doc = Doc {
    use_line: "clear <name> [key]",
    short: "Remove an agent's state entries, or one key",
    long: r#"Clear removes one key, or all of an agent's own keys. Without a key it keeps
the "_" keys status reports write, since removing them would stop its hooks'
reports from being recorded; name one of them to remove it."#,
    example: r#"  agent state clear researcher
  agent state clear researcher progress"#,
    args: ArgRule::Range(1, 2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent state clear`.
#[derive(clap::Args, Debug, Clone)]
pub struct StateClearArgs {
    /// The positional arguments, checked against STATE_CLEAR.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn state_clear_def() -> Def {
    leaf!(STATE_CLEAR, StateClearArgs, run_state_clear)
}

pub fn run_state_clear(ctx: &Ctx, args: &StateClearArgs) -> anyhow::Result<()> {
    let name = state_agent(ctx, &args.args[0])?;
    let store = ctx.store()?;

    if let Some(key) = args.args.get(1) {
        if store.get_state(&name, key)?.is_none() {
            return Err(missing_key(ctx, &name, key));
        }
        store.delete_state_key(&name, key)?;
        return ctx.write_out(&format!("Cleared {name}.{key}.\n"));
    }

    let entries = store.list_state(&name)?;
    let (mut cleared, mut kept) = (0, 0);
    for e in &entries {
        if e.key.starts_with('_') {
            kept += 1;
            continue;
        }
        store.delete_state_key(&name, &e.key)?;
        cleared += 1;
    }

    let mut line = format!("Cleared {} for agent {name}", plural(cleared, "key"));
    if kept > 0 {
        line += &format!(" (kept {} that status reports write)", plural(kept, "\"_\" key"));
    }
    ctx.write_out(&(line + ".\n"))
}

#[cfg(test)]
#[path = "state_test.rs"]
mod state_test;
