//! [`Layout`]: which parts of the window are folded away. The sidebar, each page's detail
//! pane and each section inside a detail pane can be collapsed; the UI draws what this
//! says and calls it when the person folds or unfolds something, so the state is the
//! model's and not only the widgets'. Nothing here knows how wide the screen is: a narrow
//! UI may start with more folded.

use std::collections::BTreeSet;

use crate::state::Page;

/// A foldable section of a detail pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// An agent's fields: status, model, cwd, tokens.
    AgentDetail,
    /// An agent's recent logs.
    AgentLogs,
    /// A session's fields: where it ran, who owns it, its transcript.
    SessionDetail,
    /// A session's tokens, by kind and by model.
    SessionTokens,
    /// A friction entry's text.
    FrictionDetail,
    /// The note and button that resolve a friction entry.
    FrictionResolve,
    /// A history command's fields.
    HistoryDetail,
}

impl Section {
    pub const ALL: [Section; 7] = [
        Section::AgentDetail,
        Section::AgentLogs,
        Section::SessionDetail,
        Section::SessionTokens,
        Section::FrictionDetail,
        Section::FrictionResolve,
        Section::HistoryDetail,
    ];

    pub fn title(&self) -> &'static str {
        match self {
            Section::AgentDetail => "Agent",
            Section::AgentLogs => "Recent logs",
            Section::SessionDetail => "Session",
            Section::SessionTokens => "Tokens",
            Section::FrictionDetail => "Entry",
            Section::FrictionResolve => "Resolve",
            Section::HistoryDetail => "Command",
        }
    }

    /// The page whose detail pane holds it.
    pub fn page(&self) -> Page {
        match self {
            Section::AgentDetail | Section::AgentLogs => Page::Agents,
            Section::SessionDetail | Section::SessionTokens => Page::Sessions,
            Section::FrictionDetail | Section::FrictionResolve => Page::Friction,
            Section::HistoryDetail => Page::History,
        }
    }

    /// The header's text: an arrow that says whether it is open, and the title.
    pub fn header(&self, open: bool) -> String {
        format!("{} {}", if open { "▾" } else { "▸" }, self.title())
    }
}

/// What is folded away. The default has everything open.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layout {
    /// The sidebar shows only a narrow strip of initials.
    pub sidebar_collapsed: bool,
    /// The pages whose detail pane is folded away.
    detail_collapsed: BTreeSet<Page>,
    /// The sections folded to their header.
    folded: BTreeSet<Section>,
}

impl Layout {
    /// Whether a page has a list with a detail pane beside it (the Data page doesn't).
    pub fn has_detail(page: Page) -> bool {
        page != Page::Data
    }

    pub fn toggle_sidebar(&mut self) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
    }

    /// Whether the page's detail pane shows. A page without one never shows it.
    pub fn detail_open(&self, page: Page) -> bool {
        Layout::has_detail(page) && !self.detail_collapsed.contains(&page)
    }

    pub fn set_detail_open(&mut self, page: Page, open: bool) {
        if !Layout::has_detail(page) {
            return;
        }
        if open {
            self.detail_collapsed.remove(&page);
        } else {
            self.detail_collapsed.insert(page);
        }
    }

    pub fn toggle_detail(&mut self, page: Page) {
        let open = self.detail_open(page);
        self.set_detail_open(page, !open);
    }

    pub fn section_open(&self, s: Section) -> bool {
        !self.folded.contains(&s)
    }

    pub fn toggle_section(&mut self, s: Section) {
        if !self.folded.remove(&s) {
            self.folded.insert(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_starts_open() {
        let l = Layout::default();
        assert!(!l.sidebar_collapsed);
        for p in Page::ALL {
            assert_eq!(l.detail_open(p), p != Page::Data, "{p:?}");
        }
        for s in Section::ALL {
            assert!(l.section_open(s), "{s:?}");
        }
    }

    #[test]
    fn the_sidebar_folds_and_comes_back() {
        let mut l = Layout::default();
        l.toggle_sidebar();
        assert!(l.sidebar_collapsed);
        l.toggle_sidebar();
        assert!(!l.sidebar_collapsed);
    }

    #[test]
    fn each_page_folds_its_own_detail() {
        let mut l = Layout::default();
        l.toggle_detail(Page::Agents);
        assert!(!l.detail_open(Page::Agents));
        assert!(l.detail_open(Page::Sessions), "other pages keep theirs");
        l.set_detail_open(Page::Agents, false);
        assert!(!l.detail_open(Page::Agents), "folding twice stays folded");
        l.set_detail_open(Page::Agents, true);
        assert!(l.detail_open(Page::Agents));
    }

    #[test]
    fn the_data_page_has_no_detail_to_open() {
        let mut l = Layout::default();
        l.set_detail_open(Page::Data, true);
        l.toggle_detail(Page::Data);
        assert!(!l.detail_open(Page::Data));
        assert_eq!(l, Layout::default(), "nothing recorded for it");
    }

    #[test]
    fn sections_fold_one_at_a_time() {
        let mut l = Layout::default();
        l.toggle_section(Section::AgentLogs);
        assert!(!l.section_open(Section::AgentLogs));
        assert!(l.section_open(Section::AgentDetail));
        l.toggle_section(Section::AgentLogs);
        assert!(l.section_open(Section::AgentLogs));
    }

    #[test]
    fn every_section_belongs_to_a_page_with_a_detail_pane() {
        for s in Section::ALL {
            assert!(Layout::has_detail(s.page()), "{s:?}");
        }
        assert_eq!(Section::AgentLogs.header(true), "▾ Recent logs");
        assert_eq!(Section::AgentLogs.header(false), "▸ Recent logs");
    }
}
