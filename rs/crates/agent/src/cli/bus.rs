//! `agent bus`, `agent bus init`, `agent bus export`, `agent bus trust`, `agent bus run`, `agent bus start`, `agent bus stop`, `agent bus status` (Go: internal/cli/bus.go).
//!
//! Unlike the Go, `bus run` doesn't hold the NATS server: it runs the Go
//! sidecar `agent-busd` as a child (agent_bus::start_daemon), connects to it
//! as a local client, and serves the RPC ops and the status subject over that
//! connection. Everything a person sees (the session, bus/settings.json, the
//! status reply, the messages) is the Go's.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use agent_base::gostd::{filepath, os};
use agent_base::util::shell_quote;
use agent_bus::{Conn, DaemonConfig, Identity};
use agent_store::gojson;
use clap::ArgAction;
use serde::{Deserialize, Deserializer, Serialize};

use super::bus_identity::{BusSettings, load_bus_settings, save_bus_settings, check_bus_settings};
use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::remote::trusted_hosts;
use super::root::Ctx;
use super::ssh::ssh_dest_for;

#[path = "bus_macos.rs"]
mod bus_macos;

/// The termd session `agent bus start` runs the bus in.
pub const BUS_SESSION: &str = "gomux-bus";

thread_local! {
    /// A test's own name for the bus session, to stay clear of a real bus
    /// (Go: tests assigning busSession).
    static TEST_BUS_SESSION: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// The termd session `agent bus start` runs the bus in (Go: busSession).
pub fn bus_session() -> String {
    TEST_BUS_SESSION
        .with(|s| s.borrow().clone())
        .unwrap_or_else(|| BUS_SESSION.to_string())
}

/// Renames the bus session for this test's thread.
#[cfg(test)]
pub fn set_test_bus_session(name: Option<&str>) {
    TEST_BUS_SESSION.with(|s| *s.borrow_mut() = name.map(str::to_string));
}

// `agent bus`

pub const DOC: Doc = Doc {
    use_line: "bus",
    short: "Run and pair this host's agent bus",
    long: r#"The agent bus links hosts, so "name@host" commands, status events, and
"agent wait" work across machines. Each host runs one bus; one host (the hub)
accepts links from the others.

Pairing works like SSH keys: each host has its own keys and certificate, and
trusts a peer by installing the peer's public bundle. Nothing secret is copied.

  # on both hosts
  agent bus init                          # the hub adds: --url tls://<hub>:7422
  # exchange the printed .pub.json files (e.g. with scp), then on each host
  agent bus trust <peer>.pub.json
  # start: the hub listens for peers, the others connect to it
  agent bus start --listen 0.0.0.0:7422   # hub
  agent bus start                         # other hosts

Peers may only call the ops in --remote-ops on this host (default: list,
status, capture, send, respond, friction, logs, fleet, template). keys,
spawn and resume stay local-only unless listed: spawn lets every linked
peer start agents here (agent spawn name@<this host>), and resume lets
them start an ended agent again (agent resume name@<this host>), which
amounts to letting them run commands as this user (start an agent, then
approve its prompts); list them only for peers you'd give a shell. E.g.
--remote-ops list,status,capture,send,respond,friction,logs,fleet,template,spawn,resume.

Changing trust:

  agent bus untrust <peer>       # drop a peer; restarts the bus
  agent bus rename <new-name>    # rename this host, keeping its keys,
                                 # and print what each peer runs:
  agent bus trust <new>.pub.json --renames <old>
  agent bus restart              # apply a trust, with the same settings

nats-server can't reload trust, so a change applies when the bus restarts.

Only reaching other hosts needs a bus: everything on one host works without."#,
    example: r#"  agent bus status
  agent bus init && agent bus start
  agent bus untrust old-laptop
  agent bus rename studio"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![
            bus_init_def(),
            bus_export_def(),
            bus_trust_def(),
            bus_run_def(),
            bus_start_def(),
            bus_stop_def(),
            bus_status_def(),
            super::bus_identity::bus_untrust_def(),
            super::bus_identity::bus_rename_def(),
            super::bus_identity::bus_restart_def(),
            super::ssh::bus_ssh_def(),
        ],
    )
}

// `agent bus init`

pub const BUS_INIT: Doc = Doc {
    use_line: "init",
    short: "Create this host's bus keys and certificate",
    example: r#"  agent bus init
  agent bus init --url tls://hub.example:7422   # on the hub"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus init`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusInitArgs {
    #[arg(
        long = "url",
        value_name = "string",
        default_value = "",
        help = "Where this host accepts peer links, if it is the hub (tls://host:7422)",
    )]
    pub url: String,
    /// The positional arguments, checked against BUS_INIT.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_init_def() -> Def {
    leaf!(BUS_INIT, BusInitArgs, run_bus_init)
}

