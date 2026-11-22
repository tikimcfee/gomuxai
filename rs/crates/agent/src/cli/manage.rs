//! `agent manage` (no Go file: new in the Rust build). The view the manager
//! GUI draws (`agent-manage` has its types and rules), and the data
//! management behind it: untracking sessions, backups, pruning. It is also
//! what this host's bus answers op `manage` with (remote_ops.rs).
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use agent_base::buildinfo;
use agent_base::gostd::filepath;
use agent_bus::{Context, FleetAgent, RpcClient};
use agent_discovery::{self as discovery, Query, Session, sessions::Liveness};
use agent_fleet::fleet;
use agent_manage::{
    AgentRow, Backup, DataReport, Facts, Footprint, FrictionRow, HistoryRow, HostView, LlmUsage, Request,
    SessionRow, SessionState, Tokens, View, classify, human_bytes, human_tokens, is_stale, llm_usage,
};
use agent_store::{CommandFilter, FrictionFilter, Time, Untracked, UsageRow};
use agent_transcript::UsageScan;
use anyhow::Context as _;
use chrono::{DateTime, Local, Utc};

use super::command::{ArgRule, Def, Doc};
use super::output::plural;
use super::remote::{call_remote, canonical_host, is_remote_host, trusted_hosts};
use super::root::Ctx;
use super::sessions::{parse_since, workgroup_agents};
use super::transcript::{live_now, session_dirs};

// `agent manage`

pub const DOC: Doc = Doc {
    use_line: "manage",
    short: "See every agent and session with its tokens and state, and manage the data: untrack, back up, prune",
    long: r#"Manage is the manager's side of agent: one view of who is running, where,
on what model, with how much context and how many tokens, and the commands
that keep the data in hand. The manager GUI (agent-manager) draws "agent
manage view --json" and runs these commands; they work without it.

Nothing here is a second record. The view is read from this store and the
harnesses' transcripts each time it is asked for. The store keeps two
things for it: which sessions a person stopped tracking, and a cache of
each transcript's token count (rebuilt from the transcript when missing).

  view       the view, for a person or as JSON
  untrack    stop tracking sessions (the transcripts stay)
  track      track them again
  backup     copy the database to backups/
  prune      delete old records of finished things"#,
    example: r#"  agent manage view
  agent manage view --json --usage-budget 2s
  agent manage untrack --state ended,dormant --older-than 30d --dry-run
  agent manage backup
  agent manage prune --older-than 30d --dry-run"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(
        &DOC,
        vec![view_def(), untrack_def(), track_def(), backup_def(), prune_def()],
    )
}

// `agent manage view`

pub const VIEW: Doc = Doc {
    use_line: "view",
    short: "Show agents, sessions, tokens, friction, history and data for this host and its peers",
    long: r#"View shows, for this host and every peer its bus trusts (asked at once):
its agents; every harness session (Claude Code, agy, Kimi Code; spawned by
agent or started by a person) with its state and token usage; its friction
box; its recent command history; and what its data takes on disk. Without
--json it prints a summary per host; --json prints the whole view as one
document (agent-manage's View).

A session's STATE is decided from its status, as agent sessions resolves
it, and whether its transcript is on disk:
  running   a CLI holds it now (idle, processing, waiting, error; or agy
            holds the conversation open)
  dormant   not on right now, and nobody ended it: ended (gone), what a
            reboot or a closed terminal leaves. It can be resumed.
  ended     ended on purpose: quit, finished, closed, replaced, parent
  missing   an agent's record names it, but its transcript isn't there
  unknown   its status can't be told
A session that isn't running and hasn't been written for --stale-days
days (default 14) is also flagged stale.

Tokens are counted from each transcript's own records of its model calls:
input, output, cache read and cache write, and "context", the size of the
latest call's prompt (no harness records the model's window, so it is a
count, not a share). A subagent's session has its own count; the session
that started it also carries the sum with its subagents. Counting reads
whole transcripts, so the counts are cached in the store by path and a
later view reads only what a transcript gained. --usage-budget limits how
long one view spends counting (sessions it didn't reach come without
usage, and "usage_pending" says how many); a peer is always asked with a
budget of 2s, so its first answers fill in over a few views.

A host's LLM plan limits ("llm_usage": the share of each Claude, GLM,
Kimi Code and agy limit used and when it resets, Claude's extra-usage
credits, OpenRouter spend) are llm-exp's, as its usage widget shows them:
view reads the snapshot llm-exp caches in
~/.cache/llm-exp/usage-snapshot.json, as it is, and asks no provider
itself. A host without that file has none.

Sessions a person stopped tracking (agent manage untrack) are left out
unless --untracked. --host shows one host; --local this host only. A peer
that doesn't answer (its bus is down, not linked, on an older agent, or
doesn't list manage in its --remote-ops) is shown as unreachable, with why."#,
    example: r#"  agent manage view
  agent manage view --local --json
  agent manage view --host hub --sessions 100"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent manage view`.
#[derive(clap::Args, Debug, Clone)]
pub struct ViewArgs {
    #[arg(long = "history", value_name = "int", default_value = "200", value_parser = super::flags::parse_int,
        help = "The newest N commands of each host's history (0: none)")]
    pub history: i64,
    #[arg(
        long = "host",
        value_name = "string",
        default_value = "",
        help = "Show only this host (default: this host and every trusted peer)"
    )]
    pub host: String,
    #[arg(long = "json", help = "Print the view as one JSON document")]
    pub json: bool,
    #[arg(long = "local", help = "This host only, asking no peer")]
    pub local: bool,
    #[arg(long = "sessions", value_name = "int", default_value = "0", value_parser = super::flags::parse_int,
        help = "Only the newest N sessions of each host, each with its subagents (0: all here, the newest 300 of a peer)")]
    pub sessions: i64,
    #[arg(long = "stale-days", value_name = "int", default_value = "14", value_parser = super::flags::parse_int,
        help = "Flag a session that isn't running and wasn't written for this many days as stale (0: never)")]
    pub stale_days: i64,
    #[arg(long = "untracked", help = "Include the sessions a person stopped tracking")]
    pub untracked: bool,
    #[arg(
        long = "usage-budget",
        value_name = "duration",
        default_value = "",
        help = "Spend at most this long counting tokens in transcripts not counted yet (2s, 500ms; default: as long as it takes)"
    )]
    pub usage_budget: String,
    /// The positional arguments, checked against VIEW.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn view_def() -> Def {
    leaf!(VIEW, ViewArgs, run_view)
}

