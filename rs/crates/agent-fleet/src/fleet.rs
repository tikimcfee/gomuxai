//! Cross-host fleet snapshot, peer gathering, and plain-text rendering
//! (`docs/FLEET.md`, section ``"`agent fleet`"``).
//!
//! Every field and flag is decided strictly from state (SQLite store, session
//! discovery metadata, git commit vs. handoff mtime, systemd/process job state,
//! and peer RPC replies) — never by scraping terminal or log lines.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_base::buildinfo;
use agent_base::config::Config;
use agent_base::gostd::strconv;
use agent_bus::{
    AgentInfo, Context, FLEET_TIMEOUT, FleetAgent, FleetFriction, FleetHost, FleetJob,
    FleetLongSession, FleetReport, FleetRequest, FleetView, ListResponse, OP_FLEET, RpcClient,
    Time, is_unanswered,
};
use agent_discovery::{Overrides as DiscoveryOverrides, dirs as discovery_dirs, new_index};
use agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING, AgentService, FleetFn,
    STATE_KEY_MODEL, friction_is_open, one_line,
};
use agent_store::{
    AGENT_STATUS, Agent, AgentEvent, FRICTION_RESOLVED, FrictionFilter, Store, Time as StoreTime,
};
use agent_transcript::{KIND_SYSTEM, read_file as read_transcript_file};
use anyhow::Result;
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{Connection, OpenFlags};

use crate::{focus, handoff, run};

/// Status `waiting` strictly longer than this raises `stuck: waiting <dur>`.
pub const WAITING_STUCK_AFTER: Duration = Duration::from_secs(10 * 60);

/// Held message count at or above this raises `stuck: <N> held, oldest <dur>`.
pub const HELD_STUCK_COUNT: i64 = 3;

/// Oldest held message strictly older than this raises `stuck: <N> held, oldest <dur>`.
pub const HELD_STUCK_AFTER: Duration = Duration::from_secs(10 * 60);

/// Rolling window (days) for `--report` session and message counts.
pub const REPORT_WINDOW_DAYS: i64 = 7;

/// Live agents older than this appear in `FleetReport::long_sessions`.
pub const LONG_SESSION_AFTER: Duration = Duration::from_secs(3 * 24 * 60 * 60);

/// Per-peer timeout used when querying `fleet` (and `list` fallback).
pub const GATHER_TIMEOUT: Duration = FLEET_TIMEOUT;

/// Optional hook returning a running CLI session's own name/title for an agent.
pub type SessionNameFn = dyn Fn(&Agent, &str) -> Option<String> + Send + Sync;

/// Options controlling [`local`] and [`gather`].
#[derive(Clone, Default)]
pub struct Options {
    /// Populate [`FleetHost::report`] with 7-day and long-session baselines.
    pub report: bool,
    /// Optional clock override for deterministic threshold tests (`None` uses
    /// [`Time::now`]).
    pub now: Option<Time>,
    /// Optional session-name reader override (until `agent_backend`'s
    /// session-name reader is wired in).
    pub session_name: Option<Arc<SessionNameFn>>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("report", &self.report)
            .field("now", &self.now)
            .field("has_session_name_fn", &self.session_name.is_some())
            .finish()
    }
}

/// Builds a [`FleetFn`] closure suitable for [`agent_service::RpcExtras::fleet`].
pub fn rpc_handler(service: Arc<AgentService>, store: Arc<Store>, cfg: Config) -> FleetFn {
    Box::new(move |_ctx, req| {
        local(
            &service,
            &store,
            &cfg,
            &Options {
                report: req.report,
                ..Default::default()
            },
        )
    })
}

