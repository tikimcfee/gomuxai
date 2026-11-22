//! The agents page: every host's agents as a tree by `parent`, each joined to its current
//! session for its token counts.

use agent_manage::{AgentRow, HostView, SessionRow, SessionState, View, human_tokens};

use crate::sort::{self, Natural, SortOrder, Value};

/// One line of the agents page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentLine {
    /// A host's heading.
    Host { host: usize },
    /// An agent, `depth` levels under its tree's root.
    Agent {
        host: usize,
        agent: usize,
        depth: usize,
    },
    /// A host with no agents, or one that didn't answer.
    Note { host: usize, text: String },
}

/// Names an agent across reloads.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentKey {
    pub host: String,
    pub name: String,
}

/// The agents list's columns, as the header shows them. The order sorts each agent's
/// helpers among themselves, and the roots of each host's tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Column {
    Name,
    Status,
    Backend,
    Model,
    Context,
    Tokens,
    Held,
    /// Flags, then the cwd.
    Flags,
}

impl sort::Column for Column {
    const ALL: &'static [Column] = &[
        Column::Name,
        Column::Status,
        Column::Backend,
        Column::Model,
        Column::Context,
        Column::Tokens,
        Column::Held,
        Column::Flags,
    ];
    const DEFAULT: &'static str = "agent A–Z, helpers under their parent";
    fn label(self) -> &'static str {
        match self {
            Column::Name => "agent",
            Column::Status => "status",
            Column::Backend => "CLI",
            Column::Model => "model",
            Column::Context => "context",
            Column::Tokens => "tokens",
            Column::Held => "held",
            Column::Flags => "flags · cwd",
        }
    }
    fn natural(self) -> Natural {
        match self {
            Column::Status => Natural::Rank,
            Column::Context | Column::Tokens | Column::Held => Natural::Number,
            _ => Natural::Text,
        }
    }
    fn rank_words(self) -> (&'static str, &'static str) {
        ("waiting first", "ended first")
    }
}

/// How the agents list is ordered.
pub type Order = SortOrder<Column>;

/// An agent's value in a column, for sorting.
pub fn value(h: &HostView, a: &AgentRow, c: Column) -> Value {
    let usage = || session_of(h, a).and_then(|s| s.usage.as_ref().map(|u| (s, u)));
    match c {
        Column::Name => sort::text(&a.name),
        Column::Status => Value::Rank(sort::status_rank(&a.status)),
        Column::Backend => sort::text(&a.backend),
        Column::Model => sort::text(&model_of(a, session_of(h, a))),
        Column::Context => sort::num(usage().map(|(_, u)| u.context_tokens)),
        Column::Tokens => sort::num(usage().map(|(s, u)| {
            s.usage_with_subagents
                .map(|t| t.total())
                .unwrap_or(u.tokens.total())
        })),
        Column::Held => Value::Num(a.held),
        Column::Flags => {
            let mut t = a.flags.join("; ");
            if t.is_empty() {
                // No flags after any flags, so what needs a look comes first.
                t.push('\u{10FFFF}');
            }
            t.push_str(&a.cwd);
            sort::text(&t)
        }
    }
}

