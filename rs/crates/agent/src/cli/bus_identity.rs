//! `agent bus untrust`, `agent bus rename`, `agent bus restart`, and `agent bus
//! trust`'s run (Go: internal/cli/bus_identity.go).
//!
//! Changing trust, and renaming this host. nats-server can't reload a leaf
//! listener's pinned certificates or a leaf's remotes (its config reload
//! refuses changes to the leafnode block), so a trust change applies when the
//! bus restarts: untrust and trust --renames restart it themselves.

use std::cell::RefCell;
use std::fmt;
use std::io::Read as _;
use std::time::Duration;

use agent_base::gostd::{filepath, os, strconv::quote};
use agent_bus::{PeerBundle, SameKeysError};
use clap::ArgAction;
use serde::{Deserialize, Deserializer, Serialize};

use super::bus::{
    BusTrustArgs, bus_run_command, bus_session, bus_set_up, bus_status, check_own_bus_session,
    pause_bus_session, query_bus, run_bus_in, write_bundle,
};
use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::output::plural;
use super::remote::canonical_host;
use super::root::Ctx;
use super::ssh::{load_ssh_dests, save_ssh_dests};

// `agent bus untrust`

pub const BUS_UNTRUST: Doc = Doc {
    use_line: "untrust <host>",
    short: "Stop trusting a peer host",
    long: r#"Untrust removes a peer's bundle (bus/peers/<host>.json) and its ssh
destination (agent bus ssh), then restarts the bus so the peer is dropped
at once (--no-restart leaves that to agent bus restart). Friction links
and agents' spawned_by that name it stay, as history.

Untrusting on one side is enough to cut the link: the hub refuses a peer
it doesn't pin, and a leaf no longer dials a hub it doesn't trust. The
other side keeps listing this host until it untrusts it too."#,
    example: r#"  agent bus untrust old-laptop
  agent bus untrust hub --no-restart"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus untrust`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusUntrustArgs {
    #[arg(
        long = "no-restart",
        help = "Don't restart the bus; agent bus restart applies the change"
    )]
    pub no_restart: bool,
    /// The positional arguments, checked against BUS_UNTRUST.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_untrust_def() -> Def {
    leaf!(BUS_UNTRUST, BusUntrustArgs, run_bus_untrust)
}

// `agent bus rename`

pub const BUS_RENAME: Doc = Doc {
    use_line: "rename <new-name>",
    short: "Rename this host on the bus, keeping its keys",
    long: r#"Rename binds this host's bus identity to a new name. The keys stay:
peers pin the certificate's public key and the nkey, neither of which
names the host, so they can check the renamed host is the same machine.
The certificate is signed again under the new name with the same key,
the old name is kept in bus/host.previous, and bus/<new>.pub.json
replaces bus/<old>.pub.json (its url is --url, else the old bundle's).

A running bus is stopped, and started again as the new name with the
same settings. Agents spawned before the rename keep AGENT_HOST=<old>,
which now means the new name, and replies to <name>@<old> stay here.

Each peer then re-trusts this host: agent bus trust <new>.pub.json
--renames <old>, which rename prints. Until the hub has, this host's link
comes up (its keys are the same), but requests to <name>@<new> don't
reach it."#,
    example: r#"  agent bus rename studio
  agent bus rename hub2 --url tls://hub2.lan:7422   # on the hub"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus rename`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusRenameArgs {
    #[arg(
        long = "url",
        value_name = "string",
        default_value = "",
        help = "Where this host accepts peer links, if it is the hub (default: the old bundle's)"
    )]
    pub url: String,
    /// The positional arguments, checked against BUS_RENAME.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_rename_def() -> Def {
    leaf!(BUS_RENAME, BusRenameArgs, run_bus_rename)
}

// `agent bus restart`

pub const BUS_RESTART: Doc = Doc {
    use_line: "restart",
    short: "Restart the background bus with the settings it runs with",
    long: r#"Restart stops the bus in session gomux-bus and starts it
again with the --listen, --client-port and --remote-ops it was running
with: what agent bus trust needs to apply.

Give any of those flags to change just that setting, e.g. --remote-ops
with the whole list peers may call. The settings it restarts with are
saved, as agent bus start saves them, so a later agent bus start (after a
reboot, say) runs the same way."#,
    example: r#"  agent bus restart
  agent bus restart --remote-ops list,status,capture,send,respond,friction,logs,fleet,template,resume"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus restart`: unset means "as it runs".
#[derive(clap::Args, Debug, Clone)]
pub struct BusRestartArgs {
    #[arg(
        long = "client-port",
        value_name = "int",
        default_value = "0",
        value_parser = flags::parse_int,
        help = "Local client port (127.0.0.1) instead",
    )]
    pub client_port: i64,
    #[arg(
        long = "listen",
        value_name = "string",
        default_value = "",
        help = r#"Accept peer links on this address instead ("" stops accepting them)"#
    )]
    pub listen: String,
    #[arg(
        long = "remote-ops",
        value_name = "strings",
        value_delimiter = ',',
        action = ArgAction::Append,
        help = "The ops peers may call on this host instead: the whole list",
    )]
    pub remote_ops: Vec<String>,
    /// The positional arguments, checked against BUS_RESTART.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_restart_def() -> Def {
    leaf!(BUS_RESTART, BusRestartArgs, run_bus_restart)
}