pub fn run_bus_init(ctx: &Ctx, args: &BusInitArgs) -> anyhow::Result<()> {
    let cfg = ctx.cfg();
    if bus_set_up(ctx) {
        anyhow::bail!(
            "this host ({}) already has a bus identity in {}: agent bus export prints its public bundle",
            cfg.host,
            cfg.bus_dir()
        );
    }
    let id = agent_bus::init_identity(cfg.bus_dir(), &cfg.host)?;
    let path = write_bundle(ctx, &id, &args.url)?;
    ctx.write_out(&format!(
        "Created bus identity for host {} in {}\nPublic bundle: {path}\n  key {}, certificate pin {}\nCopy it to each peer and run `agent bus trust` there.\n",
        agent_base::gostd::strconv::quote(&cfg.host),
        cfg.bus_dir(),
        id.public_bundle("").nkey,
        id.cert_sha
    ))
}

// `agent bus export`

pub const BUS_EXPORT: Doc = Doc {
    use_line: "export",
    short: "Print this host's public bundle",
    example: r#"  agent bus export > hub.pub.json
  agent bus export --url tls://hub.example:7422"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus export`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusExportArgs {
    #[arg(
        long = "url",
        value_name = "string",
        default_value = "",
        help = "Where this host accepts peer links, if it is the hub (tls://host:7422)",
    )]
    pub url: String,
    /// The positional arguments, checked against BUS_EXPORT.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_export_def() -> Def {
    leaf!(BUS_EXPORT, BusExportArgs, run_bus_export)
}

pub fn run_bus_export(ctx: &Ctx, args: &BusExportArgs) -> anyhow::Result<()> {
    let cfg = ctx.cfg();
    let id = agent_bus::load_identity(cfg.bus_dir(), &cfg.host)?;
    ctx.write_out(&bundle_json(&id, &args.url)?)
}

// `agent bus trust`

pub const BUS_TRUST: Doc = Doc {
    use_line: "trust <bundle.json | ->",
    short: "Trust a peer host's public bundle",
    long: r#"Trust installs a peer's public bundle (from its "agent bus init",
"agent bus export" or "agent bus rename"). Check the key and pin it prints
against the peer's own output before relying on the link. Re-trusting a
host replaces its bundle. Restart the bus to apply: agent bus restart.

A peer renamed with "agent bus rename" keeps its keys: trust its new
bundle with --renames <old name>. That checks the bundle has the old
name's key and certificate pin, replaces the old bundle, rewrites this
host's references to the old name (its agents' spawned_by, friction
links, its ssh destination), keeps the old name as an alias, and restarts
the bus (--no-restart skips that)."#,
    example: r#"  agent bus trust hub.pub.json
  agent bus trust - < hub.pub.json
  agent bus trust new-name.pub.json --renames old-name"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus trust`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusTrustArgs {
    #[arg(long = "no-restart", help = "With --renames: don't restart the bus")]
    pub no_restart: bool,
    #[arg(
        long = "renames",
        value_name = "string",
        default_value = "",
        help = "The peer's former name: its bundle has the same keys under a new name (agent bus rename)",
    )]
    pub renames: String,
    /// The positional arguments, checked against BUS_TRUST.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_trust_def() -> Def {
    leaf!(BUS_TRUST, BusTrustArgs, super::bus_identity::run_bus_trust)
}

// `agent bus run`