/// The lines, host by host: roots are agents with no parent on their own host (spawned by
/// a person, or by an agent on another host), each followed by its helpers; siblings by
/// name, or by `order` when it has keys.
pub fn lines(view: &View, order: &Order) -> Vec<AgentLine> {
    let mut out = Vec::new();
    for (hi, h) in view.hosts.iter().enumerate() {
        out.push(AgentLine::Host { host: hi });
        if !h.reachable {
            let why = if h.error.is_empty() {
                "didn't answer"
            } else {
                &h.error
            };
            out.push(AgentLine::Note {
                host: hi,
                text: format!("unreachable: {why}"),
            });
            continue;
        }
        if h.agents.is_empty() {
            out.push(AgentLine::Note {
                host: hi,
                text: "no agents".to_string(),
            });
            continue;
        }
        let local_parent = |a: &AgentRow| -> Option<usize> {
            if a.parent.is_empty() {
                return None;
            }
            let name = match a.parent.split_once('@') {
                Some((_, host)) if host != h.host => return None,
                Some((n, _)) => n,
                None => a.parent.as_str(),
            };
            h.agents
                .iter()
                .position(|p| p.name == name && p.name != a.name)
        };
        let parents: Vec<Option<usize>> = h.agents.iter().map(local_parent).collect();
        // Siblings by name, then by the order's keys.
        let siblings = |mut ids: Vec<usize>| -> Vec<usize> {
            ids.sort_by(|&a, &b| h.agents[a].name.cmp(&h.agents[b].name));
            order.sort(ids, |&i, c| value(h, &h.agents[i], c))
        };
        let roots = siblings(
            (0..h.agents.len())
                .filter(|&i| parents[i].is_none())
                .collect(),
        );
        let mut seen = vec![false; h.agents.len()];
        let emit = |start: Vec<(usize, usize)>, seen: &mut Vec<bool>, out: &mut Vec<AgentLine>| {
            let mut stack = start;
            while let Some((i, depth)) = stack.pop() {
                if std::mem::replace(&mut seen[i], true) {
                    continue;
                }
                out.push(AgentLine::Agent {
                    host: hi,
                    agent: i,
                    depth,
                });
                let kids = siblings(
                    (0..h.agents.len())
                        .filter(|&k| parents[k] == Some(i))
                        .collect(),
                );
                // Reversed onto the stack, so the first pops first.
                stack.extend(kids.into_iter().rev().map(|k| (k, depth + 1)));
            }
        };
        emit(
            roots.into_iter().rev().map(|i| (i, 0)).collect(),
            &mut seen,
            &mut out,
        );
        // A parent loop leaves agents no root reaches: show them at the top level.
        for i in 0..h.agents.len() {
            if !seen[i] {
                emit(vec![(i, 0)], &mut seen, &mut out);
            }
        }
    }
    out
}

/// The session an agent runs, on its own host.
pub fn session_of<'a>(h: &'a HostView, a: &AgentRow) -> Option<&'a SessionRow> {
    if a.session_id.is_empty() {
        return None;
    }
    h.sessions.iter().find(|s| s.session_id == a.session_id)
}

/// `idle`, `ended (quit)`.
pub fn status_text(a: &AgentRow) -> String {
    if a.end_reason.is_empty() {
        a.status.clone()
    } else {
        format!("{} ({})", a.status, a.end_reason)
    }
}

/// What a line shows, as text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCells {
    /// The name, indented by depth.
    pub name: String,
    pub status: String,
    pub backend: String,
    pub model: String,
    /// The current session's context, "" without a counted session.
    pub context: String,
    /// The current session's tokens, with its subagents' when it has any.
    pub tokens: String,
    pub held: String,
    pub flags: String,
    pub cwd: String,
    /// The parent, when it is on another host.
    pub remote_parent: String,
}

pub fn cells(view: &View, host: usize, agent: usize, depth: usize) -> AgentCells {
    let h = &view.hosts[host];
    let a = &h.agents[agent];
    let s = session_of(h, a);
    let (context, tokens) = match s {
        Some(s) => match &s.usage {
            Some(u) => {
                let total = s
                    .usage_with_subagents
                    .map(|t| t.total())
                    .unwrap_or(u.tokens.total());
                (human_tokens(u.context_tokens), human_tokens(total))
            }
            None if s.state == SessionState::Missing => (String::new(), String::new()),
            None => ("…".to_string(), "…".to_string()),
        },
        None => (String::new(), String::new()),
    };
    let remote_parent = match a.parent.split_once('@') {
        Some((_, ph)) if ph != h.host => a.parent.clone(),
        _ => String::new(),
    };
    let mut name = format!(
        "{}{}",
        "   ".repeat(depth),
        if depth > 0 { "└ " } else { "" }
    );
    name.push_str(&a.name);
    if a.adopted {
        name.push_str(" (adopted)");
    }
    AgentCells {
        name,
        status: status_text(a),
        backend: a.backend.clone(),
        model: model_of(a, s),
        context,
        tokens,
        held: if a.held > 0 {
            a.held.to_string()
        } else {
            String::new()
        },
        flags: a.flags.join("; "),
        cwd: a.cwd.clone(),
        remote_parent,
    }
}

