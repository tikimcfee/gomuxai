//! What each row offers, why an action is off, the `agent` command line it runs, and what a
//! confirmation names before anything destructive runs.

use agent_manage::{AgentRow, SessionState, View};

use crate::history::command_line;
use crate::is_local;
use crate::sessions::SessionLine;

/// An `agent` command: its arguments, without the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub args: Vec<String>,
}

impl Command {
    pub fn new<S: AsRef<str>>(args: &[S]) -> Command {
        Command {
            args: args.iter().map(|s| s.as_ref().to_string()).collect(),
        }
    }
    /// As a person would type it.
    pub fn line(&self) -> String {
        command_line(&self.args)
    }
}

/// An action a row offers, ready to run or off with why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    Ready(Command),
    Off(String),
}

impl Offer {
    pub fn command(&self) -> Option<&Command> {
        match self {
            Offer::Ready(c) => Some(c),
            Offer::Off(_) => None,
        }
    }
    pub fn why_off(&self) -> Option<&str> {
        match self {
            Offer::Ready(_) => None,
            Offer::Off(w) => Some(w),
        }
    }
}

/// A question asked before a destructive command runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub title: String,
    /// Names exactly what the command touches.
    pub body: String,
    /// The confirming button's text.
    pub yes: String,
    pub command: Command,
}

fn local_only(view: &View, host: usize, what: &str) -> Option<String> {
    let h = &view.hosts[host];
    if !is_local(view, host) {
        return Some(format!(
            "{what} works on this host only; {} is another host (run it there)",
            h.host
        ));
    }
    if !h.reachable {
        return Some(format!("{} didn't answer", h.host));
    }
    None
}

fn is_claude_or_agy(a: &AgentRow) -> bool {
    matches!(a.backend.as_str(), "claude" | "agy")
}

/// The agent's name as a command addresses it: `name`, or `name@host` on another host.
pub fn address(view: &View, host: usize, a: &AgentRow) -> String {
    if is_local(view, host) {
        a.name.clone()
    } else {
        format!("{}@{}", a.name, view.hosts[host].host)
    }
}

/// The actions of an agent's detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentOffers {
    pub resume: Offer,
    pub revive: Offer,
    pub cleanup: Offer,
    pub logs: Offer,
}

pub fn agent_offers(view: &View, host: usize, agent: usize) -> AgentOffers {
    let h = &view.hosts[host];
    let a = &h.agents[agent];
    let ended = a.status == "ended";
    let resume = if !h.reachable {
        Offer::Off(format!("{} didn't answer", h.host))
    } else if !ended {
        Offer::Off(format!("it is {}: nothing to resume", a.status))
    } else if a.adopted {
        Offer::Off("adopted: start its CLI in its session yourself".to_string())
    } else if !is_claude_or_agy(a) {
        Offer::Off(format!(
            "resume continues claude and agy agents, not {}",
            a.backend
        ))
    } else if !a.resumable {
        Offer::Off("agent resume can't continue it (no session to resume)".to_string())
    } else {
        Offer::Ready(Command::new(&["resume", &address(view, host, a)]))
    };
    let revive = if let Some(why) = local_only(view, host, "revive") {
        Offer::Off(why)
    } else if !ended {
        Offer::Off(format!("it is {}: nothing to revive", a.status))
    } else if a.adopted {
        Offer::Off("adopted: start its CLI in its session yourself".to_string())
    } else if !is_claude_or_agy(a) {
        Offer::Off(format!(
            "revive continues claude and agy agents, not {}",
            a.backend
        ))
    } else {
        Offer::Ready(Command::new(&["revive", &a.name]))
    };
    let cleanup = match local_only(view, host, "cleanup") {
        Some(why) => Offer::Off(why),
        None => Offer::Ready(Command::new(&["cleanup", &a.name])),
    };
    let logs = if h.reachable {
        Offer::Ready(Command::new(&[
            "logs",
            &address(view, host, a),
            "--lines",
            "40",
        ]))
    } else {
        Offer::Off(format!("{} didn't answer", h.host))
    };
    AgentOffers {
        resume,
        revive,
        cleanup,
        logs,
    }
}