pub const BUS_RUN: Doc = Doc {
    use_line: "run",
    short: "Run the bus in the foreground",
    long: r#"Run serves the bus until interrupted. "agent bus start" runs it in a background session."#,
    example: "  agent bus run --listen 0.0.0.0:7422",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus run`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusRunArgs {
    #[arg(
        long = "client-port",
        value_name = "int",
        default_value = "7420",
        value_parser = flags::parse_int,
        help = "Local client port (127.0.0.1)",
    )]
    pub client_port: i64,
    #[arg(
        long = "listen",
        value_name = "string",
        default_value = "",
        help = "Accept peer links on this address (makes this host the hub), e.g. 0.0.0.0:7422",
    )]
    pub listen: String,
    #[arg(
        long = "remote-ops",
        value_name = "strings",
        value_delimiter = ',',
        action = ArgAction::Append,
        default_values = agent_bus::DEFAULT_REMOTE_OPS,
        help = "Ops peers may call on this host",
    )]
    pub remote_ops: Vec<String>,
    /// The positional arguments, checked against BUS_RUN.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_run_def() -> Def {
    leaf!(BUS_RUN, BusRunArgs, run_bus_run)
}

/// Saves this host's public bundle next to its identity.
pub fn write_bundle(ctx: &Ctx, id: &Identity, url: &str) -> anyhow::Result<String> {
    let data = bundle_json(id, url)?;
    let path = filepath::join(&[&ctx.cfg().bus_dir(), &format!("{}.pub.json", id.host)]);
    os::write_file(&path, data.as_bytes(), 0o644)?;
    Ok(path)
}

/// This host's public bundle as "agent bus trust" reads it (Go:
/// json.MarshalIndent, with a newline).
fn bundle_json(id: &Identity, url: &str) -> anyhow::Result<String> {
    let data = serde_json::to_string_pretty(&id.public_bundle(url))?;
    Ok(gojson::escape_html(&data) + "\n")
}

/// A JSON array that may be null (a Go nil slice) as empty.
fn null_as_empty<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    Ok(Option::<Vec<String>>::deserialize(d)?.unwrap_or_default())
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// The bus's answer on its local-only status subject (Go: busInfo).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct BusInfo {
    #[serde(default)]
    pub host: String,
    #[serde(default)]
    pub client_url: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub leaf_addr: String,
    #[serde(default)]
    pub linked_peers: i64,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub trusted: Vec<String>,
    #[serde(default, deserialize_with = "null_as_empty")]
    pub remote_ops: Vec<String>,
    /// Listen and ClientPort are the --listen and --client-port it runs
    /// with, so agent bus restart can start it again the same way. A bus
    /// built before them sends neither (see running_bus_settings).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub listen: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub client_port: i64,
}

/// Set when SIGINT or SIGTERM arrives: bus run stops.
static STOP_SIGNALED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_sig: libc::c_int) {
    STOP_SIGNALED.store(true, Ordering::SeqCst);
}

/// Makes SIGINT and SIGTERM set STOP_SIGNALED instead of killing the process
/// (Go: signal.NotifyContext(ctx, os.Interrupt, syscall.SIGTERM)).
fn notify_stop_signals() {
    // SAFETY: installing a handler that only stores to an atomic.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_stop_signal as *const () as libc::sighandler_t;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
        libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut());
    }
}

pub fn run_bus_run(ctx: &Ctx, args: &BusRunArgs) -> anyhow::Result<()> {
    notify_stop_signals();
    serve_bus(ctx, args, None, &|| STOP_SIGNALED.load(Ordering::SeqCst))
}

/// Serves the bus as bus run does, with agent-busd at busd (None: beside
/// this binary, else on PATH), until stop says to.
pub fn serve_bus(
    ctx: &Ctx,
    args: &BusRunArgs,
    busd: Option<std::path::PathBuf>,
    stop: &dyn Fn() -> bool,
) -> anyhow::Result<()> {
    let cfg = ctx.cfg();
    let host = cfg.host.clone();
    let client_port = i32::try_from(args.client_port)
        .map_err(|_| anyhow::anyhow!("--client-port {} isn't a port", args.client_port))?;
    // On macOS the daemon spawns agent-busd as its own responsible process
    // when it can hold the Local Network permission (Go: runBusDisclaimed,
    // for the agent binary itself), and agent-busd watches this process
    // (Go: watchBusParent).
    let d = Arc::new(agent_bus::start_daemon(DaemonConfig {
        dir: cfg.bus_dir().into(),
        host: host.clone(),
        client_port,
        leaf_listen: args.listen.clone(),
        remote_ops: Some(args.remote_ops.clone()),
        busd_path: busd.clone(),
    })?);

    let nc = ctx
        .bus_connect()
        .map_err(|e| e.context("connecting to own bus"))?;

    // Local callers may use every op; peers are limited to --remote-ops by
    // the link permissions agent-busd sets.
    let handler = Arc::new(super::remote_ops::bus_handler(ctx)?);
    let rpc = agent_bus::serve_rpc(&nc, &host, handler, agent_bus::ALL_OPS)?;

    let peers = agent_bus::load_peers(cfg.bus_dir()).unwrap_or_default();
    let trusted: Vec<String> = peers.iter().map(|p| p.host.clone()).collect();
    let info = {
        let d = Arc::clone(&d);
        let (host, trusted) = (host.clone(), trusted.clone());
        let (remote_ops, listen) = (args.remote_ops.clone(), args.listen.clone());
        let client_port = args.client_port;
        move || BusInfo {
            host: host.clone(),
            client_url: d.client_url().to_string(),
            leaf_addr: d.leaf_addr().to_string(),
            linked_peers: d.linked_peers() as i64,
            trusted: trusted.clone(),
            remote_ops: remote_ops.clone(),
            listen: listen.clone(),
            client_port,
        }
    };
    let mut status_sub = nc.subscribe_callback(
        &agent_bus::subject_local(&host, "bus"),
        move |m| {
            if let Ok(data) = serde_json::to_vec(&info()) {
                let _ = m.respond(data);
            }
        },
    )?;

    let mut line = format!(
        "agent bus for {}: clients on {}",
        agent_base::gostd::strconv::quote(&host),
        d.client_url()
    );
    if !d.leaf_addr().is_empty() {
        line += &format!(", peer links on {}", d.leaf_addr());
    }
    line += &format!("; trusted peers: {}\n", trusted.join(", "));
    ctx.write_out(&line)?;
    // TODO(lead): agent_bus::daemon's start_pipeless also prints its own
    // LOCAL_NETWORK_WARNING on macOS (naming "the agent binary"), so a Mac
    // with a plist-less agent-busd gets the note twice; the CLI's (naming
    // agent-busd) should be the only one.
    bus_macos::warn_local_network(ctx, busd.as_deref(), &peers, &args.listen);

    // Until interrupted, or agent-busd exits under it.
    let mut died = None;
    while !stop() {
        if let Err(e) = d.check_alive() {
            // A Ctrl-C in the session reaches agent-busd too (one process
            // group): give this process's own signal a moment to land before
            // calling it a crash.
            std::thread::sleep(Duration::from_millis(200));
            if !stop() {
                died = Some(e);
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = status_sub.unsubscribe();
    rpc.stop();
    nc.close();
    d.shutdown();
    if let Some(e) = died {
        return Err(e.context("agent-busd stopped"));
    }
    ctx.write_out("agent bus stopping\n")
}

// `agent bus start`

pub const BUS_START: Doc = Doc {
    use_line: "start",
    short: "Start the bus in the background (session gomux-bus)",
    long: r#"Start runs the bus in background session gomux-bus.

It saves the --listen, --client-port and --remote-ops it starts with (in
bus/settings.json), and a later start without them uses the saved ones: on
the hub, agent bus start after a reboot listens for peers again, and peers
may call the same ops. A flag given changes just that setting, and is
saved. agent bus restart takes the same flags."#,
    example: r#"  agent bus start
  agent bus start --listen 0.0.0.0:7422   # on the hub, once; saved after
  agent bus start --remote-ops list,status,capture,send,respond,friction,logs,fleet,template,resume"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus start`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusStartArgs {
    #[arg(
        long = "client-port",
        value_name = "int",
        default_value = "7420",
        value_parser = flags::parse_int,
        help = "Local client port (127.0.0.1)",
    )]
    pub client_port: i64,
    #[arg(
        long = "listen",
        value_name = "string",
        default_value = "",
        help = "Accept peer links on this address (makes this host the hub), e.g. 0.0.0.0:7422",
    )]
    pub listen: String,
    #[arg(
        long = "remote-ops",
        value_name = "strings",
        value_delimiter = ',',
        action = ArgAction::Append,
        default_values = agent_bus::DEFAULT_REMOTE_OPS,
        help = "Ops peers may call on this host",
    )]
    pub remote_ops: Vec<String>,
    /// The positional arguments, checked against BUS_START.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_start_def() -> Def {
    leaf!(BUS_START, BusStartArgs, run_bus_start)
}

