//! [`Saved`]: the part of the manager's state that outlives the window — the page, each
//! list's order, filters and selection, what is folded, the splitters and the window — as a
//! plain JSON file, so a restart (by hand, or the live build relaunching itself after a
//! rebuild) comes back where it was.
//!
//! The UI reads its widgets into a [`Saved`] (with [`Saved::of`] for the model's part),
//! writes it when it has settled and at exit, and at start [`load`]s it and [`Saved::apply`]s
//! it. Everything is named by text (a page's label, a column's label, a section's name), so a
//! file from an older or newer build applies what it still knows and skips the rest; a file
//! that can't be read, or has another [`VERSION`], is ignored.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::path::{Path, PathBuf};

use agent_manage::SessionState;
use serde::{Deserialize, Serialize};

use crate::agents::AgentKey;
use crate::friction::FrictionKey;
use crate::history::HistoryKey;
use crate::layout::{Layout, Section};
use crate::sessions::{Filter, SessionKey};
use crate::sort::{Column, SortOrder};
use crate::state::{Model, Page, SortList};

/// The file's format. A file with another one is ignored.
pub const VERSION: u32 = 1;

/// The environment variable naming the file, over the platform's default.
pub const ENV: &str = "AGENT_MANAGER_STATE";

/// What survives a restart. Every field has a default, so a file missing some still loads.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Saved {
    pub version: u32,
    /// The page's label (`Agents`); one this build doesn't have is skipped.
    pub page: String,
    pub auto_refresh: Option<bool>,
    /// Each sorted list's keys, by the list's name (`agents`), first key first.
    pub sort: BTreeMap<String, Vec<SavedKey>>,
    pub filter: SavedFilter,
    pub history_text: String,
    pub history_failed_only: bool,

    pub selected_agent: Option<AgentKey>,
    pub selected_sessions: Vec<SessionKey>,
    pub focused_session: Option<SessionKey>,
    pub selected_friction: Option<FrictionKey>,
    pub selected_history: Option<HistoryKey>,

    pub sidebar_collapsed: bool,
    /// The pages whose detail pane is folded, by label.
    pub details_folded: Vec<String>,
    /// The folded detail sections, by name (`AgentLogs`).
    pub sections_folded: Vec<String>,
    /// The hosts folded on the Data page.
    pub data_folded: BTreeSet<String>,

    /// Each splitter's bar, by the page it splits (`Agents`).
    pub splits: BTreeMap<String, Split>,
    pub window: Option<WindowGeom>,
}

/// One key of a list's order: the column's label, and whether it is reversed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedKey {
    pub column: String,
    #[serde(default)]
    pub reversed: bool,
}

/// The sessions page's filter.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedFilter {
    pub text: String,
    pub host: Option<String>,
    pub harness: Option<String>,
    pub state: Option<SessionState>,
    pub stale_only: bool,
    pub show_untracked: bool,
}

/// Where a splitter's bar is, as Makepad's `SplitterAlign` says it: points from the first
/// pane's edge, points from the second's, or a share of the room.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    FromA(f64),
    FromB(f64),
    Weighted(f64),
}

/// The window's inner size and its top-left corner, in what the engine reports.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WindowGeom {
    pub width: f64,
    pub height: f64,
    pub x: f64,
    pub y: f64,
}

impl WindowGeom {
    /// Whether it is worth putting back: a window that isn't there yet reports zeros.
    pub fn usable(&self) -> bool {
        self.width >= 200.0 && self.height >= 150.0 && self.x.is_finite() && self.y.is_finite()
    }
}

/// A list's name in the file.
pub fn list_name(list: SortList) -> &'static str {
    match list {
        SortList::Agents => "agents",
        SortList::Sessions => "sessions",
        SortList::Friction => "friction",
        SortList::History => "history",
    }
}

/// A page's name in the file: its label.
pub fn page_name(page: Page) -> &'static str {
    page.label()
}

/// The page called `name`, if this build has it.
pub fn page_named(name: &str) -> Option<Page> {
    Page::ALL.into_iter().find(|p| p.label() == name)
}

fn name_of<T: Debug>(t: &T) -> String {
    format!("{t:?}")
}

fn named<T: Debug + Copy>(all: &[T], name: &str) -> Option<T> {
    all.iter().copied().find(|t| name_of(t) == name)
}

fn keys_of<C: Column>(order: &SortOrder<C>) -> Vec<SavedKey> {
    order
        .keys()
        .iter()
        .map(|k| SavedKey {
            column: k.column.label().to_string(),
            reversed: k.reversed,
        })
        .collect()
}

