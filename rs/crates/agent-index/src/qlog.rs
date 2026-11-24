//! The query log (docs/INDEX-V2.md, "Usage"): one JSON line per search,
//! show, tree and related call, appended to a file in the user's data
//! directory (`[log]` in the configuration), so `agent index usage` can say
//! who asks what, how often and how fast. Writing it never fails or slows a
//! command: one append, and any error is dropped.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// One logged call.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Entry {
    /// When it ran (UTC, RFC 3339).
    pub time: String,
    /// The agent that asked (`AGENT_NAME`), else null.
    pub agent: Option<String>,
    /// search, show, tree, related.
    pub command: String,
    /// The query, or the id or path asked about.
    pub query: String,
    /// The flags given, by name (defaults left out).
    pub flags: BTreeMap<String, serde_json::Value>,
    /// The first five results' paths.
    pub top: Vec<String>,
    /// Milliseconds the command took, start to answer.
    pub ms: f64,
    /// The configuration's fingerprint.
    pub fingerprint: String,
    /// sqlite or pg.
    pub store: String,
    /// What failed, when the command did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Entry {
    /// An entry for a command now, from the environment's agent name.
    pub fn new(command: &str, query: &str) -> Entry {
        Entry {
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            agent: std::env::var("AGENT_NAME").ok().filter(|a| !a.is_empty()),
            command: command.to_string(),
            query: query.to_string(),
            ..Default::default()
        }
    }

    /// Records a flag when it isn't its default.
    pub fn flag(&mut self, name: &str, value: impl Into<serde_json::Value>, default: bool) {
        if !default {
            self.flags.insert(name.to_string(), value.into());
        }
    }
}

/// Appends one entry as a line. Errors are dropped: the log must never cost
/// the command its answer.
pub fn append(path: &Path, e: &Entry) {
    let _ = try_append(path, e);
}

fn try_append(path: &Path, e: &Entry) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = serde_json::to_vec(e).map_err(std::io::Error::other)?;
    line.push(b'\n');
    // One write with O_APPEND: lines from processes logging at once don't
    // interleave.
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(&line)
}

/// Counts by agent and command, by day, over the log.
#[derive(Debug, Clone, Serialize, Default)]
pub struct Usage {
    pub entries: usize,
    /// Lines that weren't an entry (skipped).
    pub unreadable: usize,
    pub first: String,
    pub last: String,
    /// (agent, command) → calls; "(none)" for a person or a script.
    pub by_agent: BTreeMap<String, BTreeMap<String, usize>>,
    /// Day (UTC) → command → calls.
    pub by_day: BTreeMap<String, BTreeMap<String, usize>>,
    /// Command → (median, p90) milliseconds.
    pub latency: BTreeMap<String, (f64, f64)>,
    pub errors: usize,
}

/// Reads the log (entries at or after `since`, an RFC 3339 prefix such as
/// a date, when given). A missing file is an empty log.
pub fn usage(path: &Path, since: Option<&str>) -> std::io::Result<Usage> {
    let mut u = Usage::default();
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(u),
        Err(e) => return Err(e),
    };
    let mut ms: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for line in BufReader::new(f).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(e) = serde_json::from_str::<Entry>(&line) else {
            u.unreadable += 1;
            continue;
        };
        if since.is_some_and(|s| e.time.as_str() < s) {
            continue;
        }
        u.entries += 1;
        if u.first.is_empty() {
            u.first = e.time.clone();
        }
        u.last = e.time.clone();
        let agent = e.agent.clone().unwrap_or_else(|| "(none)".into());
        *u.by_agent
            .entry(agent)
            .or_default()
            .entry(e.command.clone())
            .or_default() += 1;
        let day = e.time.get(..10).unwrap_or("").to_string();
        *u.by_day
            .entry(day)
            .or_default()
            .entry(e.command.clone())
            .or_default() += 1;
        if e.error.is_some() {
            u.errors += 1;
        } else {
            ms.entry(e.command).or_default().push(e.ms);
        }
    }
    for (cmd, mut v) in ms {
        v.sort_by(|a, b| a.total_cmp(b));
        // Nearest rank: the smallest value with at least q of them at or below.
        let at = |q: f64| v[((v.len() as f64 * q).ceil() as usize).clamp(1, v.len()) - 1];
        u.latency.insert(cmd, (at(0.5), at(0.9)));
    }
    Ok(u)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/q.jsonl");
        for (agent, cmd, ms, err) in [
            (Some("a1"), "search", 10.0, None),
            (Some("a1"), "search", 30.0, None),
            (None, "show", 5.0, None),
            (Some("a2"), "search", 20.0, Some("boom".to_string())),
        ] {
            let mut e = Entry::new(cmd, "numbat counter");
            e.agent = agent.map(str::to_string);
            e.ms = ms;
            e.error = err;
            e.flag("limit", 5, false);
            e.flag("json", false, true);
            append(&path, &e);
        }
        // A torn or foreign line is skipped, not an error.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        let u = usage(&path, None).unwrap();
        assert_eq!((u.entries, u.unreadable, u.errors), (4, 1, 1));
        assert_eq!(u.by_agent["a1"]["search"], 2);
        assert_eq!(u.by_agent["(none)"]["show"], 1);
        assert_eq!(u.latency["search"], (10.0, 30.0));
        let first: Entry = serde_json::from_str(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first.flags.len(), 1, "a default flag isn't recorded");
        assert!(usage(&path, Some("2999")).unwrap().entries == 0);
        // Never fails: a path under a file can't be written, and nothing is said.
        append(&path.join("under-a-file"), &first);
        assert!(usage(&dir.path().join("none.jsonl"), None).unwrap().entries == 0);
    }
}