pub fn run_bus_start(ctx: &Ctx, args: &BusStartArgs) -> anyhow::Result<()> {
    let session = bus_session();
    if ctx.termd()?.session_exists(&session).unwrap_or(false) {
        check_own_bus_session(ctx)?;
        anyhow::bail!(
            "the bus is already running in session {session} (agent bus status shows it; agent bus restart restarts it)"
        );
    }
    let (s, from) = start_settings(ctx, args, &|f| ctx.changed(f))?;
    start_bus_session(ctx, &s)?;
    if let Err(e) = save_bus_settings(ctx, &s) {
        return Err(e.context(format!("the bus started ({s}), but saving its settings failed")));
    }
    ctx.write_out(&format!(
        "Bus started in session {} ({s}{from})\n",
        agent_base::gostd::strconv::quote(&session)
    ))?;
    bus_status(ctx, false)
}

/// What agent bus start runs with: the flags given, and for the rest the
/// saved settings, or the defaults if none were saved. The second value says
/// where they came from, for start's output. changed says which flags were
/// given (ctx.changed).
pub fn start_settings(
    ctx: &Ctx,
    args: &BusStartArgs,
    changed: &dyn Fn(&str) -> bool,
) -> anyhow::Result<(BusSettings, String)> {
    let mut s = BusSettings {
        listen: args.listen.clone(),
        client_port: args.client_port,
        remote_ops: args.remote_ops.clone(),
    };
    let mut from = String::new();
    if let Some(saved) = load_bus_settings(ctx)? {
        if !changed("listen") {
            s.listen = saved.listen;
        }
        if !changed("client-port") {
            s.client_port = saved.client_port;
        }
        if !changed("remote-ops") {
            s.remote_ops = saved.remote_ops;
        }
        from = "; saved settings".to_string();
        if changed("listen") || changed("client-port") || changed("remote-ops") {
            from = "; saved settings, with the flags given".to_string();
        }
    }
    check_bus_settings(&s)?;
    Ok((s, from))
}

/// Starts the bus in a new session with s, and waits for it to answer.
fn start_bus_session(ctx: &Ctx, s: &BusSettings) -> anyhow::Result<()> {
    let host = ctx.cfg().host.clone();
    let run = bus_run_command(ctx, &host, s)?;
    ctx.termd()?.create_session(&bus_session(), &HashMap::new())?;
    run_bus_in(ctx, &host, &run)
}

