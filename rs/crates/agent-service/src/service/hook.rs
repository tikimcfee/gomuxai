//! Go `internal/service/hook.go`: the global hook (agent hook), which Claude
//! Code runs for every event of every session.

use std::collections::HashMap;

use anyhow::{Result, anyhow};
use serde::Deserialize;

use agent_backend::{END_FINISHED, END_REPLACED, HookProvider, namedagents};
use agent_base::util;
use agent_claudereg::{self as claudereg, KIND_BACKGROUND, System, Verdict};
use agent_store::{SessionReport, Time};

use super::agent_service::{AGENT_STATUS_ENDED, AGENT_STATUS_PROCESSING, AgentService};
use super::status::{STATE_KEY_HOOKED_SESSION, from_backend_status};

/// HookSession is what the processes say about the Claude Code session a
/// hook fired in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HookSession {
    /// CLIPID is the Claude Code process that ran the hook: the nearest of the
    /// hook's ancestors with a proven registry entry; 0 if none was found.
    pub cli_pid: i32,
    /// Owner is the workgroup agent whose pane runs that process as its
    /// session (the shallowest Claude Code process under the pane); "" for a
    /// session no agent owns, such as one a person started, or a `claude -p`
    /// run from inside an agent's session.
    pub owner: String,
    /// SpawnHooked says the process was launched with gomuxai's per-spawn
    /// hooks (an agent spawned before the global hook), whose own
    /// `agent report --hook` reports it.
    pub spawn_hooked: bool,
}

/// The panes (pane pid -> tmux session) and agents (tmux session -> agent)
/// place_hook reads.
pub(crate) type Panes = (HashMap<i32, String>, HashMap<String, String>);

/// placeHook finds, in chain (a hook's ancestors, nearest first), the CLI
/// that ran it (the nearest process own_cli accepts) and the agent whose pane
/// holds that CLI as its own session: walking on up, the first process that
/// is a pane (panes: pane pid -> tmux session; agents: tmux session -> agent),
/// unless another CLI (other_cli, given the CLI that ran the hook) comes
/// first, which makes this one a CLI started from inside that one's session. A CLI whose pane is gone (its
/// parent died, as when cleanup kills a session) reaches no pane: no agent.
pub(crate) fn place_hook(
    chain: &[i32],
    mut own_cli: impl FnMut(i32) -> Result<bool>,
    mut other_cli: impl FnMut(i32, i32) -> Result<bool>,
    panes: impl FnOnce() -> Result<Panes>,
) -> (HookSession, Result<()>) {
    let mut at = None;
    for (i, &pid) in chain.iter().enumerate() {
        match own_cli(pid) {
            Err(e) => return (HookSession::default(), Err(e)),
            Ok(true) => {
                at = Some(i);
                break;
            }
            Ok(false) => {}
        }
    }
    let Some(at) = at else {
        return (HookSession::default(), Ok(()));
    };
    let mut hs = HookSession {
        cli_pid: chain[at],
        ..Default::default()
    };
    let (pane_of, agent_of) = match panes() {
        Ok(p) => p,
        Err(e) => return (hs, Err(e)),
    };
    for &pid in &chain[at + 1..] {
        if let Some(session) = pane_of.get(&pid) {
            hs.owner = agent_of.get(session).cloned().unwrap_or_default();
            break;
        }
        match other_cli(hs.cli_pid, pid) {
            Err(e) => return (hs, Err(e)),
            Ok(true) => return (hs, Ok(())),
            Ok(false) => {}
        }
    }
    (hs, Ok(()))
}

/// spawnHooked reports whether a Claude Code command line carries gomuxai's
/// per-spawn hooks: a --settings whose hooks run `agent report --hook`. The
/// words are joined first: on macOS ps splits the settings JSON on spaces.
pub(crate) fn spawn_hooked(args: &[String]) -> bool {
    match args.iter().position(|a| a == "--settings") {
        Some(i) => args[i + 1..].join(" ").contains(" report --hook"),
        None => false,
    }
}