/// The order the saved keys say, skipping columns this build doesn't have.
fn order_of<C: Column>(keys: &[SavedKey]) -> SortOrder<C> {
    let mut order = SortOrder::default();
    for k in keys {
        let Some(&c) = C::ALL.iter().find(|c| c.label() == k.column) else {
            continue;
        };
        if order.find(c).is_some() {
            continue;
        }
        order.click(c, true);
        if k.reversed {
            order.click(c, true);
        }
    }
    order
}

impl Saved {
    /// The model's part: everything but the splitters, the window and the Data page's
    /// folds, which the UI holds and fills in.
    pub fn of(m: &Model) -> Saved {
        let mut sort = BTreeMap::new();
        for list in SortList::ALL {
            let keys = match list {
                SortList::Agents => keys_of(&m.agent_order),
                SortList::Sessions => keys_of(&m.session_order),
                SortList::Friction => keys_of(&m.friction_order),
                SortList::History => keys_of(&m.history_order),
            };
            if !keys.is_empty() {
                sort.insert(list_name(list).to_string(), keys);
            }
        }
        let f = &m.filter;
        Saved {
            version: VERSION,
            page: page_name(m.page).to_string(),
            auto_refresh: Some(m.auto_refresh),
            sort,
            filter: SavedFilter {
                text: f.text.clone(),
                host: f.host.clone(),
                harness: f.harness.clone(),
                state: f.state,
                stale_only: f.stale_only,
                show_untracked: f.show_untracked,
            },
            history_text: m.history_text.clone(),
            history_failed_only: m.history_failed_only,
            selected_agent: m.selected_agent.clone(),
            selected_sessions: m.selected_sessions.iter().cloned().collect(),
            focused_session: m.focused_session.clone(),
            selected_friction: m.selected_friction.clone(),
            selected_history: m.selected_history.clone(),
            sidebar_collapsed: m.layout.sidebar_collapsed,
            details_folded: Page::ALL
                .into_iter()
                .filter(|&p| Layout::has_detail(p) && !m.layout.detail_open(p))
                .map(|p| page_name(p).to_string())
                .collect(),
            sections_folded: Section::ALL
                .into_iter()
                .filter(|&s| !m.layout.section_open(s))
                .map(|s| name_of(&s))
                .collect(),
            data_folded: BTreeSet::new(),
            splits: BTreeMap::new(),
            window: None,
        }
    }

    /// Puts the saved state into a fresh model, before its first view. A selection names
    /// its row by key, so one whose row is gone is dropped when the view arrives
    /// ([`Model::set_view`]).
    pub fn apply(&self, m: &mut Model) {
        if let Some(p) = page_named(&self.page) {
            m.page = p;
        }
        if let Some(on) = self.auto_refresh {
            m.auto_refresh = on;
        }
        let keys = |list| {
            self.sort
                .get(list_name(list))
                .map(Vec::as_slice)
                .unwrap_or(&[])
        };
        m.agent_order = order_of(keys(SortList::Agents));
        m.session_order = order_of(keys(SortList::Sessions));
        m.friction_order = order_of(keys(SortList::Friction));
        m.history_order = order_of(keys(SortList::History));
        let f = &self.filter;
        m.filter = Filter {
            text: f.text.clone(),
            host: f.host.clone(),
            harness: f.harness.clone(),
            state: f.state,
            stale_only: f.stale_only,
            show_untracked: f.show_untracked,
        };
        m.history_text = self.history_text.clone();
        m.history_failed_only = self.history_failed_only;
        m.selected_agent = self.selected_agent.clone();
        m.selected_sessions = self.selected_sessions.iter().cloned().collect();
        m.focused_session = self.focused_session.clone();
        m.selected_friction = self.selected_friction.clone();
        m.selected_history = self.selected_history.clone();
        let mut l = Layout::default();
        l.sidebar_collapsed = self.sidebar_collapsed;
        for p in self.details_folded.iter().filter_map(|n| page_named(n)) {
            l.set_detail_open(p, false);
        }
        for s in self.sections_folded.iter() {
            if let Some(s) = named(&Section::ALL, s)
                && l.section_open(s)
            {
                l.toggle_section(s);
            }
        }
        m.layout = l;
    }
}

/// Where the state is kept: `$AGENT_MANAGER_STATE`, else `agent-manager/state.json` under
/// the platform's place for it (`~/Library/Application Support` on macOS, `$XDG_STATE_HOME`
/// or `~/.local/state` elsewhere). None when there is no home to put it in, and on a
/// fixture (`AGENT_MANAGER_FIXTURE`) unless the variable names a file, so screenshots and
/// tests neither read nor change the person's own.
pub fn path() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if var("AGENT_MANAGER_FIXTURE").is_some() && var(ENV).is_none() {
        return None;
    }
    path_from(
        var(ENV),
        var("XDG_STATE_HOME"),
        var("HOME"),
        cfg!(target_os = "macos"),
    )
}