/// Builds this host's [`FleetHost`] snapshot strictly from state.
pub fn local(
    service: &AgentService,
    store: &Store,
    cfg: &Config,
    opts: &Options,
) -> Result<FleetHost> {
    let now = opts.now.unwrap_or_else(Time::now);
    let tree_state = store.read_tree(false)?;
    let (events, _) = store.agent_events_after(0, i64::MAX)?;

    let mut run_by_agent: HashMap<String, i64> = HashMap::new();
    for (agent_name, r) in &tree_state.runs {
        if r.ended_at.is_zero() {
            run_by_agent.insert(agent_name.clone(), r.id);
        }
    }

    let mut events_by_agent: HashMap<String, Vec<&AgentEvent>> = HashMap::new();
    for ev in &events {
        if ev.kind == AGENT_STATUS {
            events_by_agent
                .entry(ev.agent.clone())
                .or_default()
                .push(ev);
        }
    }

    let mut flat_agents = Vec::with_capacity(tree_state.agents.len());
    let mut handoffs = Vec::new();
    let mut transcripts_by_agent: HashMap<String, String> = HashMap::new();

    for a in &tree_state.agents {
        let (status, end_reason, detail, model, updated_at, reported_status) =
            match service.status_info(&a.name) {
                Ok(info) => (
                    info.status.to_string(),
                    info.end_reason.as_str().to_string(),
                    info.detail,
                    info.model,
                    info.updated_at,
                    info.reported_status.to_string(),
                ),
                Err(_) => {
                    let fallback_model = store
                        .get_state(&a.name, STATE_KEY_MODEL)?
                        .map(|e| e.value)
                        .unwrap_or_default();
                    (
                        AGENT_STATUS_UNKNOWN.to_string(),
                        String::new(),
                        String::new(),
                        fallback_model,
                        StoreTime::default(),
                        String::new(),
                    )
                }
            };

        let run_id = run_by_agent.get(&a.name).copied().unwrap_or(0);
        let since = resolve_since(
            events_by_agent
                .get(&a.name)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
            run_id,
            &status,
            &reported_status,
            &updated_at,
            &a.created_at,
        );

        let pending = store.get_pending_messages(&a.name, 10_000)?;
        let held = pending.len() as i64;
        let oldest_held = pending
            .first()
            .map(|m| bus_time(&m.created_at))
            .unwrap_or_default();

        let session_id = service.last_session_id(&a.name);
        if let Some(tr) = store.get_state(&a.name, "_transcript")?
            && !tr.value.is_empty()
        {
            transcripts_by_agent.insert(a.name.clone(), tr.value);
        }

        let session_name = match opts.session_name.as_ref() {
            Some(f) => f(a, &session_id).unwrap_or_default(),
            None => service.cli_session_name(&a.name),
        };

        let mut fa = FleetAgent {
            name: a.name.clone(),
            session: a.session_name.clone(),
            spawned_by: a.spawned_by.clone(),
            backend: a.backend.clone(),
            model,
            status,
            end_reason,
            detail,
            since,
            held,
            oldest_held,
            session_id,
            session_name,
            cwd: a.cwd.clone(),
            flags: Vec::new(),
            children: Vec::new(),
        };
        fa.flags = agent_flags(&fa, now);
        flat_agents.push(fa);

        let state_handoff = store
            .get_state(&a.name, handoff::STATE_KEY_HANDOFF)?
            .map(|e| e.value);
        if let Some(ho_path) = handoff::find(&a.cwd, &a.name, state_handoff.as_deref())
            && let Some(st) = handoff::stale(&ho_path)
        {
            handoffs.push(st.into_fleet_handoff(&a.name, &ho_path));
        }
    }

    let focus_ev = focus::get(store)?;
    let jobs = run::list(store, false)?
        .into_iter()
        .map(|j| FleetJob {
            id: j.id,
            script: j.script,
            by: j.by,
            started_at: bus_time(&j.started_at),
            unit: j.unit,
            memory_max: j.memory_max,
        })
        .collect();

    let fr = collect_friction(store)?;

    let report = if opts.report {
        Some(build_report(
            &tree_state.agents,
            &flat_agents,
            &transcripts_by_agent,
            cfg,
            now,
            fr.open_by_kind,
            fr.resolved_by_kind,
        ))
    } else {
        None
    };

    let agents = build_agent_tree(flat_agents);

    Ok(FleetHost {
        host: cfg.host.clone(),
        build: buildinfo::id(),
        reachable: true,
        partial: false,
        error: String::new(),
        agents,
        focus: focus_ev,
        jobs,
        handoffs,
        friction: fr.summary,
        report,
    })
}