/// claudeHookPayload holds what the global hook reads before handing the
/// payload on.
#[derive(Debug, Default, Deserialize)]
struct ClaudeHookPayload {
    #[serde(default, rename = "hook_event_name")]
    event: String,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    transcript_path: String,
    #[serde(default)]
    agent_id: String,
    #[serde(default)]
    agent_transcript_path: String,
    #[serde(default)]
    reason: String,
}

impl AgentService {
    /// FindHookSession places the hook process self_pid, fired for
    /// session_id, among the running processes: the Claude Code CLI above it,
    /// and the agent pane above that. claude_pid is $CLAUDE_PID, which Claude
    /// Code sets for its hooks ("" if unset). As in the Go, what was found
    /// comes with the error too.
    pub fn find_hook_session(
        &self,
        self_pid: i32,
        session_id: &str,
        claude_pid: &str,
    ) -> (HookSession, Result<()>) {
        let chain = match util::proc_ancestors(self_pid) {
            Ok(c) => c,
            Err(e) => return (HookSession::default(), Err(e)),
        };
        let dir = match claudereg::dir() {
            Ok(d) => d,
            Err(e) => return (HookSession::default(), Err(e)),
        };
        // The CLI that ran the hook is one of its own ancestors, so it runs. It
        // is the nearest whose registry entry is proven, or holds the payload's
        // session (proof it can't give: a start time Claude Code doesn't record),
        // or that Claude Code named in $CLAUDE_PID (its first event can come
        // before it writes its registry file).
        let own_cli = |pid: i32| -> Result<bool> {
            if !claude_pid.is_empty() && pid.to_string() == claude_pid {
                return Ok(true);
            }
            Ok(claudereg::read_pid(&dir, pid, &System)?.is_some_and(|p| {
                p.verdict == Verdict::Live
                    || (p.verdict == Verdict::Unproven && p.entry.session_id == session_id)
            }))
        };
        // Any running process with a registry entry above that CLI, below the
        // pane, is another CLI: the one whose session this one started inside.
        let other_cli = |_: i32, pid: i32| -> Result<bool> {
            Ok(claudereg::read_pid(&dir, pid, &System)?
                .is_some_and(|p| p.verdict == Verdict::Live || p.verdict == Verdict::Unproven))
        };
        let panes = || -> Result<Panes> {
            let panes = self.tmux.pane_pids()?;
            let agents = self.storage.list_agents()?;
            let by_session = agents
                .into_iter()
                .map(|a| (a.session_name, a.name))
                .collect();
            Ok((panes, by_session))
        };
        let (mut hs, res) = place_hook(&chain, own_cli, other_cli, panes);
        if res.is_err() || hs.cli_pid == 0 {
            return (hs, res);
        }
        if let Some(args) = util::proc_args(hs.cli_pid) {
            hs.spawn_hooked = spawn_hooked(&args);
        }
        // A background session's process runs where Claude Code's daemon put
        // it, often under another agent's pane: its place in the process tree
        // says nothing about whose conversation it is. Its lineage does.
        if let Ok(Some(p)) = claudereg::read_pid(&dir, hs.cli_pid, &System)
            && p.entry.kind == KIND_BACKGROUND
        {
            match self.background_owner(session_id) {
                Ok(owner) => hs.owner = owner,
                Err(e) => {
                    hs.owner = String::new();
                    return (hs, Err(e));
                }
            }
        }
        (hs, Ok(()))
    }

    /// SessionReports returns every session's last report from the global hook,
    /// by session id.
    pub fn session_reports(&self) -> Result<HashMap<String, SessionReport>> {
        self.storage.session_reports()
    }

