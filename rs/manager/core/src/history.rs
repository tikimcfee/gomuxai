//! The history page: recent `agent` commands of every host, newest first.

use agent_manage::{HistoryRow, View, human_age};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryLine {
    pub host: usize,
    pub index: usize,
}

/// `agent send helper 'run the tests'`: the command as a person would type it.
pub fn command_line(args: &[String]) -> String {
    let mut s = String::from("agent");
    for a in args {
        s.push(' ');
        s.push_str(&quote(a));
    }
    s
}

/// Quotes an argument for a POSIX shell when it needs it.
pub fn quote(a: &str) -> String {
    let plain = !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@,=+%".contains(c));
    if plain {
        a.to_string()
    } else {
        format!("'{}'", a.replace('\'', "'\\''"))
    }
}

pub fn duration_text(ms: i64) -> String {
    match ms {
        ..1_000 => format!("{ms}ms"),
        1_000..60_000 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000),
    }
}

fn matches(q: &str, host: &str, r: &HistoryRow) -> bool {
    q.is_empty()
        || host.to_lowercase().contains(q)
        || r.agent.to_lowercase().contains(q)
        || r.error.to_lowercase().contains(q)
        || r.args.iter().any(|a| a.to_lowercase().contains(q))
}

/// Every host's commands that match `text` (in the command line, who ran it, the host or
/// the error), newest first; `failed_only` keeps those that exited non-zero.
pub fn lines(view: &View, text: &str, failed_only: bool) -> Vec<HistoryLine> {
    let q = text.trim().to_lowercase();
    let mut out: Vec<HistoryLine> = Vec::new();
    for (hi, h) in view.hosts.iter().enumerate() {
        for (i, r) in h.history.iter().enumerate() {
            if failed_only && r.exit_code == 0 {
                continue;
            }
            if matches(&q, &h.host, r) {
                out.push(HistoryLine { host: hi, index: i });
            }
        }
    }
    out.sort_by(|a, b| {
        let ra = &view.hosts[a.host].history[a.index];
        let rb = &view.hosts[b.host].history[b.index];
        rb.at.cmp(&ra.at)
    });
    out
}

pub struct HistoryCells {
    pub when: String,
    pub host: String,
    pub who: String,
    pub command: String,
    pub exit: String,
    pub duration: String,
    pub failed: bool,
    pub error: String,
}

pub fn cells(view: &View, l: &HistoryLine, now: DateTime<Utc>) -> HistoryCells {
    let h = &view.hosts[l.host];
    let r = &h.history[l.index];
    HistoryCells {
        when: human_age(&r.at, now),
        host: h.host.clone(),
        who: if r.agent.is_empty() {
            "a person".to_string()
        } else {
            r.agent.clone()
        },
        command: command_line(&r.args),
        exit: r.exit_code.to_string(),
        duration: duration_text(r.duration_ms),
        failed: r.exit_code != 0,
        error: r.error.clone(),
    }
}

/// Names one command across refreshes: its host, when it ran and what it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryKey {
    pub host: String,
    pub at: String,
    pub args: Vec<String>,
}

pub fn key(view: &View, l: &HistoryLine) -> HistoryKey {
    let h = &view.hosts[l.host];
    let r = &h.history[l.index];
    HistoryKey {
        host: h.host.clone(),
        at: r.at.clone(),
        args: r.args.clone(),
    }
}

pub fn find(view: &View, k: &HistoryKey) -> Option<HistoryLine> {
    let host = view.hosts.iter().position(|h| h.host == k.host)?;
    let index = view.hosts[host]
        .history
        .iter()
        .position(|r| r.at == k.at && r.args == k.args)?;
    Some(HistoryLine { host, index })
}

/// The detail of one command, as lines of text: the whole command line and error, which
/// the list cuts short.
pub fn detail(view: &View, l: &HistoryLine, now: DateTime<Utc>) -> String {
    let c = cells(view, l, now);
    let r = &view.hosts[l.host].history[l.index];
    let mut out = format!("{}\n\n", c.command);
    let age = if c.when.is_empty() {
        String::new()
    } else {
        format!(" ({} ago)", c.when)
    };
    out.push_str(&format!("host     {}\n", c.host));
    out.push_str(&format!("who      {}\n", c.who));
    out.push_str(&format!("at       {}{age}\n", r.at));
    out.push_str(&format!("exit     {}\n", c.exit));
    out.push_str(&format!("took     {}\n", c.duration));
    if !c.error.is_empty() {
        out.push_str(&format!("\nerror\n{}\n", c.error));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn a_key_finds_its_command_again() {
        let v = fixture::view(fixture::default_now());
        let all = lines(&v, "", false);
        let k = key(&v, &all[3]);
        assert_eq!(find(&v, &k), Some(all[3]));
        let d = detail(&v, &all[3], fixture::default_now());
        assert!(d.starts_with("agent "));
        assert!(d.contains("\nexit "));
        let failed = lines(&v, "", true);
        let with_error = failed
            .iter()
            .find(|l| !v.hosts[l.host].history[l.index].error.is_empty());
        if let Some(l) = with_error {
            assert!(detail(&v, l, fixture::default_now()).contains("\nerror\n"));
        }
    }

    #[test]
    fn quoting() {
        assert_eq!(
            command_line(&["send".into(), "a@b".into(), "it's done".into()]),
            "agent send a@b 'it'\\''s done'"
        );
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn durations() {
        assert_eq!(duration_text(12), "12ms");
        assert_eq!(duration_text(1_500), "1.5s");
        assert_eq!(duration_text(125_000), "2m05s");
    }

    #[test]
    fn newest_first_and_filters() {
        let v = fixture::view(fixture::default_now());
        let all = lines(&v, "", false);
        assert_eq!(all.len(), 120);
        let ats: Vec<&str> = all
            .iter()
            .map(|l| v.hosts[l.host].history[l.index].at.as_str())
            .collect();
        assert!(ats.windows(2).all(|w| w[0] >= w[1]));
        let failed = lines(&v, "", true);
        assert!(!failed.is_empty() && failed.len() < all.len());
        let tiny = lines(&v, "PHONE", false);
        assert!(tiny.iter().all(|l| {
            let r = &v.hosts[l.host].history[l.index];
            l.host == 2 || r.agent.contains("phone") || r.args.iter().any(|a| a.contains("phone"))
        }));
        let c = cells(&v, &all[0], fixture::default_now());
        assert!(c.command.starts_with("agent "));
    }
}
