//! `agent export` (Go: internal/cli/export.go).
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_base::config::{DEFAULT_TERMD_NAMESPACE, STORAGE_DIR_NAME};
use agent_base::gostd::{filepath, strconv};
use agent_bus::Context;
use agent_export::{
    Agent as ExportAgent, Deps, Line, REASON_SESSION, REASON_STATUS, Snapshot, Tree, LINE_REMOVED,
    LINE_SPAWNED,
};
use chrono::Local;
use clap::ArgAction;

use super::command::{ArgRule, Def, Doc};
use super::flags;
use super::logs::go_json;
use super::output::plural;
use super::root::Ctx;
use super::transcript::Interrupt;

// `agent export`

pub const DOC: Doc = Doc {
    use_line: "export",
    short: "Print the agent tree for a program to read, and follow its changes",
    long: r#"Export prints every agent this binary's store records, with what a display
needs: parent, backend, session and g3d-termd namespace (and whether the
session exists), cwd, the last reported status, and the CLI transcript's path,
size, and time. --json prints it as one JSON object; "agent list" is the view
for a person.

--follow then prints one JSON line per change until Ctrl-C: spawned,
changed, and removed agents, read from the store's agent events (which
spawn, cleanup, and every status report record), plus terminal sessions and
transcripts checked every second. Each event line carries its id; --since
<id> resumes after it, without a snapshot, if the store still keeps the
events after it (the newest 20000). docs/ARCHITECTURE.md ("Export") has
every line's shape.

Status is the last report from the agent's hooks, not the resolved status
"agent status" shows: export runs no backend CLI. An interrupted
permission prompt stays "waiting", and a crashed CLI keeps its last status,
until the next hook.

Stores: an agent spawned with another agent binary (a worktree's
./bin/agent) is recorded in that binary's store. Export finds those stores
through the PATH of each agent-* session no store records, and reads
them read-only; --store adds one. Sessions it still can't match are listed
as unrecorded.

Each agent says whether "agent resume" would continue it (resumable): a
claude or agy agent, not adopted, with a session, that has ended.

--all adds departed agents (cleaned up), with when they ended. --hosts adds
other hosts' agents through the bus ("all" for every trusted peer): no
store or transcript, and the status, detail, session id, cwd, attached and
resumable that host works out. A host whose agent predates them leaves
them out, and absent means unknown (not false). Following re-lists them
every 30s.

Export only reads, and records one row in the command log per run however
long it follows."#,
    example: r#"  agent export
  agent export --json
  agent export --json --follow
  agent export --json --follow --since 1234
  agent export --json --all --store ~/src/gomuxai/bin
  agent export --json --hosts all"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent export`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(long = "all", help = "Include departed (cleaned up) agents")]
    pub all: bool,
    #[arg(long = "follow", help = "After the snapshot, print each change until Ctrl-C")]
    pub follow: bool,
    #[arg(
        long = "hosts",
        value_name = "string",
        default_value = "",
        help = r#"Also list these hosts' agents over the bus: "all" (every trusted peer) or host,host"#,
    )]
    pub hosts: String,
    #[arg(long = "json", help = "Print JSON (with --follow, one object per line)")]
    pub json: bool,
    #[arg(
        long = "since",
        value_name = "int",
        default_value = "0",
        value_parser = flags::parse_int,
        help = "With --follow: resume after this event id, without a snapshot",
    )]
    pub since: i64,
    #[arg(
        long = "store",
        value_name = "stringArray",
        action = ArgAction::Append,
        help = "Also read this store (a .gomuxai dir, the dir holding one, or an agent binary); repeatable",
    )]
    pub store: Vec<String>,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

/// How often a follower reads agent events, checks sessions and transcripts,
/// and re-lists other hosts (Go: package variables, for tests).
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExportIntervals {
    pub poll: Duration,
    pub session: Duration,
    pub host: Duration,
}

impl Default for ExportIntervals {
    fn default() -> Self {
        ExportIntervals {
            poll: Duration::from_millis(250),
            session: Duration::from_secs(1),
            host: Duration::from_secs(30),
        }
    }
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let (bctx, cancel) = Context::with_cancel(&Context::background());
    let _interrupt = args.follow.then(|| Interrupt::notify(cancel));
    run_export(ctx, &bctx, args, ExportIntervals::default())
}