    /// HandleClaudeHook is the global hook: Claude Code runs it for every event
    /// of every session (agent hooks install registers it). It records the
    /// session's report, and when an agent owns the session, the agent's report
    /// too, as its own hook would; the output is what the hook must print (held
    /// messages, a pre-approval), only ever for an agent's session.
    pub fn handle_claude_hook(
        &self,
        hs: &HookSession,
        payload: &[u8],
        at: Time,
    ) -> (Option<Vec<u8>>, Result<()>) {
        // Go's json.Unmarshal: a null leaves a field "", half a surrogate pair
        // or a stray byte is U+FFFD, a name matches ignoring case. A payload
        // Go takes is never dropped here: every session's every event runs
        // this.
        let p: ClaudeHookPayload = match agent_base::gostd::json::from_slice(payload) {
            Ok(p) => p,
            Err(e) => return (None, Err(anyhow!(e).context("decoding hook payload"))),
        };
        if p.session_id.is_empty() {
            return (
                None,
                Err(anyhow!("a {} payload with no session_id", p.event)),
            );
        }
        if hs.spawn_hooked {
            return (None, Ok(())); // its spawn's own hook reports it
        }
        if let Err(e) = self.record_session_report(&p, payload, at) {
            return (None, Err(e));
        }
        if hs.owner.is_empty() {
            return (None, Ok(()));
        }
        if p.agent_id.is_empty() {
            // Seen first, so this very Stop may hand over held messages.
            if let Err(e) = self.set_state(&hs.owner, STATE_KEY_HOOKED_SESSION, &p.session_id) {
                return (None, Err(e));
            }
        }
        self.handle_hook(&hs.owner, "", "", payload)
    }

