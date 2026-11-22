//! Saved picks (docs/ROSTER.md, "Saved picks"): a pick's startup command
//! kept by name, a new version only when the command changes, and replayed
//! exactly, with whether its entry is still available.
use agent_store::{RosterPick, Store, Time};
use anyhow::bail;
use serde::{Deserialize, Serialize};

use crate::key::Key;
use crate::pick::Pick;
use crate::view::HostRoster;

/// check_pick_name returns name if it can name a saved pick: letters,
/// digits, '.', '_' and '-', starting with a letter or digit.
pub fn check_pick_name(name: &str) -> anyhow::Result<String> {
    let name = name.trim();
    let ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!(
            "{name:?} can't name a saved pick: use letters, digits, '.', '_' and '-' (e.g. quick-edit)"
        );
    }
    Ok(name.to_string())
}

/// parse_saved_ref reads NAME[@VERSION]; the version is 0 (the newest)
/// when not given.
pub fn parse_saved_ref(s: &str) -> anyhow::Result<(String, i64)> {
    let (name, version) = match s.trim().rsplit_once('@') {
        None => (s.trim(), 0),
        Some((n, v)) => match v.parse::<i64>() {
            Ok(v) if v > 0 => (n, v),
            _ => bail!("{s:?}: the version after @ is a number from 1 (e.g. quick-edit@2)"),
        },
    };
    Ok((check_pick_name(name)?, version))
}

/// What saving a pick did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SaveOutcome {
    /// The name is new: this is version 1.
    New,
    /// The command differs from the newest version's: this is a new one.
    NewVersion,
    /// The newest version already has this command: nothing was saved.
    Unchanged,
}

/// SaveResult is a saved pick's version, as saved or as it already was.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SaveResult {
    pub outcome: SaveOutcome,
    pub pick: RosterPick,
}

/// save_pick keeps p's command as the saved pick name: version 1 for a new
/// name, the next version when the command differs from the newest one's,
/// and nothing when it is the same.
pub fn save_pick(
    store: &Store,
    name: &str,
    p: &Pick,
    task: &str,
    by: &str,
) -> anyhow::Result<SaveResult> {
    let name = check_pick_name(name)?;
    let newest = store.get_roster_pick(&name, 0)?;
    if let Some(n) = &newest
        && n.argv == p.argv
    {
        return Ok(SaveResult {
            outcome: SaveOutcome::Unchanged,
            pick: n.clone(),
        });
    }
    let mut saved = RosterPick {
        id: 0,
        name,
        version: newest.as_ref().map_or(1, |n| n.version + 1),
        argv: p.argv.clone(),
        harness: p.harness.clone(),
        model: p.model.clone(),
        effort: p.effort.clone(),
        host: p.host.clone(),
        task: task.to_string(),
        why: p.why.clone(),
        saved_by: by.to_string(),
        saved_at: Time::now(),
    };
    store.add_roster_pick(&mut saved)?;
    Ok(SaveResult {
        outcome: if newest.is_some() {
            SaveOutcome::NewVersion
        } else {
            SaveOutcome::New
        },
        pick: saved,
    })
}

/// saved returns version of the saved pick name (0: the newest), or an
/// error saying what there is.
pub fn saved(store: &Store, name: &str, version: i64) -> anyhow::Result<RosterPick> {
    if let Some(p) = store.get_roster_pick(name, version)? {
        return Ok(p);
    }
    let Some(newest) = store.get_roster_pick(name, 0)? else {
        bail!(
            "no saved pick {name} on this host (agent roster picks lists them; agent roster pick --task KIND --save {name} keeps one)"
        );
    };
    bail!(
        "{name} has no version {version}: it has 1 to {} (agent roster picks {name} lists them)",
        newest.version
    )
}

/// SavedCheck is a saved pick with whether its entry is available now.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SavedCheck {
    pub pick: RosterPick,
    /// None when the entry's host wasn't read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub available: Option<bool>,
    /// In words: "available", "off: disabled", "gone from the roster", ...
    pub status: String,
}

/// check_saved says whether p's entry is available in hosts' rosters now.
pub fn check_saved(p: RosterPick, hosts: &[HostRoster]) -> SavedCheck {
    let key = Key::new(&p.harness, &p.model, &p.effort).to_string();
    let Some(h) = hosts.iter().find(|h| h.host.eq_ignore_ascii_case(&p.host)) else {
        let status = format!("not checked: {}'s roster isn't read from here", p.host);
        return SavedCheck {
            pick: p,
            available: None,
            status,
        };
    };
    let (available, status) = match h.entries.iter().find(|e| e.key == key) {
        None => (false, format!("gone: {} has no entry {key} now", h.host)),
        Some(e) if e.available => (true, "available".to_string()),
        Some(e) => (false, format!("off: {}", e.unavailable)),
    };
    SavedCheck {
        pick: p,
        available: Some(available),
        status,
    }
}