/// run, until bctx is done (Go: runExport, until interrupted).
pub(crate) fn run_export(ctx: &Ctx, bctx: &Context, args: &Args, iv: ExportIntervals) -> anyhow::Result<()> {
    if ctx.changed("since") && !args.follow {
        anyhow::bail!(
            "--since resumes a follow: use it with --follow (agent export --json --follow --since {})",
            args.since
        );
    }
    let mut dirs = vec![ctx.data_dir()];
    for p in &args.store {
        let dir = data_dir_for(p);
        if dir.is_empty() {
            anyhow::bail!(
                "--store {p}: no agent store there (looked for {} and {})",
                filepath::join(&[p.as_str(), "gomuxai.db"]),
                filepath::join(&[p.as_str(), STORAGE_DIR_NAME, "gomuxai.db"])
            );
        }
        dirs.push(dir);
    }
    let hosts = export_host_list(ctx, &args.hosts)?;

    let mut tree = Tree::new(export_deps(ctx), &dirs, &hosts, args.all);
    let res = export_tree(ctx, bctx, args, iv, &mut tree, &dirs, &hosts);
    tree.close();
    res
}

fn export_tree(
    ctx: &Ctx,
    bctx: &Context,
    args: &Args,
    iv: ExportIntervals,
    tree: &mut Tree,
    dirs: &[String],
    hosts: &[String],
) -> anyhow::Result<()> {
    if !args.follow {
        let snap = tree.snapshot(bctx)?;
        if args.json {
            return ctx.print_json(&snap);
        }
        let mut out = Vec::new();
        print_export(&mut out, &snap)?;
        return ctx.write_out(&String::from_utf8_lossy(&out));
    }

    let mut resumed = false;
    if ctx.changed("since") {
        resumed = tree.resume(bctx, args.since)?;
        if !resumed {
            ctx.notice(format!(
                "Events after {} are no longer all kept (or not written yet): starting from a snapshot.",
                args.since
            ));
        }
    }
    if !resumed {
        let snap = tree.snapshot(bctx)?;
        if !args.json {
            let mut out = Vec::new();
            print_export(&mut out, &snap)?;
            ctx.write_out(&String::from_utf8_lossy(&out))?;
        } else {
            ctx.write_out(&format!("{}\n", go_json(&snap)?))?;
        }
    }
    ctx.notice(format!(
        "Following the agent tree ({}). Ctrl-C stops.",
        follow_scope(dirs, hosts)
    ));
    follow_export(ctx, bctx, tree, args.json, iv)
}

/// Prints the tree's changes until bctx is done (Go: followExport).
fn follow_export(ctx: &Ctx, bctx: &Context, tree: &mut Tree, json: bool, iv: ExportIntervals) -> anyhow::Result<()> {
    let start = Instant::now();
    let (mut poll_due, mut sessions_due, mut hosts_due) = (start + iv.poll, start + iv.session, start + iv.host);
    loop {
        let next = poll_due.min(sessions_due).min(hosts_due);
        if bctx
            .sleep_or_done(next.saturating_duration_since(Instant::now()))
            .is_some()
        {
            return Ok(());
        }
        let now = Instant::now();
        let mut lines = Vec::new();
        if now >= poll_due {
            poll_due += iv.poll;
            if let Err(e) = tree.poll_into(bctx, &mut lines) {
                let _ = writeln!(ctx.err(), "agent export: {e:#}");
            }
        } else if now >= sessions_due {
            sessions_due += iv.session;
            lines = tree.poll_sessions(bctx);
        } else {
            hosts_due += iv.host;
            lines = tree.poll_hosts(bctx);
        }
        for l in &lines {
            emit_line(ctx, json, l)?;
        }
    }
}

/// Prints a follow line: compact JSON with --json, a line of text otherwise
/// (Go: lineWriter).
fn emit_line(ctx: &Ctx, json: bool, l: &Line) -> anyhow::Result<()> {
    if json {
        return ctx.write_out(&format!("{}\n", go_json(l)?));
    }
    let s = format_export_line(l);
    if !s.is_empty() {
        ctx.write_out(&format!("{s}\n"))?;
    }
    Ok(())
}

fn follow_scope(dirs: &[String], hosts: &[String]) -> String {
    let mut s = if dirs.len() == 1 {
        "this store".to_string()
    } else {
        plural(dirs.len(), "store")
    };
    s += " and the stores its sessions point at";
    if !hosts.is_empty() {
        s += &format!(", and {}", hosts.join(", "));
    }
    s
}