/// The command line that runs the bus as host with s, checking first that
/// it can (Go: busRunCommand).
pub fn bus_run_command(ctx: &Ctx, host: &str, s: &BusSettings) -> anyhow::Result<String> {
    let cfg = ctx.cfg();
    agent_bus::load_identity(cfg.bus_dir(), host)?;
    if cfg.agent_binary.is_empty() {
        anyhow::bail!("can't locate the agent binary to run the bus");
    }
    // Pin the host, so the bus runs as the identity that started it even if
    // AGENT_HOST isn't set in the session's shell.
    let mut run = vec![
        "env".to_string(),
        format!("AGENT_HOST={}", shell_quote(host)),
        shell_quote(&cfg.agent_binary),
        "bus".to_string(),
        "run".to_string(),
        "--client-port".to_string(),
        s.client_port.to_string(),
        "--remote-ops".to_string(),
        shell_quote(&s.remote_ops.join(",")),
    ];
    if !s.listen.is_empty() {
        run.push("--listen".to_string());
        run.push(shell_quote(&s.listen));
    }
    Ok(run.join(" "))
}

/// A connection to this host's bus as host (this host's name, or the one
/// rename just gave it).
fn connect_as(ctx: &Ctx, host: &str) -> anyhow::Result<Conn> {
    if host == ctx.cfg().host {
        return ctx.bus_connect();
    }
    agent_bus::connect_local(ctx.cfg().bus_dir(), host)
}

/// How long bus start, restart and rename wait for the bus they started to
/// answer. On macOS the first launch of a newly installed, newly signed
/// agent and agent-busd can take seconds of signature checks, so it is
/// generous; a bus that fails exits, and that is reported as soon as it is
/// seen. agent-busd's own wait for its ready file
/// (agent_bus::BUSD_READY_TIMEOUT) is shorter, so its error shows here.
pub const BUS_START_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the wait goes before it says the bus is still starting.
const BUS_START_NOTE_AFTER: Duration = Duration::from_secs(5);

/// How long the wait goes without ever seeing the bus run in its session
/// before it takes it that the bus ran and exited between two looks (a bad
/// flag exits in milliseconds), unless the screen shows the shell back at
/// its prompt sooner. The shell in a fresh session reads the typed command
/// once its startup files have run.
const BUS_UNSEEN_GRACE: Duration = Duration::from_secs(5);

/// The timing of wait_for_bus (tests shorten it).
#[derive(Debug, Clone, Copy)]
pub struct BusWaitTiming {
    pub timeout: Duration,
    pub note_after: Duration,
    pub unseen_grace: Duration,
    pub poll: Duration,
}

impl Default for BusWaitTiming {
    fn default() -> Self {
        BusWaitTiming {
            timeout: BUS_START_TIMEOUT,
            note_after: BUS_START_NOTE_AFTER,
            unseen_grace: BUS_UNSEEN_GRACE,
            poll: Duration::from_millis(100),
        }
    }
}

/// How wait_for_bus ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusWait {
    /// The bus answered.
    Up,
    /// The bus command isn't running in its session (it exited, or was never
    /// seen there within the grace).
    Exited,
    /// The timeout passed while the bus command still ran: it may yet come up.
    StillStarting,
    /// The timeout passed, and whether the bus command still runs couldn't
    /// be read.
    NoAnswer,
}

/// What a look at the bus session finds of the bus command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusProc {
    /// It runs there.
    Running,
    /// It doesn't run there (not yet, or no more).
    NotSeen,
    /// It ran and is done: the shell is back at its prompt after it, or
    /// the session is gone.
    Done,
    /// The processes can't be read.
    Unknown,
}

/// Waits for the bus to answer (answers), checking with look whether the
/// bus command still runs in its session, and calling note once if the wait
/// goes past timing.note_after. A command never seen running (a bus that
/// fails in milliseconds can run and exit between two looks) counts as
/// exited once timing.unseen_grace has passed; Unknown counts as running.
pub fn wait_for_bus(
    timing: BusWaitTiming,
    answers: &dyn Fn() -> bool,
    look: &dyn Fn() -> BusProc,
    note: &mut dyn FnMut(),
) -> BusWait {
    let start = Instant::now();
    let mut seen = false;
    let mut noted = false;
    loop {
        if answers() {
            return BusWait::Up;
        }
        let found = look();
        let exited = match found {
            BusProc::Running | BusProc::Unknown => {
                seen = true;
                false
            }
            BusProc::Done => true,
            BusProc::NotSeen => seen || start.elapsed() >= timing.unseen_grace,
        };
        if exited {
            // One more look: it may have come up and handed over just now.
            if answers() {
                return BusWait::Up;
            }
            return BusWait::Exited;
        }
        if start.elapsed() >= timing.timeout {
            return match found {
                BusProc::Running => BusWait::StillStarting,
                _ => BusWait::NoAnswer,
            };
        }
        if !noted && start.elapsed() >= timing.note_after {
            noted = true;
            note();
        }
        std::thread::sleep(timing.poll);
    }
}

