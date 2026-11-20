//! The sessions page: every session of every host, filtered and sorted, each subagent's
//! under the session that started it, with what a selection may do.

use std::collections::{BTreeSet, HashMap};

use agent_manage::{SessionRow, SessionState, View, human_age, human_bytes, human_tokens};
use chrono::{DateTime, Utc};

/// What the list shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Matches title, project, cwd, agent, session id and model, ignoring case.
    pub text: String,
    /// A host's name, or all.
    pub host: Option<String>,
    pub harness: Option<String>,
    pub state: Option<SessionState>,
    pub stale_only: bool,
    pub show_untracked: bool,
}

impl Filter {
    pub fn matches(&self, host: &str, s: &SessionRow) -> bool {
        if !self.show_untracked && s.untracked {
            return false;
        }
        if self.stale_only && !s.stale {
            return false;
        }
        if self.host.as_deref().is_some_and(|h| h != host)
            || self.harness.as_deref().is_some_and(|h| h != s.harness)
            || self.state.is_some_and(|st| st != s.state)
        {
            return false;
        }
        let q = self.text.trim().to_lowercase();
        if q.is_empty() {
            return true;
        }
        [
            &s.title,
            &s.project,
            &s.cwd,
            &s.agent,
            &s.session_id,
            &s.model,
        ]
        .iter()
        .any(|f| f.to_lowercase().contains(&q))
    }
}

/// How the list is ordered. A subagent's session always follows its parent's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Sort {
    /// Newest first.
    #[default]
    When,
    /// Most tokens first (with subagents).
    Tokens,
    /// Largest transcript first.
    Size,
}

impl Sort {
    pub const ALL: [Sort; 3] = [Sort::When, Sort::Tokens, Sort::Size];
    pub fn label(&self) -> &'static str {
        match self {
            Sort::When => "newest",
            Sort::Tokens => "most tokens",
            Sort::Size => "largest",
        }
    }
}

/// Names a session across reloads and hosts.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SessionKey {
    pub host: String,
    /// `harness:session_id`, what `agent manage untrack` takes.
    pub key: String,
}

/// One shown session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLine {
    pub host: usize,
    pub index: usize,
    /// 1 for a subagent's session.
    pub depth: usize,
    /// On a session that started subagents, how many the host lists (shown or not).
    pub subagents: usize,
}

impl SessionLine {
    pub fn row<'a>(&self, view: &'a View) -> &'a SessionRow {
        &view.hosts[self.host].sessions[self.index]
    }
    pub fn key(&self, view: &View) -> SessionKey {
        SessionKey {
            host: view.hosts[self.host].host.clone(),
            key: self.row(view).key(),
        }
    }
}

/// Tokens of the session with its subagents' where counted; `None` if not counted.
pub fn tokens_with_subagents(s: &SessionRow) -> Option<i64> {
    s.usage_with_subagents
        .map(|t| t.total())
        .or_else(|| s.usage.as_ref().map(|u| u.tokens.total()))
}

/// The shown lines: every host's sessions that pass the filter, grouped as a session with
/// its subagents, the groups ordered by `sort` across hosts.
pub fn lines(view: &View, filter: &Filter, sort: Sort) -> Vec<SessionLine> {
    // (sort key, lines) per group.
    let mut groups: Vec<((i64, String), Vec<SessionLine>)> = Vec::new();
    for (hi, h) in view.hosts.iter().enumerate() {
        // A subagent joins the group of the session that started it, wherever the host
        // listed it (a host may order by time rather than put it after its parent). One
        // whose parent isn't listed heads a group of its own.
        let ids: HashMap<&str, usize> = h
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.parent_session.is_empty())
            .map(|(i, s)| (s.session_id.as_str(), i))
            .collect();
        let mut by_head: Vec<Vec<usize>> = vec![Vec::new(); h.sessions.len()];
        let mut heads = Vec::new();
        for (i, s) in h.sessions.iter().enumerate() {
            match ids.get(s.parent_session.as_str()) {
                Some(&p) if !s.parent_session.is_empty() => by_head[p].push(i),
                _ => {
                    heads.push(i);
                    by_head[i].insert(0, i);
                }
            }
        }
        for head in heads {
            let members = &mut by_head[head];
            // The head first, its subagents newest first.
            members[1..].sort_by(|&a, &b| h.sessions[b].mtime.cmp(&h.sessions[a].mtime));
            let subs = members.len() - 1;
            let ls: Vec<SessionLine> = members
                .iter()
                .filter(|&&i| filter.matches(&h.host, &h.sessions[i]))
                .map(|&i| SessionLine {
                    host: hi,
                    index: i,
                    depth: usize::from(!h.sessions[i].parent_session.is_empty()),
                    subagents: if i == head { subs } else { 0 },
                })
                .collect();
            if ls.is_empty() {
                continue;
            }
            let s = &h.sessions[head];
            let k = match sort {
                Sort::When => 0,
                Sort::Tokens => -tokens_with_subagents(s).unwrap_or(-1),
                Sort::Size => -s.size,
            };
            // Ties (all of them for `When`) go newest first: RFC 3339 in UTC sorts as
            // text, and the sort below compares it reversed.
            groups.push(((k, s.mtime.clone()), ls));
        }
    }
    // Stable: equal keys keep each host's own order (newest first), hosts in order.
    groups.sort_by(|(a, _), (b, _)| a.0.cmp(&b.0).then_with(|| b.1.cmp(&a.1)));
    groups.into_iter().flat_map(|(_, ls)| ls).collect()
}