/// Computes the diagnostic flags for a single [`FleetAgent`] at instant `now`.
pub fn agent_flags(agent: &FleetAgent, now: Time) -> Vec<String> {
    let mut flags = Vec::new();

    if agent.status == AGENT_STATUS_WAITING.as_str() && !agent.since.is_zero() {
        let waited = elapsed_between(now, agent.since);
        if waited > WAITING_STUCK_AFTER {
            flags.push(format!("stuck: waiting {}", format_duration(waited)));
        }
    }

    if agent.held > 0 {
        let oldest_age = if agent.oldest_held.is_zero() {
            Duration::ZERO
        } else {
            elapsed_between(now, agent.oldest_held)
        };
        if agent.held >= HELD_STUCK_COUNT || oldest_age > HELD_STUCK_AFTER {
            flags.push(format!(
                "stuck: {} held, oldest {}",
                agent.held,
                format_duration(oldest_age)
            ));
        }
    }

    if agent.status == AGENT_STATUS_ENDED.as_str() {
        if agent.end_reason.is_empty() {
            flags.push("ended, still recorded".to_string());
        } else {
            flags.push(format!("ended ({}), still recorded", agent.end_reason));
        }
    }

    if !agent.session_name.is_empty() && agent.session_name != agent.name {
        flags.push(format!(
            "name mismatch: session is {}",
            strconv::quote(&agent.session_name)
        ));
    }

    flags
}

/// Builds a partial [`FleetHost`] from an older host's `list` reply.
pub fn from_list_response(host: &str, list: ListResponse, now: Time) -> FleetHost {
    let flat: Vec<FleetAgent> = list
        .agents
        .into_iter()
        .map(|a: AgentInfo| {
            let mut fa = FleetAgent {
                name: a.name,
                session: a.session,
                spawned_by: a.spawned_by,
                backend: a.backend,
                status: a.status,
                end_reason: a.end_reason,
                detail: a.detail,
                session_id: a.session_id,
                cwd: a.cwd,
                ..Default::default()
            };
            fa.flags = agent_flags(&fa, now);
            fa
        })
        .collect();

    FleetHost {
        host: host.to_string(),
        reachable: true,
        partial: true,
        agents: build_agent_tree(flat),
        ..Default::default()
    }
}

/// Queries a single remote `host` over `rpc` (3s timeout), falling back to
/// `list` (`partial: true`) when `fleet` is unanswered/unserved, or returning
/// `reachable: false` with `error` when neither succeeds.
pub fn query_host(rpc: &RpcClient, host: &str, opts: &Options) -> FleetHost {
    try_query_host(rpc, host, opts).unwrap_or_else(|err| FleetHost {
        host: host.to_string(),
        reachable: false,
        error: format!("{err:#}"),
        ..Default::default()
    })
}

/// [`query_host`] for one host asked on its own (`agent fleet --host`): the
/// error itself when neither `fleet` nor `list` answered, for the caller to
/// report (an unknown or offline host is a failed command there, not a row
/// in a view).
pub fn try_query_host(rpc: &RpcClient, host: &str, opts: &Options) -> Result<FleetHost> {
    let now = opts.now.unwrap_or_else(Time::now);
    let ctx = Context::background();
    match rpc.fleet(
        &ctx,
        host,
        FleetRequest {
            report: opts.report,
        },
    ) {
        Ok(mut fh) => {
            if fh.host.is_empty() {
                fh.host = host.to_string();
            }
            fh.reachable = true;
            Ok(fh)
        }
        Err(fleet_err) if is_unanswered(&fleet_err) || is_fleet_unserved(&fleet_err) => {
            let (list_ctx, _cancel) = Context::with_timeout(&ctx, GATHER_TIMEOUT);
            let list = rpc.list_host(&list_ctx, host)?;
            Ok(from_list_response(host, list, now))
        }
        Err(err) => Err(err),
    }
}

