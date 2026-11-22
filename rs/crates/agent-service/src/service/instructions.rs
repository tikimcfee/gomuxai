//! Go `internal/service/instructions.go`: the briefing every spawned agent
//! gets, the shared `agent` skill text, its launch script, and the agent
//! commands it may run unasked.

use anyhow::{Context, Result};

use agent_allowcmd::Checker;
use agent_backend::{HookReport, Provider};
use agent_base::gostd::{filepath, os as goos, strconv};
use agent_base::util;

use super::agent_service::AgentService;

/// Shared map of how to use `agent` (`agent skill show`, `SKILL.md` body, and
/// the second half of every spawned agent's `instructions.md`).
///
/// Kept short (~40 lines) as a map to `agent help <command>` rather than a
/// full manual.
pub fn skill_text(binary: &str) -> String {
    let bin = if binary.is_empty() { "agent" } else { binary };
    format!(
        r#"# Coordinating agents with `agent`

`agent` coordinates AI CLIs (`claude`, `agy`, `aider`, `shell`) in `g3d-termd` sessions on this host and across hosts (`NAME@HOST`); use `{bin}` (or `agent` on `PATH`) rather than `g3d` to reach other agents. Run `agent` for an orientation, `agent help` for all commands, and `agent help <command>` for flags and examples. Run each call as one plain command (not through a shell variable like `$A send ...`, pipes, or `&&`) so pre-allowed commands run without a permission prompt.

## Core loop

    agent list                                                # agents here; --host HOST for another host
    agent roster pick --task review                           # which harness, model and host suit the task (see below)
    agent spawn helper --backend agy --model MODEL            # start a helper (<yourname>-<role> inside an agent)
    agent send helper "the task"                            # send now, or hold while busy (--now types in at once)
    agent wait helper --for idle,waiting --next --timeout 30m # wait for that turn (--any returns at first of several)
    agent state get helper _result                            # latest final reply (claude and agy on this host)
    agent status helper -v                                    # status, source, pending prompt detail, held count
    agent capture helper --lines 50                           # live screen; agent logs helper for transcript history
    agent cleanup helper                                      # stop and remove your helper (--ended cleans ended ones)

## Choosing a harness and model

Don't default to your own harness and model: each one sees a problem differently and runs at its own speed and cost. Before a spawn, ask the roster: `agent roster list` lists each host's harnesses and models with what they're good at, speed, cost, availability, and past evaluations; `agent roster pick --task KIND` (`edit`, `bugfix`, `feature`, `long-horizon`, `review`, `research`, `design`, `test`, `kernel`, `chat`) prints a ready `agent spawn` line. Mix them: a review or a second opinion on another harness or model catches what yours misses; a fast, cheap model suits mechanical edits; save the strongest for long or hard work. Inside Claude Code, the Agent tool's `model` option runs a subagent on another Claude model without a new agent. When a run teaches you something about a model, record it, citing the session (`agent sessions` lists them):

    agent roster note HARNESS/MODEL --task KIND --verdict good|mixed|bad --session HARNESS:SESSION-ID "what happened" --why "..."

## Messages and prompts

- Messages from other agents start with `"[from NAME@HOST]"`: the sender can't see your terminal, so reply once with `agent send NAME@HOST "..."`, and wait for replies before reporting them.
- Messages starting with `"[from a person on HOST]"` or `"[from a Claude session on HOST]"` came from someone with no agent record: answer in your own terminal output.
- Messages sent to a busy `claude` or `agy` agent are held until its turn or step ends (`agent send NAME --now "..."` types in at once instead); act on held messages and reply as asked.
- `waiting` means blocked on a permission, workspace-trust, or CLI dialog: inspect `agent status NAME -v` and `agent capture NAME`; a person answers with `agent approve NAME --expect TOOL` or `agent deny NAME` (`agent wait --any --for waiting` watches for prompts).

## Fleet commands (`agent help <command>` for details)

- `agent fleet [--host HOST] [--json] [--report]`: tree across hosts, stuck flags, focus, running jobs, stale handoffs, and friction.
- `agent focus [NAME] [--clear] [--host HOST] [--follow]`: show, set, or follow which agent has focus.
- `agent run <script> [args...]` (`--list`, `--show ID`, `--promote FILE`): run heavy scripts in a memory-limited scope so OOM spares your terminal.
- `agent template list|start|resume [NAME] [--host HOST]` (local `add`/`remove`): start or resume pre-approved agent templates across hosts.
- `agent revive NAME [--handoff FILE]`: continue an ended `claude` or `agy` agent on its conversation and re-brief it from its handoff doc.

## When the agent tool gets in your way

Check `agent friction list` first, vote on a match with `agent friction +1 ID`, or file one and carry on (`agent friction link ID ID@HOST` links cross-host duplicates):

    agent friction add "summary" --kind help|error|missing|bug|docs|brief|handover|skill|other
"#
    )
}