    /// recordSessionReport stores the report the event makes for the session it
    /// is about: the subagent's (agent-<id>, as agent sessions names it) for a
    /// subagent's event, else the session's.
    fn record_session_report(&self, p: &ClaudeHookPayload, payload: &[u8], at: Time) -> Result<()> {
        let mut r = SessionReport {
            session_id: p.session_id.clone(),
            harness: "claude".into(),
            event: p.event.clone(),
            at,
            transcript: p.transcript_path.clone(),
            ..Default::default()
        };
        if !p.agent_id.is_empty() {
            r.session_id = format!("agent-{}", p.agent_id);
            r.transcript = p.agent_transcript_path.clone();
            r.status = AGENT_STATUS_PROCESSING.to_string();
            if p.event == "SubagentStop" {
                r.status = AGENT_STATUS_ENDED.to_string();
                r.end_reason = END_FINISHED.as_str().to_string();
            }
            return self.storage.record_session_report(&r);
        }
        if p.event == "SessionEnd" && p.reason == "clear" {
            // The CLI goes on in a new session (its SessionStart follows).
            r.status = AGENT_STATUS_ENDED.to_string();
            r.end_reason = END_REPLACED.as_str().to_string();
            r.detail = "/clear".into();
            return self.storage.record_session_report(&r);
        }
        let cb = namedagents::new_claude_backend();
        let Some(hr) = HookProvider::parse_hook_event(&cb, &p.event, payload)? else {
            return Ok(());
        };
        r.status = from_backend_status(hr.status).to_string();
        r.end_reason = hr.end_reason.as_str().to_string();
        r.detail = hr.detail;
        self.storage.record_session_report(&r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes_fn(
        pane_of: &HashMap<i32, String>,
        agent_of: &HashMap<String, String>,
    ) -> impl FnOnce() -> Result<Panes> {
        let p = (pane_of.clone(), agent_of.clone());
        move || Ok(p)
    }

    fn hs(cli_pid: i32, owner: &str) -> HookSession {
        HookSession {
            cli_pid,
            owner: owner.into(),
            spawn_hooked: false,
        }
    }

    // The process shapes the global hook meets, as seen on Linux with
    // Claude Code 2.1.316: hook -> CLI -> (wrappers) -> pane shell.
    #[test]
    fn test_place_hook() {
        let pane_of: HashMap<i32, String> = [
            (100, "agent-worker"),
            (200, "agent-other"),
            (300, "someone-elses"),
        ]
        .into_iter()
        .map(|(k, v)| (k, v.to_string()))
        .collect();
        let agent_of: HashMap<String, String> =
            [("agent-worker", "worker"), ("agent-other", "other")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();

        let cases: Vec<(&str, Vec<i32>, Vec<i32>, HookSession)> = vec![
            (
                "an agent's own CLI",
                vec![102, 101, 100, 50],
                vec![102],
                hs(102, "worker"),
            ),
            (
                "through wrappers (sh -c, env)",
                vec![105, 104, 103, 100],
                vec![105],
                hs(105, "worker"),
            ),
            // A `claude -p` a tool ran inside the agent's session: the agent's
            // CLI sits between it and the pane.
            (
                "a CLI started from inside the session",
                vec![110, 109, 102, 101, 100],
                vec![110, 102],
                hs(110, ""),
            ),
            // A background session (bg CLI -> bg-pty-host -> daemon -> the CLI
            // that started the daemon -> its pane) is nobody's by the process
            // tree: find_hook_session places it by its lineage instead.
            (
                "a background CLI under an agent's CLI",
                vec![170, 159, 158, 102, 101, 100],
                vec![170, 102],
                hs(170, ""),
            ),
            // cleanup killed the pane: the dying CLI's SessionEnd runs with its
            // parent gone (reparented to init or a subreaper), so it reaches no
            // pane and can't land on a respawn of the same name.
            (
                "a CLI whose pane is gone",
                vec![120, 1500],
                vec![120],
                hs(120, ""),
            ),
            (
                "a person's terminal",
                vec![130, 129, 50],
                vec![130],
                hs(130, ""),
            ),
            (
                "a pane no agent records",
                vec![140, 300],
                vec![140],
                hs(140, ""),
            ),
            (
                "no CLI above the hook",
                vec![150, 100],
                vec![],
                HookSession::default(),
            ),
        ];
        for (name, chain, cli, want) in cases {
            let is_cli = |pid: i32| -> Result<bool> { Ok(cli.contains(&pid)) };
            let other = |_: i32, pid: i32| -> Result<bool> { Ok(cli.contains(&pid)) };
            let (got, err) = place_hook(&chain, is_cli, other, panes_fn(&pane_of, &agent_of));
            assert!(
                err.is_ok() && got == want,
                "{name}: got {got:?}, {err:?}; want {want:?}"
            );
        }
    }

    #[test]
    fn test_spawn_hooked() {
        let old: Vec<String> = [
            "claude",
            "--session-id",
            "x",
            "--settings",
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"'/b/agent' report --hook --as 'w' --spawn-id 'x'"}]}]}}"#,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(spawn_hooked(&old), "a CLI spawned with per-spawn hooks");
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(
            !spawn_hooked(&s(&["claude", "--session-id", "x"]))
                && !spawn_hooked(&s(&["claude", "--settings", "/tmp/s.json"])),
            "a CLI without them"
        );
    }

    // When the registry can't prove a CLI (no start time recorded), the nesting
    // check still counts it: a `claude -p` inside an agent's session stays
    // unowned, while the agent's own CLI, holding the payload's session, is
    // still found and placed.
    #[test]
    fn test_place_hook_without_proof() {
        let pane_of: HashMap<i32, String> =
            [(100, "agent-worker".to_string())].into_iter().collect();
        let agent_of: HashMap<String, String> =
            [("agent-worker".to_string(), "worker".to_string())]
                .into_iter()
                .collect();
        let registered = [102, 110]; // both unproven
        let other = |_: i32, pid: i32| -> Result<bool> { Ok(registered.contains(&pid)) };

        // The nested CLI's hook: its own CLI is 110; 102 (the agent's) is above.
        let (got, _) = place_hook(
            &[110, 109, 102, 101, 100],
            |pid| Ok(pid == 110),
            other,
            panes_fn(&pane_of, &agent_of),
        );
        assert_eq!(got, hs(110, ""), "nested");
        // The agent's own CLI's hook.
        let (got, _) = place_hook(
            &[102, 101, 100],
            |pid| Ok(pid == 102),
            other,
            panes_fn(&pane_of, &agent_of),
        );
        assert_eq!(got, hs(102, "worker"), "own");
    }

    #[test]
    fn test_spawn_hooked_ps_words() {
        // macOS ps -o args= split on spaces.
        let words: Vec<String> = [
            "claude",
            "--session-id",
            "x",
            "--settings",
            r#"{"hooks":{"Stop":[{"hooks":[{"command":"'/b/agent'"#,
            "report",
            "--hook",
            "--as",
            "'w'",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(spawn_hooked(&words), "split settings JSON");
    }
}