/// Wires the tree to the multiplexer, the bus, and this host (Go:
/// exportDeps).
pub(crate) fn export_deps(ctx: &Ctx) -> Deps {
    let cfg = ctx.cfg();
    let mut ns = cfg.termd_namespace.clone();
    if ns.is_empty() {
        ns = DEFAULT_TERMD_NAMESPACE.to_string();
    }
    let mut d = Deps {
        host: cfg.host.clone(),
        namespace: ns,
        session_prefix: cfg.session_prefix.clone(),
        ..Deps::default()
    };
    if let Ok(share) = ctx.share() {
        d.list_host = Some(Box::new(move |bctx: &Context, host: &str| {
            let ctx = share.quiet_ctx();
            super::remote::call_remote(&ctx, host, |c| c.list(bctx, host))
        }));
    }
    if let Ok(termd) = ctx.termd() {
        let cns = termd.namespace();
        if !cns.is_empty() {
            d.namespace = cns.to_string();
        }
        let t = Arc::clone(termd);
        d.sessions = Some(Box::new(move || t.session_names()));
        let t = Arc::clone(termd);
        d.session_store = Some(Box::new(move |session: &str| {
            // spawn puts its binary's dir first on the session's PATH.
            let path = t.session_env(session, "PATH");
            let first = path.split(':').next().unwrap_or("");
            data_dir_for(first)
        }));
    }
    d
}

/// Reads --hosts: "all" is every trusted peer (Go: exportHostList).
pub(crate) fn export_host_list(ctx: &Ctx, flag: &str) -> anyhow::Result<Vec<String>> {
    if flag.is_empty() {
        return Ok(Vec::new());
    }
    let hosts: Vec<String> = if flag == "all" {
        let peers = agent_bus::load_peers(ctx.cfg().bus_dir())
            .map_err(|e| anyhow::anyhow!("--hosts all: reading the trusted peers: {e:#}"))?;
        let hosts: Vec<String> = peers.into_iter().map(|p| p.host).collect();
        if hosts.is_empty() {
            anyhow::bail!(
                "--hosts all: this host trusts no peers (agent bus trust <host>.pub.json adds one), so there are no other hosts to list"
            );
        }
        hosts
    } else {
        flag.split(',').map(str::to_string).collect()
    };
    let mut out: Vec<String> = Vec::new();
    for h in hosts {
        let h = h.trim();
        agent_bus::validate_host(h).map_err(|e| anyhow::anyhow!("--hosts: {e:#}"))?;
        let h = super::remote::canonical_host(ctx, h);
        if h == ctx.cfg().host || out.contains(&h) {
            continue; // this host's agents are in its store
        }
        out.push(h);
    }
    out.sort();
    Ok(out)
}

/// Maps a data dir, the dir holding one, or an agent binary to its data dir,
/// or "" if no store is there (Go: dataDirFor).
pub(crate) fn data_dir_for(p: &str) -> String {
    if p.is_empty() {
        return String::new();
    }
    let p = filepath::eval_symlinks(p).unwrap_or_else(|_| p.to_string());
    let mut cands = vec![p.clone(), filepath::join(&[p.as_str(), STORAGE_DIR_NAME])];
    if std::fs::metadata(&p).is_ok_and(|i| !i.is_dir()) {
        cands = vec![filepath::join(&[filepath::dir(&p).as_str(), STORAGE_DIR_NAME])];
    }
    for c in cands {
        if std::fs::metadata(filepath::join(&[c.as_str(), "gomuxai.db"])).is_ok_and(|i| !i.is_dir()) {
            return c;
        }
    }
    String::new()
}