/// Queries `local` plus every peer in `hosts` concurrently (3s budget each).
///
/// When `rpc` is `None` (no bus running) or `hosts` names no remote peers,
/// returns a [`FleetView`] containing only `local`.
pub fn gather(
    local: FleetHost,
    rpc: Option<&RpcClient>,
    hosts: &[String],
    opts: &Options,
) -> FleetView {
    let at = opts.now.unwrap_or_else(Time::now);
    let Some(rpc) = rpc else {
        return FleetView {
            at,
            hosts: vec![local],
        };
    };

    let mut seen: HashSet<String> = HashSet::new();
    if !local.host.is_empty() {
        seen.insert(local.host.to_ascii_lowercase());
    }
    let peers: Vec<&str> = hosts
        .iter()
        .map(String::as_str)
        .filter(|h| !h.is_empty() && seen.insert(h.to_ascii_lowercase()))
        .collect();

    if peers.is_empty() {
        return FleetView {
            at,
            hosts: vec![local],
        };
    }

    let remote_hosts: Vec<FleetHost> = std::thread::scope(|s| {
        let handles: Vec<_> = peers
            .into_iter()
            .map(|peer| s.spawn(move || query_host(rpc, peer, opts)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("fleet query worker panicked"))
            .collect()
    });

    let mut all = Vec::with_capacity(1 + remote_hosts.len());
    all.push(local);
    all.extend(remote_hosts);
    FleetView { at, hosts: all }
}

/// Nests agents under their local `spawned_by` parent (preserving input order
/// among roots and siblings, keeping cross-host `spawned_by` agents at the top
/// level, and breaking parent cycles deterministically).
pub fn build_agent_tree(agents: Vec<FleetAgent>) -> Vec<FleetAgent> {
    if agents.is_empty() {
        return Vec::new();
    }

    // If callers already constructed nested `children`, keep that structure.
    if agents.iter().any(|a| !a.children.is_empty()) {
        return agents;
    }

    let mut nodes: HashMap<String, FleetAgent> = HashMap::with_capacity(agents.len());
    let mut order: Vec<String> = Vec::with_capacity(agents.len());
    for a in agents {
        if !nodes.contains_key(&a.name) {
            order.push(a.name.clone());
            nodes.insert(a.name.clone(), a);
        }
    }

    let listed: HashSet<String> = order.iter().cloned().collect();
    let mut children_names: HashMap<String, Vec<String>> = HashMap::new();
    for name in &order {
        let parent = &nodes[name].spawned_by;
        if !parent.is_empty() && parent != name && listed.contains(parent) {
            children_names
                .entry(parent.clone())
                .or_default()
                .push(name.clone());
        }
    }

    let mut placed: HashSet<String> = HashSet::with_capacity(order.len());
    fn take_subtree(
        name: &str,
        nodes: &mut HashMap<String, FleetAgent>,
        children_names: &HashMap<String, Vec<String>>,
        placed: &mut HashSet<String>,
    ) -> Option<FleetAgent> {
        if !placed.insert(name.to_string()) {
            return None;
        }
        let mut agent = nodes.remove(name)?;
        if let Some(kids) = children_names.get(name) {
            for kid in kids {
                if let Some(child) = take_subtree(kid, nodes, children_names, placed) {
                    agent.children.push(child);
                }
            }
        }
        Some(agent)
    }

    let mut roots = Vec::new();
    for name in &order {
        let has_local_parent = nodes.get(name).is_some_and(|a| {
            !a.spawned_by.is_empty() && a.spawned_by != *name && listed.contains(&a.spawned_by)
        });
        if !has_local_parent
            && let Some(root) = take_subtree(name, &mut nodes, &children_names, &mut placed)
        {
            roots.push(root);
        }
    }

    // Break any parent cycle (from reused agent names) deterministically.
    for name in &order {
        if let Some(root) = take_subtree(name, &mut nodes, &children_names, &mut placed) {
            roots.push(root);
        }
    }

    roots
}

/// Renders a [`FleetView`] in plain `agent list` style, omitting empty sections.
pub fn render(view: &FleetView) -> String {
    let mut out = String::new();
    for (idx, host) in view.hosts.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        write_host_header(&mut out, host);
        if !host.reachable {
            continue;
        }

        if let Some(ref f) = host.focus
            && !f.agent.is_empty()
        {
            let _ = write!(out, "  focus: {}", f.agent);
            if !f.session.is_empty() {
                let _ = write!(out, " ({})", f.session);
            }
            if !f.by.is_empty() {
                let _ = write!(out, " by {}", f.by);
            }
            out.push('\n');
        }

        for (agent, depth) in walk_agents(&host.agents) {
            render_agent(&mut out, agent, depth);
        }

        if !host.jobs.is_empty() {
            out.push_str("  jobs:\n");
            for j in &host.jobs {
                let _ = write!(out, "    #{} {}", j.id, j.script);
                let mut meta = Vec::new();
                if !j.by.is_empty() {
                    meta.push(format!("by {}", j.by));
                }
                if !j.memory_max.is_empty() {
                    meta.push(format!("max {}", j.memory_max));
                }
                if !j.unit.is_empty() {
                    meta.push(j.unit.clone());
                }
                if !meta.is_empty() {
                    let _ = write!(out, " ({})", meta.join(", "));
                }
                out.push('\n');
            }
        }

        if !host.handoffs.is_empty() {
            out.push_str("  stale handoffs:\n");
            for h in &host.handoffs {
                let _ = writeln!(out, "    {}: {}", h.agent, h.path);
            }
        }

        if let Some(ref fr) = host.friction
            && (fr.open > 0 || !fr.by_kind.is_empty())
        {
            if fr.by_kind.is_empty() {
                let _ = writeln!(out, "  friction: {} open", fr.open);
            } else {
                let _ = writeln!(
                    out,
                    "  friction: {} open ({})",
                    fr.open,
                    format_counts(&fr.by_kind)
                );
            }
        }
    }
    out
}

