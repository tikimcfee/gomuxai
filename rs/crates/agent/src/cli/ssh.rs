//! `agent bus ssh` (Go: internal/cli/ssh.go).
use std::collections::BTreeMap;
use std::sync::OnceLock;

use agent_base::gostd::strconv::quote;
use agent_base::gostd::{filepath, os};
use agent_base::util::shell_quote;
use agent_bus::{AgentRef, Context};
use agent_service::AGENT_STATUS_ENDED;
use anyhow::Context as _;
use regex::Regex;

use super::attach::gone_attach_error;
use super::command::{ArgRule, Def, Doc};
use super::exec::exec_into;
use super::output::plural;
use super::remote::{call_remote, canonical_host, trusted_hosts};
use super::root::{Ctx, stdin_is_terminal};

// `agent bus ssh`

pub const BUS_SSH: Doc = Doc {
    use_line: "ssh <host> [<destination>]",
    short: "Show or set where agent attach name@host runs ssh to",
    long: r#"The bus can't carry a terminal, so "agent attach name@host" (and
"agent resume name@host --attach") runs ssh to that host, and runs the
host's own agent binary there: ssh -t <destination> <its binary> attach
<name>. The host says where its binary is, through the bus.

The destination defaults to the host's name. Set one per host when ssh
knows it by another name or needs a user: "me@hub.lan", or an alias from
~/.ssh/config, which still applies (ports, keys, jump hosts). It is plain
config, not a secret, kept in this host's bus/ssh.json; agent bus status
shows it.

ssh is a separate trust path from the bus: it uses your ssh keys, not the
bus's pinned ones, so attaching to another host needs ssh to work there
first ("ssh <destination>" logs in).

