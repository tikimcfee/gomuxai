//! [`Model`]: the manager's whole state, and what each thing a person does leads to.
//!
//! A UI draws from the model's fields and derived lines, calls a method when the person
//! does something, and carries out the [`Effect`] it returns: run a [`Job`] off the UI
//! thread (handing its [`Output`] back to [`Model::finished`]), or refresh the view.

use std::collections::BTreeSet;

use agent_manage::View;
use chrono::{DateTime, Local, Utc};

use crate::agents::{self, AgentKey, AgentLine};
use crate::commands::{self, Command, Confirm, Offer};
use crate::friction::{self, FrictionKey, FrictionLine};
use crate::history::{self, HistoryKey, HistoryLine};
use crate::layout::Layout;
use crate::runner::Output;
use crate::sessions::{self, Filter, SessionKey, SessionLine, Totals};
use crate::sort::Orders;

/// A list a person can sort by its columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortList {
    Agents,
    Sessions,
    Friction,
    History,
}

impl SortList {
    pub const ALL: [SortList; 4] = [
        SortList::Agents,
        SortList::Sessions,
        SortList::Friction,
        SortList::History,
    ];
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Page {
    #[default]
    Agents,
    Sessions,
    Friction,
    History,
    Data,
    Usage,
}

impl Page {
    pub const ALL: [Page; 6] = [
        Page::Agents,
        Page::Sessions,
        Page::Friction,
        Page::History,
        Page::Data,
        Page::Usage,
    ];
    pub fn label(&self) -> &'static str {
        match self {
            Page::Agents => "Agents",
            Page::Sessions => "Sessions",
            Page::Friction => "Friction",
            Page::History => "History",
            Page::Data => "Data",
            Page::Usage => "Usage",
        }
    }
}

/// Why a command runs, which decides what its output leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    /// An action: its output is shown, then the view is read again.
    Action,
    /// An agent's recent logs, for its detail.
    Logs(AgentKey),
    /// The dry run before a prune: its output goes into the confirmation.
    PruneDryRun(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub purpose: Purpose,
    pub command: Command,
}

/// What the UI does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Run(Job),
    Refresh,
}

/// What the dialog shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dialog {
    /// A question before a destructive command.
    Confirm(Confirm),
    /// What an action printed and how it ended.
    Result(Output),
}

/// Auto refresh's period.
pub const AUTO_REFRESH_SECS: f64 = 10.0;

#[derive(Debug, Clone, Default)]
pub struct Model {
    pub view: View,
    /// When the view was last read (by this app's clock).
    pub read_at: Option<DateTime<Utc>>,
    /// Why the last read failed; the last good view stays.
    pub load_error: Option<String>,
    pub refreshing: bool,
    pub auto_refresh: bool,
    pub page: Page,
    /// The action running now; one at a time.
    pub running: Option<Command>,
    /// The last action's one-line result.
    pub last_action: Option<String>,
    pub dialog: Option<Dialog>,

    pub agent_order: agents::Order,
    pub agent_lines: Vec<AgentLine>,
    pub selected_agent: Option<AgentKey>,
    /// The selected agent's logs, once read.
    pub agent_logs: Option<(AgentKey, String)>,

    pub filter: Filter,
    pub session_order: sessions::Order,
    pub session_lines: Vec<SessionLine>,
    pub totals: Totals,
    pub selected_sessions: BTreeSet<SessionKey>,
    /// The session whose detail shows: the last one clicked.
    pub focused_session: Option<SessionKey>,

    pub friction_order: friction::Order,
    pub friction_lines: Vec<FrictionLine>,
    pub selected_friction: Option<FrictionKey>,

    pub history_text: String,
    pub history_failed_only: bool,
    pub history_order: history::Order,
    pub history_lines: Vec<HistoryLine>,
    /// The command whose detail shows.
    pub selected_history: Option<HistoryKey>,

    /// The prune field, in days.
    pub prune_days: String,

    /// What is folded away: the sidebar, detail panes, sections.
    pub layout: Layout,
}

