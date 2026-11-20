//! Host focus state and live `follow` stream (`docs/FLEET.md`, section
//! `"Focus and follow"`).
//!
//! Each host keeps at most one focused agent in `host_state` under key
//! `"focus"`, serialized as a [`FocusEvent`] JSON document. Setting or clearing
//! focus updates `host_state` and best-effort publishes the [`FocusEvent`] on
//! `gomux.<host>.focus` when a bus [`Publisher`] is present.

use std::time::Duration;

use agent_base::util;
use agent_bus::{
    Conn, Context, DEFAULT_RPC_TIMEOUT, ErrNoBus, FleetRequest, FocusEvent, Publisher, RpcClient,
    Time, subscribe_focus, validate_agent, validate_host,
};
use agent_service::AgentNotFoundError;
use agent_store::Store;
use anyhow::{Context as _, Result, anyhow};

/// Key in SQLite `host_state` storing this host's current [`FocusEvent`] JSON.
pub const HOST_STATE_KEY: &str = "focus";

/// Bounded flush timeout after publishing a focus change so short-lived CLI
/// processes push the event to `agent-busd` before exiting.
const PUBLISH_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// Reads this host's current focus from `store`.
///
/// Returns `Ok(None)` when no focus has been set or when focus was cleared
/// (`agent == ""`).
pub fn get(store: &Store) -> Result<Option<FocusEvent>> {
    Ok(last_change(store)?.filter(|ev| !ev.agent.is_empty()))
}

/// Reads the last focus change stored on this host, a clear included (an
/// event with `agent == ""`, saying who cleared it and when); `Ok(None)` when
/// focus was never set. `agent focus --follow` without a bus polls this, so
/// it sees each clear as the bus carries it.
pub fn last_change(store: &Store) -> Result<Option<FocusEvent>> {
    let Some(raw) = store.get_host_state(HOST_STATE_KEY)? else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let ev: FocusEvent =
        serde_json::from_str(trimmed).context("decoding focus event from host_state[\"focus\"]")?;
    Ok(Some(ev))
}

/// Sets this host's focused agent to `agent` (which must exist in `store`),
/// persists the [`FocusEvent`] in `host_state["focus"]`, and best-effort
/// publishes it over `publisher` when provided.
pub fn set(
    store: &Store,
    publisher: Option<&Publisher>,
    host: &str,
    agent: &str,
    by: &str,
) -> Result<FocusEvent> {
    validate_agent(agent)?;
    let Some(rec) = store.get_agent(agent)? else {
        return Err(agent_not_found(store, host, agent).into());
    };
    let ev = FocusEvent {
        host: host.to_string(),
        agent: rec.name,
        session: rec.session_name,
        at: Time::now(),
        by: by.to_string(),
    };
    let json = serde_json::to_string(&ev).context("encoding focus event")?;
    store.set_host_state(HOST_STATE_KEY, &json)?;
    publish_best_effort(publisher, &ev);
    Ok(ev)
}

/// Clears this host's focus: stores the empty-agent [`FocusEvent`] (`agent:
/// ""`, with who cleared it and when) in `host_state["focus"]`, which
/// [`get`] reads as nothing having focus, and best-effort publishes it when
/// `publisher` is provided.
pub fn clear(
    store: &Store,
    publisher: Option<&Publisher>,
    host: &str,
    by: &str,
) -> Result<FocusEvent> {
    let ev = FocusEvent {
        host: host.to_string(),
        agent: String::new(),
        session: String::new(),
        at: Time::now(),
        by: by.to_string(),
    };
    let json = serde_json::to_string(&ev).context("encoding focus event")?;
    store.set_host_state(HOST_STATE_KEY, &json)?;
    publish_best_effort(publisher, &ev);
    Ok(ev)
}

/// Clears focus if `agent` is currently the focused agent on this host.
///
/// Intended for `cleanup` when tearing down an agent without a bus publisher;
/// returns `Ok(true)` if focus matched `agent` and was cleared.
pub fn clear_if(store: &Store, agent: &str) -> Result<bool> {
    clear_if_with_publisher(store, None, "", agent, "")
}