/// The question before `agent cleanup`, naming what goes and saying when it is running.
pub fn cleanup_confirm(view: &View, host: usize, agent: usize) -> Option<Confirm> {
    let offers = agent_offers(view, host, agent);
    let command = offers.cleanup.command()?.clone();
    let h = &view.hosts[host];
    let a = &h.agents[agent];
    let mut body = String::new();
    if a.status != "ended" {
        body.push_str(&format!(
            "{} is running now ({}). Cleanup asks it to exit and then kills its session.\n\n",
            a.name, a.status
        ));
    }
    if a.adopted {
        body.push_str(&format!(
            "{} is adopted: cleanup unregisters it and leaves its session running.\n",
            a.name
        ));
    } else {
        body.push_str(&format!("Kills session agent-{}.\n", a.name));
    }
    body.push_str(&format!(
        "Deletes {}'s record, state, files{}.\nIts logs stay.\n",
        a.name,
        if a.held > 0 {
            format!(" and {} held messages", a.held)
        } else {
            " and held messages".to_string()
        }
    ));
    let helpers: Vec<&str> = h
        .agents
        .iter()
        .filter(|c| c.parent == a.name || c.parent == format!("{}@{}", a.name, h.host))
        .map(|c| c.name.as_str())
        .collect();
    if !helpers.is_empty() {
        body.push_str(&format!(
            "\nIts helpers are not cleaned up with it: {}.\n",
            helpers.join(", ")
        ));
    }
    Some(Confirm {
        title: format!("Clean up {}?", a.name),
        body,
        yes: "Clean up".to_string(),
        command,
    })
}

/// Lists up to `max` names, then how many more.
fn name_list(names: &[String], max: usize) -> String {
    let mut s = String::new();
    for n in names.iter().take(max) {
        s.push_str("  ");
        s.push_str(n);
        s.push('\n');
    }
    if names.len() > max {
        s.push_str(&format!("  … and {} more\n", names.len() - max));
    }
    s
}

fn describe(view: &View, l: &SessionLine) -> String {
    let s = l.row(view);
    let title = if s.title.is_empty() {
        String::new()
    } else {
        format!("  {}", crate::clip(&s.title, 32))
    };
    format!(
        "{}:{} ({}){}",
        s.harness,
        crate::sessions::short_id(&s.session_id),
        s.state.as_str(),
        title
    )
}

/// The question before `agent manage untrack` of `lines`: only this host's sessions, not
/// those already untracked; running ones are named as running.
pub fn untrack_confirm(view: &View, lines: &[SessionLine], title: &str) -> Result<Confirm, String> {
    let mut keys = Vec::new();
    let mut names = Vec::new();
    let mut running = Vec::new();
    let mut elsewhere = 0;
    for l in lines {
        let s = l.row(view);
        if s.untracked {
            continue;
        }
        if !is_local(view, l.host) {
            elsewhere += 1;
            continue;
        }
        keys.push(s.key());
        if s.state == SessionState::Running {
            running.push(s.key());
            names.push(format!("RUNNING  {}", describe(view, l)));
        } else {
            names.push(describe(view, l));
        }
    }
    if keys.is_empty() {
        return Err(if elsewhere > 0 {
            "untracking works on this host only, and none of these are on it".to_string()
        } else {
            "nothing to stop tracking".to_string()
        });
    }
    let h = &view.hosts[crate::local_index(view)];
    let host = &h.host;
    // `agent manage untrack` takes a session's subagents with it.
    let named: std::collections::HashSet<&str> = lines
        .iter()
        .filter(|l| is_local(view, l.host))
        .map(|l| l.row(view).session_id.as_str())
        .collect();
    let subagents = h
        .sessions
        .iter()
        .filter(|s| {
            !s.untracked
                && !s.parent_session.is_empty()
                && named.contains(s.parent_session.as_str())
                && !named.contains(s.session_id.as_str())
        })
        .count();
    let mut body = format!(
        "Stops tracking {} session{} on {host}{}: they leave the session list (show untracked lists them again). Transcripts aren't deleted, and the harness can still resume them.\n\n",
        keys.len(),
        if keys.len() == 1 { "" } else { "s" },
        if subagents > 0 {
            format!(", and {subagents} subagent sessions with them")
        } else {
            String::new()
        }
    );
    if !running.is_empty() {
        body.push_str(&format!(
            "{} of them {} RUNNING now (marked below): a CLI holds {} open, and untracking {} needs --force.\n\n",
            running.len(),
            if running.len() == 1 { "is" } else { "are" },
            if running.len() == 1 { "it" } else { "them" },
            if running.len() == 1 { "it" } else { "them" },
        ));
    }
    // Running ones first, so a long list can't hide them past its cut.
    names.sort_by_key(|n| !n.starts_with("RUNNING"));
    body.push_str(&name_list(&names, 14));
    if elsewhere > 0 {
        body.push_str(&format!(
            "\n{elsewhere} on other hosts are left out: untracking works on this host only.\n"
        ));
    }
    let mut args = vec!["manage".to_string(), "untrack".to_string()];
    if !running.is_empty() {
        // It refuses a running session unless told; the body above said they are running.
        args.push("--force".to_string());
    }
    args.extend(keys);
    Ok(Confirm {
        title: title.to_string(),
        body,
        yes: "Stop tracking".to_string(),
        command: Command { args },
    })
}