fn path_from(
    explicit: Option<std::ffi::OsString>,
    xdg_state: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
    macos: bool,
) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(PathBuf::from(p));
    }
    let base = if macos {
        PathBuf::from(home?).join("Library/Application Support")
    } else {
        match xdg_state {
            Some(x) => PathBuf::from(x),
            None => PathBuf::from(home?).join(".local/state"),
        }
    };
    Some(base.join("agent-manager/state.json"))
}

/// The state in `text`, or None when it isn't this version's JSON.
pub fn parse(text: &str) -> Option<Saved> {
    let s: Saved = serde_json::from_str(text).ok()?;
    (s.version == VERSION).then_some(s)
}

/// The state in the file, or None when there is none or it can't be read.
pub fn load(path: &Path) -> Option<Saved> {
    parse(&std::fs::read_to_string(path).ok()?)
}

/// Writes the state, whole or not at all: to a file beside it, then renamed over it.
pub fn save(path: &Path, s: &Saved) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(s).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text + "\n")?;
    std::fs::rename(&tmp, path)
}

/// The environment variable naming the file whose appearance asks the window to relaunch
/// (set by the live build's launcher, `tools/live setup`).
pub const RELAUNCH_ENV: &str = "AGENT_MANAGER_RELAUNCH";

/// The exit status that asks the launcher to start the window again.
pub const RELAUNCH_EXIT: i32 = 75;

