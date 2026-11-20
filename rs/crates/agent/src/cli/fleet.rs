//! `agent fleet` (no Go file: new in the Rust build; docs/FLEET.md, "`agent
//! fleet`"; the work is agent_fleet::fleet). It is also what this host's bus
//! answers op `fleet` with (remote_ops.rs).
use std::time::Duration;

use agent_bus::{FleetHost, FleetView, RpcClient, Time};
use agent_fleet::fleet;
use agent_service::AgentService;

use super::command::{ArgRule, Def, Doc};
use super::remote::{call_remote, canonical_host, is_remote_host, trusted_hosts};
use super::root::Ctx;

// `agent fleet`

pub const DOC: Doc = Doc {
    use_line: "fleet",
    short: "Show the agents of this host and every linked peer, and what needs a look",
    long: r#"Fleet shows one view across this host and every peer its bus trusts,
asked at once (3s each). For each host: which agent has focus (agent
focus), its agents as a tree (as agent list nests them) with flags on the
ones that need a look, its running jobs (agent run), handoff docs older
than their repo's last commit, and its open friction. Sections with
nothing in them are left out.

Each flag is decided from the store and the CLIs' own records:
  stuck: waiting 14m              waiting on a person for over 10 minutes
  stuck: 3 held, oldest 12m       3 or more held messages, or one older than 10 minutes
  ended (quit), still recorded    ended, and not cleaned up
  name mismatch: session is "x"   the CLI's session name or title isn't the agent's name

A peer on an older agent, which doesn't serve fleet, is shown from what
its list answers ("partial: list only"); one that doesn't answer is shown
as unreachable, with why. With no bus running here, fleet shows this host
only. --host shows one host.

--report prints a baseline per host instead: agents by status and by
backend, sessions in the last 7 days by harness, messages sent per day
over 7 days, friction open and resolved by kind, and sessions running for
over 3 days with how often each was compacted.

--json prints the view as one JSON document: {"at", "hosts": [...]}, each
host as docs/FLEET.md describes (with its "report" under --report)."#,
    example: r#"  agent fleet
  agent fleet --host hub
  agent fleet --report
  agent fleet --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent fleet`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Show only this host (default: this host and every trusted peer)",
    )]
    pub host: String,
    #[arg(long = "json", help = "Print the view as one JSON document")]
    pub json: bool,
    #[arg(long = "report", help = "Print each host's baseline numbers instead of its tree")]
    pub report: bool,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let view = if args.host.is_empty() {
        gather_view(ctx, args.report)?
    } else {
        agent_bus::validate_host(&args.host)?;
        let host = canonical_host(ctx, &args.host);
        let one = if is_remote_host(ctx, &host) {
            let opts = fleet::Options {
                report: args.report,
                ..Default::default()
            };
            // One host asked for by name: one that doesn't answer (or that
            // nobody knows) is the command's error, naming the hosts this
            // one knows, as for any name@host.
            call_remote(ctx, &host, |c| fleet::try_query_host(c, &host, &opts))?
        } else {
            local_host(ctx.service()?, args.report)?
        };
        FleetView {
            at: Time::now(),
            hosts: vec![one],
        }
    };
    if args.json {
        return ctx.print_json(&view);
    }
    if args.report {
        return ctx.write_out(&fleet::render_report(&view));
    }
    ctx.write_out(&fleet::render(&view))
}

/// This host and every trusted peer, each peer asked through the bus. With no
/// bus running, this host alone, said in a hint.
fn gather_view(ctx: &Ctx, report: bool) -> anyhow::Result<FleetView> {
    let local = local_host(ctx.service()?, report)?;
    let opts = fleet::Options {
        report,
        ..Default::default()
    };
    let peers = trusted_hosts(ctx);
    match ctx.bus_connect() {
        Ok(nc) => {
            let rpc = RpcClient::new(nc.clone(), Duration::ZERO);
            let view = fleet::gather(local, Some(&rpc), &peers, &opts);
            nc.close();
            Ok(view)
        }
        Err(e) if agent_bus::is_no_bus(&e) => {
            let hint = if peers.is_empty() {
                "no agent bus runs here, so this is this host only (agent bus start links it to other hosts)".to_string()
            } else {
                format!(
                    "no agent bus runs here, so this is this host only, without {} (agent bus start)",
                    peers.join(", ")
                )
            };
            ctx.hint(hint);
            Ok(fleet::gather(local, None, &peers, &opts))
        }
        Err(e) => Err(e.context("connecting to the agent bus")),
    }
}

/// This host's part of the view (fleet::local reads each agent's CLI
/// session name back, so a name that isn't the agent's is flagged). The
/// bus's fleet op answers with the same (fleet::rpc_handler).
fn local_host(svc: &AgentService, report: bool) -> anyhow::Result<FleetHost> {
    let opts = fleet::Options {
        report,
        ..Default::default()
    };
    fleet::local(svc, svc.storage(), svc.config(), &opts)
}

#[cfg(test)]
#[path = "fleet_test.rs"]
pub(crate) mod fleet_test;