/// How many sessions a peer is asked for unless --sessions says: a bus
/// message holds about 1 MB.
const PEER_SESSIONS: i64 = 300;
/// How long a peer may spend counting tokens for one answer.
const PEER_USAGE_BUDGET_MS: i64 = 2000;
/// A peer's history is cut to this many commands, for the same reason.
const PEER_HISTORY: i64 = 100;

pub fn run_view(ctx: &Ctx, args: &ViewArgs) -> anyhow::Result<()> {
    if args.local && !args.host.is_empty() {
        anyhow::bail!("--local and --host don't go together: --local is this host, --host names one");
    }
    let budget = if args.usage_budget.is_empty() {
        0
    } else {
        parse_duration(&args.usage_budget)
            .with_context(|| format!("--usage-budget {}", args.usage_budget))?
            .as_millis()
            .max(1) as i64
    };
    let req = Request {
        sessions: args.sessions.max(0),
        history: args.history.max(0),
        untracked: args.untracked,
        stale_days: args.stale_days.max(0),
        usage_budget_ms: budget,
    };
    let peer_req = peer_request(&req);
    let mut hosts = Vec::new();
    if !args.host.is_empty() {
        agent_bus::validate_host(&args.host)?;
        let host = canonical_host(ctx, &args.host);
        if is_remote_host(ctx, &host) {
            hosts.push(call_remote(ctx, &host, |c| {
                c.manage(&Context::background(), &host, &peer_req)
            })?);
        } else {
            hosts.push(local_host(ctx, &req)?);
        }
    } else {
        hosts.push(local_host(ctx, &req)?);
        if !args.local {
            hosts.extend(peer_hosts(ctx, &peer_req));
        }
    }
    let view = View {
        at: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        host: ctx.cfg().host.clone(),
        hosts,
    };
    if args.json {
        return ctx.print_json(&view);
    }
    ctx.write_out(&render(&view))
}

/// What a peer is asked for, given what this host was: less, so the answer
/// fits a bus message and comes within the bus's timeout.
fn peer_request(req: &Request) -> Request {
    Request {
        sessions: if req.sessions > 0 {
            req.sessions.min(PEER_SESSIONS)
        } else {
            PEER_SESSIONS
        },
        history: req.history.min(PEER_HISTORY),
        usage_budget_ms: PEER_USAGE_BUDGET_MS,
        ..req.clone()
    }
}

/// Every trusted peer's view, asked at once. One that doesn't answer is
/// unreachable, with why; with no bus running here, so is every peer.
fn peer_hosts(ctx: &Ctx, req: &Request) -> Vec<HostView> {
    let peers = trusted_hosts(ctx);
    if peers.is_empty() {
        return Vec::new();
    }
    let unreachable = |host: &str, why: String| HostView {
        host: host.to_string(),
        reachable: false,
        error: why,
        ..HostView::default()
    };
    let nc = match ctx.bus_connect() {
        Ok(nc) => nc,
        Err(e) => {
            let why = if agent_bus::is_no_bus(&e) {
                "no agent bus runs here (agent bus start)".to_string()
            } else {
                format!("connecting to the agent bus: {e:#}")
            };
            return peers.iter().map(|p| unreachable(p, why.clone())).collect();
        }
    };
    let rpc = RpcClient::new(nc.clone(), Duration::ZERO);
    let out = std::thread::scope(|s| {
        let handles: Vec<_> = peers
            .iter()
            .map(|p| {
                let rpc = &rpc;
                s.spawn(move || rpc.manage(&Context::background(), p, req))
            })
            .collect();
        handles
            .into_iter()
            .zip(&peers)
            .map(|(h, p)| match h.join() {
                Ok(Ok(v)) => v,
                Ok(Err(e)) => unreachable(p, format!("{e:#}")),
                Err(_) => unreachable(p, "asking it failed".to_string()),
            })
            .collect()
    });
    nc.close();
    out
}

/// This host's part of the view. The bus's manage op answers with the same.
pub(crate) fn local_host(ctx: &Ctx, req: &Request) -> anyhow::Result<HostView> {
    let svc = ctx.service()?;
    let store = ctx.store()?;
    let now = Utc::now();

    let recorded = store.list_agents()?;
    let adopted: HashSet<&str> = recorded.iter().filter(|a| a.adopted).map(|a| a.name.as_str()).collect();
    let fleet_host = fleet::local(svc, svc.storage(), svc.config(), &fleet::Options::default())?;
    let mut agents = Vec::new();
    flatten_agents(&fleet_host.agents, &adopted, &mut agents);

    let (mut sessions, omitted, footprint) = local_sessions(ctx, req, now)?;
    let untracked = store.untracked_sessions()?;
    let hidden: HashSet<String> = untracked
        .iter()
        .map(|u| format!("{}:{}", u.harness, u.session_id))
        .collect();
    for s in &mut sessions {
        s.untracked = hidden.contains(&s.key());
    }
    if !req.untracked {
        sessions.retain(|s| !s.untracked);
    }
    let usage_pending = count_usage(ctx, &mut sessions, req.usage_budget_ms)?;
    sum_subagents(&mut sessions);

    Ok(HostView {
        host: ctx.cfg().host.clone(),
        build: buildinfo::id(),
        reachable: true,
        error: String::new(),
        agents,
        sessions,
        sessions_omitted: omitted as i64,
        usage_pending,
        friction: friction_rows(ctx)?,
        history: history_rows(ctx, req.history)?,
        data: data_report(ctx, &recorded, footprint, untracked.len() as i64)?,
        llm_usage: read_llm_usage(),
    })
}