fn null_as_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

/// How a bus runs: what agent bus start was given. agent bus start and
/// restart save them (bus/settings.json), and start reuses them for the flags
/// it isn't given.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
pub struct BusSettings {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub listen: String,
    #[serde(default)]
    pub client_port: i64,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub remote_ops: Vec<String>,
}

impl fmt::Display for BusSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.listen.is_empty() {
            write!(f, "--listen {} ", self.listen)?;
        }
        write!(
            f,
            "--client-port {} --remote-ops {}",
            self.client_port,
            self.remote_ops.join(",")
        )
    }
}

pub fn bus_settings_path(ctx: &Ctx) -> String {
    filepath::join(&[&ctx.cfg().bus_dir(), "settings.json"])
}

/// Reads the saved settings: None if none were saved.
pub fn load_bus_settings(ctx: &Ctx) -> anyhow::Result<Option<BusSettings>> {
    let path = bus_settings_path(ctx);
    let data = match os::read_file(&path) {
        Ok(d) => d,
        Err(e) if e.err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::new(e).context("reading the bus's saved settings"));
        }
    };
    let s: BusSettings = serde_json::from_slice(&data).map_err(|e| {
        anyhow::anyhow!(
            "reading the bus's saved settings in {path}: {e} (fix or delete it; agent bus start then saves new ones)"
        )
    })?;
    Ok(Some(s))
}

pub fn save_bus_settings(ctx: &Ctx, s: &BusSettings) -> anyhow::Result<()> {
    let data = serde_json::to_string_pretty(s)? + "\n";
    os::write_file(&bus_settings_path(ctx), data.as_bytes(), 0o644)?;
    Ok(())
}

/// Refuses settings the bus would fail to start with, before a restart stops
/// the running one.
pub fn check_bus_settings(s: &BusSettings) -> anyhow::Result<()> {
    for op in &s.remote_ops {
        if !agent_bus::ALL_OPS.contains(&op.as_str()) {
            anyhow::bail!(
                "unknown op {} in --remote-ops: the ops are {}",
                quote(op),
                agent_bus::ALL_OPS.join(", ")
            );
        }
    }
    if s.client_port < -1 || s.client_port > 65535 {
        anyhow::bail!("--client-port {} isn't a port", s.client_port);
    }
    Ok(())
}

/// The port of a URL like "tls://127.0.0.1:7420", 0 if it has none.
fn url_port(url: &str) -> i64 {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split('/').next().unwrap_or_default();
    authority
        .rsplit_once(':')
        .filter(|(h, _)| !h.ends_with(']') || authority.starts_with('['))
        .and_then(|(_, p)| p.parse().ok())
        .unwrap_or(0)
}

/// Asks this host's bus how it runs: None if no bus is running.
pub fn running_bus_settings(ctx: &Ctx) -> anyhow::Result<Option<BusSettings>> {
    let info = match query_bus(ctx, Duration::from_secs(2)) {
        Err(e) if agent_bus::is_no_bus(&e) => return Ok(None),
        Err(e) => return Err(e),
        Ok(i) => i,
    };
    let mut s = BusSettings {
        listen: info.listen,
        client_port: info.client_port,
        remote_ops: info.remote_ops,
    };
    // A bus built before the reply carried them: where it accepts links, and
    // the port of its client URL, are what it was started with
    // (--client-port -1 aside).
    if s.listen.is_empty() {
        s.listen = info.leaf_addr;
    }
    if s.client_port == 0 {
        s.client_port = url_port(&info.client_url);
    }
    Ok(Some(s))
}

/// Stops the bus in its session and starts it again with s in the same
/// session, which never closes (see pause_bus_session). A bus that runs
/// elsewhere (agent bus run) isn't this command's to stop.
pub fn restart_bus(ctx: &Ctx, s: &BusSettings) -> anyhow::Result<()> {
    check_bus_in_session(ctx)?;
    // Check it can start before stopping it.
    let host = ctx.cfg().host.clone();
    bus_run_command(ctx, &host, s)?;
    pause_bus_session(ctx)?;
    start_bus_with(ctx, &host, s)
}