impl Model {
    pub fn new() -> Model {
        Model {
            auto_refresh: true,
            prune_days: "30".to_string(),
            ..Model::default()
        }
    }

    // --- the view --------------------------------------------------------------------

    pub fn set_view(&mut self, view: View, now: DateTime<Utc>) {
        self.view = view;
        self.read_at = Some(now);
        self.load_error = None;
        self.refreshing = false;
        self.agent_lines = agents::lines(&self.view, &self.agent_order);
        self.friction_lines = friction::lines(&self.view, &self.friction_order);
        // Selections name rows that may have gone.
        let keys: BTreeSet<SessionKey> = self
            .view
            .hosts
            .iter()
            .flat_map(|h| {
                h.sessions.iter().map(|s| SessionKey {
                    host: h.host.clone(),
                    key: s.key(),
                })
            })
            .collect();
        self.selected_sessions.retain(|k| keys.contains(k));
        if self
            .focused_session
            .as_ref()
            .is_some_and(|k| !keys.contains(k))
        {
            self.focused_session = None;
        }
        if let Some(k) = &self.selected_agent
            && agents::find(&self.view, k).is_none()
        {
            self.selected_agent = None;
        }
        if let Some(k) = &self.selected_friction
            && friction::find(&self.view, k).is_none()
        {
            self.selected_friction = None;
        }
        if let Some(k) = &self.selected_history
            && history::find(&self.view, k).is_none()
        {
            self.selected_history = None;
        }
        // A filter naming a host or harness that's gone shows nothing; keep it anyway,
        // so a host that's briefly unreachable comes back filtered as it was.
        self.refilter();
    }

    pub fn set_load_error(&mut self, error: String) {
        self.load_error = Some(error);
        self.refreshing = false;
    }

    /// Starts a refresh unless one is running.
    pub fn begin_refresh(&mut self) -> bool {
        if self.refreshing {
            return false;
        }
        self.refreshing = true;
        true
    }

    pub fn refilter(&mut self) {
        self.session_lines = sessions::lines(&self.view, &self.filter, &self.session_order);
        self.totals = sessions::totals(&self.view, &self.session_lines);
        self.rehistory();
    }

    fn rehistory(&mut self) {
        self.history_lines = history::lines(
            &self.view,
            &self.history_text,
            self.history_failed_only,
            &self.history_order,
        );
    }

    // --- sorting ---------------------------------------------------------------------

    /// A list's order, by column index (the header's left to right).
    pub fn order(&self, list: SortList) -> &dyn Orders {
        match list {
            SortList::Agents => &self.agent_order,
            SortList::Sessions => &self.session_order,
            SortList::Friction => &self.friction_order,
            SortList::History => &self.history_order,
        }
    }

    fn order_mut(&mut self, list: SortList) -> &mut dyn Orders {
        match list {
            SortList::Agents => &mut self.agent_order,
            SortList::Sessions => &mut self.session_order,
            SortList::Friction => &mut self.friction_order,
            SortList::History => &mut self.history_order,
        }
    }

    /// Header `column` of `list` was clicked; `add` (the "add columns" switch, or a
    /// modifier) adds it as the next key instead of sorting by it alone.
    pub fn sort_click(&mut self, list: SortList, column: usize, add: bool) {
        self.order_mut(list).click(column, add);
        self.resort(list);
    }

    /// Takes key `i` out of `list`'s order.
    pub fn sort_remove(&mut self, list: SortList, i: usize) {
        self.order_mut(list).remove_at(i);
        self.resort(list);
    }

    /// Back to `list`'s default order.
    pub fn sort_reset(&mut self, list: SortList) {
        self.order_mut(list).reset();
        self.resort(list);
    }

    /// Orders `list`'s lines again; only when the view or its order changes.
    fn resort(&mut self, list: SortList) {
        match list {
            SortList::Agents => self.agent_lines = agents::lines(&self.view, &self.agent_order),
            SortList::Sessions => {
                self.session_lines = sessions::lines(&self.view, &self.filter, &self.session_order)
            }
            SortList::Friction => {
                self.friction_lines = friction::lines(&self.view, &self.friction_order)
            }
            SortList::History => self.rehistory(),
        }
    }