/// heldBriefing tells an agent on backend_name how messages sent while it
/// works reach it (see held.rs).
fn held_briefing(backend_name: &str) -> &'static str {
    if backend_name == "agy" {
        "A message sent to you while you're working is held until your current step \
(a model call and its tools) ends, then handed to you as a new prompt, starting with \
its \"[from ...]\" tag, in the middle of your turn: it's a message like any other, so \
act on it and reply as asked."
    } else {
        "A message sent to you while you're working is held until your turn ends, then \
handed to you as Stop hook feedback (\"A message arrived while you were working: ...\"): \
it's a message like any other, so act on it and reply as asked."
    }
}

/// agentBriefingHeader is the per-agent part of `instructions.md`: who this
/// agent is, where it runs, who started it, the binary's full path, how to
/// name its helpers, and how held messages reach its backend.
fn agent_briefing_header(
    name: &str,
    host: &str,
    spawned_by: &str,
    bin: &str,
    backend_name: &str,
) -> String {
    let by = if spawned_by.is_empty() {
        "a user".to_string()
    } else {
        format!("agent {}", strconv::quote(spawned_by))
    };
    format!(
        r#"# Working with other agents (gomuxai)

You are agent {qname} on host {qhost}, started by {by} through gomuxai. Run `agent` by its full path, {bin}, since your shell's PATH may not include it. Your status (idle, processing, waiting on a permission prompt, ended) is reported automatically. Name any helper you spawn {name}-<role> (for example, `agent spawn {name}-review --backend agy` and `agent cleanup {name}-review`; `agent roster pick` suggests a harness and model), and clean up only your own helpers and theirs.

{held}
"#,
        qname = strconv::quote(name),
        qhost = strconv::quote(host),
        held = held_briefing(backend_name),
    )
}

/// agentSafeCommands are the agent subcommands a spawned agent may run without
/// a permission prompt: they read status, screens, transcripts, sessions, or
/// history (a coordinator needs capture and logs to see why a helper is stuck,
/// and sessions to find its peers), deliver
/// messages that
/// the recipient's own permission prompts still guard, or file and vote on
/// friction, which only writes a note, and link entries to the same problem
/// on other hosts (unlink is left out: it erases a link on both hosts).
/// approve/deny, keys, and send --force
/// are left out: they would let one agent answer another agent's prompts,
/// which is the user's call. So are spawn and cleanup, which start and stop
/// agents: the user approves each one. So are friction accept and resolve,
/// which are for the people working on the tool.
pub const AGENT_SAFE_COMMANDS: &[&str] = &[
    "send",
    "deliver",
    "list",
    "status",
    "statuses",
    "wait",
    "events",
    "capture",
    "logs",
    "sessions",
    "transcript",
    "state get",
    "history",
    "friction add",
    "friction +1",
    "friction list",
    "friction show",
    "friction link",
    "friction links",
    // Not bare "roster": safe commands match as a prefix, so it would allow
    // roster set, disable and remove too. "roster list" is the same read.
    "roster list",
    "roster show",
    "roster pick",
    "roster picks",
    "roster note",
];

impl AgentService {
    /// Returns the shared `agent` skill text for `binary`.
    pub fn skill_text(binary: &str) -> String {
        skill_text(binary)
    }

    /// agentInstructions is the briefing every spawned agent gets, built from
    /// its own identity header plus [`skill_text`]. Backends deliver it
    /// through their own mechanism (`backend::InstructionsProvider`).
    pub(crate) fn agent_instructions(
        &self,
        name: &str,
        spawned_by: &str,
        backend_name: &str,
    ) -> String {
        let bin = if self.cfg.agent_binary.is_empty() {
            "agent"
        } else {
            self.cfg.agent_binary.as_str()
        };
        let mut out = agent_briefing_header(name, &self.cfg.host, spawned_by, bin, backend_name);
        out.push('\n');
        out.push_str(&skill_text(bin));
        out
    }

