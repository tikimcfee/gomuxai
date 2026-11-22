//! agent roster pick: the available entries ranked for a task kind, the
//! best as a ready spawn line. It never spawns.
use std::cmp::Reverse;

use serde::{Deserialize, Serialize};

use crate::key::{COSTS, SPEEDS, check_prefer, check_task};
use crate::view::{EntryView, HostRoster, NoteCounts};

/// How much an entry's own tasks naming the kind counts, against one
/// session's verdict.
const SUITS_SCORE: i64 = 2;

/// PickOptions are what to pick for.
#[derive(Clone, Debug, Default)]
pub struct PickOptions {
    /// The task kind (one of TASK_KINDS).
    pub task: String,
    /// fast, cheap or strong, to break ties; "" for none.
    pub prefer: String,
}

/// Pick is one ranked entry.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Pick {
    pub host: String,
    /// HARNESS/MODEL[@EFFORT].
    pub key: String,
    pub harness: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    pub score: i64,
    /// A spawn line to start it, with NAME for the agent's name.
    pub command: String,
    /// One line of why it ranks where it does.
    pub why: String,
}

/// The rank of v in list (its index), or list's length for "".
fn rank(list: &[&str], v: &str) -> usize {
    list.iter().position(|x| *x == v).unwrap_or(list.len())
}

/// tie_break orders entries of equal score by prefer: fast (by speed),
/// cheap (by cost) or strong (dearest first, cost standing in for
/// strength). Smaller sorts first.
fn tie_break(e: &EntryView, prefer: &str) -> i64 {
    match prefer {
        "fast" => rank(SPEEDS, &e.entry.speed) as i64,
        "cheap" => rank(COSTS, &e.entry.cost) as i64,
        "strong" => {
            // Unsaid cost goes last, not first.
            if e.entry.cost.is_empty() {
                i64::MAX
            } else {
                -(rank(COSTS, &e.entry.cost) as i64)
            }
        }
        _ => 0,
    }
}

fn spawn_line(host: &str, local_host: &str, e: &EntryView) -> String {
    let mut s = String::from("agent spawn NAME");
    if !host.eq_ignore_ascii_case(local_host) {
        s.push('@');
        s.push_str(host);
    }
    s.push_str(" --backend ");
    s.push_str(&e.entry.harness);
    if !e.entry.model.is_empty() {
        s.push_str(" --model ");
        s.push_str(&e.entry.model);
    }
    s
}

fn why(e: &EntryView, task: &str, suits: bool, c: &NoteCounts) -> String {
    let mut parts: Vec<String> = Vec::new();
    if suits {
        parts.push(format!("suits {task}"));
    } else if e.entry.tasks.is_empty() {
        parts.push("no tasks set".into());
    } else {
        parts.push(format!("set for {}, not {task}", e.entry.tasks.join(",")));
    }
    if c.total() == 0 {
        parts.push(format!("no notes on {task}"));
    } else {
        let mut v = Vec::new();
        for (name, t) in [("good", &c.good), ("mixed", &c.mixed), ("bad", &c.bad)] {
            if t.notes > 0 {
                let runs = if t.sessions == 1 {
                    "session"
                } else {
                    "sessions"
                };
                v.push(format!("{} {name} ({} {runs})", t.notes, t.sessions));
            }
        }
        parts.push(format!("on {task}: {}", v.join(", ")));
    }
    let mut traits = Vec::new();
    if !e.entry.speed.is_empty() {
        traits.push(e.entry.speed.clone());
    }
    if !e.entry.cost.is_empty() {
        traits.push(format!("{} cost", e.entry.cost));
    }
    if !traits.is_empty() {
        parts.push(traits.join(", "));
    }
    if !e.entry.effort.is_empty() {
        parts.push(format!(
            "effort {} (spawn has no --effort yet)",
            e.entry.effort
        ));
    }
    if !e.entry.good_at.is_empty() {
        parts.push(format!("good at: {}", e.entry.good_at));
    }
    parts.join("; ")
}

/// pick ranks every available entry of hosts for opts.task: its tasks
/// naming the kind add SUITS_SCORE, each distinct session with a good note
/// on the kind adds one and each with a bad one takes one away;
/// opts.prefer breaks ties, then more good sessions, then local_host before
/// others, then the host and key. It fails when no host has an available entry. local_host's
/// entries spawn without @HOST.
pub fn pick(
    hosts: &[HostRoster],
    local_host: &str,
    opts: &PickOptions,
) -> anyhow::Result<Vec<Pick>> {
    let task = check_task(&opts.task)?;
    let prefer = if opts.prefer.is_empty() {
        String::new()
    } else {
        check_prefer(&opts.prefer)?
    };
    let mut ranked: Vec<(i64, i64, i64, bool, Pick)> = Vec::new();
    let mut entries = 0;
    for h in hosts {
        for e in &h.entries {
            entries += 1;
            if !e.available {
                continue;
            }
            let suits = e.entry.tasks.contains(&task);
            let c = e.by_task.get(&task).cloned().unwrap_or_default();
            let score = if suits { SUITS_SCORE } else { 0 } + c.good.sessions - c.bad.sessions;
            ranked.push((
                score,
                tie_break(e, &prefer),
                c.good.sessions,
                !h.host.eq_ignore_ascii_case(local_host),
                Pick {
                    host: h.host.clone(),
                    key: e.key.clone(),
                    harness: e.entry.harness.clone(),
                    model: e.entry.model.clone(),
                    effort: e.entry.effort.clone(),
                    score,
                    command: spawn_line(&h.host, local_host, e),
                    why: why(e, &task, suits, &c),
                },
            ));
        }
    }
    if ranked.is_empty() {
        if entries == 0 {
            anyhow::bail!(
                "the roster has no entries to pick from (agent roster set HARNESS/MODEL --tasks {task} adds one)"
            );
        }
        anyhow::bail!(
            "no roster entry is available: each is disabled or its CLI isn't on PATH (agent roster shows why)"
        );
    }
    ranked.sort_by(|a, b| {
        (Reverse(a.0), a.1, Reverse(a.2), a.3, &a.4.host, &a.4.key).cmp(&(
            Reverse(b.0),
            b.1,
            Reverse(b.2),
            b.3,
            &b.4.host,
            &b.4.key,
        ))
    });
    Ok(ranked.into_iter().map(|(.., p)| p).collect())
}