    /// The host names, for the host filter.
    pub fn host_names(&self) -> Vec<String> {
        self.view.hosts.iter().map(|h| h.host.clone()).collect()
    }

    // --- agents ----------------------------------------------------------------------

    /// The selected agent's (host, index).
    pub fn agent(&self) -> Option<(usize, usize)> {
        agents::find(&self.view, self.selected_agent.as_ref()?)
    }

    /// Selects the agent on line `i` and reads its logs.
    pub fn select_agent_line(&mut self, i: usize) -> Effect {
        let Some(AgentLine::Agent { host, agent, .. }) = self.agent_lines.get(i).cloned() else {
            return Effect::None;
        };
        let key = AgentKey {
            host: self.view.hosts[host].host.clone(),
            name: self.view.hosts[host].agents[agent].name.clone(),
        };
        self.selected_agent = Some(key.clone());
        if self.agent_logs.as_ref().is_some_and(|(k, _)| *k != key) {
            self.agent_logs = None;
        }
        match commands::agent_offers(&self.view, host, agent).logs {
            Offer::Ready(command) => Effect::Run(Job {
                purpose: Purpose::Logs(key),
                command,
            }),
            Offer::Off(why) => {
                self.agent_logs = Some((key, why));
                Effect::None
            }
        }
    }

    /// Reads the selected agent's logs when nothing has (a selection restored at start).
    pub fn load_selected_logs(&mut self) -> Effect {
        let Some(key) = self.selected_agent.clone() else {
            return Effect::None;
        };
        if self.agent_logs.as_ref().is_some_and(|(k, _)| *k == key) {
            return Effect::None;
        }
        let Some((h, a)) = self.agent() else {
            return Effect::None;
        };
        let line = self.agent_lines.iter().position(
            |l| matches!(l, AgentLine::Agent { host, agent, .. } if (*host, *agent) == (h, a)),
        );
        match line {
            Some(i) => self.select_agent_line(i),
            None => Effect::None,
        }
    }

    pub fn agent_offers(&self) -> Option<commands::AgentOffers> {
        let (h, a) = self.agent()?;
        Some(commands::agent_offers(&self.view, h, a))
    }

    pub fn resume(&mut self) -> Effect {
        let Some(o) = self.agent_offers() else {
            return Effect::None;
        };
        self.action(o.resume)
    }

    pub fn revive(&mut self) -> Effect {
        let Some(o) = self.agent_offers() else {
            return Effect::None;
        };
        self.action(o.revive)
    }

    pub fn cleanup(&mut self) -> Effect {
        if let Some((h, a)) = self.agent()
            && let Some(c) = commands::cleanup_confirm(&self.view, h, a)
        {
            self.dialog = Some(Dialog::Confirm(c));
        }
        Effect::None
    }

    // --- sessions --------------------------------------------------------------------

    pub fn set_filter_text(&mut self, text: &str) {
        self.filter.text = text.to_string();
        self.refilter();
    }

    /// Toggles line `i` in the selection and shows its detail.
    pub fn toggle_session_line(&mut self, i: usize) {
        let Some(l) = self.session_lines.get(i) else {
            return;
        };
        let key = l.key(&self.view);
        if !self.selected_sessions.remove(&key) {
            self.selected_sessions.insert(key.clone());
        }
        self.focused_session = Some(key);
    }

    pub fn is_selected(&self, l: &SessionLine) -> bool {
        self.selected_sessions.contains(&l.key(&self.view))
    }