With only a host, it prints the destination. --clear goes back to the
default."#,
    example: r#"  agent bus ssh hub
  agent bus ssh hub me@hub.lan
  agent bus ssh hub --clear"#,
    args: ArgRule::Range(1, 2),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus ssh`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusSshArgs {
    #[arg(
        long = "clear",
        help = "Go back to the default destination: the host's name"
    )]
    pub clear: bool,
    /// The positional arguments, checked against BUS_SSH.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_ssh_def() -> Def {
    leaf!(BUS_SSH, BusSshArgs, run_bus_ssh)
}

pub fn run_bus_ssh(ctx: &Ctx, args: &BusSshArgs) -> anyhow::Result<()> {
    let args_v = &args.args;
    agent_bus::validate_host(&args_v[0])?;
    let host = canonical_host(ctx, &args_v[0]);
    if host == ctx.cfg().host {
        anyhow::bail!("{host} is this host: agent attach <name> opens its agents without ssh");
    }
    let mut dests = load_ssh_dests(ctx)?;
    if args.clear && args_v.len() == 2 {
        anyhow::bail!("--clear goes back to the default: give it no destination");
    } else if args.clear {
        dests.remove(&host);
        save_ssh_dests(ctx, &dests)?;
        ctx.write_out(&format!(
            "agent attach name@{host} runs ssh to {host} (the default: its host name).\n"
        ))?;
    } else if args_v.len() == 2 {
        validate_ssh_dest(&args_v[1])?;
        dests.insert(host.clone(), args_v[1].clone());
        save_ssh_dests(ctx, &dests)?;
        ctx.write_out(&format!(
            "agent attach name@{host} runs ssh to {}.\n",
            args_v[1]
        ))?;
    } else {
        match dests.get(&host) {
            Some(d) => ctx.write_out(&format!("{host}: ssh to {d}\n"))?,
            None => ctx.write_out(&format!(
                "{host}: ssh to {host} (the default: its host name)\n"
            ))?,
        }
        return Ok(());
    }
    if !trusted_hosts(ctx).contains(&host) {
        ctx.hint(format!(
            "this host's bus doesn't trust {host} yet, and attach finds its agent binary through the bus (agent bus trust <bundle>)"
        ));
    }
    Ok(())
}

/// What a destination may be: a host name, user@host, an alias, or an IPv6
/// address in brackets. It never starts with "-", which ssh would take for an
/// option (Go: sshDestPattern).
fn ssh_dest_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z0-9_\[][A-Za-z0-9_.@:%\[\]-]*$").expect("a valid pattern")
    })
}

pub fn validate_ssh_dest(dest: &str) -> anyhow::Result<()> {
    if !ssh_dest_pattern().is_match(dest) {
        anyhow::bail!(
            "invalid ssh destination {}: give a host, user@host, or an alias from ~/.ssh/config (letters, digits and ._@:%[]-, not starting with -)",
            quote(dest)
        );
    }
    Ok(())
}

fn ssh_dests_path(ctx: &Ctx) -> String {
    filepath::join(&[ctx.cfg().bus_dir().as_str(), "ssh.json"])
}

/// The destinations set per host; none set is empty (Go: loadSSHDests).
pub fn load_ssh_dests(ctx: &Ctx) -> anyhow::Result<BTreeMap<String, String>> {
    let path = ssh_dests_path(ctx);
    let data = match os::read_file(&path) {
        Ok(d) => d,
        Err(e) => {
            let e = anyhow::Error::from(e);
            if os::is_not_exist(&e) {
                return Ok(BTreeMap::new());
            }
            return Err(e.context("reading the ssh destinations"));
        }
    };
    // Go: json.Unmarshal into a map, which takes "null" as empty.
    let dests: Option<BTreeMap<String, String>> = serde_json::from_slice(&data)
        .map_err(|e| anyhow::anyhow!("reading the ssh destinations in {path}: {e}"))?;
    Ok(dests.unwrap_or_default())
}

pub fn save_ssh_dests(ctx: &Ctx, dests: &BTreeMap<String, String>) -> anyhow::Result<()> {
    os::mkdir_all(&ctx.cfg().bus_dir(), 0o700)?;
    let mut data = serde_json::to_string_pretty(dests)?;
    data.push('\n');
    os::write_file(&ssh_dests_path(ctx), data.as_bytes(), 0o644)?;
    Ok(())
}

/// Where ssh to host goes: its destination if one is set (and still
/// valid), else its name (Go: sshDestFor).
pub fn ssh_dest_for(ctx: &Ctx, host: &str) -> String {
    match load_ssh_dests(ctx).unwrap_or_default().get(host) {
        Some(d) if validate_ssh_dest(d).is_ok() => d.clone(),
        _ => host.to_string(),
    }
}

/// Whether this terminal can be handed over (Go: attachTerminal); tests say
/// with ctx.attach_terminal.
fn attach_terminal(ctx: &Ctx) -> bool {
    ctx.attach_terminal.unwrap_or_else(stdin_is_terminal)
}

/// Replaces this process with ssh to r.host, running that host's agent
/// binary there with verb and r.name, then extra: "attach <name>", or
/// "resume <name> --attach". The host says through the bus where its binary
/// is, and that the agent exists (Go: sshAttach).
pub fn ssh_attach(ctx: &Ctx, r: &AgentRef, verb: &str, extra: &[&str]) -> anyhow::Result<()> {
    if !attach_terminal(ctx) {
        anyhow::bail!(
            "{verb} {r} opens its session in this terminal, over ssh, so it needs one: run it in a terminal (agent capture {r} shows its screen from here)"
        );
    }
    let resp = call_remote(ctx, &r.host, |c| {
        c.list_host(&Context::background(), &r.host)
    })
    .with_context(|| format!("asking {} where its agent binary is", r.host))?;
    let Some(a) = resp.agents.iter().find(|a| a.name == r.name) else {
        anyhow::bail!(
            "agent {} not found on host {} ({} there: agent list --host {})",
            quote(&r.name),
            r.host,
            plural(resp.agents.len(), "agent"),
            r.host
        );
    };
    // Attaching to an agent whose session is gone would only reach tmux's
    // "can't find session" over there, after ssh. Resuming it is another
    // matter: that starts the session first. (A host that predates
    // Attached leaves it out, and ssh goes ahead as before.)
    if verb == "attach" && a.attached == Some(false) {
        let reason = if a.status == AGENT_STATUS_ENDED.as_str() {
            a.end_reason.as_str()
        } else {
            ""
        };
        return Err(gone_attach_error(
            &r.to_string(),
            &a.session,
            reason,
            a.resumable == Some(true),
        ));
    }
    let bin = &resp.agent_binary;
    if bin.is_empty() {
        anyhow::bail!(
            "{}'s agent predates attach over ssh and doesn't say where its binary is: update it there, or ssh {} and run agent {verb} {}",
            r.host,
            ssh_dest_for(ctx, &r.host),
            r.name
        );
    }
    agent_bus::validate_agent_binary(bin)
        .with_context(|| format!("won't run {}'s agent over ssh", r.host))?;
    let dest = ssh_dest_for(ctx, &r.host);
    let mut words = vec![shell_quote(bin), verb.to_string(), shell_quote(&r.name)];
    words.extend(extra.iter().map(|s| s.to_string()));
    let remote = words.join(" ");
    ctx.notice(format!(
        "Attaching to {r} over ssh to {dest}. That is ssh, not the bus: it needs \"ssh {dest}\" to work from here (agent bus ssh {} <destination> changes where). Ctrl-\\ twice detaches.",
        r.host
    ));
    let argv: Vec<String> = ["ssh", "-t", "--", &dest, &remote]
        .iter()
        .map(|s| s.to_string())
        .collect();
    exec_into(ctx, "ssh", &argv)
        .with_context(|| format!("attaching to {r} runs ssh to {dest}, which didn't start"))
}
