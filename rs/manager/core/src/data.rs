//! The data page: what each host's agent data takes on disk.

use agent_manage::{HostView, human_bytes};
use chrono::{DateTime, Utc};

/// One host's report as lines of text.
pub fn text(h: &HostView, now: DateTime<Utc>) -> String {
    if !h.reachable {
        return format!(
            "unreachable: {}\n",
            if h.error.is_empty() {
                "didn't answer"
            } else {
                &h.error
            }
        );
    }
    let d = &h.data;
    let mut out = String::new();
    out.push_str(&format!("store      {}\n", d.dir));
    if !h.build.is_empty() {
        out.push_str(&format!("build      {}\n", h.build));
    }
    out.push_str(&format!("database   {}\n", human_bytes(d.db_bytes)));
    out.push_str(&format!(
        "logs       {}, of which {} in {} logs of departed agents\n",
        human_bytes(d.logs_bytes),
        human_bytes(d.departed_logs_bytes),
        d.departed_logs
    ));
    if d.untracked > 0 {
        out.push_str(&format!("untracked  {} sessions\n", d.untracked));
    }
    out.push_str("\ntranscripts\n");
    if d.transcripts.is_empty() {
        out.push_str("  none\n");
    }
    for (harness, f) in &d.transcripts {
        out.push_str(&format!(
            "  {harness:<10} {:>6} files  {:>10}\n",
            f.count,
            human_bytes(f.bytes)
        ));
    }
    out.push_str("\nrows per table\n");
    let w = d.tables.keys().map(|t| t.len()).max().unwrap_or(0).max(16);
    for (table, n) in &d.tables {
        out.push_str(&format!("  {table:<w$} {n:>9}\n"));
    }
    out.push_str("\nbackups\n");
    if d.backups.is_empty() {
        out.push_str("  none yet\n");
    }
    for b in &d.backups {
        let age = agent_manage::human_age(&b.at, now);
        out.push_str(&format!(
            "  {:>10}  {:>4} ago  {}\n",
            human_bytes(b.bytes),
            age,
            b.path
        ));
    }
    out
}

/// Parses the prune field: whole days, at least 1.
pub fn parse_days(s: &str) -> Result<u32, String> {
    let t = s.trim().trim_end_matches('d');
    match t.parse::<u32>() {
        Ok(n) if n >= 1 => Ok(n),
        _ => Err(format!(
            "\"{}\" isn't a number of days (1 or more)",
            s.trim()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn report_text() {
        let v = fixture::view(fixture::default_now());
        let t = text(&v.hosts[0], fixture::default_now());
        assert!(t.contains("departed agents"));
        assert!(t.contains("command_log"));
        assert!(t.contains("claude"));
        assert!(t.contains("13h ago"));
        assert!(text(&v.hosts[1], fixture::default_now()).starts_with("unreachable"));
    }

    #[test]
    fn days() {
        assert_eq!(parse_days("30"), Ok(30));
        assert_eq!(parse_days(" 7d "), Ok(7));
        assert!(parse_days("0").is_err());
        assert!(parse_days("x").is_err());
    }
}