/// Whether a relaunch has been asked for: `$AGENT_MANAGER_RELAUNCH` names a file that
/// exists. The launcher removes it before each start.
pub fn relaunch_requested() -> bool {
    std::env::var_os(RELAUNCH_ENV)
        .filter(|v| !v.is_empty())
        .is_some_and(|p| Path::new(&p).exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;

    fn model() -> Model {
        let mut m = Model::new();
        m.set_view(
            fixture::view(fixture::default_now()),
            fixture::default_now(),
        );
        m
    }

    /// A model with something changed in every part the file keeps.
    fn changed() -> Model {
        let mut m = model();
        m.page = Page::Friction;
        m.auto_refresh = false;
        m.sort_click(SortList::Agents, 5, false);
        m.sort_click(SortList::Agents, 5, false); // reversed
        m.sort_click(SortList::Agents, 0, true);
        m.sort_click(SortList::Sessions, 2, false);
        m.filter.text = "build".into();
        m.filter.state = Some(SessionState::Dormant);
        m.filter.show_untracked = true;
        m.set_history_failed_only(true);
        m.refilter();
        let i = m
            .agent_lines
            .iter()
            .position(|l| matches!(l, crate::agents::AgentLine::Agent { .. }))
            .unwrap();
        let _ = m.select_agent_line(i);
        m.toggle_session_line(0);
        m.select_friction_line(1);
        m.layout.toggle_sidebar();
        m.layout.set_detail_open(Page::Sessions, false);
        m.layout.toggle_section(Section::AgentLogs);
        m
    }

    fn restored(s: &Saved) -> Model {
        let mut m = Model::new();
        s.apply(&mut m);
        m.set_view(
            fixture::view(fixture::default_now()),
            fixture::default_now(),
        );
        m
    }

    #[test]
    fn a_round_trip_through_the_file_restores_the_model() {
        let m = changed();
        let mut s = Saved::of(&m);
        s.splits.insert("Agents".into(), Split::FromB(310.0));
        s.data_folded.insert("laptop".into());
        s.window = Some(WindowGeom {
            width: 1300.0,
            height: 800.0,
            x: 40.0,
            y: 30.0,
        });
        let dir = std::env::temp_dir().join(format!("agent-manager-saved-{}", std::process::id()));
        let path = dir.join("sub/state.json");
        save(&path, &s).unwrap();
        let back = load(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(back, s);

        let r = restored(&back);
        assert_eq!(r.page, Page::Friction);
        assert!(!r.auto_refresh);
        assert_eq!(r.agent_order, m.agent_order);
        assert_eq!(r.session_order, m.session_order);
        assert_eq!(
            r.order(SortList::Agents).words(),
            m.order(SortList::Agents).words()
        );
        assert_eq!(r.filter, m.filter);
        assert!(r.history_failed_only);
        assert!(r.selected_agent.is_some());
        assert_eq!(r.selected_agent, m.selected_agent);
        assert_eq!(r.selected_sessions, m.selected_sessions);
        assert_eq!(r.focused_session, m.focused_session);
        assert_eq!(r.selected_friction, m.selected_friction);
        assert_eq!(r.layout, m.layout);
        assert_eq!(r.session_lines, m.session_lines, "same filter and order");
        assert_eq!(Saved::of(&r), Saved::of(&m));
    }

    #[test]
    fn a_fresh_model_saves_nothing_unusual() {
        let s = Saved::of(&Model::new());
        let mut m = Model::new();
        s.apply(&mut m);
        assert_eq!(Saved::of(&m), s);
        assert!(s.sort.is_empty() && s.details_folded.is_empty());
    }

    #[test]
    fn another_version_or_a_broken_file_is_ignored() {
        let mut s = Saved::of(&changed());
        assert!(parse(&serde_json::to_string(&s).unwrap()).is_some());
        s.version = VERSION + 1;
        assert!(parse(&serde_json::to_string(&s).unwrap()).is_none());
        assert!(parse(r#"{"page": "Agents"}"#).is_none(), "no version");
        assert!(parse("{not json").is_none());
        assert!(parse(r#"{"version": 1, "page": 7}"#).is_none());
        assert!(load(Path::new("/nonexistent/agent-manager/state.json")).is_none());
    }

    #[test]
    fn what_this_build_doesnt_know_is_skipped() {
        let s = parse(
            r#"{"version": 1, "page": "Elsewhere", "unknown_field": [1, 2],
                "sort": {"agents": [{"column": "gone"}, {"column": "tokens", "reversed": true}],
                         "elsewhere": [{"column": "x"}]},
                "details_folded": ["Elsewhere", "Agents"],
                "sections_folded": ["ElsewhereChart", "AgentLogs"]}"#,
        )
        .unwrap();
        let mut m = Model::new();
        s.apply(&mut m);
        assert_eq!(m.page, Page::Agents, "an unknown page leaves the default");
        assert_eq!(m.order(SortList::Agents).key_words().len(), 1);
        assert!(m.order(SortList::Agents).sorts_by(5));
        assert!(!m.layout.detail_open(Page::Agents));
        assert!(!m.layout.section_open(Section::AgentLogs));
        assert!(m.layout.section_open(Section::AgentDetail));
    }

    #[test]
    fn a_selection_whose_row_is_gone_is_dropped() {
        let mut s = Saved::of(&changed());
        s.selected_agent = Some(AgentKey {
            host: "hub".into(),
            name: "no-such-agent".into(),
        });
        s.selected_friction = Some(FrictionKey {
            host: "nowhere".into(),
            id: 1,
        });
        let gone = SessionKey {
            host: "hub".into(),
            key: "claude:gone".into(),
        };
        s.selected_sessions.push(gone.clone());
        s.focused_session = Some(gone.clone());
        let r = restored(&s);
        assert_eq!(r.selected_agent, None);
        assert_eq!(r.selected_friction, None);
        assert_eq!(r.focused_session, None);
        assert!(!r.selected_sessions.contains(&gone));
        assert_eq!(r.selected_sessions.len(), 1, "the one still there stays");
    }

    #[test]
    fn every_page_round_trips_by_name() {
        for p in Page::ALL {
            assert_eq!(page_named(page_name(p)), Some(p));
        }
        for s in Section::ALL {
            assert_eq!(named(&Section::ALL, &name_of(&s)), Some(s));
        }
    }

    #[test]
    fn the_file_goes_where_the_platform_keeps_state() {
        let os = |s: &str| Some(std::ffi::OsString::from(s));
        assert_eq!(
            path_from(os("/x/s.json"), os("/xdg"), os("/home/user"), false),
            Some(PathBuf::from("/x/s.json"))
        );
        assert_eq!(
            path_from(None, os("/xdg"), os("/home/user"), false),
            Some(PathBuf::from("/xdg/agent-manager/state.json"))
        );
        assert_eq!(
            path_from(None, None, os("/home/user"), false),
            Some(PathBuf::from(
                "/home/user/.local/state/agent-manager/state.json"
            ))
        );
        assert_eq!(
            path_from(None, os("/xdg"), os("/Users/user"), true),
            Some(PathBuf::from(
                "/Users/user/Library/Application Support/agent-manager/state.json"
            ))
        );
        assert_eq!(path_from(None, None, None, false), None);
    }

    #[test]
    fn a_window_not_there_yet_is_not_put_back() {
        let w = WindowGeom {
            width: 0.0,
            height: 0.0,
            x: 0.0,
            y: 0.0,
        };
        assert!(!w.usable());
        assert!(
            WindowGeom {
                width: 1100.0,
                height: 700.0,
                ..w
            }
            .usable()
        );
    }
}
