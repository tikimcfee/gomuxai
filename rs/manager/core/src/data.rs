//! The data page: what each host's agent data takes on disk, as a plain list of lines a
//! UI draws top to bottom (one section per host, foldable), with no layout in it.

use std::collections::BTreeSet;

use agent_manage::{HostView, View, human_age, human_bytes};
use chrono::{DateTime, Utc};

/// One line of the data page.
#[derive(Debug, Clone, PartialEq)]
pub enum DataLine {
    /// A host's header; pressing it folds or unfolds the host's section.
    Host {
        host: usize,
        name: String,
        /// Build and when the view was read, or why the host didn't answer.
        note: String,
        reachable: bool,
        folded: bool,
        /// The host `agent manage` ran on, whose data Back up and Prune act on.
        local: bool,
    },
    /// A group heading inside a host's section, with its total.
    Heading { title: String, summary: String },
    /// A name and value inside a group. `share` is the value over the group's largest
    /// (0 to 1), for a bar, and None in a group of one; `top` marks the group's largest
    /// when the group has several.
    Item {
        name: String,
        value: String,
        detail: String,
        share: Option<f32>,
        top: bool,
    },
    /// A remark: "none", the host's error, where its data is managed.
    Note { text: String },
}

/// The page's lines, one section per host; the hosts named in `folded` show only their
/// header.
pub fn lines(view: &View, now: DateTime<Utc>, folded: &BTreeSet<String>) -> Vec<DataLine> {
    let mut out = Vec::new();
    let local = crate::local_index(view);
    let read = match human_age(&view.at, now).as_str() {
        "" => String::new(),
        "now" => "read just now".to_string(),
        a => format!("read {a} ago"),
    };
    for (i, h) in view.hosts.iter().enumerate() {
        let is_folded = folded.contains(&h.host);
        let note = if h.reachable {
            [h.build.as_str(), read.as_str()]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" · ")
        } else {
            format!("unreachable: {}", error(h))
        };
        out.push(DataLine::Host {
            host: i,
            name: h.host.clone(),
            note,
            reachable: h.reachable,
            folded: is_folded,
            local: i == local,
        });
        if is_folded {
            continue;
        }
        if !h.reachable {
            out.push(note_line("Its data isn't known until it answers."));
            continue;
        }
        if i != local {
            out.push(note_line(&format!(
                "Its data is managed on {}: back up and prune there.",
                h.host
            )));
        }
        host_lines(h, now, &mut out);
    }
    out
}

fn error(h: &HostView) -> &str {
    if h.error.is_empty() {
        "didn't answer"
    } else {
        &h.error
    }
}

fn note_line(text: &str) -> DataLine {
    DataLine::Note {
        text: text.to_string(),
    }
}

fn heading(title: &str, summary: String) -> DataLine {
    DataLine::Heading {
        title: title.to_string(),
        summary,
    }
}