/// The snapshot llm-exp caches for its usage widget, when this host has one. Only the file
/// is read: no provider is asked and no credential touched (`agent_manage::llm_usage`).
fn read_llm_usage() -> Option<LlmUsage> {
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty())?;
    let path = filepath::join(&[&home, llm_usage::SNAPSHOT_PATH]);
    match std::fs::read(&path) {
        Ok(data) => Some(LlmUsage::parse(&path, &data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(LlmUsage::failed(&path, &format!("reading it: {e}"))),
    }
}

fn flatten_agents(tree: &[FleetAgent], adopted: &HashSet<&str>, out: &mut Vec<AgentRow>) {
    for a in tree {
        let is_adopted = adopted.contains(a.name.as_str());
        out.push(AgentRow {
            name: a.name.clone(),
            parent: a.spawned_by.clone(),
            backend: a.backend.clone(),
            model: a.model.clone(),
            status: a.status.clone(),
            end_reason: a.end_reason.clone(),
            detail: a.detail.clone(),
            since: if a.since.is_zero() {
                String::new()
            } else {
                a.since.format_rfc3339_nano()
            },
            held: a.held,
            session_id: a.session_id.clone(),
            cwd: a.cwd.clone(),
            flags: a.flags.clone(),
            resumable: a.status == "ended"
                && !is_adopted
                && !a.session_id.is_empty()
                && matches!(a.backend.as_str(), "claude" | "agy"),
            adopted: is_adopted,
        });
        flatten_agents(&a.children, adopted, out);
    }
}

fn rfc3339(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

/// This host's sessions as rows (state set, usage not yet), how many groups
/// `req.sessions` left out, and every transcript's footprint by harness.
fn local_sessions(
    ctx: &Ctx,
    req: &Request,
    now: DateTime<Utc>,
) -> anyhow::Result<(Vec<SessionRow>, usize, BTreeMap<String, Footprint>)> {
    let dirs = session_dirs(ctx);
    let agents = workgroup_agents(ctx)?;
    let (live, _) = live_now(ctx);
    let mut index = discovery::new_index(&dirs, &agents);
    let mut footprint: BTreeMap<String, Footprint> = BTreeMap::new();
    for s in index.sessions() {
        if s.path.is_empty() {
            continue;
        }
        let f = footprint.entry(s.harness).or_default();
        f.count += 1;
        f.bytes += s.size;
    }
    let q = Query {
        limit: req.sessions.max(0) as usize,
        ..Query::default()
    };
    let (picked, left) = index.select(&q);
    let resolved = index.resolve(&picked, &live);
    let rows = resolved.iter().map(|s| session_row(s, now, req.stale_days)).collect();
    Ok((rows, left, footprint))
}

fn session_row(s: &Session, now: DateTime<Utc>, stale_days: i64) -> SessionRow {
    // A listed transcript is on disk; one only an agent's record names may
    // not be.
    let transcript = !s.path.is_empty() && (s.workgroup.is_none() || Path::new(&s.path).exists());
    let state = classify(&Facts {
        status: &s.status,
        end_reason: &s.end_reason,
        transcript,
        held_open: s.live == Liveness::Yes,
    });
    SessionRow {
        session_id: s.session_id.clone(),
        harness: s.harness.clone(),
        path: if transcript { s.path.clone() } else { String::new() },
        size: s.size,
        mtime: rfc3339(s.mtime),
        cwd: s.cwd.clone(),
        project: s.project.clone(),
        title: s.title.clone(),
        model: s.model.clone(),
        git_branch: s.git_branch.clone(),
        agent: s.workgroup.as_ref().map(|w| w.name.clone()).unwrap_or_default(),
        parent_session: s
            .parent_session
            .as_ref()
            .map(|p| p.session_id.clone())
            .unwrap_or_default(),
        status: s.status.clone(),
        end_reason: s.end_reason.clone(),
        state,
        stale: is_stale(state, s.mtime, now, stale_days),
        untracked: false,
        usage: None,
        usage_with_subagents: None,
    }
}

/// Sets each row's usage from the store's cache, counting what a transcript
/// gained since (or all of one not counted yet) while the budget lasts, and
/// stores what it counted. Returns how many rows it left without usage.
fn count_usage(ctx: &Ctx, rows: &mut [SessionRow], budget_ms: i64) -> anyhow::Result<i64> {
    let store = ctx.store()?;
    let cache = store.session_usage()?;
    let deadline = (budget_ms > 0).then(|| Instant::now() + Duration::from_millis(budget_ms as u64));
    let mut updates = Vec::new();
    let mut pending = 0;
    for row in rows.iter_mut() {
        if row.path.is_empty() {
            continue;
        }
        let cached = cache
            .get(&row.path)
            .and_then(|c| serde_json::from_str::<UsageScan>(&c.scan).ok().map(|s| (c.size, s)))
            .filter(|(_, s)| s.harness == row.harness);
        if let Some((size, scan)) = &cached
            && *size == row.size
        {
            row.usage = Some(scan.usage.clone());
            continue;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            // What was counted before still says most of it.
            row.usage = cached.map(|(_, s)| s.usage);
            pending += 1;
            continue;
        }
        let mut scan = cached
            .map(|(_, s)| s)
            .unwrap_or_else(|| UsageScan::new(&row.harness));
        if scan.read_file(&row.path).is_err() {
            continue;
        }
        updates.push(UsageRow {
            path: row.path.clone(),
            size: row.size,
            scan: serde_json::to_string(&scan)?,
        });
        row.usage = Some(scan.usage);
    }
    store.set_session_usage(&updates)?;
    Ok(pending)
}

/// Gives each session that started subagents the sum of its own tokens and
/// theirs (a subagent of a subagent counts toward both above it).
fn sum_subagents(rows: &mut [SessionRow]) {
    let index: HashMap<String, usize> = rows.iter().enumerate().map(|(i, r)| (r.key(), i)).collect();
    let mut sums: HashMap<usize, Tokens> = HashMap::new();
    for r in rows.iter() {
        let Some(u) = &r.usage else { continue };
        let mut parent = r.parent_session.clone();
        let mut seen = HashSet::new();
        while !parent.is_empty() && seen.insert(parent.clone()) {
            let Some(&pi) = index.get(&format!("{}:{}", r.harness, parent)) else {
                break;
            };
            sums.entry(pi).or_default().add(&u.tokens);
            parent = rows[pi].parent_session.clone();
        }
    }
    for (i, mut t) in sums {
        if let Some(own) = &rows[i].usage {
            t.add(&own.tokens);
        }
        rows[i].usage_with_subagents = Some(t);
    }
}

/// How many closed friction entries a view carries, newest first, after
/// every open one.
const CLOSED_FRICTION: usize = 50;

fn friction_rows(ctx: &Ctx) -> anyhow::Result<Vec<FrictionRow>> {
    let store = ctx.store()?;
    let mut links: HashMap<i64, Vec<String>> = HashMap::new();
    for l in store.list_friction_links(0)? {
        links
            .entry(l.friction_id)
            .or_default()
            .push(format!("{}@{}", l.peer_id, l.peer_host));
    }
    let mut entries = store.list_friction(&FrictionFilter::default())?;
    let open = |status: &str| matches!(status, "open" | "accepted");
    entries.sort_by(|a, b| {
        open(&b.status)
            .cmp(&open(&a.status))
            .then(b.created_at.0.cmp(&a.created_at.0))
    });
    let mut closed = 0;
    Ok(entries
        .into_iter()
        .filter(|f| {
            if open(&f.status) {
                return true;
            }
            closed += 1;
            closed <= CLOSED_FRICTION
        })
        .map(|f| FrictionRow {
            id: f.id,
            kind: f.kind,
            status: f.status,
            summary: f.summary,
            detail: f.detail,
            filed_by: f.filed_by,
            created_at: f.created_at.format_rfc3339(),
            votes: f.votes,
            links: links.remove(&f.id).unwrap_or_default(),
            resolution: f.resolution,
        })
        .collect())
}

fn history_rows(ctx: &Ctx, limit: i64) -> anyhow::Result<Vec<HistoryRow>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let records = ctx.store()?.list_commands(&CommandFilter {
        limit,
        ..CommandFilter::default()
    })?;
    Ok(records
        .into_iter()
        .map(|r| HistoryRow {
            at: r.started_at.format_rfc3339(),
            agent: r.agent,
            args: r.args,
            exit_code: r.exit_code,
            duration_ms: r.duration_ms,
            error: r.error,
        })
        .collect())
}

fn file_size(path: &Path) -> i64 {
    std::fs::metadata(path).map(|m| m.len() as i64).unwrap_or(0)
}

/// The logs in `logs_dir` of agents that are no longer recorded: each
/// file's path and size. A log is `<name>.log` or `<name>.transcripts`.
fn departed_logs(logs_dir: &str, recorded: &HashSet<String>) -> Vec<(std::path::PathBuf, i64)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(logs_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(agent) = name.strip_suffix(".log").or_else(|| name.strip_suffix(".transcripts")) else {
            continue;
        };
        // hook-errors.log is the hook's own log, not an agent's.
        if agent != "hook-errors" && !recorded.contains(agent) && path.is_file() {
            let size = file_size(&path);
            out.push((path, size));
        }
    }
    out.sort();
    out
}