/// The shown lines that "stop tracking all shown that aren't running" asks about, on every
/// host: [`untrack_confirm`] leaves out (and names) those of other hosts.
pub fn not_running(view: &View, lines: &[SessionLine]) -> Vec<SessionLine> {
    lines
        .iter()
        .filter(|l| {
            let s = l.row(view);
            s.state != SessionState::Running && !s.untracked
        })
        .copied()
        .collect()
}

/// `agent manage track` for the untracked sessions of this host among `lines`.
pub fn track(view: &View, lines: &[SessionLine]) -> Offer {
    let keys: Vec<String> = lines
        .iter()
        .filter(|l| is_local(view, l.host) && l.row(view).untracked)
        .map(|l| l.row(view).key())
        .collect();
    if keys.is_empty() {
        return Offer::Off("no untracked session of this host is selected".to_string());
    }
    let mut args = vec!["manage".to_string(), "track".to_string()];
    args.extend(keys);
    Offer::Ready(Command { args })
}

/// `agent friction resolve`, which needs a note and an entry of this host still open.
pub fn friction_resolve(view: &View, host: usize, index: usize, note: &str) -> Offer {
    if let Some(why) = local_only(view, host, "resolving") {
        return Offer::Off(why);
    }
    let f = &view.hosts[host].friction[index];
    if !crate::friction::is_open(f) {
        return Offer::Off(format!("#{} is already resolved", f.id));
    }
    let note = note.trim();
    if note.is_empty() {
        return Offer::Off("write a note saying how it was resolved".to_string());
    }
    Offer::Ready(Command::new(&[
        "friction",
        "resolve",
        &f.id.to_string(),
        "--note",
        note,
    ]))
}

/// The question before resolving an entry linked to others, which resolves them too.
pub fn friction_confirm(
    view: &View,
    host: usize,
    index: usize,
    command: Command,
) -> Option<Confirm> {
    let f = &view.hosts[host].friction[index];
    if f.links.is_empty() {
        return None;
    }
    Some(Confirm {
        title: format!("Resolve #{}?", f.id),
        body: format!(
            "Resolves #{} \"{}\" on {}, and the entries linked to it: {}.\n",
            f.id,
            f.summary,
            view.hosts[host].host,
            f.links.join(", ")
        ),
        yes: "Resolve".to_string(),
        command,
    })
}

pub fn backup(view: &View) -> Offer {
    if view.hosts.is_empty() {
        return Offer::Off("no view read yet".to_string());
    }
    match local_only(view, crate::local_index(view), "backup") {
        Some(why) => Offer::Off(why),
        None => Offer::Ready(Command::new(&["manage", "backup"])),
    }
}