/// Looks for the bus command run in session: Running if bus_runs_in finds
/// it, Done if the session is gone or the screen shows the shell back at its
/// prompt after the command (shell_back_after; before is the screen before
/// it was typed), else NotSeen (or Unknown).
pub fn look_for_bus(termd: &dyn agent_termd::Ops, session: &str, before: &str, run: &str) -> BusProc {
    match bus_runs_in(termd, session) {
        Some(true) => BusProc::Running,
        None => BusProc::Unknown,
        Some(false) => {
            if !termd.session_exists(session).unwrap_or(true) {
                return BusProc::Done;
            }
            match termd.capture(session, 0) {
                Ok(screen) if shell_back_after(&screen, before, run) => BusProc::Done,
                _ => BusProc::NotSeen,
            }
        }
    }
}

/// Whether screen shows the shell back at its prompt after running run: a
/// line where run starts after the prompt (its echo), one more of them than
/// the screen had before it was typed (before: a restarted session still
/// shows the last run), and as the screen's last non-blank line that prompt
/// alone. A prompt that changes between commands (a clock, the last exit
/// status) doesn't match, which only means the wait goes on to its grace.
pub fn shell_back_after(screen: &str, before: &str, run: &str) -> bool {
    let needle: String = run.chars().take(16).collect();
    if needle.trim().is_empty() {
        return false;
    }
    // The echo lines: (index, the prompt before run).
    let echoes = |text: &str| -> Vec<(usize, String)> {
        text.lines()
            .enumerate()
            .filter_map(|(i, l)| {
                let at = l.find(&needle)?;
                let prompt = l[..at].trim();
                (!prompt.is_empty()).then(|| (i, prompt.to_string()))
            })
            .collect()
    };
    let now = echoes(screen);
    let Some((i, prompt)) = now.last() else {
        return false;
    };
    if now.len() <= echoes(before).len() {
        return false;
    }
    let lines: Vec<&str> = screen.lines().collect();
    lines[i + 1..]
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .is_some_and(|l| l.trim() == prompt)
}

/// Whether `agent bus run` runs in session (a process under its shell whose
/// command line has the words "bus run"): Some(false) if the session is gone
/// or nothing under it does, None if the processes can't be read.
pub fn bus_runs_in(termd: &dyn agent_termd::Ops, session: &str) -> Option<bool> {
    let pane = match termd.first_pane(session) {
        Ok(p) => p,
        Err(_) => {
            return match termd.session_exists(session) {
                Ok(false) => Some(false),
                _ => None,
            };
        }
    };
    if pane.pid <= 0 {
        return None;
    }
    let tree = agent_base::util::read_proc_tree().ok()?;
    Some(tree.keys().any(|&pid| {
        tree.depth(pid, pane.pid).is_some_and(|d| d > 0)
            && agent_base::util::proc_args(pid).is_some_and(|args| is_bus_run_args(&args))
    }))
}

/// Whether a command line (as its words) runs `agent bus run`: the words
/// "bus" and "run" next to each other (the env that starts it has them too).
pub fn is_bus_run_args(args: &[String]) -> bool {
    args.windows(2).any(|w| w[0] == "bus" && w[1] == "run")
}

/// Types run into the bus session's shell and waits for the bus to answer:
/// it returns once it does, says once on stderr that it's still starting
/// past BUS_START_NOTE_AFTER, and gives up when the bus exits or after
/// BUS_START_TIMEOUT, saying which.
pub fn run_bus_in(ctx: &Ctx, host: &str, run: &str) -> anyhow::Result<()> {
    run_bus_in_with(ctx, host, run, BusWaitTiming::default())
}

/// run_bus_in with its timing (tests shorten it).
pub fn run_bus_in_with(
    ctx: &Ctx,
    host: &str,
    run: &str,
    timing: BusWaitTiming,
) -> anyhow::Result<()> {
    let session = bus_session();
    let termd = ctx.termd()?;
    let before = termd.capture(&session, 0).unwrap_or_default();
    termd.send_keys(&session, run)?;
    termd.send_special_key(&session, "Enter")?;
    let answers = || match connect_as(ctx, host) {
        Ok(nc) => {
            nc.close();
            true
        }
        Err(_) => false,
    };
    let look = || look_for_bus(termd.as_ref(), &session, &before, run);
    let mut note = || {
        use std::io::Write;
        let _ = ctx.err().write_all(b"still starting...\n");
    };
    let output = || {
        termd
            .capture(&session, 20)
            .map(|o| o.trim().to_string())
            .unwrap_or_else(|e| format!("(can't read it: {e:#})"))
    };
    match wait_for_bus(timing, &answers, &look, &mut note) {
        BusWait::Up => Ok(()),
        BusWait::Exited => anyhow::bail!(
            "the bus exited in session {session} without coming up; session output:\n{}",
            output()
        ),
        BusWait::StillStarting => anyhow::bail!(
            "the bus hasn't answered within {}s, but it is still starting in session {session}: it may yet come up (agent bus status shows whether it has); session output:\n{}",
            timing.timeout.as_secs(),
            output()
        ),
        BusWait::NoAnswer => anyhow::bail!(
            "the bus hasn't answered within {}s, and whether it still runs in session {session} can't be read: agent bus status shows whether it came up; session output:\n{}",
            timing.timeout.as_secs(),
            output()
        ),
    }
}