fn dir_size(dir: &str) -> i64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_size(&p.to_string_lossy())
            } else {
                file_size(&p)
            }
        })
        .sum()
}

fn backups_dir(ctx: &Ctx) -> String {
    filepath::join(&[ctx.data_dir().as_str(), "backups"])
}

/// The database backups in backups/ (`gomuxai-*.db`), newest first.
fn list_backups(ctx: &Ctx) -> Vec<Backup> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(backups_dir(ctx)) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("gomuxai-") || !name.ends_with(".db") {
            continue;
        }
        let Ok(meta) = e.metadata() else { continue };
        out.push(Backup {
            path: path.to_string_lossy().into_owned(),
            bytes: meta.len() as i64,
            at: rfc3339(meta.modified().ok().map(DateTime::<Utc>::from)),
        });
    }
    out.sort_by(|a, b| b.at.cmp(&a.at).then(b.path.cmp(&a.path)));
    out
}

fn data_report(
    ctx: &Ctx,
    recorded: &[agent_store::Agent],
    transcripts: BTreeMap<String, Footprint>,
    untracked: i64,
) -> anyhow::Result<DataReport> {
    let cfg = ctx.cfg();
    let db = &cfg.database_path;
    let db_bytes = [String::new(), "-wal".to_string(), "-shm".to_string()]
        .iter()
        .map(|suffix| file_size(Path::new(&format!("{db}{suffix}"))))
        .sum();
    let names: HashSet<String> = recorded.iter().map(|a| a.name.clone()).collect();
    let departed = departed_logs(&cfg.log_dir, &names);
    Ok(DataReport {
        dir: ctx.data_dir(),
        db_bytes,
        logs_bytes: dir_size(&cfg.log_dir),
        departed_logs_bytes: departed.iter().map(|(_, n)| n).sum(),
        departed_logs: departed.len() as i64,
        tables: ctx.store()?.table_counts()?,
        transcripts,
        backups: list_backups(ctx),
        untracked,
    })
}

