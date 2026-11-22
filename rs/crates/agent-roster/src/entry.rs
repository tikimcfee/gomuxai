//! Setting, switching and removing entries (agent roster set, enable,
//! disable, remove).
use agent_store::{RosterEntry, Store, Time};
use anyhow::bail;

use crate::key::{Key, check_cost, check_effort, check_speed, parse_tasks};

/// EntryChanges are the fields `roster set` was given; None leaves a field
/// as it is (on a new entry, empty).
#[derive(Clone, Debug, Default)]
pub struct EntryChanges {
    pub good_at: Option<String>,
    pub avoid: Option<String>,
    /// Comma-separated task kinds.
    pub tasks: Option<String>,
    pub speed: Option<String>,
    pub cost: Option<String>,
    pub limits: Option<String>,
}

/// set_entry adds the entry key names, or changes the fields given on the
/// one there is, recording by as who set it. A new entry is enabled. It
/// returns the entry as saved and whether it is new.
pub fn set_entry(
    store: &Store,
    key: &Key,
    changes: &EntryChanges,
    by: &str,
) -> anyhow::Result<(RosterEntry, bool)> {
    if !key.effort.is_empty() {
        check_effort(&key.effort)?;
    }
    let existing = store.get_roster_entry(&key.harness, &key.model, &key.effort)?;
    let created = existing.is_none();
    let mut e = existing.unwrap_or_else(|| RosterEntry {
        harness: key.harness.clone(),
        model: key.model.clone(),
        effort: key.effort.clone(),
        enabled: true,
        ..Default::default()
    });
    let opt = |v: &str, check: fn(&str) -> anyhow::Result<String>| -> anyhow::Result<String> {
        if v.trim().is_empty() {
            Ok(String::new())
        } else {
            check(v)
        }
    };
    if let Some(v) = &changes.good_at {
        e.good_at = v.trim().to_string();
    }
    if let Some(v) = &changes.avoid {
        e.avoid = v.trim().to_string();
    }
    if let Some(v) = &changes.tasks {
        e.tasks = parse_tasks(v)?;
    }
    if let Some(v) = &changes.speed {
        e.speed = opt(v, check_speed)?;
    }
    if let Some(v) = &changes.cost {
        e.cost = opt(v, check_cost)?;
    }
    if let Some(v) = &changes.limits {
        e.limits = v.trim().to_string();
    }
    e.set_by = by.to_string();
    e.updated_at = Time::now();
    store.put_roster_entry(&e)?;
    Ok((e, created))
}

fn no_entry(key: &Key) -> anyhow::Error {
    anyhow::anyhow!(
        "no roster entry {key} on this host (agent roster lists them; agent roster set {key} adds it)"
    )
}

/// set_enabled switches an entry on or off, keeping its notes and fields,
/// and returns it as saved.
pub fn set_enabled(
    store: &Store,
    key: &Key,
    enabled: bool,
    by: &str,
) -> anyhow::Result<RosterEntry> {
    let Some(mut e) = store.get_roster_entry(&key.harness, &key.model, &key.effort)? else {
        return Err(no_entry(key));
    };
    e.enabled = enabled;
    e.set_by = by.to_string();
    e.updated_at = Time::now();
    store.put_roster_entry(&e)?;
    Ok(e)
}

/// remove_entry deletes an entry; its notes stay.
pub fn remove_entry(store: &Store, key: &Key) -> anyhow::Result<()> {
    if !store.remove_roster_entry(&key.harness, &key.model, &key.effort)? {
        bail!(no_entry(key));
    }
    Ok(())
}