    /// writeInstructions saves the briefing in the agent's own directory and
    /// returns its path.
    pub(crate) fn write_instructions(
        &self,
        name: &str,
        spawned_by: &str,
        backend_name: &str,
    ) -> Result<String> {
        let dir = self.cfg.agent_dir(name);
        goos::mkdir_all(&dir, 0o700).context("creating agent dir")?;
        let path = filepath::abs(&filepath::join(&[dir.as_str(), "instructions.md"]))?;
        goos::write_file(
            &path,
            self.agent_instructions(name, spawned_by, backend_name)
                .as_bytes(),
            0o600,
        )
        .context("writing instructions")?;
        Ok(path)
    }

    /// writeLaunchScript saves the launch command in the agent's directory and
    /// returns the short command that runs it. Typing the full command into the
    /// new session's shell can lose its end: until the shell's line editor is up,
    /// the terminal holds typed input in canonical mode, which macOS caps at 1024
    /// bytes (MAX_CANON), which a launch command with its flags and pre-allowed
    /// command rules can pass.
    pub(crate) fn write_launch_script(&self, name: &str, launch_cmd: &str) -> Result<String> {
        let dir = self.cfg.agent_dir(name);
        goos::mkdir_all(&dir, 0o700).context("creating agent dir")?;
        let path = filepath::abs(&filepath::join(&[dir.as_str(), "launch.sh"]))?;
        goos::write_file(&path, format!("{launch_cmd}\n").as_bytes(), 0o600)
            .context("writing launch script")?;
        Ok(format!("sh {}", util::shell_quote(&path)))
    }

    /// hookApproval is the hook output that approves hr's shell command without
    /// a prompt, or None. It approves what the allow rules can't match, since they
    /// match the command's text: this binary by any path or symlink, or with -q
    /// before the subcommand. Only one plain run of it with a subcommand in
    /// AGENT_SAFE_COMMANDS qualifies (see allowcmd); anything else is left to the
    /// normal permission flow, so nothing is ever denied here.
    pub(crate) fn hook_approval(
        &self,
        provider: &dyn Provider,
        hr: &HookReport,
    ) -> Option<Vec<u8>> {
        let ha = provider.as_hook_allower()?;
        if hr.command.is_empty() || self.cfg.agent_binary.is_empty() {
            return None;
        }
        let c = Checker::new(self.cfg.agent_binary.clone(), AGENT_SAFE_COMMANDS);
        if c.check(&hr.command, &hr.cwd).is_err() {
            return None;
        }
        Some(ha.allow_output("gomuxai: a safe agent command, pre-allowed for spawned agents"))
    }