/// A duration as Go writes one (`2s`, `500ms`, `1m30s`), or whole days or
/// weeks (`30d`, `2w`).
fn parse_duration(v: &str) -> anyhow::Result<Duration> {
    let v = v.trim();
    let mut total = 0f64;
    let mut rest = v;
    if rest.is_empty() {
        anyhow::bail!("give a duration (2s, 500ms, 30d)");
    }
    while !rest.is_empty() {
        let digits = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
        let n: f64 = rest[..digits]
            .parse()
            .map_err(|_| anyhow::anyhow!("{v:?} isn't a duration (2s, 500ms, 30d)"))?;
        rest = &rest[digits..];
        let unit_len = rest.find(|c: char| c.is_ascii_digit() || c == '.').unwrap_or(rest.len());
        let secs = match &rest[..unit_len] {
            "ms" => 0.001,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            "d" => 86400.0,
            "w" => 7.0 * 86400.0,
            u => anyhow::bail!("{v:?}: unknown unit {u:?} (ms, s, m, h, d, w)"),
        };
        total += n * secs;
        rest = &rest[unit_len..];
    }
    Ok(Duration::from_secs_f64(total))
}

/// The view for a person: a summary per host.
pub(crate) fn render(view: &View) -> String {
    let mut out = String::new();
    for (i, h) in view.hosts.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if !h.reachable {
            let _ = writeln!(out, "{}: unreachable: {}", h.host, h.error);
            continue;
        }
        let _ = writeln!(out, "{} (build {})", h.host, h.build);
        let _ = writeln!(out, "  agents:   {}", counted(h.agents.iter().map(|a| a.status.as_str())));
        let mut states = counted(h.sessions.iter().map(|s| s.state.as_str()));
        let stale = h.sessions.iter().filter(|s| s.stale).count();
        if stale > 0 {
            let _ = write!(states, "; {stale} stale");
        }
        if h.sessions_omitted > 0 {
            let _ = write!(states, "; {} older left out", h.sessions_omitted);
        }
        if h.data.untracked > 0 {
            let _ = write!(states, "; {} untracked", h.data.untracked);
        }
        let _ = writeln!(out, "  sessions: {states}");
        let mut t = Tokens::default();
        for s in &h.sessions {
            if let Some(u) = &s.usage {
                t.add(&u.tokens);
            }
        }
        let mut tokens = format!(
            "{} in {} (input {}, output {}, cache read {}, cache write {})",
            human_tokens(t.total()),
            plural(t.calls as usize, "model call"),
            human_tokens(t.input),
            human_tokens(t.output),
            human_tokens(t.cache_read),
            human_tokens(t.cache_creation)
        );
        if h.usage_pending > 0 {
            let _ = write!(tokens, "; {} not counted yet", plural(h.usage_pending as usize, "session"));
        }
        let _ = writeln!(out, "  tokens:   {tokens}");
        let open = h.friction.iter().filter(|f| matches!(f.status.as_str(), "open" | "accepted")).count();
        let _ = writeln!(out, "  friction: {open} open");
        let d = &h.data;
        let mut data = format!("store {}, logs {}", human_bytes(d.db_bytes), human_bytes(d.logs_bytes));
        if d.departed_logs > 0 {
            let _ = write!(
                data,
                " ({} of {})",
                human_bytes(d.departed_logs_bytes),
                plural(d.departed_logs as usize, "departed agent's log")
            );
        }
        for (harness, f) in &d.transcripts {
            let _ = write!(data, ", {harness} transcripts {} in {}", human_bytes(f.bytes), f.count);
        }
        let _ = writeln!(out, "  data:     {data}");
        match d.backups.first() {
            Some(b) => {
                let _ = writeln!(out, "  backups:  {}, newest {} ({})", d.backups.len(), b.path, b.at);
            }
            None => {
                let _ = writeln!(out, "  backups:  none (agent manage backup)");
            }
        }
        if let Some(u) = &h.llm_usage {
            let mut limits = u.summary();
            match agent_manage::human_age(&u.captured_at, Utc::now()).as_str() {
                "" => {}
                "now" => limits.push_str(" (fetched just now)"),
                age => {
                    let _ = write!(limits, " (fetched {age} ago)");
                }
            }
            let _ = writeln!(out, "  limits:   {limits}");
        }
    }
    out
}

/// "5 (3 idle, 2 ended)": how many, and how many of each, most first.
fn counted<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let mut by: BTreeMap<&str, usize> = BTreeMap::new();
    let mut n = 0;
    for i in items {
        *by.entry(i).or_default() += 1;
        n += 1;
    }
    if n == 0 {
        return "none".to_string();
    }
    let mut parts: Vec<_> = by.into_iter().collect();
    parts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let list: Vec<String> = parts.iter().map(|(k, v)| format!("{v} {k}")).collect();
    format!("{n} ({})", list.join(", "))
}

// `agent manage untrack`

pub const UNTRACK: Doc = Doc {
    use_line: "untrack [session...]",
    short: "Stop tracking sessions: agent manage leaves them out (their transcripts stay)",
    long: r#"Untrack makes agent manage leave sessions out of its view. Nothing is
deleted: the transcript stays where its harness keeps it, the harness can
still resume it, and agent sessions still lists it. "agent manage track"
brings a session back, and "agent manage view --untracked" shows them.

Name sessions as HARNESS:ID (claude:3f3a9c..., as the view's JSON keys
them) or by an id or the start of one, if only one session has it. Or pick
them by what they are, with any of:
  --state S[,S]      running, dormant, ended, missing, unknown
  --older-than WHEN  not written since: a duration back from now (30d, 2w,
                     12h) or a local date (2026-11-01)
  --harness H        claude, agy, kimi
  --stale            flagged stale (see agent manage view)
A subagent's session goes with the session that started it.

A running session is never untracked unless it is named and --force is
given. --dry-run lists what would be untracked and changes nothing.

An agent's own record is a different thing: "agent cleanup <name>" removes
an ended agent. Untracking its sessions doesn't."#,
    example: r#"  agent manage untrack claude:3f3a9c12-88aa-4c0e-9a51-0f6f0c1d2e3f
  agent manage untrack 3f3a9c
  agent manage untrack --state ended,dormant --older-than 30d --dry-run"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent manage untrack`.
#[derive(clap::Args, Debug, Clone)]
pub struct UntrackArgs {
    #[arg(long = "dry-run", help = "List what would be untracked; change nothing")]
    pub dry_run: bool,
    #[arg(long = "force", help = "Untrack a named session even though it is running")]
    pub force: bool,
    #[arg(
        long = "harness",
        value_name = "string",
        default_value = "",
        help = "Only this harness's sessions: claude, agy, kimi"
    )]
    pub harness: String,
    #[arg(
        long = "older-than",
        value_name = "string",
        default_value = "",
        help = "Only sessions not written since then: a duration back from now (30d, 2w) or a local date"
    )]
    pub older_than: String,
    #[arg(long = "stale", help = "Only sessions flagged stale (not running, not written for 14 days)")]
    pub stale: bool,
    #[arg(
        long = "state",
        value_name = "string",
        default_value = "",
        help = "Only sessions in one of these states: running, dormant, ended, missing, unknown"
    )]
    pub state: String,
    /// The sessions, as HARNESS:ID or an id's start.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn untrack_def() -> Def {
    leaf!(UNTRACK, UntrackArgs, run_untrack)
}