/// Renders the `--report` baseline metrics for each host in `view`.
pub fn render_report(view: &FleetView) -> String {
    let mut out = String::new();
    for (idx, host) in view.hosts.iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        write_host_header(&mut out, host);
        if !host.reachable {
            continue;
        }
        let Some(ref rep) = host.report else {
            if host.partial {
                out.push_str("  (no report: older agent)\n");
            }
            continue;
        };

        if !rep.agents_by_status.is_empty() {
            let _ = writeln!(
                out,
                "  agents by status: {}",
                format_counts(&rep.agents_by_status)
            );
        }
        if !rep.agents_by_backend.is_empty() {
            let _ = writeln!(
                out,
                "  agents by backend: {}",
                format_counts(&rep.agents_by_backend)
            );
        }
        if !rep.sessions_7d_by_harness.is_empty() {
            let _ = writeln!(
                out,
                "  sessions (7d): {}",
                format_counts(&rep.sessions_7d_by_harness)
            );
        }
        if !rep.messages_per_day_7d.is_empty() {
            let _ = writeln!(
                out,
                "  messages/day (7d): {}",
                format_counts(&rep.messages_per_day_7d)
            );
        }
        if !rep.friction_open_by_kind.is_empty() {
            let _ = writeln!(
                out,
                "  friction open: {}",
                format_counts(&rep.friction_open_by_kind)
            );
        }
        if !rep.friction_resolved_by_kind.is_empty() {
            let _ = writeln!(
                out,
                "  friction resolved: {}",
                format_counts(&rep.friction_resolved_by_kind)
            );
        }
        if !rep.long_sessions.is_empty() {
            out.push_str("  long sessions (>3d):\n");
            for ls in &rep.long_sessions {
                let started = if ls.started_at.is_zero() {
                    "-".to_string()
                } else {
                    ls.started_at
                        .0
                        .with_timezone(&Utc)
                        .format("%Y-%m-%d")
                        .to_string()
                };
                let backend = if ls.backend.is_empty() {
                    "-"
                } else {
                    ls.backend.as_str()
                };
                let _ = writeln!(
                    out,
                    "    {} ({backend}, started {started}, {} compactions)",
                    ls.agent, ls.compactions
                );
            }
        }
    }
    out
}