pub fn prune_dry_run(days: u32) -> Command {
    Command::new(&[
        "manage",
        "prune",
        "--older-than",
        &format!("{days}d"),
        "--dry-run",
    ])
}

pub fn prune(days: u32) -> Command {
    Command::new(&["manage", "prune", "--older-than", &format!("{days}d")])
}

/// The question after a dry run, quoting what it said it would delete.
pub fn prune_confirm(view: &View, days: u32, dry_run_output: &str) -> Confirm {
    let host = view
        .hosts
        .get(crate::local_index(view))
        .map(|h| h.host.as_str())
        .unwrap_or("this host");
    let said = dry_run_output.trim();
    Confirm {
        title: format!("Prune data older than {days} days on {host}?"),
        body: format!(
            "The dry run says this would be deleted:\n\n{}\n",
            if said.is_empty() {
                "(it printed nothing)"
            } else {
                said
            }
        ),
        yes: "Prune".to_string(),
        command: prune(days),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use crate::sessions::{self, Filter, Sort};

    fn v() -> View {
        fixture::view(fixture::default_now())
    }

    fn idx(v: &View, host: usize, name: &str) -> usize {
        v.hosts[host]
            .agents
            .iter()
            .position(|a| a.name == name)
            .unwrap()
    }

    #[test]
    fn agent_offers_follow_status_backend_and_host() {
        let v = v();
        let docs = agent_offers(&v, 0, idx(&v, 0, "lead-docs"));
        assert_eq!(
            docs.resume.command().unwrap().line(),
            "agent resume lead-docs"
        );
        assert_eq!(
            docs.revive.command().unwrap().line(),
            "agent revive lead-docs"
        );
        assert_eq!(
            docs.cleanup.command().unwrap().line(),
            "agent cleanup lead-docs"
        );
        assert_eq!(
            docs.logs.command().unwrap().line(),
            "agent logs lead-docs --lines 40"
        );

        let lead = agent_offers(&v, 0, idx(&v, 0, "lead"));
        assert_eq!(lead.resume.why_off(), Some("it is idle: nothing to resume"));
        assert!(
            lead.cleanup.command().is_some(),
            "offered, but the confirm says it runs"
        );

        let watcher = agent_offers(&v, 0, idx(&v, 0, "watcher"));
        assert!(watcher.revive.why_off().is_some());
        let mut v2 = v.clone();
        let np = idx(&v, 0, "notes-pane");
        v2.hosts[0].agents[np].status = "ended".into();
        let adopted = agent_offers(&v2, 0, np);
        assert!(adopted.resume.why_off().unwrap().contains("adopted"));
        assert!(adopted.revive.why_off().unwrap().contains("adopted"));

        // phone is another host: resume by name@host, the rest off with why.
        let ui = agent_offers(&v, 2, idx(&v, 2, "phone-shell-ui"));
        assert!(ui.resume.why_off().unwrap().contains("not kimi"));
        let shell = agent_offers(&v, 2, idx(&v, 2, "phone-shell"));
        assert!(shell.cleanup.why_off().unwrap().contains("this host only"));
        assert!(shell.revive.why_off().unwrap().contains("phone"));
        assert_eq!(
            shell.logs.command().unwrap().line(),
            "agent logs phone-shell@phone --lines 40"
        );
        let mut v2 = v.clone();
        v2.hosts[2].agents[1].backend = "claude".into();
        v2.hosts[2].agents[1].resumable = true;
        let ui = agent_offers(&v2, 2, 1);
        assert_eq!(
            ui.resume.command().unwrap().line(),
            "agent resume phone-shell-ui@phone"
        );
    }

    #[test]
    fn cleanup_confirm_names_what_goes() {
        let v = v();
        let c = cleanup_confirm(&v, 0, idx(&v, 0, "lead-gui")).unwrap();
        assert!(c.body.contains("running now (processing)"), "{}", c.body);
        assert!(c.body.contains("agent-lead-gui"));
        assert!(
            c.body
                .contains("helpers are not cleaned up with it: lead-gui-review")
        );
        let b = cleanup_confirm(&v, 0, idx(&v, 0, "builder")).unwrap();
        assert!(b.body.contains("3 held messages") && !b.body.contains("running now"));
        let n = cleanup_confirm(&v, 0, idx(&v, 0, "notes-pane")).unwrap();
        assert!(n.body.contains("leaves its session running"));
        assert!(cleanup_confirm(&v, 2, 0).is_none(), "another host");
    }

    #[test]
    fn untrack_says_running_and_leaves_other_hosts_out() {
        let v = v();
        let ls = sessions::lines(&v, &Filter::default(), Sort::When);
        let running: Vec<SessionLine> = ls
            .iter()
            .filter(|l| l.row(&v).state == SessionState::Running)
            .copied()
            .collect();
        let c = untrack_confirm(&v, &running, "Stop tracking 1?").unwrap();
        assert!(c.body.contains("RUNNING now"), "{}", c.body);
        assert!(c.body.contains("on other hosts are left out"));
        assert_eq!(&c.command.args[..3], ["manage", "untrack", "--force"]);
        assert!(c.command.args[3..].iter().all(|k| k.contains(':')));

        let nr = not_running(&v, &ls);
        assert!(
            nr.iter().any(|l| l.host == 2),
            "other hosts are asked about too"
        );
        let c = untrack_confirm(&v, &nr, "x").unwrap();
        assert!(!c.body.contains("RUNNING"));
        assert!(c.body.contains("on other hosts are left out"));
        assert!(!c.command.args.contains(&"--force".to_string()));
        assert_eq!(
            c.command.args.len() - 2,
            nr.iter().filter(|l| l.host == 0).count()
        );

        // A parent alone takes its subagents along, and says so.
        let parent: Vec<SessionLine> = ls
            .iter()
            .filter(|l| l.host == 0 && l.row(&v).usage_with_subagents.is_some())
            .filter(|l| l.row(&v).state != SessionState::Running)
            .take(1)
            .copied()
            .collect();
        let c = untrack_confirm(&v, &parent, "x").unwrap();
        assert!(c.body.contains("subagent sessions with them"), "{}", c.body);

        let remote: Vec<SessionLine> = ls.iter().filter(|l| l.host == 2).copied().collect();
        assert!(
            untrack_confirm(&v, &remote, "x")
                .unwrap_err()
                .contains("this host only")
        );
    }

    #[test]
    fn track_takes_untracked_ones() {
        let v = v();
        let f = Filter {
            show_untracked: true,
            ..Filter::default()
        };
        let ls = sessions::lines(&v, &f, Sort::When);
        let Offer::Ready(c) = track(&v, &ls) else {
            panic!()
        };
        assert_eq!(c.args[1], "track");
        assert!(track(&v, &ls[..0]).why_off().is_some());
    }

    #[test]
    fn friction_resolve_needs_a_note_and_says_links() {
        let v = v();
        let i34 = v.hosts[0].friction.iter().position(|f| f.id == 34).unwrap();
        assert!(friction_resolve(&v, 0, i34, " ").why_off().is_some());
        let Offer::Ready(c) = friction_resolve(&v, 0, i34, "fixed in abc") else {
            panic!()
        };
        assert_eq!(c.line(), "agent friction resolve 34 --note 'fixed in abc'");
        let q = friction_confirm(&v, 0, i34, c).unwrap();
        assert!(q.body.contains("4@phone"));
        assert!(friction_resolve(&v, 2, 0, "x").why_off().is_some());
        let i21 = v.hosts[0].friction.iter().position(|f| f.id == 21).unwrap();
        assert!(
            friction_resolve(&v, 0, i21, "x")
                .why_off()
                .unwrap()
                .contains("already")
        );
    }

    #[test]
    fn prune_runs_dry_first() {
        assert_eq!(
            prune_dry_run(30).line(),
            "agent manage prune --older-than 30d --dry-run"
        );
        let c = prune_confirm(&v(), 30, "would delete 12 rows\n");
        assert!(c.body.contains("would delete 12 rows"));
        assert_eq!(c.command.line(), "agent manage prune --older-than 30d");
        assert_eq!(
            backup(&v()).command().unwrap().line(),
            "agent manage backup"
        );
    }
}