/// Starts the bus as host with s in its session, which pause_bus_session
/// left with a fresh shell.
fn start_bus_with(ctx: &Ctx, host: &str, s: &BusSettings) -> anyhow::Result<()> {
    let run = bus_run_command(ctx, host, s)?;
    run_bus_in(ctx, host, &run)
}

/// Fails unless this data dir's bus runs in its session, where restart can
/// stop and start it.
fn check_bus_in_session(ctx: &Ctx) -> anyhow::Result<()> {
    let session = bus_session();
    if !ctx.termd()?.session_exists(&session)? {
        anyhow::bail!(
            "the bus is running, but not in session {session} (started with agent bus run?): restart it where it runs (Ctrl-C, then the same command)"
        );
    }
    check_own_bus_session(ctx)
}

/// A test's stand-in for restart_bus.
type RestartFn = Box<dyn Fn(&BusSettings) -> anyhow::Result<()>>;

thread_local! {
    static TEST_RESTART_BUS: RefCell<Option<RestartFn>> = const { RefCell::new(None) };
}

/// Makes this test thread's trust changes call f instead of restarting the
/// bus (Go: tests assigning restartBusFn).
#[cfg(test)]
pub fn set_test_restart_bus(f: Option<RestartFn>) {
    TEST_RESTART_BUS.with(|r| *r.borrow_mut() = f);
}

/// Restarts the bus for a trust change (Go: restartBusFn).
fn restart_bus_fn(ctx: &Ctx, s: &BusSettings) -> anyhow::Result<()> {
    let faked = TEST_RESTART_BUS.with(|r| r.borrow().as_ref().map(|f| f(s)));
    match faked {
        Some(res) => res,
        None => restart_bus(ctx, s),
    }
}

pub fn run_bus_restart(ctx: &Ctx, args: &BusRestartArgs) -> anyhow::Result<()> {
    bus_restart(ctx, args, &|f| ctx.changed(f))
}

/// agent bus restart, with changed saying which flags were given
/// (ctx.changed).
pub fn bus_restart(
    ctx: &Ctx,
    args: &BusRestartArgs,
    changed: &dyn Fn(&str) -> bool,
) -> anyhow::Result<()> {
    let Some(mut s) = running_bus_settings(ctx)? else {
        anyhow::bail!(
            "no bus is running on host {}, so there is nothing to restart: agent bus start (with the saved settings, if any; add --listen 0.0.0.0:7422 on the hub)",
            ctx.cfg().host
        );
    };
    if changed("listen") {
        s.listen = args.listen.clone();
    }
    if changed("client-port") {
        s.client_port = args.client_port;
    }
    if changed("remote-ops") {
        s.remote_ops = args.remote_ops.clone();
    }
    check_bus_settings(&s)?;
    restart_bus_fn(ctx, &s)?;
    if let Err(e) = save_bus_settings(ctx, &s) {
        return Err(e.context(format!(
            "the bus restarted ({s}), but saving its settings failed"
        )));
    }
    ctx.write_out(&format!(
        "Bus restarted in session {} ({s}); settings saved for agent bus start\n",
        quote(&bus_session())
    ))?;
    bus_status(ctx, false)
}

/// Restarts a running bus so a trust change takes effect, unless --no-restart,
/// and says what happened.
fn apply_trust_change(ctx: &Ctx, no_restart: bool) -> anyhow::Result<()> {
    match running_bus_settings(ctx) {
        Err(e) => ctx.write_out(&format!(
            "Couldn't ask the bus how it runs ({e:#}): agent bus restart applies the change.\n"
        )),
        Ok(None) => ctx.write_out("The bus isn't running; agent bus start applies the change.\n"),
        Ok(Some(_)) if no_restart => ctx.write_out("Restart the bus to apply: agent bus restart\n"),
        Ok(Some(s)) => {
            if let Err(e) = restart_bus_fn(ctx, &s) {
                return Err(e.context("the change is saved, but restarting the bus failed"));
            }
            ctx.write_out(&format!(
                "Restarted the bus ({s}), so the change applies now.\n"
            ))
        }
    }
}