/// What the footer says about the shown lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub sessions: usize,
    /// Each shown session's own tokens (subagents are shown and counted on their own).
    pub tokens: i64,
    pub bytes: i64,
    /// Shown sessions whose tokens aren't counted yet.
    pub uncounted: usize,
}

pub fn totals(view: &View, lines: &[SessionLine]) -> Totals {
    let mut t = Totals::default();
    for l in lines {
        let s = l.row(view);
        t.sessions += 1;
        t.bytes += s.size;
        match &s.usage {
            Some(u) => t.tokens += u.tokens.total(),
            None if s.state != SessionState::Missing => t.uncounted += 1,
            None => {}
        }
    }
    t
}

impl Totals {
    pub fn text(&self) -> String {
        let mut s = format!(
            "{} sessions · {} tokens · {}",
            self.sessions,
            human_tokens(self.tokens),
            human_bytes(self.bytes)
        );
        if self.uncounted > 0 {
            s.push_str(&format!(" · {} not counted yet", self.uncounted));
        }
        s
    }
}

/// What a line shows, as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionCells {
    pub when: String,
    pub host: String,
    pub harness: String,
    pub project: String,
    pub state: String,
    pub status: String,
    pub agent: String,
    pub model: String,
    pub context: String,
    /// Own tokens, then `Σ` and the total with subagents when it has any.
    pub tokens: String,
    pub size: String,
    /// Indented for a subagent's session.
    pub title: String,
}

pub fn cells(view: &View, line: &SessionLine, now: DateTime<Utc>) -> SessionCells {
    let h = &view.hosts[line.host];
    let s = line.row(view);
    let mut state = s.state.as_str().to_string();
    if s.stale {
        state.push_str(" · stale");
    }
    let status = if s.end_reason.is_empty() {
        s.status.clone()
    } else {
        format!("{} ({})", s.status, s.end_reason)
    };
    let (context, tokens) = match &s.usage {
        Some(u) => {
            let mut t = human_tokens(u.tokens.total());
            if let Some(w) = s.usage_with_subagents {
                t = format!("{t} Σ{}", human_tokens(w.total()));
            }
            (human_tokens(u.context_tokens), t)
        }
        None if s.state == SessionState::Missing => (String::new(), String::new()),
        None => ("…".to_string(), "…".to_string()),
    };
    let mut title = if s.title.is_empty() {
        format!("({})", short_id(&s.session_id))
    } else {
        s.title.clone()
    };
    // The state column is narrow; untracked goes first in the title, which has room.
    if s.untracked {
        title = format!("[untracked] {title}");
    }
    SessionCells {
        when: if s.mtime.is_empty() {
            "—".to_string()
        } else {
            human_age(&s.mtime, now)
        },
        host: h.host.clone(),
        harness: s.harness.clone(),
        project: s.project.clone(),
        state,
        status,
        agent: s.agent.clone(),
        model: s.model.clone(),
        context,
        tokens,
        size: if s.size > 0 {
            human_bytes(s.size)
        } else {
            String::new()
        },
        title: if line.depth > 0 {
            format!("  ↳ {title}")
        } else if line.subagents > 0 {
            // In front: a long title would cut it off at the end.
            format!("[{} subagents] {title}", line.subagents)
        } else {
            title
        },
    }
}