/// Every session of this host as a row, untracked ones marked.
fn all_rows(ctx: &Ctx) -> anyhow::Result<Vec<SessionRow>> {
    let req = Request::default();
    let (mut rows, _, _) = local_sessions(ctx, &req, Utc::now())?;
    let hidden: HashSet<String> = ctx
        .store()?
        .untracked_sessions()?
        .iter()
        .map(|u| format!("{}:{}", u.harness, u.session_id))
        .collect();
    for r in &mut rows {
        r.untracked = hidden.contains(&r.key());
    }
    Ok(rows)
}

/// The row a word names: `HARNESS:ID` exactly, or the one session whose id
/// starts with it.
fn find_row<'a>(rows: &'a [SessionRow], word: &str) -> anyhow::Result<&'a SessionRow> {
    if let Some(r) = rows.iter().find(|r| r.key() == word) {
        return Ok(r);
    }
    let id = word.split_once(':').map_or(word, |(_, id)| id);
    let harness = word.split_once(':').map(|(h, _)| h);
    let found: Vec<&SessionRow> = rows
        .iter()
        .filter(|r| harness.is_none_or(|h| r.harness == h) && r.session_id.starts_with(id) && !id.is_empty())
        .collect();
    match found.as_slice() {
        [one] => Ok(one),
        [] => anyhow::bail!("no session {word} (agent sessions --all lists them)"),
        many => anyhow::bail!(
            "{word} is the start of {} sessions: {}; name one as HARNESS:ID",
            many.len(),
            many.iter().take(5).map(|r| r.key()).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// `--state`'s states.
fn parse_states(v: &str) -> anyhow::Result<Vec<SessionState>> {
    v.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            SessionState::parse(s)
                .ok_or_else(|| anyhow::anyhow!("--state {s}: use running, dormant, ended, missing or unknown"))
        })
        .collect()
}

pub fn run_untrack(ctx: &Ctx, args: &UntrackArgs) -> anyhow::Result<()> {
    let states = parse_states(&args.state)?;
    let before = if args.older_than.is_empty() {
        None
    } else {
        Some(parse_since(&args.older_than, Local::now()).with_context(|| format!("--older-than {}", args.older_than))?)
    };
    let filtered = !states.is_empty() || before.is_some() || args.stale || !args.harness.is_empty();
    if args.args.is_empty() && !filtered {
        anyhow::bail!("name the sessions to untrack, or pick them with --state, --older-than, --stale or --harness");
    }
    if !args.args.is_empty() && filtered {
        anyhow::bail!("name sessions or pick them with flags, not both");
    }
    let rows = all_rows(ctx)?;
    let mut picked: Vec<&SessionRow> = Vec::new();
    let mut skipped_running = 0;
    if filtered {
        let matches = |r: &SessionRow| {
            (states.is_empty() || states.contains(&r.state))
                && (args.harness.is_empty() || r.harness == args.harness)
                && (!args.stale || r.stale)
                && before.is_none_or(|b| {
                    DateTime::parse_from_rfc3339(&r.mtime).is_ok_and(|m| m.with_timezone(&Utc) < b)
                })
        };
        // A subagent goes with the session that started it, never alone.
        let tops: HashSet<String> = rows
            .iter()
            .filter(|r| r.parent_session.is_empty() && !r.untracked && matches(r))
            .map(|r| r.key())
            .collect();
        for r in &rows {
            if r.untracked {
                continue;
            }
            let top = top_of(&rows, r);
            if !tops.contains(&top) {
                continue;
            }
            if r.state == SessionState::Running {
                skipped_running += 1;
                continue;
            }
            picked.push(r);
        }
    } else {
        for word in &args.args {
            let r = find_row(&rows, word)?;
            if r.state == SessionState::Running && !args.force {
                anyhow::bail!(
                    "{} is running ({}): --force untracks it anyway",
                    r.key(),
                    r.status
                );
            }
            picked.push(r);
            // Its subagents go with it.
            for sub in &rows {
                if !sub.parent_session.is_empty() && top_of(&rows, sub) == r.key() && !sub.untracked {
                    picked.push(sub);
                }
            }
        }
        let mut seen = HashSet::new();
        picked.retain(|r| seen.insert(r.key()));
    }

    let mut out = String::new();
    let verb = if args.dry_run { "Would untrack" } else { "Untracked" };
    if picked.is_empty() {
        out.push_str("No sessions to untrack.\n");
    } else {
        let bytes: i64 = picked.iter().map(|r| r.size).sum();
        if !args.dry_run {
            let store = ctx.store()?;
            let by = match ctx.caller_agent() {
                a if a.is_empty() => "a person".to_string(),
                a => a,
            };
            for r in &picked {
                store.untrack_session(&Untracked {
                    harness: r.harness.clone(),
                    session_id: r.session_id.clone(),
                    path: r.path.clone(),
                    title: r.title.clone(),
                    by: by.clone(),
                    at: Time::now(),
                })?;
            }
        }
        const SHOWN: usize = 40;
        for r in picked.iter().take(SHOWN) {
            let _ = writeln!(
                out,
                "  {}  {:<8} {:<16} {}",
                r.key(),
                r.state.as_str(),
                r.project,
                r.title
            );
        }
        if picked.len() > SHOWN {
            let _ = writeln!(out, "  ... and {} more", picked.len() - SHOWN);
        }
        let _ = writeln!(
            out,
            "{verb} {} ({} of transcripts, left in place).",
            plural(picked.len(), "session"),
            human_bytes(bytes)
        );
    }
    if skipped_running > 0 {
        let _ = writeln!(
            out,
            "Left {} tracked.",
            plural(skipped_running, "running session")
        );
    }
    ctx.write_out(&out)
}