/// Like [`clear_if`], also publishing the clear [`FocusEvent`] when `publisher`
/// is present.
pub fn clear_if_with_publisher(
    store: &Store,
    publisher: Option<&Publisher>,
    host: &str,
    agent: &str,
    by: &str,
) -> Result<bool> {
    if agent.is_empty() {
        return Ok(false);
    }
    let Some(cur) = get(store)? else {
        return Ok(false);
    };
    if cur.agent != agent {
        return Ok(false);
    }
    let effective_host = if host.is_empty() {
        cur.host.as_str()
    } else {
        host
    };
    clear(store, publisher, effective_host, by)?;
    Ok(true)
}

/// Streams focus for a host: emits the current [`FocusEvent`] first (from
/// `store` when local, or via RPC op `fleet` on `host` when remote), then emits
/// each subsequent [`FocusEvent`] from `subscribe_focus` until `callback`
/// returns `false` or `ctx` ends.
///
/// When nothing currently has focus, the initial callback receives a
/// [`FocusEvent`] with `agent == ""`.
pub fn follow<F>(
    ctx: &Context,
    store: Option<&Store>,
    nc: Option<&Conn>,
    host: Option<&str>,
    mut callback: F,
) -> Result<()>
where
    F: FnMut(&FocusEvent) -> bool,
{
    let explicit_host = host.map(str::trim).filter(|h| !h.is_empty() && *h != "*");
    if let Some(h) = explicit_host
        && (store.is_none() || nc.is_some())
    {
        validate_host(h)?;
    }

    // Subscribe before reading the snapshot so a concurrent change between the
    // read and the loop is never missed.
    let mut sub = match nc {
        Some(conn) => Some(subscribe_focus(conn, host)?),
        None => None,
    };

    // When the snapshot was taken: the last change's time, a stored clear's
    // included (the first line shows plain "nothing has focus" either way),
    // so events from before it are dropped and a clear after it is shown.
    let mut snapshot_at = Time::default();
    let initial = match store {
        Some(st) => {
            let last = last_change(st)?;
            snapshot_at = last.as_ref().map(|ev| ev.at).unwrap_or_default();
            last.filter(|ev| !ev.agent.is_empty())
                .unwrap_or_else(|| FocusEvent {
                    host: explicit_host.unwrap_or_default().to_string(),
                    ..Default::default()
                })
        }
        None => {
            let conn = nc.ok_or_else(|| anyhow::Error::new(ErrNoBus))?;
            let target = explicit_host
                .ok_or_else(|| anyhow!("following remote focus requires a host name"))?;
            let rpc = RpcClient::new(conn.clone(), DEFAULT_RPC_TIMEOUT);
            let fh = rpc.fleet(ctx, target, FleetRequest { report: false })?;
            fh.focus.unwrap_or_else(|| FocusEvent {
                host: target.to_string(),
                ..Default::default()
            })
        }
    };

    if !callback(&initial) {
        return Ok(());
    }

    let Some(ref mut sub) = sub else {
        return Ok(());
    };

    let initial_at = if initial.at.is_zero() {
        snapshot_at
    } else {
        initial.at
    };
    let mut skip_empty_clear = initial.agent.is_empty() && initial_at.is_zero();
    loop {
        let ev = sub.next(ctx)?;
        if !initial_at.is_zero() && !ev.at.is_zero() && !ev.at.after(&initial_at) {
            continue;
        }
        if skip_empty_clear {
            skip_empty_clear = false;
            if ev.agent.is_empty() {
                continue;
            }
        }
        if !callback(&ev) {
            return Ok(());
        }
    }
}

/// Builds a rich [`AgentNotFoundError`] from `store` (populating `count`,
/// `session_of`, and `similar`), matching `AgentService::agent`.
pub(crate) fn agent_not_found(store: &Store, host: &str, name: &str) -> AgentNotFoundError {
    let mut e = AgentNotFoundError {
        name: name.to_string(),
        host: host.to_string(),
        ..Default::default()
    };
    if let Ok(agents) = store.list_agents() {
        e.count = agents.len();
        let mut names = Vec::with_capacity(agents.len());
        for a in &agents {
            names.push(a.name.clone());
            if a.session_name == name {
                e.session_of = a.name.clone();
            }
        }
        e.similar = util::similar(name, &names);
    }
    e
}

fn publish_best_effort(publisher: Option<&Publisher>, ev: &FocusEvent) {
    let Some(pub_) = publisher else {
        return;
    };
    if ev.host.is_empty() {
        return;
    }
    if pub_.publish_focus(ev).is_ok() {
        let _ = pub_.flush(PUBLISH_FLUSH_TIMEOUT);
    }
}