fn write_host_header(out: &mut String, host: &FleetHost) {
    let name = if host.host.is_empty() {
        "local"
    } else {
        host.host.as_str()
    };
    if !host.reachable {
        if host.error.is_empty() {
            let _ = writeln!(out, "{name}: unreachable");
        } else {
            let _ = writeln!(out, "{name}: unreachable ({})", one_line(&host.error));
        }
        return;
    }

    let mut notes = Vec::new();
    if !host.build.is_empty() {
        notes.push(format!("build {}", host.build));
    }
    if host.partial {
        notes.push("partial: list only".to_string());
    }
    if notes.is_empty() {
        let _ = writeln!(out, "{name}");
    } else {
        let _ = writeln!(out, "{name} ({})", notes.join(", "));
    }
}

fn render_agent(out: &mut String, agent: &FleetAgent, depth: usize) {
    let mut status = if agent.status.is_empty() {
        "-".to_string()
    } else {
        agent.status.clone()
    };
    if !agent.end_reason.is_empty() {
        let _ = write!(status, " ({})", agent.end_reason);
    }

    let backend = if agent.backend.is_empty() {
        "-"
    } else {
        agent.backend.as_str()
    };
    let backend_label = if agent.model.is_empty() {
        backend.to_string()
    } else {
        format!("{backend} {}", agent.model)
    };

    let (head_indent, meta_indent) = if depth == 0 {
        ("  ".to_string(), "    ".to_string())
    } else {
        (
            format!("  {}└ ", "  ".repeat(depth)),
            format!("  {}", "  ".repeat(depth + 2)),
        )
    };

    let _ = writeln!(
        out,
        "{head_indent}{}: {status} ({backend_label})",
        agent.name
    );
    if depth == 0 && !agent.spawned_by.is_empty() {
        let _ = writeln!(out, "{meta_indent}parent: {}", agent.spawned_by);
    }
    if !agent.detail.is_empty() {
        let _ = writeln!(out, "{meta_indent}detail: {}", one_line(&agent.detail));
    }
    if !agent.flags.is_empty() {
        let _ = writeln!(out, "{meta_indent}flags:  {}", agent.flags.join("; "));
    }
}