/// The key of the session at the top of `r`'s group: itself, or the session
/// that (through however many subagents) started it.
fn top_of(rows: &[SessionRow], r: &SessionRow) -> String {
    let mut cur = r;
    let mut seen = HashSet::new();
    while !cur.parent_session.is_empty() && seen.insert(cur.key()) {
        let parent = format!("{}:{}", cur.harness, cur.parent_session);
        match rows.iter().find(|p| p.key() == parent) {
            Some(p) => cur = p,
            None => break,
        }
    }
    cur.key()
}

// `agent manage track`

pub const TRACK: Doc = Doc {
    use_line: "track [session...]",
    short: "Track untracked sessions again",
    long: r#"Track undoes "agent manage untrack": the sessions are in the view again.
Name them as HARNESS:ID or by the start of an id; --all tracks every one.
"agent manage view --untracked --json" lists the untracked ones."#,
    example: r#"  agent manage track claude:3f3a9c12-88aa-4c0e-9a51-0f6f0c1d2e3f
  agent manage track --all"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent manage track`.
#[derive(clap::Args, Debug, Clone)]
pub struct TrackArgs {
    #[arg(long = "all", help = "Track every untracked session again")]
    pub all: bool,
    /// The sessions, as HARNESS:ID or an id's start.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn track_def() -> Def {
    leaf!(TRACK, TrackArgs, run_track)
}

pub fn run_track(ctx: &Ctx, args: &TrackArgs) -> anyhow::Result<()> {
    let store = ctx.store()?;
    let untracked = store.untracked_sessions()?;
    if args.all == !args.args.is_empty() {
        anyhow::bail!("name the sessions to track again, or --all for every one");
    }
    let mut picked = Vec::new();
    if args.all {
        picked.extend(untracked.iter());
    } else {
        for word in &args.args {
            let id = word.split_once(':').map_or(word.as_str(), |(_, id)| id);
            let harness = word.split_once(':').map(|(h, _)| h);
            let found: Vec<_> = untracked
                .iter()
                .filter(|u| harness.is_none_or(|h| u.harness == h) && !id.is_empty() && u.session_id.starts_with(id))
                .collect();
            match found.as_slice() {
                [one] => picked.push(*one),
                [] => anyhow::bail!("no untracked session {word}"),
                many => anyhow::bail!(
                    "{word} is the start of {} untracked sessions; name one as HARNESS:ID",
                    many.len()
                ),
            }
        }
    }
    for u in &picked {
        store.track_session(&u.harness, &u.session_id)?;
    }
    ctx.write_out(&format!("Tracking {} again.\n", plural(picked.len(), "session")))
}

// `agent manage backup`

pub const BACKUP: Doc = Doc {
    use_line: "backup",
    short: "Copy this host's database to backups/",
    long: r#"Backup writes a consistent copy of this host's database (agents, state,
messages, friction, history, jobs, everything agent stores) to
backups/gomuxai-<date>-<time>.db in the data directory, while agents keep
running. It prints the file. To restore one, stop the agents and the bus
and copy it over gomuxai.db.

Transcripts are the harnesses' files, and terminal logs are in logs/:
neither is in the database, so neither is in a backup.

--keep N then deletes all but the newest N backups this command made."#,
    example: r#"  agent manage backup
  agent manage backup --keep 5"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent manage backup`.
#[derive(clap::Args, Debug, Clone)]
pub struct BackupArgs {
    #[arg(long = "keep", value_name = "int", default_value = "0", value_parser = super::flags::parse_int,
        help = "Afterwards, delete all but the newest N backups this command made (0: keep all)")]
    pub keep: i64,
    /// The positional arguments, checked against BACKUP.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn backup_def() -> Def {
    leaf!(BACKUP, BackupArgs, run_backup)
}

/// The names `agent manage backup` gives its files; only these does --keep
/// delete.
const BACKUP_PREFIX: &str = "gomuxai-";

/// Backs the database up and returns the file.
fn backup_now(ctx: &Ctx) -> anyhow::Result<Backup> {
    let dir = backups_dir(ctx);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {dir}"))?;
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    let mut path = filepath::join(&[dir.as_str(), &format!("{BACKUP_PREFIX}{stamp}.db")]);
    let mut n = 1;
    while Path::new(&path).exists() {
        n += 1;
        path = filepath::join(&[dir.as_str(), &format!("{BACKUP_PREFIX}{stamp}-{n}.db")]);
    }
    ctx.store()?.backup_to(&path)?;
    Ok(Backup {
        bytes: file_size(Path::new(&path)),
        at: rfc3339(Some(Utc::now())),
        path,
    })
}