    pub fn select_all_shown(&mut self) {
        for l in &self.session_lines {
            self.selected_sessions.insert(l.key(&self.view));
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected_sessions.clear();
    }

    /// The selected sessions, shown or not, as lines.
    pub fn selected_lines(&self) -> Vec<SessionLine> {
        let mut out = Vec::new();
        for (hi, h) in self.view.hosts.iter().enumerate() {
            for (i, s) in h.sessions.iter().enumerate() {
                let k = SessionKey {
                    host: h.host.clone(),
                    key: s.key(),
                };
                if self.selected_sessions.contains(&k) {
                    out.push(SessionLine {
                        host: hi,
                        index: i,
                        depth: usize::from(!s.parent_session.is_empty()),
                        subagents: 0,
                    });
                }
            }
        }
        out
    }

    pub fn focused_line(&self) -> Option<SessionLine> {
        let k = self.focused_session.as_ref()?;
        let hi = self.view.hosts.iter().position(|h| h.host == k.host)?;
        let i = self.view.hosts[hi]
            .sessions
            .iter()
            .position(|s| s.key() == k.key)?;
        Some(SessionLine {
            host: hi,
            index: i,
            depth: 0,
            subagents: 0,
        })
    }

    pub fn untrack_selected(&mut self) -> Effect {
        let lines = self.selected_lines();
        let n = lines.len();
        match commands::untrack_confirm(
            &self.view,
            &lines,
            &format!("Stop tracking the {n} selected?"),
        ) {
            Ok(c) => self.dialog = Some(Dialog::Confirm(c)),
            Err(why) => self.last_action = Some(why),
        }
        Effect::None
    }

    /// How many "stop tracking all shown that aren't running" would take: this host's.
    pub fn not_running_count(&self) -> usize {
        commands::not_running(&self.view, &self.session_lines)
            .iter()
            .filter(|l| crate::is_local(&self.view, l.host))
            .count()
    }

    pub fn untrack_not_running(&mut self) -> Effect {
        let lines = commands::not_running(&self.view, &self.session_lines);
        let n = self.not_running_count();
        match commands::untrack_confirm(
            &self.view,
            &lines,
            &format!("Stop tracking all {n} shown that aren't running?"),
        ) {
            Ok(c) => self.dialog = Some(Dialog::Confirm(c)),
            Err(why) => self.last_action = Some(why),
        }
        Effect::None
    }

    pub fn track_selected(&mut self) -> Effect {
        let lines = self.selected_lines();
        self.action(commands::track(&self.view, &lines))
    }

    // --- friction --------------------------------------------------------------------

    pub fn select_friction_line(&mut self, i: usize) {
        if let Some(FrictionLine::Entry { host, index }) = self.friction_lines.get(i) {
            self.selected_friction = Some(FrictionKey {
                host: self.view.hosts[*host].host.clone(),
                id: self.view.hosts[*host].friction[*index].id,
            });
        }
    }

    pub fn friction(&self) -> Option<(usize, usize)> {
        friction::find(&self.view, self.selected_friction.as_ref()?)
    }

    pub fn resolve_offer(&self, note: &str) -> Offer {
        match self.friction() {
            Some((h, i)) => commands::friction_resolve(&self.view, h, i, note),
            None => Offer::Off("select an entry".to_string()),
        }
    }

    pub fn resolve(&mut self, note: &str) -> Effect {
        let Some((h, i)) = self.friction() else {
            return Effect::None;
        };
        match commands::friction_resolve(&self.view, h, i, note) {
            Offer::Ready(c) => match commands::friction_confirm(&self.view, h, i, c.clone()) {
                Some(q) => {
                    self.dialog = Some(Dialog::Confirm(q));
                    Effect::None
                }
                None => self.action(Offer::Ready(c)),
            },
            off => self.action(off),
        }
    }

    // --- history ---------------------------------------------------------------------

    pub fn set_history_text(&mut self, text: &str) {
        self.history_text = text.to_string();
        self.rehistory();
    }

    pub fn set_history_failed_only(&mut self, on: bool) {
        self.history_failed_only = on;
        self.set_history_text(&self.history_text.clone());
    }

    /// Shows a command's detail; clicking the shown one again hides it.
    pub fn select_history_line(&mut self, i: usize) {
        let Some(l) = self.history_lines.get(i) else {
            return;
        };
        let k = history::key(&self.view, l);
        if self.selected_history.as_ref() == Some(&k) {
            self.selected_history = None;
        } else {
            self.selected_history = Some(k);
        }
    }

    /// The command whose detail shows, if it is still in the view.
    pub fn history_line(&self) -> Option<HistoryLine> {
        history::find(&self.view, self.selected_history.as_ref()?)
    }

    // --- data ------------------------------------------------------------------------

    pub fn backup(&mut self) -> Effect {
        let offer = commands::backup(&self.view);
        self.action(offer)
    }

    /// Runs the dry run of a prune; its output becomes the confirmation.
    pub fn prune(&mut self) -> Effect {
        if let Offer::Off(why) = commands::backup(&self.view) {
            self.last_action = Some(why.replace("backup", "prune"));
            return Effect::None;
        }
        match crate::data::parse_days(&self.prune_days) {
            Ok(days) => self.start(Job {
                purpose: Purpose::PruneDryRun(days),
                command: commands::prune_dry_run(days),
            }),
            Err(why) => {
                self.last_action = Some(why);
                Effect::None
            }
        }
    }

    // --- running ---------------------------------------------------------------------

    fn action(&mut self, offer: Offer) -> Effect {
        match offer {
            Offer::Ready(command) => self.start(Job {
                purpose: Purpose::Action,
                command,
            }),
            Offer::Off(why) => {
                self.last_action = Some(why);
                Effect::None
            }
        }
    }

    fn start(&mut self, job: Job) -> Effect {
        if let Some(r) = &self.running {
            self.last_action = Some(format!("wait: {} is still running", r.line()));
            return Effect::None;
        }
        self.running = Some(job.command.clone());
        Effect::Run(job)
    }

    /// The person answered yes to the dialog's question.
    pub fn confirmed(&mut self) -> Effect {
        match self.dialog.take() {
            Some(Dialog::Confirm(c)) => self.action(Offer::Ready(c.command)),
            _ => Effect::None,
        }
    }

    pub fn dismiss(&mut self) {
        self.dialog = None;
    }

    /// A job finished. Logs fill the agent's detail; an action's output is shown and the
    /// view is read again; a prune's dry run becomes its confirmation.
    pub fn finished(&mut self, job: Job, out: Output) -> Effect {
        match job.purpose {
            Purpose::Logs(key) => {
                self.agent_logs = Some((key, out.text()));
                Effect::None
            }
            Purpose::Action => {
                self.running = None;
                self.last_action = Some(out.summary());
                let ran = out.error.is_empty();
                self.dialog = Some(Dialog::Result(out));
                if ran { Effect::Refresh } else { Effect::None }
            }
            Purpose::PruneDryRun(days) => {
                self.running = None;
                self.last_action = Some(out.summary());
                if out.ok() {
                    self.dialog = Some(Dialog::Confirm(commands::prune_confirm(
                        &self.view,
                        days,
                        &out.stdout,
                    )));
                } else {
                    self.dialog = Some(Dialog::Result(out));
                }
                Effect::None
            }
        }
    }

    // --- the status line -------------------------------------------------------------

    pub fn status_line(&self, now: DateTime<Utc>) -> String {
        let mut parts = Vec::new();
        match self.read_at {
            Some(t) => {
                let ago = (now - t).num_seconds().max(0);
                parts.push(format!(
                    "read {} ({}s ago)",
                    t.with_timezone(&Local).format("%H:%M:%S"),
                    ago
                ));
            }
            None if self.load_error.is_none() => parts.push("reading…".to_string()),
            None => {}
        }
        if self.refreshing && self.read_at.is_some() {
            parts.push("refreshing…".to_string());
        }
        if let Some(e) = &self.load_error {
            parts.push(format!("read failed: {}", crate::clip(e, 160)));
        }
        let down: Vec<&str> = self
            .view
            .hosts
            .iter()
            .filter(|h| !h.reachable)
            .map(|h| h.host.as_str())
            .collect();
        if !down.is_empty() {
            parts.push(format!("unreachable: {}", down.join(", ")));
        }
        let pending: Vec<String> = self
            .view
            .hosts
            .iter()
            .filter(|h| h.usage_pending > 0)
            .map(|h| format!("{} on {}", h.usage_pending, h.host))
            .collect();
        if !pending.is_empty() {
            parts.push(format!("tokens not counted yet: {}", pending.join(", ")));
        }
        if let Some(r) = &self.running {
            parts.push(format!("running {}", r.line()));
        } else if let Some(a) = &self.last_action {
            parts.push(format!("last: {}", crate::clip(a, 120)));
        }
        parts.join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use agent_manage::SessionState;

    fn model() -> Model {
        let mut m = Model::new();
        m.set_view(
            fixture::view(fixture::default_now()),
            fixture::default_now(),
        );
        m
    }

    fn line_of(m: &Model, name: &str) -> usize {
        m.agent_lines
            .iter()
            .position(|l| match l {
                AgentLine::Agent { host, agent, .. } => {
                    m.view.hosts[*host].agents[*agent].name == name
                }
                _ => false,
            })
            .unwrap()
    }

    fn ok(command: &Command, stdout: &str) -> Output {
        Output {
            command: command.clone(),
            code: Some(0),
            stdout: stdout.to_string(),
            stderr: String::new(),
            elapsed_ms: 5,
            error: String::new(),
        }
    }

    #[test]
    fn selecting_an_agent_reads_its_logs_and_cleanup_asks() {
        let mut m = model();
        let i = line_of(&m, "builder");
        let Effect::Run(job) = m.select_agent_line(i) else {
            panic!()
        };
        assert_eq!(job.command.line(), "agent logs builder --lines 40");
        assert_eq!(
            m.finished(job.clone(), ok(&job.command, "log line\n")),
            Effect::None
        );
        assert_eq!(m.agent_logs.as_ref().unwrap().1, "log line\n");
        assert!(
            m.running.is_none(),
            "logs don't count as the running action"
        );

        assert_eq!(m.cleanup(), Effect::None);
        let Some(Dialog::Confirm(c)) = &m.dialog else {
            panic!()
        };
        assert_eq!(c.command.line(), "agent cleanup builder");
        let Effect::Run(job) = m.confirmed() else {
            panic!()
        };
        assert!(m.dialog.is_none());
        assert_eq!(
            m.finished(job.clone(), ok(&job.command, "cleaned up")),
            Effect::Refresh
        );
        assert!(matches!(m.dialog, Some(Dialog::Result(_))));
        assert!(
            m.status_line(fixture::default_now())
                .contains("last: agent cleanup builder → exit 0")
        );
    }

    #[test]
    fn one_action_at_a_time_and_off_says_why() {
        let mut m = model();
        m.select_agent_line(line_of(&m, "lead-docs"));
        let Effect::Run(_) = m.resume() else { panic!() };
        assert_eq!(m.revive(), Effect::None);
        assert!(m.last_action.as_ref().unwrap().starts_with("wait:"));
        let mut m = model();
        m.select_agent_line(line_of(&m, "lead"));
        assert_eq!(m.resume(), Effect::None);
        assert_eq!(
            m.last_action.as_deref(),
            Some("it is idle: nothing to resume")
        );
    }

    #[test]
    fn selection_survives_a_reload_and_drops_gone_rows() {
        let mut m = model();
        m.toggle_session_line(0);
        m.toggle_session_line(1);
        m.toggle_session_line(1);
        assert_eq!(m.selected_sessions.len(), 1);
        let kept = m.selected_sessions.iter().next().unwrap().clone();
        let mut v = m.view.clone();
        m.set_view(v.clone(), fixture::default_now());
        assert!(m.selected_sessions.contains(&kept));
        for h in &mut v.hosts {
            h.sessions.retain(|s| s.key() != kept.key);
        }
        m.set_view(v, fixture::default_now());
        assert!(m.selected_sessions.is_empty());
    }

    #[test]
    fn untrack_not_running_asks_with_the_count() {
        let mut m = model();
        m.filter.state = Some(SessionState::Dormant);
        m.refilter();
        let n = m.not_running_count();
        assert!(n > 0);
        m.untrack_not_running();
        let Some(Dialog::Confirm(c)) = &m.dialog else {
            panic!()
        };
        assert_eq!(
            c.title,
            format!("Stop tracking all {n} shown that aren't running?")
        );
    }

    #[test]
    fn prune_dry_run_then_confirm() {
        let mut m = model();
        m.prune_days = "nope".into();
        assert_eq!(m.prune(), Effect::None);
        m.prune_days = "45".into();
        let Effect::Run(job) = m.prune() else {
            panic!()
        };
        assert_eq!(
            job.command.line(),
            "agent manage prune --older-than 45d --dry-run"
        );
        m.finished(
            job.clone(),
            ok(&job.command, "would delete 812 history rows\n"),
        );
        let Some(Dialog::Confirm(c)) = &m.dialog else {
            panic!()
        };
        assert!(c.body.contains("812 history rows"));
        let Effect::Run(job) = m.confirmed() else {
            panic!()
        };
        assert_eq!(job.command.line(), "agent manage prune --older-than 45d");
    }

    #[test]
    fn status_line_names_unreachable_and_pending() {
        let m = model();
        let s = m.status_line(fixture::default_now() + chrono::Duration::seconds(12));
        assert!(s.contains("(12s ago)"), "{s}");
        assert!(s.contains("unreachable: laptop"));
        assert!(s.contains("tokens not counted yet: 14 on phone"));
    }

    #[test]
    fn sorting_by_header_survives_a_reload_and_resets() {
        let mut m = model();
        let default_lines = m.session_lines.clone();
        // Column 9 is tokens; 4 is state.
        m.sort_click(SortList::Sessions, 9, false);
        assert_eq!(m.order(SortList::Sessions).header(9), "↓ tokens Σ+subs");
        assert_ne!(m.session_lines, default_lines);
        m.sort_click(SortList::Sessions, 4, true);
        assert_eq!(
            m.order(SortList::Sessions).words(),
            "tokens Σ+subs largest first, then state running first"
        );
        assert_eq!(m.order(SortList::Sessions).header(4), "2↑ state");
        let sorted = m.session_lines.clone();
        m.set_view(m.view.clone(), fixture::default_now());
        assert_eq!(m.session_lines, sorted);
        m.sort_remove(SortList::Sessions, 0);
        assert_eq!(m.order(SortList::Sessions).words(), "state running first");
        m.sort_reset(SortList::Sessions);
        assert!(m.order(SortList::Sessions).is_default());
        assert_eq!(m.session_lines, default_lines);
        // Each list keeps its own order.
        m.sort_click(SortList::History, 4, false);
        m.sort_click(SortList::Agents, 1, false);
        m.sort_click(SortList::Friction, 0, false);
        assert!(m.order(SortList::Sessions).is_default());
        for l in SortList::ALL {
            assert_eq!(m.order(l).is_default(), l == SortList::Sessions);
        }
        let first = m.history_lines[0];
        assert_ne!(m.view.hosts[first.host].history[first.index].exit_code, 0);
    }

    #[test]
    fn friction_resolve_with_links_asks() {
        let mut m = model();
        let i = m
            .friction_lines
            .iter()
            .position(|l| matches!(l, FrictionLine::Entry { host: 0, index } if m.view.hosts[0].friction[*index].id == 34))
            .unwrap();
        m.select_friction_line(i);
        assert!(m.resolve_offer("").why_off().is_some());
        assert_eq!(m.resolve("fixed"), Effect::None);
        assert!(matches!(m.dialog, Some(Dialog::Confirm(_))));
    }

    #[test]
    fn history_selection_toggles_and_survives_a_refresh() {
        let mut m = model();
        assert_eq!(m.history_line(), None);
        m.select_history_line(2);
        let l = m.history_line().unwrap();
        assert_eq!(l, m.history_lines[2]);
        m.set_view(
            fixture::view(fixture::default_now()),
            fixture::default_now(),
        );
        assert_eq!(m.history_line(), Some(l), "the same command after a read");
        m.select_history_line(2);
        assert_eq!(m.history_line(), None, "clicking it again hides it");
    }

    #[test]
    fn layout_is_part_of_the_model() {
        let mut m = model();
        m.layout.toggle_detail(Page::Friction);
        m.set_view(
            fixture::view(fixture::default_now()),
            fixture::default_now(),
        );
        assert!(
            !m.layout.detail_open(Page::Friction),
            "a read leaves it alone"
        );
    }
}
