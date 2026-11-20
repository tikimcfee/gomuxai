//! The friction page: per host, open entries first.

use agent_manage::{FrictionRow, View, human_age};
use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrictionLine {
    Host { host: usize, open: usize },
    Entry { host: usize, index: usize },
    Note { host: usize, text: String },
}

/// Names an entry across reloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrictionKey {
    pub host: String,
    pub id: i64,
}

pub fn is_open(f: &FrictionRow) -> bool {
    f.status != "resolved"
}

/// Per host: entries not resolved first, by votes then newest; resolved ones after.
pub fn lines(view: &View) -> Vec<FrictionLine> {
    let mut out = Vec::new();
    for (hi, h) in view.hosts.iter().enumerate() {
        let open = h.friction.iter().filter(|f| is_open(f)).count();
        out.push(FrictionLine::Host { host: hi, open });
        if !h.reachable {
            out.push(FrictionLine::Note {
                host: hi,
                text: "unreachable".to_string(),
            });
            continue;
        }
        if h.friction.is_empty() {
            out.push(FrictionLine::Note {
                host: hi,
                text: "no friction filed".to_string(),
            });
            continue;
        }
        let mut idx: Vec<usize> = (0..h.friction.len()).collect();
        idx.sort_by(|&a, &b| {
            let (fa, fb) = (&h.friction[a], &h.friction[b]);
            is_open(fb)
                .cmp(&is_open(fa))
                .then(fb.votes.cmp(&fa.votes))
                .then(fb.id.cmp(&fa.id))
        });
        out.extend(
            idx.into_iter()
                .map(|index| FrictionLine::Entry { host: hi, index }),
        );
    }
    out
}

pub struct FrictionCells {
    pub id: String,
    pub kind: String,
    pub status: String,
    pub votes: String,
    pub summary: String,
    pub links: String,
}

pub fn cells(f: &FrictionRow) -> FrictionCells {
    FrictionCells {
        id: format!("#{}", f.id),
        kind: f.kind.clone(),
        status: f.status.clone(),
        votes: if f.votes > 0 {
            format!("+{}", f.votes)
        } else {
            String::new()
        },
        summary: f.summary.clone(),
        links: f.links.join(" "),
    }
}

pub fn detail(view: &View, host: usize, index: usize, now: DateTime<Utc>) -> String {
    let h = &view.hosts[host];
    let f = &h.friction[index];
    let mut out = format!(
        "#{} on {} · {} · {}\n\n{}\n",
        f.id, h.host, f.kind, f.status, f.summary
    );
    if !f.detail.is_empty() {
        out.push_str(&format!("\n{}\n", f.detail));
    }
    out.push('\n');
    let age = human_age(&f.created_at, now);
    out.push_str(&format!(
        "filed by {} {}\n",
        if f.filed_by.is_empty() {
            "a person"
        } else {
            &f.filed_by
        },
        if age.is_empty() {
            String::new()
        } else {
            format!("{age} ago")
        }
    ));
    out.push_str(&format!("votes    {}\n", f.votes));
    if !f.links.is_empty() {
        out.push_str(&format!("linked   {}\n", f.links.join(", ")));
    }
    if !f.resolution.is_empty() {
        out.push_str(&format!("resolved {}\n", f.resolution));
    }
    out
}

pub fn find(view: &View, key: &FrictionKey) -> Option<(usize, usize)> {
    let hi = view.hosts.iter().position(|h| h.host == key.host)?;
    let fi = view.hosts[hi]
        .friction
        .iter()
        .position(|f| f.id == key.id)?;
    Some((hi, fi))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    #[test]
    fn open_first_by_votes() {
        let v = fixture::view(fixture::default_now());
        let ls = lines(&v);
        let ids: Vec<i64> = ls
            .iter()
            .filter_map(|l| match l {
                FrictionLine::Entry { host: 0, index } => Some(v.hosts[0].friction[*index].id),
                _ => None,
            })
            .collect();
        assert_eq!(ids, [34, 36, 29, 37, 38, 21, 17]);
        assert!(matches!(ls[0], FrictionLine::Host { open: 5, .. }));
        let d = detail(&v, 0, 0, fixture::default_now());
        assert!(d.contains("linked   4@phone") && d.contains("2d ago"));
    }
}