pub fn run_bus_trust(ctx: &Ctx, args: &BusTrustArgs) -> anyhow::Result<()> {
    let file = &args.args[0];
    let data = if file == "-" {
        let mut buf = Vec::new();
        std::io::stdin()
            .read_to_end(&mut buf)
            .map_err(|e| anyhow::anyhow!("read /dev/stdin: {e}"))?;
        buf
    } else {
        os::read_file(file)?
    };
    let mut b: PeerBundle =
        serde_json::from_slice(&data).map_err(|e| anyhow::anyhow!("reading bundle: {e}"))?;
    let cfg = ctx.cfg();
    if b.host.to_lowercase() == cfg.host.to_lowercase() {
        anyhow::bail!("bundle is for this host ({}), not a peer", quote(&b.host));
    }
    b.aka = Vec::new(); // only --renames gives a peer another name
    if !args.renames.is_empty() {
        return trust_renamed(ctx, &args.renames, args.no_restart, b);
    }
    if args.no_restart {
        anyhow::bail!(
            "--no-restart goes with --renames: a plain trust doesn't restart the bus (agent bus restart does)"
        );
    }

    if let Err(e) = agent_bus::trust_peer(cfg.bus_dir(), b.clone()) {
        if let Some(same) = e.downcast_ref::<SameKeysError>() {
            anyhow::bail!("{e:#}: agent bus trust {file} --renames {}", same.peer);
        }
        return Err(e);
    }
    let mut out = format!(
        "Trusted {}: key {}, certificate pin {}\n",
        quote(&b.host),
        b.nkey,
        b.cert_sha
    );
    if !b.url.is_empty() {
        out += &format!(
            "This host will link to it at {} (unless it is the hub).\n",
            b.url
        );
    }
    out += "Restart the bus to apply: agent bus restart\n";
    ctx.write_out(&out)
}

/// trust --renames: b is a trusted peer's bundle under the name it was
/// renamed to.
fn trust_renamed(ctx: &Ctx, renames: &str, no_restart: bool, b: PeerBundle) -> anyhow::Result<()> {
    agent_bus::validate_host(renames)?;
    let (installed, replaced) = agent_bus::rename_peer(ctx.cfg().bus_dir(), renames, b.clone())?;
    let old = replaced.host;
    let mut out = format!(
        "Trusted {} as {old} renamed: same key {}, certificate pin {}\n",
        quote(&b.host),
        b.nkey,
        b.cert_sha
    );
    if !b.url.is_empty() {
        out += &format!(
            "This host will link to it at {} (unless it is the hub).\n",
            b.url
        );
    }
    ctx.write_out(&out)?;

    let n = ctx.store()?.rename_host(&old, &b.host).map_err(|e| {
        e.context(format!(
            "the new bundle is trusted, but updating references to {old} failed"
        ))
    })?;
    let moved = move_ssh_dest(ctx, &old, &b.host).map_err(|e| {
        e.context(format!(
            "the new bundle is trusted, but moving {old}'s ssh destination failed"
        ))
    })?;
    let mut updated = Vec::new();
    if n.agents > 0 {
        updated.push(format!(
            "the spawned_by of {}",
            plural(n.agents as usize, "agent")
        ));
    }
    if n.friction_links > 0 {
        updated.push(plural(n.friction_links as usize, "friction link"));
    }
    if !moved.is_empty() {
        updated.push(format!("its ssh destination ({moved})"));
    }
    let mut out = String::new();
    if !updated.is_empty() {
        out += &format!(
            "Now naming {} instead of {old}: {}\n",
            b.host,
            updated.join(", ")
        );
    }
    if !installed.aka.is_empty() {
        out += &format!(
            "{} still reaches it (an alias).\n",
            installed.aka.join(", ")
        );
    }
    ctx.write_out(&out)?;
    apply_trust_change(ctx, no_restart)
}

/// Moves old's ssh destination to new, unless new has its own, returning the
/// destination moved ("" if none).
fn move_ssh_dest(ctx: &Ctx, old: &str, new: &str) -> anyhow::Result<String> {
    let mut dests = load_ssh_dests(ctx)?;
    let olds: Vec<String> = dests
        .keys()
        .filter(|h| h.eq_ignore_ascii_case(old))
        .cloned()
        .collect();
    let mut dest = String::new();
    for h in olds {
        if let Some(d) = dests.remove(&h) {
            dest = d;
        }
    }
    if dest.is_empty() {
        return Ok(String::new());
    }
    if !dests.contains_key(new) {
        dests.insert(new.to_string(), dest.clone());
    }
    save_ssh_dests(ctx, &dests)?;
    Ok(dest)
}