/// A group's items from (name, amount, value, detail), largest amount first, each with
/// its share of the largest. A group of one has nothing to compare it with: no share.
fn group(rows: Vec<(String, i64, String, String)>, sort: bool, out: &mut Vec<DataLine>) {
    let mut rows = rows;
    if sort {
        rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }
    let max = rows.iter().map(|r| r.1).max().unwrap_or(0);
    let several = rows.len() > 1;
    for (name, amount, value, detail) in rows {
        let share = several.then(|| {
            if max > 0 {
                (amount.max(0) as f64 / max as f64) as f32
            } else {
                0.0
            }
        });
        out.push(DataLine::Item {
            name,
            value,
            detail,
            share,
            top: several && max > 0 && amount == max,
        });
    }
}

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn host_lines(h: &HostView, now: DateTime<Utc>, out: &mut Vec<DataLine>) {
    let d = &h.data;

    // The store: the database, then its rows per table.
    out.push(heading("store", d.dir.clone()));
    group(
        vec![(
            "database".to_string(),
            d.db_bytes,
            human_bytes(d.db_bytes),
            "with its write-ahead log".to_string(),
        )],
        false,
        out,
    );
    let rows: i64 = d.tables.values().sum();
    out.push(heading(
        "rows per table",
        format!(
            "{} in {}",
            plural(rows, "row", "rows"),
            plural(d.tables.len() as i64, "table", "tables")
        ),
    ));
    if d.tables.is_empty() {
        out.push(note_line("none"));
    }
    group(
        d.tables
            .iter()
            .map(|(t, n)| (t.clone(), *n, n.to_string(), String::new()))
            .collect(),
        true,
        out,
    );

    // Logs, and how much of them belongs to agents no longer recorded.
    out.push(heading("logs", human_bytes(d.logs_bytes)));
    let recorded = (d.logs_bytes - d.departed_logs_bytes).max(0);
    group(
        vec![
            (
                "recorded agents".to_string(),
                recorded,
                human_bytes(recorded),
                String::new(),
            ),
            (
                "departed agents".to_string(),
                d.departed_logs_bytes,
                human_bytes(d.departed_logs_bytes),
                format!("{}, prunable", plural(d.departed_logs, "log", "logs")),
            ),
        ],
        false,
        out,
    );

    // Transcripts per harness.
    let files: i64 = d.transcripts.values().map(|f| f.count).sum();
    let bytes: i64 = d.transcripts.values().map(|f| f.bytes).sum();
    out.push(heading(
        "transcripts",
        format!(
            "{} · {}",
            plural(files, "file", "files"),
            human_bytes(bytes)
        ),
    ));
    if d.transcripts.is_empty() {
        out.push(note_line("none"));
    }
    group(
        d.transcripts
            .iter()
            .map(|(harness, f)| {
                (
                    harness.clone(),
                    f.bytes,
                    human_bytes(f.bytes),
                    plural(f.count, "file", "files"),
                )
            })
            .collect(),
        true,
        out,
    );

    // Sessions a person stopped tracking.
    out.push(heading(
        "untracked sessions",
        if d.untracked == 0 {
            "none".to_string()
        } else {
            d.untracked.to_string()
        },
    ));

    // Backups, newest first.
    let total: i64 = d.backups.iter().map(|b| b.bytes).sum();
    out.push(heading(
        "backups",
        if d.backups.is_empty() {
            "none".to_string()
        } else {
            format!(
                "{} · {}",
                plural(d.backups.len() as i64, "backup", "backups"),
                human_bytes(total)
            )
        },
    ));
    let mut backups: Vec<_> = d.backups.iter().collect();
    backups.sort_by_key(|b| std::cmp::Reverse(DateTime::parse_from_rfc3339(&b.at).ok()));
    group(
        backups
            .iter()
            .map(|b| {
                let age = human_age(&b.at, now);
                let when = match age.as_str() {
                    "" => "unknown".to_string(),
                    "now" => "just now".to_string(),
                    a => format!("{a} ago"),
                };
                (when, b.bytes, human_bytes(b.bytes), b.path.clone())
            })
            .collect(),
        false,
        out,
    );
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

    fn items_after(
        lines: &[DataLine],
        title: &str,
    ) -> Vec<(String, String, String, Option<f32>, bool)> {
        let start = lines
            .iter()
            .position(|l| matches!(l, DataLine::Heading { title: t, .. } if t == title))
            .expect("heading");
        lines[start + 1..]
            .iter()
            .map_while(|l| match l {
                DataLine::Item {
                    name,
                    value,
                    detail,
                    share,
                    top,
                } => Some((name.clone(), value.clone(), detail.clone(), *share, *top)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn one_section_per_host() {
        let now = fixture::default_now();
        let v = fixture::view(now);
        let l = lines(&v, now, &BTreeSet::new());
        let hosts: Vec<_> = l
            .iter()
            .filter_map(|l| match l {
                DataLine::Host {
                    name,
                    local,
                    reachable,
                    ..
                } => Some((name.as_str(), *local, *reachable)),
                _ => None,
            })
            .collect();
        assert_eq!(
            hosts,
            [
                ("hub", true, true),
                ("laptop", false, false),
                ("phone", false, true)
            ]
        );
        // The unreachable host says why, and has nothing else.
        let i = l
            .iter()
            .position(|l| matches!(l, DataLine::Host { name, .. } if name == "laptop"))
            .unwrap();
        let DataLine::Host { note, .. } = &l[i] else {
            unreachable!()
        };
        assert!(note.starts_with("unreachable"));
        assert!(matches!(&l[i + 1], DataLine::Note { .. }));
        assert!(matches!(&l[i + 2], DataLine::Host { .. }));
        // Another host's section says where its data is managed; this host's doesn't.
        let managed = |t: &str| {
            l.iter()
                .any(|x| matches!(x, DataLine::Note { text } if text.contains(t)))
        };
        assert!(managed("managed on phone"));
        assert!(!managed("managed on hub"));
    }

    #[test]
    fn groups_rank_and_share() {
        let now = fixture::default_now();
        let v = fixture::view(now);
        let l = lines(&v, now, &BTreeSet::new());
        let tables = items_after(&l, "rows per table");
        assert!(tables.iter().any(|t| t.0 == "command_log"));
        assert!(tables[0].4, "the largest table comes first and is marked");
        assert_eq!(tables[0].3, Some(1.0));
        assert!(tables.iter().skip(1).all(|t| t.3.is_some_and(|s| s <= 1.0)));
        let counts: Vec<i64> = tables.iter().map(|t| t.1.parse().unwrap()).collect();
        assert!(counts.windows(2).all(|w| w[0] >= w[1]));

        let tr = items_after(&l, "transcripts");
        assert!(tr.iter().any(|t| t.0 == "claude" && t.2.ends_with("files")));

        let store = items_after(&l, "store");
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].3, None, "a group of one draws no bar");
        assert!(!store[0].4);

        let logs = items_after(&l, "logs");
        assert_eq!(logs[1].0, "departed agents");
        assert!(logs[1].2.contains("prunable"));

        let backups = items_after(&l, "backups");
        assert_eq!(backups.len(), 2);
        assert_eq!(backups[0].0, "13h ago", "newest first");
        assert!(backups[0].2.ends_with(".db"));
    }

    #[test]
    fn folded_hosts_show_only_their_header() {
        let now = fixture::default_now();
        let v = fixture::view(now);
        let all = lines(&v, now, &BTreeSet::new());
        let folded: BTreeSet<String> = ["hub".to_string()].into();
        let l = lines(&v, now, &folded);
        assert!(matches!(&l[0], DataLine::Host { folded: true, .. }));
        assert!(matches!(&l[1], DataLine::Host { name, .. } if name == "laptop"));
        assert!(l.len() < all.len());
    }

    #[test]
    fn empty_groups_say_none() {
        let now = fixture::default_now();
        let mut v = fixture::view(now);
        v.hosts.truncate(1);
        v.hosts[0].data = Default::default();
        let l = lines(&v, now, &BTreeSet::new());
        assert!(l.iter().any(
            |x| matches!(x, DataLine::Heading { title, summary } if title == "backups" && summary == "none")
        ));
        // A group whose values are all zero draws no bar and marks nothing.
        assert!(
            l.iter()
                .all(|x| !matches!(x, DataLine::Item { top: true, .. }))
        );
    }

    #[test]
    fn days() {
        assert_eq!(parse_days("30"), Ok(30));
        assert_eq!(parse_days(" 7d "), Ok(7));
        assert!(parse_days("0").is_err());
        assert!(parse_days("x").is_err());
    }
}