/// Prints the snapshot for a person (Go: printExport).
pub(crate) fn print_export(out: &mut dyn Write, snap: &Snapshot) -> std::io::Result<()> {
    for s in &snap.stores {
        let mut line = format!("Store {}", s.dir);
        if s.this {
            line += " (this binary's)";
        }
        if !s.error.is_empty() {
            line += &format!(": unreadable: {}", s.error);
        } else if !s.events {
            line += ": written by an agent built before export, so --follow sees its changes only as whole-store updates";
        } else {
            line += &format!(": events up to {}", s.cursor);
        }
        writeln!(out, "{line}")?;
    }
    for h in &snap.hosts {
        if !h.error.is_empty() {
            writeln!(out, "Host {}: {}", h.host, h.error)?;
        } else {
            writeln!(out, "Host {}: {}", h.host, plural(h.agents, "agent"))?;
        }
    }
    if snap.agents.is_empty() {
        writeln!(out, "No agents (agent spawn <name> --backend claude starts one).")?;
    } else {
        writeln!(
            out,
            "\n{:<24} {:<11} {:<18} {:<24} {:<19} TRANSCRIPT",
            "NAME", "STATUS", "PARENT", "SESSION", "CREATED"
        )?;
        for a in &snap.agents {
            let mut name = a.name.clone();
            if a.host != snap.host {
                name += &format!("@{}", a.host);
            }
            let mut status = a.status.clone();
            if !a.live {
                status = "departed".to_string();
            } else if status.is_empty() {
                status = "-".to_string();
            }
            let mut session = a.session.clone();
            if a.attached == Some(false) {
                session = "- (gone)".to_string();
            }
            let parent = if a.parent.is_empty() { "-" } else { a.parent.as_str() };
            writeln!(
                out,
                "{name:<24} {status:<11} {parent:<18} {session:<24} {:<19} {}",
                a.created_at.0.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S"),
                transcript_summary(a)
            )?;
        }
    }
    for s in &snap.unrecorded {
        let where_ = if s.store.is_empty() {
            "no store found for it".to_string()
        } else {
            format!("its binary's store {} doesn't record it", s.store)
        };
        writeln!(out, "Unrecorded session {}: {where_}", s.session)?;
    }
    Ok(())
}

fn transcript_summary(a: &ExportAgent) -> String {
    if a.transcript.is_empty() {
        return "-".to_string();
    }
    if a.transcript_mtime.is_zero() {
        return format!("missing: {}", a.transcript);
    }
    format!(
        "{}, {}",
        byte_size(a.transcript_size),
        a.transcript_mtime.0.with_timezone(&Local).format("%H:%M:%S")
    )
}

/// Go: byteSize.
pub(crate) fn byte_size(n: i64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

/// Renders a follow line for a person; "" skips it (transcript growth, which
/// would scroll by every second) (Go: formatExportLine).
pub(crate) fn format_export_line(l: &Line) -> String {
    let now = || Local::now().format("%H:%M:%S%.3f").to_string();
    match l {
        Line::Agent(l) => {
            let a = &l.agent;
            let name = format!("{}@{}", a.name, a.host);
            let at = l.at.0.with_timezone(&Local).format("%H:%M:%S%.3f").to_string();
            if l.r#type == LINE_SPAWNED {
                let by = if a.parent.is_empty() { "a person" } else { a.parent.as_str() };
                return format!("{at} spawned {name} ({}, by {by}, in {})", a.backend, a.cwd);
            }
            if l.r#type == LINE_REMOVED {
                let status = if a.status.is_empty() { "-" } else { a.status.as_str() };
                return format!("{at} removed {name} (last status {status})");
            }
            if l.reason == REASON_SESSION {
                let state = if a.attached == Some(true) { "back" } else { "gone" };
                return format!("{at} {name}: session {} {state}", a.session);
            }
            if l.reason == REASON_STATUS {
                let mut line = format!("{at} {name} {}", printable(&a.status));
                if !a.event.is_empty() {
                    line += &format!(" event={}", printable(&a.event));
                }
                if !a.detail.is_empty() {
                    line += &format!(" detail={}", strconv::quote(&agent_service::one_line(&a.detail)));
                }
                return line;
            }
            String::new()
        }
        Line::Store(l) => format!("{} store {}: {}", now(), l.store.dir, plural(l.agents.len(), "agent")),
        Line::Host(l) => {
            if !l.host.error.is_empty() {
                return format!("{} host {}: {}", now(), l.host.host, l.host.error);
            }
            format!("{} host {}: {}", now(), l.host.host, plural(l.agents.len(), "agent"))
        }
        Line::Unrecorded(l) => {
            let names: Vec<&str> = l.sessions.iter().map(|s| s.session.as_str()).collect();
            if names.is_empty() {
                return format!("{} every agent session is recorded", now());
            }
            format!("{} unrecorded sessions: {}", now(), names.join(", "))
        }
    }
}

/// s, or s quoted if it has non-printable characters or spaces (Go:
/// printable, in events.go).
fn printable(s: &str) -> String {
    if s.chars().any(|r| !strconv::is_print(r) || r.is_whitespace()) {
        return strconv::quote(s);
    }
    s.to_string()
}

#[cfg(test)]
#[path = "export_test.rs"]
mod export_test;