/// The model it runs: the one it was spawned on, else what its session's transcript says
/// (an agent spawned without `--model` records none).
pub fn model_of(a: &AgentRow, s: Option<&SessionRow>) -> String {
    if !a.model.is_empty() {
        return a.model.clone();
    }
    s.map(|s| s.model.clone()).unwrap_or_default()
}

/// Finds the line of a key after a reload.
pub fn find(view: &View, key: &AgentKey) -> Option<(usize, usize)> {
    let hi = view.hosts.iter().position(|h| h.host == key.host)?;
    let ai = view.hosts[hi]
        .agents
        .iter()
        .position(|a| a.name == key.name)?;
    Some((hi, ai))
}

/// The detail of one agent, as lines of text.
pub fn detail(view: &View, host: usize, agent: usize) -> String {
    let h = &view.hosts[host];
    let a = &h.agents[agent];
    let mut out = String::new();
    let mut line = |k: &str, v: &str| {
        if !v.is_empty() {
            out.push_str(&format!("{k:<10} {v}\n"));
        }
    };
    line("host", &h.host);
    line("status", &status_text(a));
    // To the second: `2026-11-20T22:27:00.732004402Z` wraps the detail pane.
    let since = match a.since.split_once('.') {
        Some((secs, _)) if a.since.ends_with('Z') => format!("{secs}Z"),
        _ => a.since.clone(),
    };
    line("since", &since);
    line("detail", &a.detail);
    line("backend", &a.backend);
    line("model", &model_of(a, session_of(h, a)));
    line(
        "parent",
        if a.parent.is_empty() {
            "(a person)"
        } else {
            &a.parent
        },
    );
    line("cwd", &a.cwd);
    line("session", &a.session_id);
    if let Some(s) = session_of(h, a) {
        line("harness", &s.harness);
        line("state", s.state.as_str());
        if let Some(u) = &s.usage {
            line("context", &human_tokens(u.context_tokens));
            line(
                "tokens",
                &format!(
                    "{} in {} calls ({} out, {} cache read)",
                    human_tokens(u.tokens.total()),
                    u.tokens.calls,
                    human_tokens(u.tokens.output),
                    human_tokens(u.tokens.cache_read)
                ),
            );
            if let Some(t) = s.usage_with_subagents {
                line("+subagents", &human_tokens(t.total()));
            }
            if u.compactions > 0 {
                line("compacted", &u.compactions.to_string());
            }
        } else {
            line("tokens", "not counted yet");
        }
    } else if !a.session_id.is_empty() {
        line("session", "not among the host's sessions");
    }
    if a.held > 0 {
        line("held", &format!("{} messages", a.held));
    }
    for f in &a.flags {
        line("flag", f);
    }
    line("resumable", if a.resumable { "yes" } else { "no" });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn names(view: &View, lines: &[AgentLine]) -> Vec<String> {
        lines
            .iter()
            .map(|l| match l {
                AgentLine::Host { host } => format!("# {}", view.hosts[*host].host),
                AgentLine::Agent { host, agent, depth } => {
                    format!(
                        "{}{}",
                        "-".repeat(*depth),
                        view.hosts[*host].agents[*agent].name
                    )
                }
                AgentLine::Note { text, .. } => format!("({})", text.split(':').next().unwrap()),
            })
            .collect()
    }

    #[test]
    fn sorting_keeps_helpers_under_their_parent_and_sorts_siblings() {
        let v = fixture::view(fixture::default_now());
        let mut o = Order::default();
        o.click(Column::Status, false);
        let tail = [
            "# laptop",
            "(unreachable)",
            "# phone",
            "phone-shell",
            "-phone-shell-ui",
        ];
        let mut want = vec![
            "# hub",
            "lead",
            "-lead-gui",
            "--lead-gui-review",
            "-lead-docs",
            "--old-helper",
            "notes-pane",
            "watcher",
            "builder",
        ];
        want.extend(tail);
        assert_eq!(names(&v, &lines(&v, &o)), want);
        // Flipped: ended first; ties (lead, notes-pane) keep the default, by name.
        o.click(Column::Status, false);
        let mut want = vec![
            "# hub",
            "builder",
            "watcher",
            "lead",
            "-lead-docs",
            "--old-helper",
            "-lead-gui",
            "--lead-gui-review",
            "notes-pane",
        ];
        want.extend(tail);
        assert_eq!(names(&v, &lines(&v, &o)), want);
        // Two keys: CLI, then status.
        let mut o = Order::default();
        o.click(Column::Backend, true);
        o.click(Column::Status, true);
        let got = names(&v, &lines(&v, &o));
        assert_eq!(
            got[..9],
            [
                "# hub",
                "builder",
                "lead",
                "-lead-gui",
                "--lead-gui-review",
                "-lead-docs",
                "--old-helper",
                "notes-pane",
                "watcher",
            ]
        );
    }

    #[test]
    fn tree_by_parent_per_host() {
        let v = fixture::view(fixture::default_now());
        let got = names(&v, &lines(&v, &Order::default()));
        assert_eq!(
            got,
            [
                "# hub",
                "builder",
                "lead",
                "-lead-docs",
                "--old-helper",
                "-lead-gui",
                "--lead-gui-review",
                "notes-pane",
                "watcher",
                "# laptop",
                "(unreachable)",
                "# phone",
                "phone-shell",
                "-phone-shell-ui",
            ]
        );
    }

    #[test]
    fn a_parent_loop_still_lists_everyone() {
        let mut v = View::default();
        let mut h = HostView {
            host: "x".into(),
            reachable: true,
            ..Default::default()
        };
        for (n, p) in [("a", "b"), ("b", "a"), ("c", "c@x")] {
            h.agents.push(AgentRow {
                name: n.into(),
                parent: p.into(),
                ..Default::default()
            });
        }
        v.hosts.push(h);
        let got = names(&v, &lines(&v, &Order::default()));
        assert_eq!(got.len(), 4, "{got:?}");
    }

    #[test]
    fn cells_join_the_current_session() {
        let v = fixture::view(fixture::default_now());
        let h = &v.hosts[0];
        let lead = h.agents.iter().position(|a| a.name == "lead").unwrap();
        let c = cells(&v, 0, lead, 0);
        assert_eq!(c.status, "idle");
        assert!(!c.tokens.is_empty() && !c.context.is_empty());
        // No model recorded: the session's.
        let mut v2 = v.clone();
        v2.hosts[0].agents[lead].model.clear();
        let want = session_of(&v2.hosts[0], &v2.hosts[0].agents[lead])
            .unwrap()
            .model
            .clone();
        assert!(!want.is_empty());
        assert_eq!(cells(&v2, 0, lead, 0).model, want);
        let old = h
            .agents
            .iter()
            .position(|a| a.name == "old-helper")
            .unwrap();
        let c = cells(&v, 0, old, 2);
        assert_eq!(c.status, "ended (gone)");
        assert!(c.tokens.is_empty(), "a missing transcript has no usage");
        assert!(c.name.starts_with("      └ "));
        let shell = cells(&v, 2, 0, 0);
        assert_eq!(shell.remote_parent, "lead@hub");
        assert!(detail(&v, 0, lead).contains("tokens"));
    }
}
