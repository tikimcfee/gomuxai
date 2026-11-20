//! Agents on other hosts: "name@host" arguments, and requests through this
//! host's bus (Go: internal/cli/remote.go).

use agent_base::gostd::strconv::quote;
use agent_bus::{AgentRef, NoRespondersError, RpcClient};

use super::root::Ctx;

/// Reads an agent argument that may name another host ("name@host"). remote
/// is false for an argument without "@" (returned as is, so local behavior is
/// untouched) and for "name@<this host>" (returned as the bare name) (Go:
/// parseTarget).
pub fn parse_target(ctx: &Ctx, arg: &str) -> anyhow::Result<(AgentRef, bool)> {
    if !arg.contains('@') {
        return Ok((
            AgentRef {
                name: arg.to_string(),
                host: String::new(),
            },
            false,
        ));
    }
    let mut r = agent_bus::parse_agent_ref(arg)?;
    r.host = canonical_host(ctx, &r.host);
    if r.host == ctx.cfg().host {
        return Ok((
            AgentRef {
                name: r.name,
                host: String::new(),
            },
            false,
        ));
    }
    Ok((r, true))
}

/// Reports whether host names another host (Go: isRemoteHost).
pub fn is_remote_host(ctx: &Ctx, host: &str) -> bool {
    !host.is_empty() && canonical_host(ctx, host) != ctx.cfg().host
}

/// Returns host as the bus spells it: this host's name, or a trusted peer's,
/// when host matches one ignoring case, or a name either had before it was
/// renamed; any other host as given. Host names are matched
/// case-insensitively ("dev-box-m2" is "Dev-Box-M2"), but bus subjects are
/// case-sensitive, so a request must go out under the host's own spelling. A
/// host this one doesn't trust directly (another host linked through the hub)
/// is only known by the spelling it is given (Go: canonicalHost).
pub fn canonical_host(ctx: &Ctx, host: &str) -> String {
    let cfg = ctx.cfg();
    agent_bus::canonical_host(cfg.bus_dir(), &cfg.host, host)
}

/// The hosts this host's bus trusts (agent bus trust), or none if it can't
/// read them (Go: trustedHosts).
pub fn trusted_hosts(ctx: &Ctx) -> Vec<String> {
    match agent_bus::load_peers(ctx.cfg().bus_dir()) {
        Ok(peers) => peers.into_iter().map(|p| p.host).collect(),
        Err(_) => Vec::new(),
    }
}

/// Says which host names this host knows, for an error about a host no bus
/// answered for (Go: knownHosts).
pub fn known_hosts(ctx: &Ctx) -> String {
    let mut s = format!("this host is {}", ctx.cfg().host);
    let mut peers = trusted_hosts(ctx);
    if !peers.is_empty() {
        peers.sort();
        s += &format!(", and its bus trusts {}", peers.join(", "));
    }
    s
}

/// Runs f with an RPC client connected through this host's bus, for a request
/// to an agent on another host (Go: callRemote).
pub fn call_remote<T>(
    ctx: &Ctx,
    host: &str,
    f: impl FnOnce(&RpcClient) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let nc = match ctx.bus_connect() {
        Ok(nc) => nc,
        Err(e) if agent_bus::is_no_bus(&e) => {
            return Err(e.context(format!(
                "{} is not this host ({}); reaching agents on other hosts needs a running agent bus",
                quote(host),
                quote(&ctx.cfg().host)
            )));
        }
        Err(e) => return Err(e.context("connecting to the agent bus")),
    };
    let client = RpcClient::new(nc.clone(), std::time::Duration::ZERO);
    let res = f(&client);
    nc.close();
    res.map_err(|e| {
        let no_bus = e
            .chain()
            .find_map(|c| c.downcast_ref::<NoRespondersError>())
            .cloned();
        match no_bus {
            Some(no_responders) => NoBusError {
                text: format!("{e:#}; {}", known_hosts(ctx)),
                no_responders,
            }
            .into(),
            None => e,
        }
    })
}

/// A no-responders error worded as Go's fmt.Errorf("%w; %s", err,
/// knownHosts()): the error's own text, then what this host knows. Find it
/// with is_no_responders (this module's), which also finds a bare
/// NoRespondersError.
#[derive(Debug, thiserror::Error)]
#[error("{text}")]
pub struct NoBusError {
    pub text: String,
    pub no_responders: NoRespondersError,
}

/// Go's errors.As(err, &*natsbus.NoRespondersError) for an error call_remote
/// returned.
pub fn is_no_responders(err: &anyhow::Error) -> bool {
    agent_bus::is_no_responders(err)
        || err
            .chain()
            .any(|c| c.downcast_ref::<NoBusError>().is_some())
}
