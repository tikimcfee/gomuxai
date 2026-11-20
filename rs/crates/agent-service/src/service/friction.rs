//! Go `internal/service/friction.go`.
//!
//! Friction entries linked across hosts: each host keeps its half of a link
//! (storage.FrictionLink), and the bus's friction op lets the other host
//! read the entry, record or remove its half, and resolve it. These helpers
//! serve that op and are shared with the CLI, which drives the calling side.

use anyhow::{Result, anyhow, bail};

use agent_bus::{
    FRICTION_GET, FRICTION_LINK, FRICTION_RESOLVE, FRICTION_UNLINK, FrictionEntry, FrictionRef,
    FrictionRequest, FrictionResponse,
};
use agent_store::{
    FRICTION_ACCEPTED, FRICTION_DUPLICATE, FRICTION_OPEN, Friction, FrictionLink, Store, Time,
};

use super::agent_service::AgentService;
use super::events::bus_time;

/// Reports whether an entry with status is still to be fixed.
pub fn friction_is_open(status: &str) -> bool {
    status == FRICTION_OPEN || status == FRICTION_ACCEPTED
}

/// OriginalFriction returns entry id, and the entry that stands for it: its
/// original if it was closed as a duplicate (its links moved there), else
/// the entry itself. Both are None if there is no entry id.
pub fn original_friction(store: &Store, id: i64) -> Result<Option<(Friction, Friction)>> {
    let Some(asked) = store.get_friction(id)? else {
        return Ok(None);
    };
    let mut original = asked.clone();
    if asked.status == FRICTION_DUPLICATE
        && asked.duplicate_of != 0
        && let Some(o) = store.get_friction(asked.duplicate_of)?
    {
        original = o;
    }
    Ok(Some((asked, original)))
}

/// FrictionEntryOf describes f, on host, for another host, with its links.
pub fn friction_entry_of(store: &Store, host: &str, f: &Friction) -> Result<FrictionEntry> {
    let mut e = FrictionEntry {
        host: host.to_string(),
        id: f.id,
        uid: f.uid.clone(),
        status: f.status.clone(),
        kind: f.kind.clone(),
        summary: f.summary.clone(),
        votes: f.votes,
        resolved_by: f.resolved_by.clone(),
        resolved_at: bus_time(&f.resolved_at),
        resolution: f.resolution.clone(),
        ..Default::default()
    };
    for l in store.list_friction_links(f.id)? {
        e.links.push(FrictionRef {
            host: l.peer_host,
            id: l.peer_id,
            uid: l.peer_uid,
        });
    }
    Ok(e)
}

/// SeeFrictionPeer records in l what was seen of its other end, e, at at:
/// its id and uid (when e stands for the entry l named, closed as its
/// duplicate, l follows it to e), and its state.
pub fn see_friction_peer(l: &mut FrictionLink, e: &FrictionEntry, at: Time) {
    l.peer_id = e.id;
    l.peer_uid = e.uid.clone();
    l.peer_status = e.status.clone();
    l.peer_kind = e.kind.clone();
    l.peer_summary = e.summary.clone();
    l.peer_votes = e.votes;
    l.peer_resolved_by = e.resolved_by.clone();
    l.peer_resolution = e.resolution.clone();
    l.peer_checked_at = at;
}

/// CloseFriction closes f as status (resolved or wontfix) with note, by who.
pub fn close_friction(
    store: &Store,
    f: &mut Friction,
    status: &str,
    note: &str,
    who: &str,
) -> Result<()> {
    let now = Time::now();
    f.status = status.to_string();
    f.updated_at = now;
    f.resolved_at = now;
    f.resolved_by = who.to_string();
    f.resolution = note.to_string();
    store.update_friction(f)
}