/// Whether a backup's file name is one backup_now writes
/// (`gomuxai-YYYYMMDD-HHMMSS[-N].db`): a backup made by hand or by another
/// tool under another name is never deleted.
fn made_by_backup(path: &str) -> bool {
    let name = filepath::base(path);
    let Some(stamp) = name.strip_prefix(BACKUP_PREFIX).and_then(|s| s.strip_suffix(".db")) else {
        return false;
    };
    let mut parts = stamp.split('-');
    let ok = |p: Option<&str>, len: usize| p.is_some_and(|p| p.len() == len && p.bytes().all(|b| b.is_ascii_digit()));
    ok(parts.next(), 8)
        && ok(parts.next(), 6)
        && parts.all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

pub fn run_backup(ctx: &Ctx, args: &BackupArgs) -> anyhow::Result<()> {
    let b = backup_now(ctx)?;
    let mut out = format!("Backed up to {} ({}).\n", b.path, human_bytes(b.bytes));
    if args.keep > 0 {
        let ours: Vec<Backup> = list_backups(ctx).into_iter().filter(|b| made_by_backup(&b.path)).collect();
        let mut freed = 0;
        let mut removed = 0;
        for old in ours.iter().skip(args.keep as usize) {
            if std::fs::remove_file(&old.path).is_ok() {
                freed += old.bytes;
                removed += 1;
            }
        }
        if removed > 0 {
            let _ = writeln!(out, "Deleted {} ({}).", plural(removed, "older backup"), human_bytes(freed));
        }
    }
    ctx.write_out(&out)
}

// `agent manage prune`

pub const PRUNE: Doc = Doc {
    use_line: "prune",
    short: "Delete old records of finished things: history, events, departed agents' runs and logs",
    long: r#"Prune deletes what this host recorded about things that are over and
older than --older-than (a duration back from now: 30d, 2w; or a local
date):
  command_log       agent history's records
  agent_events      status events (what agent export --follow replays)
  runs              runs of agents that were cleaned up
  session_reports   last reports of ended sessions no agent owns
  messages          delivered or failed messages (never held ones)
  jobs              finished agent run jobs, with the script each ran
  session_usage     cached token counts not refreshed since (recounted
                    from the transcript when next needed)
With --logs it also deletes the terminal logs and transcript lists in
logs/ of agents no longer recorded, not written since then.

It never touches recorded agents, their state or held messages, friction,
tasks, scripts, untracked sessions, or any harness's transcripts.

--dry-run prints what would go and changes nothing. Otherwise prune first
backs the database up (agent manage backup; --no-backup skips that), and
afterwards returns the freed space to the filesystem."#,
    example: r#"  agent manage prune --older-than 30d --dry-run
  agent manage prune --older-than 30d --logs"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent manage prune`.
#[derive(clap::Args, Debug, Clone)]
pub struct PruneArgs {
    #[arg(long = "dry-run", help = "Print what would be deleted; change nothing")]
    pub dry_run: bool,
    #[arg(long = "logs", help = "Also delete the logs of agents no longer recorded")]
    pub logs: bool,
    #[arg(long = "no-backup", help = "Don't back the database up first")]
    pub no_backup: bool,
    #[arg(
        long = "older-than",
        value_name = "string",
        default_value = "",
        help = "What counts as old: a duration back from now (30d, 2w) or a local date (required)"
    )]
    pub older_than: String,
    /// The positional arguments, checked against PRUNE.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn prune_def() -> Def {
    leaf!(PRUNE, PruneArgs, run_prune)
}

pub fn run_prune(ctx: &Ctx, args: &PruneArgs) -> anyhow::Result<()> {
    if args.older_than.is_empty() {
        anyhow::bail!("say what counts as old: --older-than 30d (or 2w, or a date)");
    }
    let before =
        parse_since(&args.older_than, Local::now()).with_context(|| format!("--older-than {}", args.older_than))?;
    let store = ctx.store()?;
    let cutoff = Time::parse_rfc3339(&before.to_rfc3339()).map_err(|e| anyhow::anyhow!("{e}"))?;

    let mut old_logs = Vec::new();
    if args.logs {
        let names: HashSet<String> = store.list_agents()?.into_iter().map(|a| a.name).collect();
        for (path, size) in departed_logs(&ctx.cfg().log_dir, &names) {
            let written = std::fs::metadata(&path).and_then(|m| m.modified()).ok().map(DateTime::<Utc>::from);
            if written.is_some_and(|w| w < before) {
                old_logs.push((path, size));
            }
        }
    }
    let log_bytes: i64 = old_logs.iter().map(|(_, n)| n).sum();

    let mut out = String::new();
    let would = store.prune(&cutoff, true)?;
    if would.total() == 0 && old_logs.is_empty() {
        return ctx.write_out(&format!(
            "Nothing older than {} to prune.\n",
            before.with_timezone(&Local).format("%Y-%m-%d %H:%M")
        ));
    }
    if !args.dry_run && !args.no_backup && would.total() > 0 {
        let b = backup_now(ctx)?;
        let _ = writeln!(out, "Backed up to {} ({}).", b.path, human_bytes(b.bytes));
    }
    let pruned = if args.dry_run { would } else { store.prune(&cutoff, false)? };
    let verb = if args.dry_run { "Would delete" } else { "Deleted" };
    for (table, n) in &pruned.rows {
        let _ = writeln!(out, "  {table:<16} {n}");
    }
    if pruned.total() > 0 {
        let _ = writeln!(out, "{verb} {}.", plural(pruned.total() as usize, "record"));
    }
    if !old_logs.is_empty() {
        let mut removed = 0;
        if !args.dry_run {
            for (path, _) in &old_logs {
                if std::fs::remove_file(path).is_ok() {
                    removed += 1;
                }
            }
        } else {
            removed = old_logs.len();
        }
        let _ = writeln!(
            out,
            "{verb} {} of departed agents ({}).",
            plural(removed, "log file"),
            human_bytes(log_bytes)
        );
    }
    if !args.dry_run && pruned.total() > 0 {
        let was = file_size(Path::new(&ctx.cfg().database_path));
        store.compact()?;
        let now = file_size(Path::new(&ctx.cfg().database_path));
        let _ = writeln!(out, "The database is {} (was {}).", human_bytes(now), human_bytes(was));
    }
    ctx.write_out(&out)
}

#[cfg(test)]
#[path = "manage_test.rs"]
pub(crate) mod manage_test;
