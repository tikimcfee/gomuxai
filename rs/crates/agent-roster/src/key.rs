//! The roster's fixed lists and its entry keys (HARNESS/MODEL[@EFFORT]).
use std::fmt;

use anyhow::bail;
use serde::{Deserialize, Serialize};

/// The task kinds an entry can suit and a note can be about
/// (docs/ROSTER.md, "Task kinds").
pub const TASK_KINDS: &[&str] = &[
    "edit",
    "bugfix",
    "feature",
    "long-horizon",
    "review",
    "research",
    "design",
    "test",
    "kernel",
    "chat",
];

/// A note's verdicts.
pub const VERDICTS: &[&str] = &["good", "mixed", "bad"];

/// An entry's speeds, fastest first.
pub const SPEEDS: &[&str] = &["fast", "medium", "slow"];

/// An entry's costs, cheapest first.
pub const COSTS: &[&str] = &["low", "medium", "high"];

/// The efforts a harness can take, lowest first.
pub const EFFORTS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// What pick --prefer takes.
pub const PREFERS: &[&str] = &["fast", "cheap", "strong"];

fn one_of(what: &str, v: &str, list: &[&str]) -> anyhow::Result<String> {
    let v = v.trim();
    if list.contains(&v) {
        return Ok(v.to_string());
    }
    if v.is_empty() {
        bail!("no {what} given: use {}", list.join(", "));
    }
    bail!("{v:?} isn't a {what}: use {}", list.join(", "))
}

/// check_task returns kind if it is one of TASK_KINDS, and otherwise an
/// error naming them.
pub fn check_task(kind: &str) -> anyhow::Result<String> {
    one_of("task kind", kind, TASK_KINDS)
}

/// check_verdict returns v if it is one of VERDICTS.
pub fn check_verdict(v: &str) -> anyhow::Result<String> {
    one_of("verdict", v, VERDICTS)
}

/// check_speed returns v if it is one of SPEEDS.
pub fn check_speed(v: &str) -> anyhow::Result<String> {
    one_of("speed", v, SPEEDS)
}

/// check_cost returns v if it is one of COSTS.
pub fn check_cost(v: &str) -> anyhow::Result<String> {
    one_of("cost", v, COSTS)
}

/// check_effort returns v if it is one of EFFORTS.
pub fn check_effort(v: &str) -> anyhow::Result<String> {
    one_of("effort", v, EFFORTS)
}

/// check_prefer returns v if it is one of PREFERS.
pub fn check_prefer(v: &str) -> anyhow::Result<String> {
    one_of("preference", v, PREFERS)
}

/// parse_tasks reads a comma-separated list of task kinds ("" is none),
/// refusing any that isn't one, and drops repeats.
pub fn parse_tasks(s: &str) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for t in s.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        let t = check_task(t)?;
        if !out.contains(&t) {
            out.push(t);
        }
    }
    Ok(out)
}

/// Key names one roster entry: a harness, a model ("" for the harness's
/// default) and an effort ("" for none).
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Key {
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
}

impl Key {
    pub fn new(harness: &str, model: &str, effort: &str) -> Key {
        Key {
            harness: harness.into(),
            model: model.into(),
            effort: effort.into(),
        }
    }

    /// parse reads HARNESS/MODEL[@EFFORT]; a bare HARNESS (or HARNESS/) is
    /// its default model. "@EFFORT" counts only when EFFORT is one of
    /// EFFORTS, so a model with an @ in its name reads whole.
    pub fn parse(s: &str) -> anyhow::Result<Key> {
        let s = s.trim();
        let usage = "name an entry as HARNESS/MODEL[@EFFORT], e.g. claude/claude-haiku-5-5@low (agent roster lists them)";
        if s.is_empty() {
            bail!("no entry named: {usage}");
        }
        let (harness, rest) = s.split_once('/').unwrap_or((s, ""));
        let (model, effort) = match rest.rsplit_once('@') {
            Some((m, e)) if EFFORTS.contains(&e) => (m, e),
            _ => (rest, ""),
        };
        let (harness, effort) = match (rest.is_empty(), harness.rsplit_once('@')) {
            // claude@low: the default model at an effort.
            (true, Some((h, e))) if EFFORTS.contains(&e) => (h, e),
            _ => (harness, effort),
        };
        if harness.is_empty() {
            bail!("{s:?} has no harness: {usage}");
        }
        if harness.contains(char::is_whitespace) || model.contains(char::is_whitespace) {
            bail!("{s:?} has a space in it: {usage}");
        }
        Ok(Key::new(harness, model, effort))
    }

    pub fn of_entry(e: &agent_store::RosterEntry) -> Key {
        Key::new(&e.harness, &e.model, &e.effort)
    }

    pub fn of_note(n: &agent_store::RosterNote) -> Key {
        Key::new(&n.harness, &n.model, &n.effort)
    }
}

impl fmt::Display for Key {
    /// HARNESS/MODEL[@EFFORT]; the default model shows as HARNESS/.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.harness, self.model)?;
        if !self.effort.is_empty() {
            write!(f, "@{}", self.effort)?;
        }
        Ok(())
    }
}
