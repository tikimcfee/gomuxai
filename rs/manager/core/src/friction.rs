//! The friction page: per host, open entries first.

use agent_manage::{FrictionRow, View, human_age};
use chrono::{DateTime, Utc};

use crate::sort::{self, Natural, SortOrder, Value};

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

/// The friction list's columns, as the header shows them. The order sorts each host's
/// entries; hosts stay in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Id,
    Kind,
    Status,
    Votes,
    Summary,
    Links,
}

impl sort::Column for Column {
    const ALL: &'static [Column] = &[
        Column::Id,
        Column::Kind,
        Column::Status,
        Column::Votes,
        Column::Summary,
        Column::Links,
    ];
    const DEFAULT: &'static str = "open first, most votes, newest";
    fn label(self) -> &'static str {
        match self {
            Column::Id => "id",
            Column::Kind => "kind",
            Column::Status => "status",
            Column::Votes => "votes",
            Column::Summary => "summary",
            Column::Links => "linked",
        }
    }
    fn natural(self) -> Natural {
        match self {
            // The newest (highest id) first.
            Column::Id | Column::Votes => Natural::Number,
            Column::Status => Natural::Rank,
            _ => Natural::Text,
        }
    }
    fn rank_words(self) -> (&'static str, &'static str) {
        ("open first", "resolved first")
    }
}

/// How the friction list is ordered.
pub type Order = SortOrder<Column>;

/// Open, accepted, anything else, then resolved.
pub fn status_rank(status: &str) -> u32 {
    match status {
        "open" => 0,
        "accepted" => 1,
        "resolved" => 3,
        _ => 2,
    }
}

/// An entry's value in a column, for sorting.
pub fn value(f: &FrictionRow, c: Column) -> Value {
    match c {
        Column::Id => Value::Num(f.id),
        Column::Kind => sort::text(&f.kind),
        Column::Status => Value::Rank(status_rank(&f.status)),
        Column::Votes => Value::Num(f.votes),
        Column::Summary => sort::text(&f.summary),
        Column::Links => sort::text(&f.links.join(" ")),
    }
}

/// Per host: entries not resolved first, by votes then newest; resolved ones after; or
/// by `order` when it has keys.
pub fn lines(view: &View, order: &Order) -> Vec<FrictionLine> {
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
        let idx = order.sort(idx, |&i, c| value(&h.friction[i], c));
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

    fn hub_ids(v: &View, ls: &[FrictionLine]) -> Vec<i64> {
        ls.iter()
            .filter_map(|l| match l {
                FrictionLine::Entry { host: 0, index } => Some(v.hosts[0].friction[*index].id),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sorted_by_columns_within_each_host() {
        let v = fixture::view(fixture::default_now());
        let mut o = Order::default();
        o.click(Column::Id, false);
        assert_eq!(hub_ids(&v, &lines(&v, &o)), [38, 37, 36, 34, 29, 21, 17]);
        o.click(Column::Id, false);
        assert_eq!(hub_ids(&v, &lines(&v, &o)), [17, 21, 29, 34, 36, 37, 38]);
        // Kind, then votes: ties in both keep the default (open first, newest).
        let mut o = Order::default();
        o.click(Column::Kind, true);
        o.click(Column::Votes, true);
        let ls = lines(&v, &o);
        let rows: Vec<(String, i64)> = hub_ids(&v, &ls)
            .iter()
            .map(|id| {
                let f = v.hosts[0].friction.iter().find(|f| f.id == *id).unwrap();
                (f.kind.clone(), f.votes)
            })
            .collect();
        assert!(
            rows.windows(2)
                .all(|w| w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 >= w[1].1)),
            "{rows:?}"
        );
        // Hosts keep their headings and order.
        assert!(matches!(ls[0], FrictionLine::Host { host: 0, .. }));
        assert_eq!(ls.len(), lines(&v, &Order::default()).len());
    }

    #[test]
    fn open_first_by_votes() {
        let v = fixture::view(fixture::default_now());
        let ls = lines(&v, &Order::default());
        assert_eq!(hub_ids(&v, &ls), [34, 36, 29, 37, 38, 21, 17]);
        assert!(matches!(ls[0], FrictionLine::Host { open: 5, .. }));
        let d = detail(&v, 0, 0, fixture::default_now());
        assert!(d.contains("linked   4@phone") && d.contains("2d ago"));
    }
}
