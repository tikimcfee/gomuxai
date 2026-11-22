//! The agent manager's model: everything a manager UI shows and does, with no UI in it.
//!
//! - [`Model`] holds the last [`View`] read from `agent manage --json`, the filters, the
//!   selections and the last action's result, and derives the lines each page draws.
//! - [`agents`], [`sessions`], [`friction`], [`history`] and [`data`] turn a [`View`] into
//!   rows of display strings, without assuming how wide the screen is; each list's
//!   columns sort it through a [`sort::SortOrder`].
//! - [`commands`] decides which actions a row offers (or why not) and the `agent` command
//!   line each runs, and what a confirmation names.
//! - [`runner`] runs the `agent` binary (or reads a fixture) off the UI thread.
//! - [`fixture`] builds a realistic [`View`] for tests and screenshots.

pub mod agents;
pub mod commands;
pub mod data;
pub mod fixture;
pub mod friction;
pub mod history;
pub mod layout;
pub mod runner;
pub mod sessions;
pub mod sort;
pub mod state;

pub use agent_manage::{
    AgentRow, DataReport, FrictionRow, HistoryRow, HostView, SessionRow, SessionState, View,
    human_age, human_bytes, human_tokens,
};
pub use state::Model;

/// The index of the host `agent manage` ran on: the one `View::host` names, else the first
/// (an older `agent` sends no `host`, and puts its own answer before its peers').
/// Actions other than `resume` work there only.
pub fn local_index(view: &View) -> usize {
    if !view.host.is_empty()
        && let Some(i) = view.hosts.iter().position(|h| h.host == view.host)
    {
        return i;
    }
    0
}

pub fn is_local(view: &View, host_index: usize) -> bool {
    host_index == local_index(view)
}

/// Truncates `s` to at most `max` characters, ending it with `…` when cut.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_local_host_is_the_one_the_view_names() {
        let mut v = fixture::view(fixture::default_now());
        assert_eq!(local_index(&v), 0);
        v.host = "phone".into();
        assert_eq!(local_index(&v), 2);
        assert!(is_local(&v, 2) && !is_local(&v, 0));
        let o = commands::agent_offers(&v, 2, 0);
        assert_eq!(
            o.cleanup.command().unwrap().line(),
            "agent cleanup phone-shell"
        );
        let o = commands::agent_offers(&v, 0, 0);
        assert!(o.cleanup.why_off().unwrap().contains("this host only"));
        // An older agent sends no host, or one not among the hosts: the first.
        v.host = String::new();
        assert_eq!(local_index(&v), 0);
        v.host = "elsewhere".into();
        assert_eq!(local_index(&v), 0);
    }

    #[test]
    fn clip_counts_characters() {
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "ab…");
        assert_eq!(clip("ééé", 2), "é…");
    }
}
