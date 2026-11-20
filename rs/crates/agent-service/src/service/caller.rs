//! Go `internal/service/caller.go`: who runs a command, and whose
//! conversation a background session is.
//!
//! AGENT_NAME and AGENT_HOST are set in an agent's tmux session, and every
//! process started from it inherits them: a terminal daemon a tool started,
//! a shell it opened, and Claude Code's background-session daemon, which is
//! one per user (per config dir), started by whichever CLI first needs it,
//! with that CLI's environment. Claude Code 2.1.325 hands a session sent to
//! the background (/bg) to a spare host the daemon started ahead of time, so
//! agent A's conversation runs, and runs its tools, with the AGENT_NAME of
//! agent B, whose /bg started the daemon (friction #48: A's messages went
//! out as "[from B@host]"). So the sender is found from what the processes
//! and Claude Code's records say, and AGENT_NAME counts only where they say
//! nothing.

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use agent_base::gostd::filepath;
use agent_claudereg::{self as claudereg, KIND_BACKGROUND, System, Verdict};

use super::agent_service::AgentService;
use super::status::{STATE_KEY_SESSION_ID, STATE_KEY_TRANSCRIPT};

impl AgentService {
    /// CallerAgent returns the agent whose session runs the process whose
    /// ancestors are chain (the process itself first, then its parent, and so
    /// on): walking up, the first that is
    ///
    ///   - an agent's pane (its tmux session's first pane process): that agent
    ///     (a pane this store records no agent for may be another agent
    ///     binary's: the processes don't say);
    ///   - a Claude Code background session's process: the agent whose
    ///     conversation it carries on (background_owner), or no agent, never the
    ///     agent whose pane the daemon happens to run under.
    ///
    /// A Claude Code CLI on the way that is no background session (a `claude
    /// -p` a tool started, say) is passed over: its pane's agent ran it. The
    /// bool (known) is false when the walk reaches neither (a process outside
    /// this tmux server, or detached from its session): the processes don't
    /// say. A background session no agent owns is known: no agent's, whatever
    /// AGENT_NAME its process inherited from the CLI that started the daemon.
    pub fn caller_agent(&self, chain: &[i32]) -> Result<(String, bool)> {
        let pane_of = self.tmux.pane_pids()?;
        let agent_of = self.agents_by_session()?;
        let dir = claudereg::dir()?;
        for &pid in chain {
            if let Some(session) = pane_of.get(&pid) {
                let name = agent_of.get(session).cloned().unwrap_or_default();
                let known = !name.is_empty();
                return Ok((name, known));
            }
            let p = match claudereg::read_pid(&dir, pid, &System) {
                Ok(Some(p)) => p,
                _ => continue,
            };
            if p.entry.kind != KIND_BACKGROUND
                || (p.verdict != Verdict::Live && p.verdict != Verdict::Unproven)
            {
                continue;
            }
            return Ok((self.background_owner(&p.entry.session_id)?, true));
        }
        Ok((String::new(), false))
    }

    /// agentsBySession maps each agent's tmux session to its name.
    fn agents_by_session(&self) -> Result<HashMap<String, String>> {
        Ok(self
            .storage
            .list_agents()?
            .into_iter()
            .map(|a| (a.session_name, a.name))
            .collect())
    }

    /// backgroundOwner returns the agent whose conversation Claude Code
    /// background session session_id carries on, or "" if none does: the agent
    /// already on that session (an earlier hook or status check moved it
    /// there), or else the agent whose transcript records the move into it (its
    /// continued-in line, followed through any later moves). The transcript is
    /// the conversation's own record: the pane's CLI may have exited when the
    /// session went to the background (/bg frees the terminal), and the
    /// background process runs wherever the daemon put it.
    pub(crate) fn background_owner(&self, session_id: &str) -> Result<String> {
        let agents = self.storage.list_agents()?;
        for a in &agents {
            if self.state_value(&a.name, STATE_KEY_SESSION_ID) == session_id {
                return Ok(a.name.clone());
            }
        }
        for a in &agents {
            if moved_into(&self.state_value(&a.name, STATE_KEY_TRANSCRIPT), session_id) {
                return Ok(a.name.clone());
            }
        }
        Ok(String::new())
    }
}

/// movedInto reports whether the conversation whose transcript is at path
/// moved into session id, directly or through later moves (each session's
/// transcript sits beside the one it continued from).
pub(crate) fn moved_into(path: &str, id: &str) -> bool {
    let mut seen = HashSet::new();
    let mut path = path.to_string();
    for _ in 0..16 {
        if path.is_empty() || !seen.insert(path.clone()) {
            return false;
        }
        let next = claudereg::continued_in(&path).unwrap_or_default();
        if next.is_empty() {
            return false;
        }
        if next == id {
            return true;
        }
        path = filepath::join(&[filepath::dir(&path), format!("{next}.jsonl")]);
    }
    false
}