pub fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Two lines about a session, for under the list: who and where, then the transcript.
pub fn brief(view: &View, line: &SessionLine) -> String {
    let h = &view.hosts[line.host];
    let s = line.row(view);
    let mut first = vec![format!("{} on {}", s.key(), h.host)];
    if !s.agent.is_empty() {
        first.push(format!("agent {}", s.agent));
    }
    if !s.parent_session.is_empty() {
        first.push(format!("subagent of {}", short_id(&s.parent_session)));
    }
    if !s.cwd.is_empty() {
        first.push(s.cwd.clone());
    }
    if !s.git_branch.is_empty() {
        first.push(format!("branch {}", s.git_branch));
    }
    if let Some(u) = &s.usage {
        first.push(format!(
            "{} calls, {} compactions",
            u.tokens.calls, u.compactions
        ));
    }
    let second = if s.path.is_empty() {
        "no transcript on disk".to_string()
    } else {
        s.path.clone()
    };
    format!("{}\n{second}", first.join("  ·  "))
}

/// The detail of one session, as lines of text.
pub fn detail(view: &View, line: &SessionLine) -> String {
    let h = &view.hosts[line.host];
    let s = line.row(view);
    let mut out = String::new();
    let mut put = |k: &str, v: &str| {
        if !v.is_empty() {
            out.push_str(&format!("{k:<9} {v}\n"));
        }
    };
    put("session", &s.key());
    put("host", &h.host);
    put("title", &s.title);
    put("state", s.state.as_str());
    put("status", &s.status);
    put("ended", &s.end_reason);
    put("agent", &s.agent);
    put("parent", &s.parent_session);
    put("cwd", &s.cwd);
    put("branch", &s.git_branch);
    put("model", &s.model);
    put("modified", &s.mtime);
    put(
        "path",
        if s.path.is_empty() {
            "(no transcript)"
        } else {
            &s.path
        },
    );
    if let Some(u) = &s.usage {
        put(
            "tokens",
            &format!(
                "{} in {} calls · context {}",
                human_tokens(u.tokens.total()),
                u.tokens.calls,
                human_tokens(u.context_tokens)
            ),
        );
    }
    out
}

/// The state filter's choices, "all" first.
pub fn state_choices() -> Vec<(String, Option<SessionState>)> {
    let mut v = vec![("all states".to_string(), None)];
    v.extend(
        SessionState::ALL
            .iter()
            .map(|s| (s.as_str().to_string(), Some(*s))),
    );
    v
}