pub fn run_bus_untrust(ctx: &Ctx, args: &BusUntrustArgs) -> anyhow::Result<()> {
    let host = &args.args[0];
    agent_bus::validate_host(host)?;
    let cfg = ctx.cfg();
    if canonical_host(ctx, host) == cfg.host {
        anyhow::bail!("{host} is this host, not a peer");
    }
    let b = agent_bus::untrust_peer(cfg.bus_dir(), host)?;
    ctx.write_out(&format!(
        "Untrusted {} (key {}, certificate pin {})\n",
        quote(&b.host),
        b.nkey,
        b.cert_sha
    ))?;

    let mut dests = load_ssh_dests(ctx)?;
    let named: Vec<(String, String)> = dests
        .iter()
        .filter(|(h, _)| b.names(h))
        .map(|(h, d)| (h.clone(), d.clone()))
        .collect();
    for (h, d) in named {
        dests.remove(&h);
        save_ssh_dests(ctx, &dests)?;
        ctx.write_out(&format!("Removed its ssh destination ({d}).\n"))?;
    }

    if !b.url.is_empty() {
        let s = running_bus_settings(ctx).ok().flatten();
        if s.is_none_or(|s| s.listen.is_empty()) {
            ctx.write_out(&format!(
                "{} was the hub this host links to ({}): this host now reaches no other host. To link again: agent bus trust <hub>.pub.json\n",
                b.host, b.url
            ))?;
        }
    }
    apply_trust_change(ctx, args.no_restart)?;
    ctx.write_out(&format!(
        "{} still trusts this host until it untrusts it too: on {}, agent bus untrust {}\n",
        b.host, b.host, cfg.host
    ))
}

pub fn run_bus_rename(ctx: &Ctx, args: &BusRenameArgs) -> anyhow::Result<()> {
    let cfg = ctx.cfg();
    let dir = cfg.bus_dir();
    let (old, new_name) = (cfg.host.clone(), args.args[0].clone());
    if !bus_set_up(ctx) {
        anyhow::bail!("this host ({old}) has no bus identity to rename: agent bus init --help");
    }
    agent_bus::check_rename(&dir, &old, &new_name)?;
    let mut peer_url = args.url.clone();
    let old_bundle = filepath::join(&[&dir, &format!("{old}.pub.json")]);
    if peer_url.is_empty()
        && let Ok(data) = os::read_file(&old_bundle)
        && let Ok(b) = serde_json::from_slice::<PeerBundle>(&data)
    {
        peer_url = b.url;
    }

    let running = running_bus_settings(ctx)?;
    if running.is_some() {
        check_bus_in_session(ctx).map_err(|e| e.context("rename stops and starts the bus"))?;
        pause_bus_session(ctx)?;
    }

    let id = match agent_bus::rename_identity(&dir, &old, &new_name) {
        Ok(id) => id,
        Err(e) => {
            if let Some(s) = &running
                && let Err(rerr) = start_bus_with(ctx, &old, s)
            {
                anyhow::bail!("{e:#} (and starting the bus again as {old} failed: {rerr:#})");
            }
            return Err(e);
        }
    };
    let path = write_bundle(ctx, &id, &peer_url)?;
    if old_bundle != path {
        let _ = std::fs::remove_file(&old_bundle);
    }
    let mut out = format!(
        "Renamed this host's bus identity from {} to {} in {dir}\n",
        quote(&old),
        quote(&id.host)
    );
    out += &format!(
        "  same key {}, certificate pin {}\n",
        id.public_bundle("").nkey,
        id.cert_sha
    );
    out += &format!("Public bundle: {path}\n");
    ctx.write_out(&out)?;
    if let Some(s) = &running {
        if let Err(e) = start_bus_with(ctx, &id.host, s) {
            return Err(e.context(format!(
                "renamed, but starting the bus as {} failed",
                id.host
            )));
        }
        ctx.write_out(&format!("Restarted the bus as {} ({s}).\n", id.host))?;
    }
    ctx.write_out(&format!(
        "Agents spawned before the rename keep AGENT_HOST={old}, which now means {}.\n",
        id.host
    ))?;

    let mut peers = agent_bus::load_peers(&dir).unwrap_or_default();
    if peers.is_empty() {
        return ctx.write_out("No trusted peers to tell.\n");
    }
    let mut out = format!(
        "Each peer re-trusts this host under its new name (until the hub has, requests to <name>@{} don't reach it). Copy {path} to it, then run there:\n",
        id.host
    );
    peers.sort_by(|a, b| a.host.cmp(&b.host));
    for p in &peers {
        out += &format!(
            "  on {}: agent bus trust {}.pub.json --renames {old}\n",
            p.host, id.host
        );
    }
    ctx.write_out(&out)
}

#[cfg(test)]
#[path = "bus_identity_test.rs"]
mod bus_identity_test;