/// Fails if this data dir's bus isn't running while the bus session exists.
/// The session name is the same for every data dir on the termd server, while
/// each bus records its address in its own data dir: the session then runs
/// another agent binary's bus (or a dead one), which isn't this binary's to
/// stop.
pub fn check_own_bus_session(ctx: &Ctx) -> anyhow::Result<()> {
    match ctx.bus_connect() {
        Ok(nc) => {
            nc.close();
            Ok(())
        }
        Err(e) if !agent_bus::is_no_bus(&e) => Err(e),
        Err(_) => {
            let session = bus_session();
            anyhow::bail!(
                "session {session} exists, but this data dir's bus ({}) isn't running: the session likely runs the bus of another agent binary with its own data dir, so it's left alone. If it's a dead bus of this one, agent session kill {session}",
                ctx.cfg().bus_dir()
            )
        }
    }
}

// `agent bus stop`

pub const BUS_STOP: Doc = Doc {
    use_line: "stop",
    short: "Stop the background bus",
    example: "  agent bus stop",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus stop`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusStopArgs {
    /// The positional arguments, checked against BUS_STOP.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_stop_def() -> Def {
    leaf!(BUS_STOP, BusStopArgs, run_bus_stop)
}

pub fn run_bus_stop(ctx: &Ctx, _args: &BusStopArgs) -> anyhow::Result<()> {
    let session = bus_session();
    if !ctx.termd()?.session_exists(&session)? {
        if query_bus(ctx, Duration::from_secs(1)).is_ok() {
            anyhow::bail!(
                "the bus is running, but not in session {session} (started with agent bus run?): stop it with Ctrl-C where it runs"
            );
        }
        anyhow::bail!(
            "no bus is running on host {} (no session {session}), so there is nothing to stop",
            ctx.cfg().host
        );
    }
    check_own_bus_session(ctx)?;
    stop_bus_session(ctx)?;
    ctx.write_out("Bus stopped\n")
}

/// Stops the bus running in its session and kills the session (agent bus
/// stop).
fn stop_bus_session(ctx: &Ctx) -> anyhow::Result<()> {
    interrupt_bus(ctx);
    if let Err(e) = ctx.termd()?.kill_session(&bus_session())
        && !format!("{e:#}").contains("not found")
    {
        return Err(e);
    }
    Ok(())
}

/// Stops the bus running in its session and leaves the session with a fresh
/// shell, preserving its starting environment, for run_bus_in to start it
/// again (restart, rename).
pub fn pause_bus_session(ctx: &Ctx) -> anyhow::Result<()> {
    interrupt_bus(ctx);
    let session = bus_session();
    let termd = ctx.termd()?;
    if let Err(e) = termd.respawn_pane(&session) {
        let fresh = |e: anyhow::Error| {
            e.context(format!(
                "the bus is stopped, but its session {session} couldn't get a fresh shell"
            ))
        };
        if !format!("{e:#}").contains("not found") {
            return Err(fresh(e));
        }
        termd
            .create_session(&session, &HashMap::new())
            .map_err(fresh)?;
    }
    Ok(())
}

/// Interrupts the bus in its session, so it shuts down cleanly and removes
/// its URL file, and waits for that (up to 3s).
fn interrupt_bus(ctx: &Ctx) {
    if let Ok(termd) = ctx.termd() {
        let _ = termd.send_special_key(&bus_session(), "c-c");
    }
    let url_file = filepath::join(&[&ctx.cfg().bus_dir(), agent_bus::BUS_URL_FILE]);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if std::fs::metadata(&url_file)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Asks this host's bus for its status, waiting up to timeout for the
/// answer. It returns agent_bus::ErrNoBus if no bus is running.
pub fn query_bus(ctx: &Ctx, timeout: Duration) -> anyhow::Result<BusInfo> {
    let nc = ctx.bus_connect()?;
    let res = nc.request(
        &agent_bus::subject_local(&ctx.cfg().host, "bus"),
        Vec::new(),
        timeout,
    );
    nc.close();
    let msg = res.map_err(|e| e.context("bus not answering"))?;
    Ok(serde_json::from_slice(&msg.data)?)
}

// `agent bus status`

pub const BUS_STATUS: Doc = Doc {
    use_line: "status",
    short: "Show whether the bus is running and which peers are linked",
    example: r#"  agent bus status
  agent bus status --json"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent bus status`.
#[derive(clap::Args, Debug, Clone)]
pub struct BusStatusArgs {
    #[arg(long = "json", help = "Print the bus status as JSON")]
    pub json: bool,
    /// The positional arguments, checked against BUS_STATUS.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn bus_status_def() -> Def {
    leaf!(BUS_STATUS, BusStatusArgs, run_bus_status)
}

pub fn run_bus_status(ctx: &Ctx, args: &BusStatusArgs) -> anyhow::Result<()> {
    bus_status(ctx, args.json)
}

/// The bus status as --json prints it (Go: busStatusOut).
#[derive(Serialize, Debug, Default)]
struct BusStatusOut {
    host: String,
    /// Has an identity ("agent bus init").
    set_up: bool,
    running: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    client_url: String,
    /// Where it accepts peer links, if it is the hub.
    #[serde(skip_serializing_if = "String::is_empty")]
    hub_addr: String,
    linked_peers: i64,
    trusted: Vec<String>,
    remote_ops: Vec<String>,
    /// Where agent attach name@host runs ssh to, per trusted peer (agent bus
    /// ssh): its set destination, or its name.
    ssh: BTreeMap<String, String>,
    /// This host's names before agent bus rename, oldest first; they still
    /// reach it.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    formerly: Vec<String>,
}

/// Each trusted peer's ssh destination, for bus status.
fn ssh_by_peer(ctx: &Ctx) -> BTreeMap<String, String> {
    trusted_hosts(ctx)
        .into_iter()
        .map(|h| {
            let d = ssh_dest_for(ctx, &h);
            (h, d)
        })
        .collect()
}

/// Whether this host has a bus identity ("agent bus init").
pub fn bus_set_up(ctx: &Ctx) -> bool {
    let cfg = ctx.cfg();
    agent_bus::load_identity(cfg.bus_dir(), &cfg.host).is_ok()
}

/// agent bus status (Go: busStatus), also what start and restart print last.
pub fn bus_status(ctx: &Ctx, json: bool) -> anyhow::Result<()> {
    let cfg = ctx.cfg();
    let info = query_bus(ctx, Duration::from_secs(2));
    let no_bus = matches!(&info, Err(e) if agent_bus::is_no_bus(e));
    if json {
        let mut out = BusStatusOut {
            host: cfg.host.clone(),
            set_up: bus_set_up(ctx),
            ssh: ssh_by_peer(ctx),
            formerly: agent_bus::previous_hosts(cfg.bus_dir()),
            ..Default::default()
        };
        if no_bus {
            return ctx.print_json(&out);
        }
        let info = info?;
        out.running = true;
        out.client_url = info.client_url;
        out.hub_addr = info.leaf_addr;
        out.linked_peers = info.linked_peers;
        out.trusted = info.trusted;
        out.remote_ops = info.remote_ops;
        return ctx.print_json(&out);
    }
    if no_bus {
        ctx.write_out(&format!("{}: bus not running\n", cfg.host))?;
        let session = bus_session();
        let exists = ctx
            .termd()
            .ok()
            .and_then(|t| t.session_exists(&session).ok())
            .unwrap_or(false);
        if exists {
            ctx.hint(format_args!(
                "session {session} exists, but runs no bus of this data dir ({}): likely another agent binary's bus",
                cfg.bus_dir()
            ));
        } else if bus_set_up(ctx) {
            ctx.hint("start it: agent bus start (add --listen 0.0.0.0:7422 on the hub)");
        } else {
            ctx.hint("set it up first: agent bus init (see agent bus --help)");
        }
        return Ok(());
    }
    let info = info?;
    let mut out = format!("{}: bus running (clients on {})\n", info.host, info.client_url);
    let prev = agent_bus::previous_hosts(cfg.bus_dir());
    if !prev.is_empty() {
        out += &format!("  formerly: {} (agent bus rename)\n", prev.join(", "));
    }
    if !info.leaf_addr.is_empty() {
        out += &format!("  hub, accepting peer links on {}\n", info.leaf_addr);
    }
    out += &format!(
        "  linked peers: {}; trusted: {}\n",
        info.linked_peers,
        info.trusted.join(", ")
    );
    out += &format!("  remote ops allowed: {}\n", info.remote_ops.join(", "));
    let ssh = ssh_by_peer(ctx);
    if !ssh.is_empty() {
        let dests: Vec<String> = ssh.iter().map(|(h, d)| format!("{h} → {d}")).collect();
        out += &format!("  attach over ssh (agent bus ssh): {}\n", dests.join(", "));
    }
    ctx.write_out(&out)
}

#[cfg(test)]
#[path = "bus_test.rs"]
pub(crate) mod bus_test;