/// Harnesses seen in the view, sorted.
pub fn harnesses(view: &View) -> Vec<String> {
    let set: BTreeSet<&str> = view
        .hosts
        .iter()
        .flat_map(|h| h.sessions.iter().map(|s| s.harness.as_str()))
        .collect();
    set.into_iter().map(String::from).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn v() -> View {
        fixture::view(fixture::default_now())
    }

    #[test]
    fn default_hides_untracked_and_keeps_subagents_under_parents() {
        let v = v();
        let ls = lines(&v, &Filter::default(), Sort::When);
        assert!(ls.iter().all(|l| !l.row(&v).untracked));
        let all: usize = v.hosts.iter().map(|h| h.sessions.len()).sum();
        let untracked: usize = v
            .hosts
            .iter()
            .map(|h| h.sessions.iter().filter(|s| s.untracked).count())
            .sum();
        assert_eq!(ls.len(), all - untracked);
        for (i, l) in ls.iter().enumerate() {
            if l.depth == 1 {
                // Its parent is the nearest line above at depth 0.
                let parent = ls[..i].iter().rev().find(|p| p.depth == 0).unwrap();
                assert_eq!(parent.row(&v).session_id, l.row(&v).parent_session);
            }
        }
        // Newest first across hosts.
        let tops: Vec<&str> = ls
            .iter()
            .filter(|l| l.depth == 0)
            .map(|l| l.row(&v).mtime.as_str())
            .filter(|m| !m.is_empty())
            .collect();
        assert!(tops.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn subagents_join_their_parent_wherever_listed() {
        let row = |id: &str, parent: &str, mtime: &str| SessionRow {
            session_id: id.into(),
            parent_session: parent.into(),
            mtime: mtime.into(),
            harness: "claude".into(),
            ..SessionRow::default()
        };
        // As a host ordering by time lists them: subagents apart from their parent.
        let v = View {
            hosts: vec![agent_manage::HostView {
                host: "h".into(),
                reachable: true,
                sessions: vec![
                    row("p", "", "2026-11-20T10:05:00Z"),
                    row("x", "", "2026-11-20T10:04:00Z"),
                    row("s1", "p", "2026-11-20T10:03:00Z"),
                    row("s2", "p", "2026-11-20T10:04:30Z"),
                    row("o", "gone", "2026-11-20T10:01:00Z"),
                ],
                ..Default::default()
            }],
            ..View::default()
        };
        let ids: Vec<(&str, usize)> = lines(&v, &Filter::default(), Sort::When)
            .iter()
            .map(|l| (l.row(&v).session_id.as_str(), l.depth))
            .collect();
        assert_eq!(ids, [("p", 0), ("s2", 1), ("s1", 1), ("x", 0), ("o", 1)]);
    }

    #[test]
    fn filters_combine() {
        let v = v();
        let f = Filter {
            host: Some("phone".into()),
            harness: Some("claude".into()),
            state: Some(SessionState::Ended),
            show_untracked: true,
            ..Filter::default()
        };
        let ls = lines(&v, &f, Sort::When);
        assert!(!ls.is_empty());
        for l in &ls {
            let s = l.row(&v);
            assert_eq!(v.hosts[l.host].host, "phone");
            assert_eq!(
                (s.harness.as_str(), s.state),
                ("claude", SessionState::Ended)
            );
        }
        let stale = lines(
            &v,
            &Filter {
                stale_only: true,
                ..Filter::default()
            },
            Sort::When,
        );
        assert!(!stale.is_empty() && stale.iter().all(|l| l.row(&v).stale));
        let text = lines(
            &v,
            &Filter {
                text: "LEAD-GUI".into(),
                ..Filter::default()
            },
            Sort::When,
        );
        assert_eq!(text.len(), 2, "the gui agent's session and its reviewer's");
    }

    #[test]
    fn sort_by_tokens_and_size() {
        let v = v();
        let ls = lines(&v, &Filter::default(), Sort::Tokens);
        let toks: Vec<i64> = ls
            .iter()
            .filter(|l| l.depth == 0)
            .map(|l| tokens_with_subagents(l.row(&v)).unwrap_or(-1))
            .collect();
        assert!(toks.windows(2).all(|w| w[0] >= w[1]));
        let ls = lines(&v, &Filter::default(), Sort::Size);
        let sizes: Vec<i64> = ls
            .iter()
            .filter(|l| l.depth == 0)
            .map(|l| l.row(&v).size)
            .collect();
        assert!(sizes.windows(2).all(|w| w[0] >= w[1]));
    }

    #[test]
    fn totals_count_own_tokens_once() {
        let v = v();
        let all = Filter {
            show_untracked: true,
            ..Filter::default()
        };
        let ls = lines(&v, &all, Sort::When);
        let t = totals(&v, &ls);
        let want: i64 = ls
            .iter()
            .filter_map(|l| l.row(&v).usage.as_ref())
            .map(|u| u.tokens.total())
            .sum();
        assert_eq!(t.tokens, want);
        assert_eq!(t.uncounted, 14);
        assert!(t.text().contains("14 not counted yet"));
    }

    #[test]
    fn cells_mark_pending_and_subagents() {
        let v = v();
        let ls = lines(&v, &Filter::default(), Sort::When);
        let now = fixture::default_now();
        let pending = ls
            .iter()
            .find(|l| l.row(&v).usage.is_none() && l.row(&v).state != SessionState::Missing)
            .unwrap();
        assert_eq!(cells(&v, pending, now).tokens, "…");
        let parent = ls
            .iter()
            .find(|l| l.row(&v).usage_with_subagents.is_some())
            .unwrap();
        assert!(cells(&v, parent, now).tokens.contains('Σ'));
        let child = ls.iter().find(|l| l.depth == 1).unwrap();
        assert!(cells(&v, child, now).title.starts_with("  ↳ "));
        let head = ls.iter().find(|l| l.subagents > 0).unwrap();
        let want = format!("[{} subagents] ", head.subagents);
        assert!(cells(&v, head, now).title.starts_with(&want));
        let all = lines(
            &v,
            &Filter {
                show_untracked: true,
                ..Filter::default()
            },
            Sort::When,
        );
        let untracked = all.iter().find(|l| l.row(&v).untracked).unwrap();
        assert!(cells(&v, untracked, now).title.starts_with("[untracked] "));
    }
}