impl AgentService {
    /// ServeFriction answers another host's friction request.
    pub fn serve_friction(&self, req: &FrictionRequest) -> Result<FrictionResponse> {
        let mut resp = FrictionResponse {
            entries: Vec::new(),
            ..Default::default()
        };
        if let Some(from) = &req.from
            && from.host.eq_ignore_ascii_case(&self.cfg.host)
        {
            bail!(
                "the friction request comes from {}, which is this host, but it came over the bus",
                from.host
            );
        }
        if req.action == FRICTION_GET {
            for &id in &req.ids {
                let Some((asked, f)) = original_friction(&self.storage, id)? else {
                    resp.missing.push(id);
                    continue;
                };
                resp.entries.push(self.friction_entry(&asked, &f)?);
            }
            return Ok(resp);
        }

        let Some((asked, mut f)) = original_friction(&self.storage, req.id)? else {
            if req.action == FRICTION_LINK {
                return Err(self.no_friction(req.id));
            }
            resp.missing = vec![req.id];
            return Ok(resp);
        };
        let now = Time::now();
        if req.action == FRICTION_LINK {
            let from = req.from.clone().unwrap_or_default();
            let mut l = FrictionLink {
                friction_id: f.id,
                peer_host: from.host.clone(),
                peer_id: from.id,
                linked_by: req.by.clone(),
                linked_at: now,
                note: req.note.clone(),
                confirmed_at: now,
                ..Default::default()
            };
            see_friction_peer(&mut l, &from, now);
            self.storage.add_friction_link(&mut l)?;
        } else if req.action == FRICTION_UNLINK {
            let from = req.from.clone().unwrap_or_default();
            self.storage
                .delete_friction_link(f.id, &from.host, from.id)?;
        } else if req.action == FRICTION_RESOLVE {
            let Some(from) = &req.from else {
                bail!(
                    "the friction resolve request names no entry it comes from: another host resolves only entries linked to its own"
                );
            };
            if !req.uid.is_empty() && asked.uid != req.uid {
                // Another entry has the id now: say what it is, change nothing.
                resp.entries
                    .push(friction_entry_of(&self.storage, &self.cfg.host, &asked)?);
                return Ok(resp);
            }
            // Only an entry linked to the caller's: a mistyped id, or a link
            // this host never recorded, closes nothing.
            if !self.links_to(f.id, &from.host, from.id)? {
                let h = &self.cfg.host;
                bail!(
                    "#{} on {h} has no link to #{}@{}, so it wasn't resolved: link it from this side first (on {h}: agent friction link {} {}@{})",
                    f.id,
                    from.id,
                    from.host,
                    f.id,
                    from.id,
                    from.host
                );
            }
            if friction_is_open(&f.status) {
                close_friction(&self.storage, &mut f, &req.status, &req.note, &req.by)?;
                resp.resolved = true;
            }
            self.see_linked_from(f.id, from, now);
        }
        // Re-read it: votes and links may have changed.
        let f = self
            .storage
            .get_friction(f.id)?
            .ok_or_else(|| anyhow!("friction #{} vanished", f.id))?;
        resp.entries.push(self.friction_entry(&asked, &f)?);
        Ok(resp)
    }

    /// frictionEntry is friction_entry_of f, saying which entry was asked for
    /// when f stands for it as its original.
    fn friction_entry(&self, asked: &Friction, f: &Friction) -> Result<FrictionEntry> {
        let mut e = friction_entry_of(&self.storage, &self.cfg.host, f)?;
        if asked.id != f.id {
            e.requested = Some(FrictionRef {
                host: self.cfg.host.clone(),
                id: asked.id,
                uid: asked.uid.clone(),
            });
        }
        Ok(e)
    }

    /// linksTo reports whether entry id has a link to peer_id on peer_host.
    fn links_to(&self, id: i64, peer_host: &str, peer_id: i64) -> Result<bool> {
        Ok(self
            .storage
            .list_friction_links(id)?
            .iter()
            .any(|l| l.peer_host.eq_ignore_ascii_case(peer_host) && l.peer_id == peer_id))
    }

    /// seeLinkedFrom updates entry id's link to from, if it has one, with
    /// from's state: a host resolving its entry and this one says how it left
    /// its own.
    fn see_linked_from(&self, id: i64, from: &FrictionEntry, at: Time) {
        let Ok(links) = self.storage.list_friction_links(id) else {
            return;
        };
        for mut l in links {
            if l.peer_host.eq_ignore_ascii_case(&from.host) && l.peer_id == from.id {
                see_friction_peer(&mut l, from, at);
                let _ = self.storage.update_friction_link(&l);
            }
        }
    }

    /// noFriction is the error for another host's request naming an entry this
    /// host doesn't have.
    fn no_friction(&self, id: i64) -> anyhow::Error {
        match self.storage.last_friction_id() {
            Err(e) => e,
            Ok(0) => anyhow!(
                "no friction #{id} on {}: nothing has been filed there yet",
                self.cfg.host
            ),
            Ok(last) => anyhow!("no friction #{id} on {} (latest is #{last})", self.cfg.host),
        }
    }
}