fn walk_agents(roots: &[FleetAgent]) -> Vec<(&FleetAgent, usize)> {
    fn visit<'a>(node: &'a FleetAgent, depth: usize, out: &mut Vec<(&'a FleetAgent, usize)>) {
        out.push((node, depth));
        for child in &node.children {
            visit(child, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for root in roots {
        visit(root, 0, &mut out);
    }
    out
}

fn format_counts(map: &BTreeMap<String, i64>) -> String {
    map.iter()
        .map(|(k, v)| format!("{k}: {v}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn resolve_since(
    status_events: &[&AgentEvent],
    run_id: i64,
    resolved_status: &str,
    reported_status: &str,
    updated_at: &StoreTime,
    created_at: &StoreTime,
) -> Time {
    let run_events: Vec<&AgentEvent> = status_events
        .iter()
        .copied()
        .filter(|ev| run_id == 0 || ev.run == run_id)
        .collect();

    if let Some(last) = run_events.last() {
        if last.status == resolved_status {
            let mut since = last.at;
            for ev in run_events.iter().rev() {
                if ev.status == resolved_status {
                    since = ev.at;
                } else {
                    break;
                }
            }
            return bus_time(&since);
        }
        if !updated_at.is_zero() && reported_status == resolved_status {
            return bus_time(updated_at);
        }
        return bus_time(&last.at);
    }

    if !updated_at.is_zero() {
        return bus_time(updated_at);
    }
    bus_time(created_at)
}

struct FrictionCounts {
    summary: Option<FleetFriction>,
    open_by_kind: BTreeMap<String, i64>,
    resolved_by_kind: BTreeMap<String, i64>,
}

fn collect_friction(store: &Store) -> Result<FrictionCounts> {
    let all = store.list_friction(&FrictionFilter::default())?;
    let mut open_count: i64 = 0;
    let mut open_by_kind: BTreeMap<String, i64> = BTreeMap::new();
    let mut resolved_by_kind: BTreeMap<String, i64> = BTreeMap::new();

    for f in all {
        let kind = if f.kind.is_empty() {
            "other".to_string()
        } else {
            f.kind
        };
        if friction_is_open(&f.status) {
            open_count += 1;
            *open_by_kind.entry(kind).or_insert(0) += 1;
        } else if f.status == FRICTION_RESOLVED {
            *resolved_by_kind.entry(kind).or_insert(0) += 1;
        }
    }

    let summary = if open_count == 0 && open_by_kind.is_empty() {
        None
    } else {
        Some(FleetFriction {
            open: open_count,
            by_kind: open_by_kind.clone(),
        })
    };
    Ok(FrictionCounts {
        summary,
        open_by_kind,
        resolved_by_kind,
    })
}

fn build_report(
    raw_agents: &[Agent],
    flat_agents: &[FleetAgent],
    transcripts_by_agent: &HashMap<String, String>,
    cfg: &Config,
    now: Time,
    friction_open_by_kind: BTreeMap<String, i64>,
    friction_resolved_by_kind: BTreeMap<String, i64>,
) -> FleetReport {
    let mut agents_by_status: BTreeMap<String, i64> = BTreeMap::new();
    let mut agents_by_backend: BTreeMap<String, i64> = BTreeMap::new();
    for fa in flat_agents {
        let unknown = AGENT_STATUS_UNKNOWN;
        let st = if fa.status.is_empty() {
            unknown.as_str()
        } else {
            fa.status.as_str()
        };
        *agents_by_status.entry(st.to_string()).or_insert(0) += 1;
        if !fa.backend.is_empty() {
            *agents_by_backend.entry(fa.backend.clone()).or_insert(0) += 1;
        }
    }

    let dirs = discovery_dirs(&DiscoveryOverrides {
        claude: cfg.claude_projects_dir.clone(),
        agy: cfg.agy_brain_dir.clone(),
        kimi: cfg.kimi_sessions_dir.clone(),
    });
    let disc_agents: Vec<agent_discovery::Agent> = raw_agents
        .iter()
        .zip(flat_agents.iter())
        .map(|(raw, fa)| agent_discovery::Agent {
            name: raw.name.clone(),
            parent: raw.spawned_by.clone(),
            store: cfg.database_path.clone(),
            host: cfg.host.clone(),
            backend: raw.backend.clone(),
            live: true,
            created_at: Some(raw.created_at.0.with_timezone(&Utc)),
            status: fa.status.clone(),
            end_reason: fa.end_reason.clone(),
            session_id: fa.session_id.clone(),
            transcript: transcripts_by_agent
                .get(&raw.name)
                .cloned()
                .unwrap_or_default(),
            ..Default::default()
        })
        .collect();

    let index = new_index(&dirs, &disc_agents);
    let sessions = index.sessions();

    let now_utc: DateTime<Utc> = if now.is_zero() {
        Utc::now()
    } else {
        now.0.with_timezone(&Utc)
    };
    let cutoff_7d = now_utc - TimeDelta::days(REPORT_WINDOW_DAYS);

    let mut sessions_7d_by_harness: BTreeMap<String, i64> = BTreeMap::new();
    for s in &sessions {
        if s.path.is_empty() || s.parent_session.is_some() {
            continue;
        }
        if let Some(mtime) = s.mtime
            && mtime >= cutoff_7d
            && !s.harness.is_empty()
        {
            *sessions_7d_by_harness.entry(s.harness.clone()).or_insert(0) += 1;
        }
    }

    let messages_per_day_7d = query_messages_per_day_7d(&cfg.database_path, cutoff_7d);

    let mut long_sessions = Vec::new();
    for (raw, fa) in raw_agents.iter().zip(flat_agents.iter()) {
        if fa.status == AGENT_STATUS_ENDED.as_str() {
            continue;
        }
        let started = bus_time(&raw.created_at);
        if started.is_zero() || elapsed_between(now, started) <= LONG_SESSION_AFTER {
            continue;
        }
        let tr_path =
            find_agent_transcript(raw, &fa.session_id, transcripts_by_agent, &sessions, cfg);
        let compactions = tr_path.as_deref().map(count_compactions).unwrap_or(0);
        long_sessions.push(FleetLongSession {
            agent: raw.name.clone(),
            session_id: fa.session_id.clone(),
            backend: raw.backend.clone(),
            started_at: started,
            compactions,
        });
    }

    FleetReport {
        agents_by_status,
        agents_by_backend,
        sessions_7d_by_harness,
        messages_per_day_7d,
        friction_open_by_kind,
        friction_resolved_by_kind,
        long_sessions,
    }
}

fn query_messages_per_day_7d(db_path: &str, cutoff: DateTime<Utc>) -> BTreeMap<String, i64> {
    let mut per_day: BTreeMap<String, i64> = BTreeMap::new();
    if db_path.is_empty() || !Path::new(db_path).exists() {
        return per_day;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = Connection::open_with_flags(db_path, flags) else {
        return per_day;
    };
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let Ok(mut stmt) = conn.prepare("SELECT created_at FROM messages") else {
        return per_day;
    };
    let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) else {
        return per_day;
    };
    for raw in rows.flatten() {
        let ts = StoreTime::parse_or_zero(&raw);
        if !ts.is_zero() {
            let utc = ts.0.with_timezone(&Utc);
            if utc >= cutoff {
                let day = utc.format("%Y-%m-%d").to_string();
                *per_day.entry(day).or_insert(0) += 1;
            }
        }
    }
    per_day
}

fn bus_time(t: &StoreTime) -> Time {
    if t.is_zero() {
        Time::default()
    } else {
        Time::from(t.0)
    }
}

fn find_agent_transcript(
    agent: &Agent,
    session_id: &str,
    transcripts_by_agent: &HashMap<String, String>,
    sessions: &[agent_discovery::Session],
    cfg: &Config,
) -> Option<String> {
    if let Some(p) = transcripts_by_agent.get(&agent.name)
        && Path::new(p).is_file()
    {
        return Some(p.clone());
    }
    if !session_id.is_empty()
        && let Some(s) = sessions.iter().find(|s| {
            s.session_id == session_id && !s.path.is_empty() && Path::new(&s.path).is_file()
        })
    {
        return Some(s.path.clone());
    }
    let list_file = cfg.transcripts_path(&agent.name);
    if !list_file.is_empty()
        && let Ok(text) = fs::read_to_string(&list_file)
        && let Some(last) = text.lines().rev().map(str::trim).find(|l| !l.is_empty())
        && Path::new(last).is_file()
    {
        return Some(last.to_string());
    }
    None
}

fn count_compactions(path: &str) -> i64 {
    let Ok(file) = read_transcript_file(path, None, 1_000_000) else {
        return 0;
    };
    let mut boundaries: i64 = 0;
    let mut summaries: i64 = 0;
    for ev in &file.events {
        if ev.kind == KIND_SYSTEM
            && let Some(ref sys) = ev.system
        {
            match sys.subtype.as_str() {
                "compact_boundary" => boundaries += 1,
                "compact_summary" => summaries += 1,
                _ => {}
            }
        }
    }
    if boundaries > 0 {
        boundaries
    } else {
        summaries
    }
}

fn is_fleet_unserved(err: &anyhow::Error) -> bool {
    let msg = format!("{err:#}");
    msg.contains(&format!("doesn't serve {OP_FLEET}"))
}

fn elapsed_between(now: Time, then: Time) -> Duration {
    let nanos =
        now.0.timestamp_nanos_opt().unwrap_or(0) - then.0.timestamp_nanos_opt().unwrap_or(0);
    if nanos <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(nanos as u64)
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m");
    }
    let hours = mins / 60;
    let rem_mins = mins % 60;
    if hours < 24 {
        return if rem_mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{rem_mins}m")
        };
    }
    let days = hours / 24;
    let rem_hours = hours % 24;
    if rem_hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d{rem_hours}h")
    }
}