    /// allowedCommandPrefixes are AGENT_SAFE_COMMANDS as the agent may type them:
    /// bare (on PATH) or by the full path the briefing gives.
    pub(crate) fn allowed_command_prefixes(&self) -> Vec<String> {
        let mut prefixes = Vec::new();
        for sub in AGENT_SAFE_COMMANDS {
            prefixes.push(format!("agent {sub}"));
            if !self.cfg.agent_binary.is_empty() {
                prefixes.push(format!("{} {sub}", self.cfg.agent_binary));
            }
        }
        prefixes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_base::config::Config;
    use std::sync::Arc;

    fn svc(cfg: Config) -> AgentService {
        let dir = tempfile::Builder::new()
            .prefix("svc-instr")
            .tempdir_in("/var/tmp")
            .unwrap();
        let store = Arc::new(agent_store::Store::open(dir.path().join("t.db")).unwrap());
        // The briefing reads nothing but the config.
        struct NoTerm;
        impl agent_termd::Ops for NoTerm {
            fn create_session(
                &self,
                _: &str,
                _: &std::collections::HashMap<String, String>,
            ) -> Result<()> {
                unreachable!()
            }
            fn create_session_in(
                &self,
                _: &str,
                _: &str,
                _: &std::collections::HashMap<String, String>,
            ) -> Result<()> {
                unreachable!()
            }
            fn kill_session(&self, _: &str) -> Result<()> {
                unreachable!()
            }
            fn respawn_pane(&self, _: &str) -> Result<()> {
                unreachable!()
            }
            fn session_exists(&self, _: &str) -> Result<bool> {
                unreachable!()
            }
            fn list_sessions(&self) -> Result<Vec<agent_termd::Session>> {
                unreachable!()
            }
            fn send_keys(&self, _: &str, _: &str) -> Result<()> {
                unreachable!()
            }
            fn paste_text(&self, _: &str, _: &str) -> Result<()> {
                unreachable!()
            }
            fn send_special_key(&self, _: &str, _: &str) -> Result<()> {
                unreachable!()
            }
            fn capture(&self, _: &str, _: i32) -> Result<String> {
                unreachable!()
            }
            fn capture_styled(&self, _: &str, _: i32) -> Result<String> {
                unreachable!()
            }
            fn pane_pids(&self) -> Result<std::collections::HashMap<i32, String>> {
                unreachable!()
            }
        }
        AgentService::new(
            cfg,
            store,
            Arc::new(NoTerm),
            Arc::new(agent_backend::new_registry()),
        )
    }

    #[test]
    fn test_briefing_explains_sender_tags() {
        let s = svc(Config {
            host: "h".into(),
            ..Default::default()
        });
        let brief = s.agent_instructions("lead", "", "claude");
        for want in [
            r#""[from NAME@HOST]""#,
            r#""[from a person on HOST]""#,
            r#""[from a Claude session on HOST]""#,
            "no agent record",
            "Stop hook feedback",
            "--now",
            "not through a shell variable",
        ] {
            assert!(brief.contains(want), "briefing lacks {want}");
        }
    }

    // agy gets held messages as prompts after a step, not as Stop hook feedback.
    #[test]
    fn test_briefing_for_agy() {
        let s = svc(Config {
            host: "h".into(),
            ..Default::default()
        });
        let brief = s.agent_instructions("lead", "", "agy");
        assert!(
            !brief.contains("Stop hook")
                && brief.contains("current step")
                && brief.contains("--now"),
            "agy briefing on held messages:\n{brief}"
        );
        assert!(
            brief.contains("rather than `g3d` to reach other agents."),
            "agy briefing lost the rule against driving g3d directly:\n{brief}"
        );
    }

    #[test]
    fn test_briefing_covers_friction() {
        let s = svc(Config {
            host: "h".into(),
            agent_binary: "/opt/x/agent".into(),
            ..Default::default()
        });
        let brief = s.agent_instructions("lead", "", "claude");
        for want in [
            "## When the agent tool gets in your way",
            "`agent friction list`",
            "`agent friction +1 ID`",
            "    agent friction add \"summary\" --kind help|error|missing|bug|docs|brief|handover|skill|other\n",
            "`agent friction link ID ID@HOST`",
        ] {
            assert!(brief.contains(want), "briefing lacks {want:?}");
        }

        let prefixes = s.allowed_command_prefixes();
        for want in [
            "agent friction add",
            "/opt/x/agent friction +1",
            "agent friction list",
            "agent friction show",
            "agent friction link",
            "/opt/x/agent friction links",
            "agent history",
            "agent capture",
            "/opt/x/agent capture",
            "agent logs",
            "/opt/x/agent logs",
        ] {
            assert!(
                prefixes.iter().any(|p| p == want),
                "pre-allowed commands lack {want:?}"
            );
        }
        for p in &prefixes {
            assert!(
                !(p.contains("accept") || p.contains("resolve") || p == "agent friction"),
                "pre-allowed {p:?}: accepting and resolving friction is for people"
            );
            assert!(
                !p.contains("unlink"),
                "pre-allowed {p:?}: unlink erases a link on both hosts"
            );
        }
    }

    #[test]
    fn test_skill_text_is_concise_and_shared_with_briefing() {
        let skill = skill_text("/opt/x/agent");
        let line_count = skill.lines().count();
        assert!(
            (30..=50).contains(&line_count),
            "skill_text should stay around 40 lines, got {line_count}:\n{skill}"
        );
        for want in [
            "/opt/x/agent",
            "agent help <command>",
            "agent list",
            "agent spawn helper --backend claude",
            "agent send helper",
            "agent wait helper --for idle,waiting --next",
            "agent state get helper _result",
            "agent cleanup helper",
            "agent approve NAME --expect TOOL",
            "agent deny NAME",
            "agent fleet",
            "agent focus",
            "agent run",
            "agent template",
            "agent revive",
            "agent friction add",
        ] {
            assert!(skill.contains(want), "skill_text lacks {want:?}");
        }

        let s = svc(Config {
            host: "box".into(),
            agent_binary: "/opt/x/agent".into(),
            ..Default::default()
        });
        let brief = s.agent_instructions("worker", "lead", "claude");
        assert!(
            brief.starts_with("# Working with other agents (gomuxai)\n\nYou are agent \"worker\" on host \"box\", started by agent \"lead\""),
            "briefing should open with the agent's identity:\n{brief}"
        );
        assert!(
            brief.ends_with(&skill),
            "briefing must end with the exact shared skill_text"
        );
    }
}
