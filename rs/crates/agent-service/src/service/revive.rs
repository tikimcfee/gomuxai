//! What `agent revive` needs from the service (no Go file: docs/FLEET.md,
//! "Revive"). Revive is resume plus a re-brief; resume itself is resume.rs.
//! This file adds the case resume can't handle: an ended agent whose pane
//! runs a CLI of its backend again, because a person started one there by
//! hand (friction #51: an agy that quit to re-authenticate, restarted in the
//! agent's own pane, stayed `ended (quit)` for good, and send refused it).
//! Resume would kill that pane; revive takes the running CLI as the agent's
//! instead, launching nothing.
//!
//! Revive never kills what it hasn't positively identified (review-3, 2.3):
//! it takes over a CLI of the agent's backend it has identified in the pane
//! (inspect_pane), resumes only when the pane's shell sits at its prompt with
//! nothing running under it (or the session is gone), and refuses anything
//! else, naming what it saw.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use agent_backend::END_QUIT;
use agent_base::gostd::filepath;
use agent_base::util::{self, ProcTree};
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, StateEntry, StateUpdate, Time};

use super::agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AgentService, AgentStatus,
};
use super::ctx::{Ctx, CtxExt};
use super::held::{SendOptions, SendResult};
use super::resume::KEPT_ON_RESUME;
use super::status::{
    STATE_KEY_END_REASON, STATE_KEY_RESULT, STATE_KEY_SESSION_ID, STATE_KEY_SPAWN_ID,
    STATE_KEY_STATUS, STATE_KEY_TRANSCRIPT, STATUS_SOURCE_NATIVE, StatusInfo, StatusReport,
    from_backend_status,
};

/// The event a revive that took over a running CLI records.
pub const EVENT_REVIVED: &str = "revived";

/// StateKeyRevivedCLI names the CLI process a revive took over, as "<pid>
/// <start time>" (the start time in clock ticks after boot, field 22 of
/// /proc/<pid>/stat): while it is set, the agent holds messages as a spawned
/// one does (holds_messages), and once that process is gone, status says the
/// agent ended (revived_cli_gone). Every resume or take-over clears it, as
/// it clears every reserved key of the run before.
pub const STATE_KEY_REVIVED_CLI: &str = "_revived_cli";

/// HeldUntilTrust is when a message held for an agent at its workspace
/// trust prompt goes in.
pub const HELD_UNTIL_TRUST: &str = "its workspace trust prompt is answered";

/// HeldUntilPrompt is when a message held for an agent that wasn't at an
/// idle prompt with an empty input line goes in, when no hook of its CLI
/// hands it over.
pub const HELD_UNTIL_PROMPT: &str = "it is at its prompt with an empty input line";

/// A CLI of the agent's backend running in its pane, identified: a
/// descendant of the pane's own process, run by this user, whose executable
/// (resolved) is the backend's CLI on PATH, in an interactive session (for
/// Claude: one its session registry proves, by pid and start time).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneCli {
    /// The CLI's process.
    pub pid: i32,
    /// When it started (clock ticks after boot), so a reused pid isn't
    /// taken for it.
    pub start: u64,
    /// The pane process it runs under.
    pub pane_pid: i32,
    /// Its command line, as the process table has it.
    pub args: Vec<String>,
    /// The session it runs (Claude's session id, agy's conversation id), ""
    /// when nothing says which.
    pub session_id: String,
    /// That session's transcript, "" when unknown (the CLI's caller finds it
    /// from the session index, which the service can't read).
    pub transcript: String,
    /// Its status as the CLI's own records say (Claude Code's session
    /// registry), None when they don't.
    pub status: Option<AgentStatus>,
    /// That status in words.
    pub detail: String,
    /// It was started with the hooks spawn wired for this agent (an agy
    /// started from the agent's own launch.sh, whose hooks bundle names the
    /// agent): its hooks report to the agent's current spawn.
    pub hooked: bool,
}

/// What revive found in an ended agent's pane that it can neither take over
/// nor replace: a process it can't tell is the agent's CLI, or a pane it
/// couldn't read (pid 0, why says what failed).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PaneProcess {
    pub pid: i32,
    /// Its command line.
    pub command: String,
    /// Why revive won't touch it.
    pub why: String,
}

/// What revive does with an agent, decided from its record, its pane and
/// its resolved status.
#[derive(Debug, Clone)]
pub enum ReviveCase {
    /// It hasn't ended: nothing to revive. The status it is in.
    Running(StatusInfo),
    /// It has ended, and its session is gone or its pane's shell sits at its
    /// prompt with nothing running: resume it. How it ended.
    Resume(StatusInfo),
    /// Its record says it ended, yet a CLI of its backend runs in its pane:
    /// take that CLI as the agent's.
    TakeOver(PaneCli),
    /// It has ended, and its pane runs something revive can't identify (or
    /// can't be read): neither take it over nor kill it.
    Refuse(PaneProcess),
}

/// What an ended agent's pane shows.
enum Pane {
    /// No session: nothing to kill.
    Gone,
    /// Its shell, at its prompt, with nothing running under it.
    Shell,
    Cli(PaneCli),
    Other(PaneProcess),
}

/// How long the re-brief waits between looks at the agent.
const REBRIEF_POLL: Duration = Duration::from_millis(200);

impl AgentService {
    /// ReviveCase decides what revive does with the agent name. Its record is
    /// read before its status is resolved: resolving a Claude agent's status
    /// can record the session its pane runs on its own, and revive must still
    /// see that the record said ended.
    pub fn revive_case(&self, name: &str) -> Result<ReviveCase> {
        let agent = self.get_agent(name)?;
        let recorded_ended = self
            .reported_status(name)?
            .is_some_and(|r| r.status == AGENT_STATUS_ENDED);
        let pane = if agent.adopted {
            Pane::Gone // revive refuses an adopted agent before it gets here
        } else {
            self.inspect_pane(&agent).unwrap_or_else(|e| {
                Pane::Other(PaneProcess {
                    pid: 0,
                    command: String::new(),
                    why: format!("{e:#}"),
                })
            })
        };
        if recorded_ended && let Pane::Cli(cli) = pane {
            return Ok(ReviveCase::TakeOver(cli));
        }
        let (info, _) = self.resolve_status(name)?;
        if info.status != AGENT_STATUS_ENDED {
            return Ok(ReviveCase::Running(info));
        }
        Ok(match pane {
            Pane::Gone | Pane::Shell => ReviveCase::Resume(info),
            Pane::Cli(cli) => ReviveCase::TakeOver(cli),
            Pane::Other(p) => ReviveCase::Refuse(p),
        })
    }

    /// inspectPane reads what the agent's pane runs. An error means it
    /// couldn't tell (termd or the process table didn't answer).
    fn inspect_pane(&self, agent: &Agent) -> Result<Pane> {
        let exists = self
            .tmux
            .session_exists(&agent.session_name)
            .with_context(|| {
                format!("checking whether its session {} exists", agent.session_name)
            })?;
        if !exists {
            return Ok(Pane::Gone);
        }
        let pane_pid = self
            .pane_pid(&agent.session_name)
            .context("finding its pane's process")?;
        if pane_pid == 0 {
            bail!("its session {} shows no pane process", agent.session_name);
        }
        let tree = util::read_proc_tree().context("reading the process table")?;
        let mut below: Vec<(usize, i32)> = tree
            .keys()
            .filter(|&&pid| pid != pane_pid)
            .filter_map(|&pid| tree.depth(pid, pane_pid).map(|d| (d, pid)))
            .collect();
        below.sort();

        let mut rejected: Option<PaneProcess> = None;
        if let Ok(provider) = self.provider(agent) {
            if let Some(pp) = provider.as_pane_session_provider() {
                // Claude: only a session its registry proves (pid and start
                // time) counts; a claude process it doesn't list may be a
                // one-shot command (`claude auth login`, `claude -p`) or a
                // script of that name.
                let cli = cli_name(&provider.launch_command());
                match pp
                    .pane_session(pane_pid)
                    .context("reading Claude Code's session registry")?
                {
                    Some(ps) if !ps.id.is_empty() => {
                        match check_process(ps.pid, pane_pid, &tree, None) {
                            Ok(proc) => {
                                return Ok(Pane::Cli(PaneCli {
                                    pid: ps.pid,
                                    start: proc.start,
                                    pane_pid,
                                    args: proc.args,
                                    session_id: ps.id.clone(),
                                    status: Some(from_backend_status(ps.status)),
                                    detail: ps.detail.clone(),
                                    ..Default::default()
                                }));
                            }
                            Err(why) => rejected = Some(process(ps.pid, why)),
                        }
                    }
                    _ => {
                        rejected = below.iter().find(|(_, pid)| names_cli(*pid, &cli)).map(|(_, pid)| {
                            process(
                                *pid,
                                format!(
                                    "Claude Code's session registry doesn't list (by pid and start time), so it isn't known to be an interactive {cli} session, nor which conversation it runs"
                                ),
                            )
                        });
                    }
                }
            } else {
                let cli = cli_program(&provider.launch_command());
                if !cli.is_empty() {
                    let paths = cli_paths(&cli, pane_pid);
                    let name = filepath::base(&cli);
                    for (_, pid) in &below {
                        let Some(found) = identify(*pid, &paths, &name) else {
                            continue;
                        };
                        match found.and_then(|cli_args| {
                            check_process(
                                *pid,
                                pane_pid,
                                &tree,
                                Some((name.as_str(), cli_args.as_slice())),
                            )
                        }) {
                            Ok(proc) => {
                                let hooked = provider.as_spawn_hooker().is_some()
                                    && !self
                                        .state_value(&agent.name, STATE_KEY_SPAWN_ID)
                                        .is_empty()
                                    && adds_dir(&proc.args, &self.cfg.agent_dir(&agent.name));
                                return Ok(Pane::Cli(PaneCli {
                                    pid: *pid,
                                    start: proc.start,
                                    pane_pid,
                                    session_id: presence_lock_session(*pid),
                                    args: proc.args,
                                    hooked,
                                    ..Default::default()
                                }));
                            }
                            Err(why) => {
                                if rejected.is_none() {
                                    rejected = Some(process(*pid, why));
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(p) = rejected {
            return Ok(Pane::Other(p));
        }
        if below.is_empty() {
            if is_shell(pane_pid) {
                return Ok(Pane::Shell);
            }
            return Ok(Pane::Other(process(
                pane_pid,
                format!(
                    "is its pane's own process, and no shell revive knows (a CLI of {} it can identify runs under one)",
                    agent.backend
                ),
            )));
        }
        let pid = below[0].1;
        Ok(Pane::Other(process(
            pid,
            format!("isn't a {} revive can identify", agent.backend),
        )))
    }

    /// TakeOverPane records cli, running in the ended agent's pane, as the
    /// agent's: its run closes with the status it ended in and a new one opens
    /// (as resume records it), what described the old run is cleared (as
    /// resume clears it, and its reply and transcript unless cli runs the same
    /// session), and the session cli runs is recorded, so status reads the
    /// running CLI again. All of it is one store transaction, so a failure
    /// leaves the agent as it was. First, cli is checked to be the process
    /// inspect_pane found (same pid, same start time, still under the pane):
    /// a pid reused since is refused. Nothing is launched, typed or killed.
    ///
    /// A CLI started with the agent's hooks keeps the spawn id they report
    /// with; one started by hand has none, so its status comes from its
    /// screen (or, for Claude, its session registry and the global hook).
    /// Either way the agent holds messages as any spawned agent does
    /// (STATE_KEY_REVIVED_CLI): send never types into a dialog or a busy
    /// turn, and held messages go first, typed by the deliverer once it sits
    /// at its prompt.
    pub fn take_over_pane(&self, name: &str, cli: &PaneCli) -> Result<Agent> {
        let agent = self.get_agent(name)?;
        let end_reason = match self.reported_status(name)? {
            Some(r) if r.status == AGENT_STATUS_ENDED => r.end_reason,
            _ => self.resolve_status(name)?.0.end_reason,
        };
        let spawn_id = self.state_value(name, STATE_KEY_SPAWN_ID);
        let before_session = self.state_value(name, STATE_KEY_SESSION_ID);
        let now = Time::now();
        let entry = |key: &str, value: &str| StateEntry {
            agent_name: name.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            updated_at: now,
        };

        // Its run closes with the status it ended in, as resume closes it.
        let before = StateUpdate {
            entries: vec![
                entry(STATE_KEY_STATUS, AGENT_STATUS_ENDED.as_str()),
                entry(STATE_KEY_END_REASON, end_reason.as_str()),
            ],
            agent_name: name.to_string(),
            ..Default::default()
        };

        let mut set = vec![entry(
            STATE_KEY_REVIVED_CLI,
            &format!("{} {}", cli.pid, cli.start),
        )];
        if cli.hooked && !spawn_id.is_empty() {
            set.push(entry(STATE_KEY_SPAWN_ID, &spawn_id));
        }
        let mut event = None;
        let mut report = None;
        if let Some(status) = &cli.status
            && !cli.session_id.is_empty()
        {
            let r = StatusReport {
                status: status.clone(),
                event: EVENT_REVIVED.into(),
                detail: format!(
                    "revived: took over session {} its pane runs (pid {}, {})",
                    cli.session_id, cli.pid, cli.detail
                ),
                session_id: cli.session_id.clone(),
                ..Default::default()
            };
            let u = self.report_update(name, &r, &now)?;
            set.extend(u.entries);
            event = u.event;
            report = Some(r);
        } else if !cli.session_id.is_empty() {
            // No status of the CLI's own to record (agy): a report would
            // stand for good, since no hook of a hand-started CLI replaces
            // it. Only the session it runs is recorded.
            set.push(entry(STATE_KEY_SESSION_ID, &cli.session_id));
        }
        if !cli.transcript.is_empty() {
            set.push(entry(STATE_KEY_TRANSCRIPT, &cli.transcript));
        }
        let keep: HashSet<String> = set.iter().map(|e| e.key.clone()).collect();
        let mut del: Vec<String> = self
            .storage
            .list_state(name)
            .context("reading its state")?
            .into_iter()
            .map(|e| e.key)
            .filter(|k| k.starts_with('_') && !KEPT_ON_RESUME.contains(&k.as_str()))
            .collect();
        // The old run's reply and transcript are this one's only if it runs
        // the same session: not when it runs another, nor when nothing says
        // which (an agy before its first turn holds no conversation lock).
        let same_session = !cli.session_id.is_empty() && cli.session_id == before_session;
        if !same_session {
            del.extend([
                STATE_KEY_RESULT.to_string(),
                STATE_KEY_TRANSCRIPT.to_string(),
            ]);
        }
        del.retain(|k| !keep.contains(k));
        del.sort();
        del.dedup();
        let after = StateUpdate {
            entries: set,
            agent_name: name.to_string(),
            delete: del,
            event,
        };

        let revived = Agent {
            created_at: now,
            adopted: false,
            ..agent.clone()
        };
        // Last, so a pid reused since inspect_pane read it isn't recorded.
        self.check_still_runs(&agent, cli)?;
        self.storage
            .replace_agent_with(&revived, &before, &after)
            .context("recording the take-over")?;
        if let Some(r) = &report {
            self.publish_status(name, r, &now);
        }
        if !cli.transcript.is_empty() {
            let _ = self.record_transcript(name, &agent.backend, &cli.transcript);
        }
        if self.held_count(name) > 0 {
            let _ = self.start_deliverer(name);
        }
        self.get_agent(name)
    }

    /// checkStillRuns fails unless cli is still the process inspect_pane
    /// found: the same pid with the same start time, under the same pane.
    fn check_still_runs(&self, agent: &Agent, cli: &PaneCli) -> Result<()> {
        let pane_pid = self.pane_pid(&agent.session_name).unwrap_or(0);
        let tree = util::read_proc_tree().unwrap_or_default();
        let same = pane_pid == cli.pane_pid
            && proc_stat(cli.pid).is_some_and(|(_, start)| start == cli.start)
            && tree.depth(cli.pid, cli.pane_pid).is_some();
        if !same {
            bail!(
                "the {} revive found in {}'s pane (pid {}) is gone or isn't the same process any more: nothing was changed; agent revive {} looks again",
                agent.backend,
                agent.name,
                cli.pid,
                agent.name
            );
        }
        Ok(())
    }

    /// RevivedCLIGone says the agent has ended when the CLI a revive took
    /// over (STATE_KEY_REVIVED_CLI) no longer runs: a CLI started by hand
    /// fires no hook as it exits, and its screen alone would read as a bare
    /// shell (unknown) for good. None while it runs, or with none taken over.
    pub(crate) fn revived_cli_gone(&self, agent: &Agent) -> Option<StatusInfo> {
        let v = self.state_value(&agent.name, STATE_KEY_REVIVED_CLI);
        let (pid, start) = v.split_once(' ')?;
        let (pid, start) = (pid.parse::<i32>().ok()?, start.parse::<u64>().ok()?);
        if !cfg!(target_os = "linux") || proc_stat(pid).is_some_and(|(_, s)| s == start) {
            return None;
        }
        Some(StatusInfo {
            status: AGENT_STATUS_ENDED,
            end_reason: END_QUIT,
            source: STATUS_SOURCE_NATIVE.into(),
            detail: format!(
                "the {} revive took over (pid {pid}) has exited; agent revive {} brings it back",
                agent.backend, agent.name
            ),
            ..Default::default()
        })
    }

    /// SendRebrief sends a revived agent its re-brief (message, from the
    /// sender from) once it can take it: it waits up to wait for the agent to
    /// sit at an idle prompt with an empty input line, then sends it as send
    /// does, after any messages held for it. An agent at its workspace trust
    /// prompt (a resume into a folder its CLI hasn't trusted) gets it held,
    /// to go in once the prompt is approved; one that isn't ready when the
    /// wait ends gets it held too, for the deliverer to type once it is. It
    /// never types into a dialog, a busy turn, or a person's typing.
    pub fn send_rebrief(
        &self,
        ctx: &Ctx,
        name: &str,
        message: &str,
        from: &str,
        wait: Duration,
    ) -> Result<SendResult> {
        let deadline = Instant::now() + wait;
        loop {
            let agent = self.get_agent(name)?;
            let info = self.status_info(name)?;
            if info.status == AGENT_STATUS_ENDED {
                // send says why, in its words.
                return self.send(
                    ctx,
                    name,
                    message,
                    SendOptions {
                        now: false,
                        from: from.to_string(),
                        ..Default::default()
                    },
                );
            }
            if info.is_trust_prompt() {
                return self.hold_rebrief(
                    &agent,
                    &info,
                    message,
                    from,
                    HELD_UNTIL_TRUST.to_string(),
                );
            }
            let at_prompt = info.status == AGENT_STATUS_IDLE || info.status == AGENT_STATUS_ERROR;
            if at_prompt && !info.is_dialog() && self.input_blocked_of(&agent).is_empty() {
                if self.held_count(name) > 0 {
                    // Held messages go first, with the re-brief behind them.
                    let (typed, unchecked, res) = self.type_held(ctx, &agent, message);
                    res?;
                    return Ok(SendResult {
                        status: info.status,
                        detail: info.detail,
                        typed,
                        unchecked,
                        ..Default::default()
                    });
                }
                return self.send(
                    ctx,
                    name,
                    message,
                    SendOptions {
                        now: false,
                        from: from.to_string(),
                        ..Default::default()
                    },
                );
            }
            if Instant::now() >= deadline {
                let until = if self.holds_messages(&agent) {
                    self.delivery_point_of(&agent)
                } else {
                    HELD_UNTIL_PROMPT.to_string()
                };
                let mut res = self.hold_rebrief(&agent, &info, message, from, until)?;
                res.input_blocked = if at_prompt {
                    self.input_blocked_of(&agent)
                } else {
                    String::new()
                };
                return Ok(res);
            }
            ctx.sleep(REBRIEF_POLL)?;
        }
    }

    /// holdRebrief holds message for agent, found in info, to go in when
    /// until says, and starts the deliverer that types it then (unless its
    /// CLI's hook hands it over first).
    fn hold_rebrief(
        &self,
        agent: &Agent,
        info: &StatusInfo,
        message: &str,
        from: &str,
        until: String,
    ) -> Result<SendResult> {
        let mut msg = Message {
            sender_id: if from.is_empty() {
                "user".into()
            } else {
                from.to_string()
            },
            receiver_id: agent.name.clone(),
            content: message.to_string(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            ..Default::default()
        };
        self.storage
            .create_message(&mut msg)
            .context("holding the re-brief")?;
        let _ = self.start_deliverer(&agent.name); // best effort: agent deliver types it too
        Ok(SendResult {
            status: info.status.clone(),
            detail: info.detail.clone(),
            held: true,
            held_count: self.held_count(&agent.name).max(1),
            held_until: until,
            ..Default::default()
        })
    }

    /// typedByDeliverer reports whether messages held for agent are typed by
    /// the deliverer rather than handed over by its CLI's hook: a CLI revive
    /// took over that runs neither the hooks spawn wired nor the global hook.
    pub(crate) fn typed_by_deliverer(&self, agent: &Agent) -> bool {
        if self
            .state_value(&agent.name, STATE_KEY_REVIVED_CLI)
            .is_empty()
            || !self.state_value(&agent.name, STATE_KEY_SPAWN_ID).is_empty()
        {
            return false;
        }
        let hooked = self.state_value(&agent.name, super::status::STATE_KEY_HOOKED_SESSION);
        hooked.is_empty() || hooked != self.state_value(&agent.name, STATE_KEY_SESSION_ID)
    }
}

/// The program a backend's launch command runs, as written there
/// ("claude" for "claude --settings '...'", "agy" for agy's, which sets
/// environment variables first; a path when the command names one).
fn cli_program(launch: &str) -> String {
    let mut words = util::split_shell_words(launch);
    if words.is_empty() {
        // A line split_shell_words doesn't take (a list, `a; b`) still
        // starts with the program.
        words = launch.split_whitespace().map(String::from).collect();
    }
    let is_assignment = |w: &str| {
        w.split_once('=').is_some_and(|(k, _)| {
            !k.is_empty()
                && !k.starts_with(|c: char| c.is_ascii_digit())
                && k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
    };
    // Past `env`, its options (`-u NAME`, as agy's launch has) and
    // assignments.
    let mut it = words.iter();
    let mut in_env = false;
    while let Some(w) = it.next() {
        if w == "env" && !in_env {
            in_env = true;
            continue;
        }
        if is_assignment(w) {
            continue;
        }
        if in_env && w.starts_with('-') {
            if matches!(w.as_str(), "-u" | "--unset" | "-C" | "--chdir") {
                it.next();
            }
            continue;
        }
        return w.clone();
    }
    String::new()
}

/// The base name of the program a backend's launch command runs.
fn cli_name(launch: &str) -> String {
    let p = cli_program(launch);
    if p.is_empty() { p } else { filepath::base(&p) }
}

/// The files the backend's CLI is, resolved (symlinks followed): cli itself
/// when it is a path, else the first `cli` on this process's PATH and the
/// first on the PATH the pane's process started with (where a person typing
/// it there finds it).
fn cli_paths(cli: &str, pane_pid: i32) -> HashSet<PathBuf> {
    let mut out = HashSet::new();
    if cli.contains('/') {
        if let Ok(p) = std::fs::canonicalize(cli) {
            out.insert(p);
        }
        return out;
    }
    let mut paths = vec![std::env::var("PATH").unwrap_or_default()];
    if let Some(p) = proc_env(pane_pid, "PATH") {
        paths.push(p);
    }
    for path in paths {
        if let Some(found) = which(cli, &path) {
            out.insert(found);
        }
    }
    out
}

/// The first executable file named prog in the PATH-style list path,
/// resolved.
fn which(prog: &str, path: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let p = Path::new(dir).join(prog);
        if std::fs::metadata(&p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0) {
            return std::fs::canonicalize(&p).ok();
        }
    }
    None
}

/// A process's environment variable key, from what it started with.
fn proc_env(pid: i32, key: &str) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    data.split(|&b| b == 0)
        .filter_map(|kv| std::str::from_utf8(kv).ok())
        .find_map(|kv| {
            kv.strip_prefix(key)
                .and_then(|r| r.strip_prefix('='))
                .map(String::from)
        })
}

/// A process revive checked: its command line and start time.
struct Checked {
    args: Vec<String>,
    start: u64,
}

/// A process's parent pid and start time (clock ticks after boot), from
/// /proc/<pid>/stat; None where there is no /proc or no such process.
fn proc_stat(pid: i32) -> Option<(i32, u64)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command, in parentheses, may hold spaces and parentheses itself.
    let rest = &stat[stat.rfind(')')? + 1..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    Some((f.get(1)?.parse().ok()?, f.get(19)?.parse().ok()?))
}

/// The real, effective, saved and filesystem uids of a process.
fn proc_uids(pid: i32) -> Option<Vec<u32>> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    line.split_whitespace().map(|u| u.parse().ok()).collect()
}

/// checkProcess checks that pid can be taken over: it runs under pane_pid,
/// as this user (every uid of it), and, when cli is given as (name, the
/// arguments it was given), in an interactive session rather than a
/// one-shot command. Err says why not.
fn check_process(
    pid: i32,
    pane_pid: i32,
    tree: &ProcTree,
    cli: Option<(&str, &[String])>,
) -> Result<Checked, String> {
    if pid == pane_pid || tree.depth(pid, pane_pid).is_none() {
        return Err("doesn't run under its pane's shell".to_string());
    }
    let Some((_, start)) = proc_stat(pid) else {
        return Err(
            "can't be read in /proc, so revive can't tell it is the process it saw".to_string(),
        );
    };
    let me = unsafe { libc::geteuid() };
    match proc_uids(pid) {
        Some(uids) if !uids.is_empty() && uids.iter().all(|&u| u == me) => {}
        Some(uids) => {
            return Err(format!(
                "runs as another user (uid {})",
                uids.first().copied().unwrap_or(0)
            ));
        }
        None => return Err("its owner can't be read".to_string()),
    }
    let args = util::proc_args(pid).unwrap_or_default();
    let one_shot = match cli {
        Some((name, cli_args)) => one_shot(cli_args).map(|w| format!("{name} {w}")),
        // Claude, proven by its registry: a print-mode run is no session to
        // take over either.
        None => args
            .iter()
            .skip(1)
            .find(|a| matches!(a.as_str(), "-p" | "--print"))
            .map(|w| format!("claude {w}")),
    };
    if let Some(cmd) = one_shot {
        return Err(format!(
            "runs a one-shot command ({cmd} ...), not an interactive session"
        ));
    }
    Ok(Checked { args, start })
}

/// identify says whether pid runs the backend's CLI (one of paths, resolved):
/// its executable is one of them, or it is an interpreter running one of
/// them as a script, the interpreter the script's `#!` line names. It
/// returns the arguments the CLI was given, or why a process that looks like
/// the CLI (named name) isn't taken for it; None for any other process.
fn identify(pid: i32, paths: &HashSet<PathBuf>, name: &str) -> Option<Result<Vec<String>, String>> {
    let args = util::proc_args(pid)?;
    let exe = std::fs::read_link(format!("/proc/{pid}/exe")).ok();
    let cwd = std::fs::read_link(format!("/proc/{pid}/cwd")).ok();
    let exe_resolved = exe.as_ref().and_then(|e| std::fs::canonicalize(e).ok());
    if exe_resolved.as_ref().is_some_and(|e| paths.contains(e)) {
        return Some(Ok(args[1..].to_vec()));
    }
    let script = script_arg(&args);
    if let Some((i, s)) = &script {
        let p = Path::new(s);
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.clone().unwrap_or_default().join(p)
        };
        if let Ok(resolved) = std::fs::canonicalize(&p)
            && paths.contains(&resolved)
        {
            if exe_resolved
                .as_ref()
                .is_some_and(|e| runs_script(&resolved, e))
            {
                return Some(Ok(args[i + 1..].to_vec()));
            }
            return Some(Err(format!(
                "runs {} with an interpreter its #! line doesn't name",
                resolved.display()
            )));
        }
    }
    let named = filepath::base(args[0].trim_start_matches('-')) == name
        || script
            .as_ref()
            .is_some_and(|(_, s)| filepath::base(s) == name)
        || exe
            .as_ref()
            .is_some_and(|e| filepath::base(&e.to_string_lossy()) == name);
    if !named {
        return None;
    }
    if exe
        .as_ref()
        .is_some_and(|e| e.to_string_lossy().ends_with(" (deleted)"))
    {
        return Some(Err("runs an executable deleted since it started (an update?), so revive can't tell it is the backend's CLI".into()));
    }
    let mut on_path: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    on_path.sort();
    let on_path = if on_path.is_empty() {
        "none found".to_string()
    } else {
        on_path.join(", ")
    };
    Some(Err(format!("isn't the {name} on PATH ({on_path})")))
}

/// The script an interpreter's command line runs: the first argument after
/// the interpreter that isn't an option, with its index.
fn script_arg(args: &[String]) -> Option<(usize, String)> {
    args.iter()
        .enumerate()
        .skip(1)
        .find(|(_, a)| !a.starts_with('-'))
        .map(|(i, a)| (i, a.clone()))
}

/// Whether exe (resolved) is the interpreter script's `#!` line names
/// (`#!/usr/bin/env node` naming node as PATH finds it).
fn runs_script(script: &Path, exe: &Path) -> bool {
    use std::io::Read;
    let mut head = [0u8; 256];
    let n = std::fs::File::open(script)
        .and_then(|mut f| f.read(&mut head))
        .unwrap_or(0);
    let head = String::from_utf8_lossy(&head[..n]);
    let Some(line) = head.strip_prefix("#!").and_then(|h| h.lines().next()) else {
        return false;
    };
    let mut words = line.split_whitespace();
    let Some(interp) = words.next() else {
        return false;
    };
    if std::fs::canonicalize(interp).is_ok_and(|p| p == exe) {
        return true;
    }
    if filepath::base(interp) == "env"
        && let Some(prog) = words.find(|w| !w.starts_with('-'))
    {
        let path = std::env::var("PATH").unwrap_or_default();
        return which(prog, &path).is_some_and(|p| p == exe);
    }
    false
}

/// Options of claude's and agy's that take a value, so that value isn't
/// taken for a subcommand.
const VALUE_OPTIONS: &[&str] = &[
    "--add-dir",
    "--conversation",
    "--model",
    "--effort",
    "--settings",
    "--resume",
    "-r",
    "--session-id",
    "--mcp-config",
    "--permission-mode",
    "--append-system-prompt",
    "--system-prompt",
    "--allowedTools",
    "--disallowedTools",
    "--allowed-tools",
    "--disallowed-tools",
    "--fallback-model",
    "--plugin-dir",
    "--setting-sources",
    "--agent",
    "--agents",
];

/// Subcommands of claude's and agy's that run once and exit.
const ONE_SHOT_COMMANDS: &[&str] = &[
    "auth",
    "login",
    "logout",
    "changelog",
    "models",
    "mcp",
    "config",
    "doctor",
    "update",
    "upgrade",
    "install",
    "uninstall",
    "migrate-installer",
    "setup-token",
    "plugin",
    "plugins",
    "version",
    "help",
    "completion",
];

/// Says how args (what the CLI was given) make it a one-shot run, if they
/// do: a print or help or version flag, or a subcommand as its first word.
fn one_shot(args: &[String]) -> Option<String> {
    let mut takes_value = false;
    for a in args {
        if takes_value {
            takes_value = false;
            continue;
        }
        if a == "--" {
            return None;
        }
        if matches!(
            a.as_str(),
            "-p" | "--print" | "-h" | "--help" | "-v" | "--version"
        ) {
            return Some(a.clone());
        }
        if a.starts_with('-') {
            takes_value = !a.contains('=') && VALUE_OPTIONS.contains(&a.as_str());
            continue;
        }
        // The first word that isn't an option: a subcommand, or a prompt.
        return ONE_SHOT_COMMANDS.contains(&a.as_str()).then(|| a.clone());
    }
    None
}

/// Whether pid looks like the CLI named name (its command or the script an
/// interpreter runs), unverified: only to say why revive leaves it be.
fn names_cli(pid: i32, name: &str) -> bool {
    util::proc_args(pid).is_some_and(|args| {
        filepath::base(args[0].trim_start_matches('-')) == name
            || script_arg(&args).is_some_and(|(_, s)| filepath::base(&s) == name)
    })
}

/// A PaneProcess for pid, its command line read now.
fn process(pid: i32, why: String) -> PaneProcess {
    let command = util::proc_args(pid)
        .map(|a| util::shell_join(&a))
        .unwrap_or_default();
    PaneProcess { pid, command, why }
}

/// Shells a pane runs, as their base names.
const SHELLS: &[&str] = &[
    "sh", "bash", "dash", "zsh", "fish", "ksh", "mksh", "oksh", "pdksh", "tcsh", "csh", "ash",
    "yash", "rbash", "nu", "elvish", "xonsh",
];

/// Whether pid runs a shell (its executable, or its argv[0], a login
/// shell's "-bash" too).
fn is_shell(pid: i32) -> bool {
    let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|e| filepath::base(&e.to_string_lossy()));
    let arg0 = util::proc_args(pid)
        .and_then(|a| a.first().cloned())
        .map(|a| filepath::base(a.trim_start_matches('-')));
    [exe, arg0]
        .into_iter()
        .flatten()
        .any(|b| SHELLS.contains(&b.as_str()))
}

/// Whether args pass dir to the CLI as a workspace (`--add-dir <dir>`, as
/// agy's spawn hooks do: the agent's own dir, or a symlink to it).
fn adds_dir(args: &[String], dir: &str) -> bool {
    let same = |p: &str| {
        let resolve = |s: &str| filepath::eval_symlinks(s).unwrap_or_else(|_| filepath::clean(s));
        resolve(p) == resolve(dir)
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--add-dir" {
            if it.next().is_some_and(|v| same(v)) {
                return true;
            }
        } else if let Some(v) = a.strip_prefix("--add-dir=")
            && same(v)
        {
            return true;
        }
    }
    false
}

/// The conversation an agy process runs, from the presence lock it holds
/// open (`<agy home>/presence/<conversation>.lock`, see agent-discovery);
/// "" when it holds none, or its open files can't be read.
fn presence_lock_session(pid: i32) -> String {
    let Ok(fds) = std::fs::read_dir(format!("/proc/{pid}/fd")) else {
        return String::new();
    };
    let mut ids: Vec<String> = fds
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .filter_map(|target| {
            let p = Path::new(&target);
            let in_presence = p
                .parent()
                .and_then(|d| d.file_name())
                .is_some_and(|n| n == "presence");
            let lock = p.extension().is_some_and(|e| e == "lock");
            if !in_presence || !lock {
                return None;
            }
            p.file_stem().map(|s| s.to_string_lossy().into_owned())
        })
        .collect();
    ids.sort();
    ids.dedup();
    if ids.len() == 1 {
        ids.pop().unwrap_or_default()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    fn words(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn cli_name_is_the_launch_commands_program() {
        assert_eq!(cli_name("claude --settings '{}'"), "claude");
        assert_eq!(cli_name("agy --add-dir /x; agent report"), "agy");
        assert_eq!(cli_name("'/opt/my bin/agy' -c"), "agy");
        assert_eq!(cli_program("'/opt/my bin/agy' -c"), "/opt/my bin/agy");
        assert_eq!(cli_name("FOO=1 BAR_2='a b' agy --settings x"), "agy");
        assert_eq!(cli_name("env A=1 claude"), "claude");
        assert_eq!(
            cli_name("env -u SSH_CONNECTION -u SSH_TTY agy --settings '{}'"),
            "agy"
        );
        assert_eq!(cli_name(""), "");
    }

    // 2.4: a one-shot subcommand or flag is no session to take over; a
    // prompt or a session's options are.
    #[test]
    fn one_shot_commands() {
        for (args, want) in [
            ("", None),
            ("--conversation 1234", None),
            ("--add-dir /a/agents/x --model m", None),
            ("--model changelog", None),
            ("fix the build", None),
            ("--dangerously-skip-permissions", None),
            ("changelog", Some("changelog")),
            ("auth login", Some("auth")),
            ("--model m models", Some("models")),
            ("-p hello", Some("-p")),
            ("--print", Some("--print")),
            ("--version", Some("--version")),
            ("mcp list", Some("mcp")),
            ("-- changelog", None),
        ] {
            assert_eq!(one_shot(&words(args)).as_deref(), want, "{args:?}");
        }
    }

    // 2.4: a process run by another user is never taken over (pid 1 is
    // root's, unless these tests run as root), nor one outside the pane.
    #[test]
    fn check_process_refuses_other_users_and_strangers() {
        let me = std::process::id() as i32;
        let tree = util::read_proc_tree().unwrap();
        let parent = tree.get(&me).copied().unwrap();
        if unsafe { libc::geteuid() } != 0 {
            let mut t = tree.clone();
            t.insert(1, parent); // as if pid 1 ran in the pane
            let e = check_process(1, parent, &t, None).err().unwrap();
            assert!(
                e.starts_with("runs as another user (uid 0)") || e.contains("can't be read"),
                "{e}"
            );
        }
        assert_eq!(
            check_process(me, 999_999, &tree, None).err().as_deref(),
            Some("doesn't run under its pane's shell")
        );
        let ok = check_process(me, parent, &tree, None).unwrap();
        assert_eq!(Some(ok.start), proc_stat(me).map(|(_, s)| s));
    }

    // 2.3, 2.4: a process is the CLI when its executable, resolved, is the
    // CLI on PATH, or it is the interpreter the CLI's #! line names running
    // it, whatever flags the interpreter got. Another file of the same name,
    // or the CLI's path handed to some other program, is not.
    #[test]
    fn identify_resolves_what_runs() {
        let dir = tempfile::Builder::new()
            .prefix("svc-revive")
            .tempdir_in("/var/tmp")
            .unwrap();
        let script = |rel: &str, body: &str| {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, body).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let agy = script("bin/agy", "#!/bin/bash\nsleep 30\n");
        let other = script("other/agy", "#!/bin/bash\nsleep 30\n");
        let link = dir.path().join("link-agy");
        std::os::unix::fs::symlink(&agy, &link).unwrap();
        let paths: HashSet<PathBuf> = [std::fs::canonicalize(&agy).unwrap()].into();
        let run = |argv: &[&str]| {
            let mut c = std::process::Command::new(argv[0]);
            c.args(&argv[1..])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let child = c.spawn().unwrap();
            std::thread::sleep(Duration::from_millis(150));
            child
        };
        let a = agy.to_str().unwrap();
        let o = other.to_str().unwrap();
        let l = link.to_str().unwrap();
        for (argv, want) in [
            (vec![a], Ok(vec![])),
            (
                vec![a, "--conversation", "x"],
                Ok(vec!["--conversation", "x"]),
            ),
            (vec!["bash", "--noprofile", a, "-c"], Ok(vec!["-c"])),
            (vec![l], Ok(vec![])),
            (vec![o], Err("isn't the agy on PATH")),
            // The CLI's path handed to some other program (as `vim agy`).
            (
                vec!["tail", "-f", a],
                Err("with an interpreter its #! line doesn't name"),
            ),
        ] {
            let mut child = run(&argv);
            let got = identify(child.id() as i32, &paths, "agy");
            let _ = child.kill();
            let _ = child.wait();
            match (got, want) {
                (Some(Ok(args)), Ok(w)) => assert_eq!(args, w, "{argv:?}"),
                (Some(Err(why)), Err(w)) => assert!(why.contains(w), "{argv:?}: {why}"),
                (got, want) => panic!("{argv:?}: got {got:?}, want {want:?}"),
            }
        }
        // Not named agy at all: not the CLI, no reason needed.
        let mut child = run(&["sleep", "30"]);
        assert_eq!(identify(child.id() as i32, &paths, "agy"), None);
        let _ = child.kill();
        let _ = child.wait();
    }

    /// A g3d-termd that doesn't answer, and fails the test if anything would
    /// kill or type.
    struct DownTerm;
    impl agent_termd::Ops for DownTerm {
        fn create_session(
            &self,
            _: &str,
            _: &std::collections::HashMap<String, String>,
        ) -> Result<()> {
            panic!("created a session")
        }
        fn create_session_in(
            &self,
            _: &str,
            _: &str,
            _: &std::collections::HashMap<String, String>,
        ) -> Result<()> {
            panic!("created a session")
        }
        fn kill_session(&self, _: &str) -> Result<()> {
            panic!("killed a session")
        }
        fn respawn_pane(&self, _: &str) -> Result<()> {
            panic!("respawned a pane")
        }
        fn session_exists(&self, _: &str) -> Result<bool> {
            Err(anyhow!("termd: connection refused"))
        }
        fn list_sessions(&self) -> Result<Vec<agent_termd::Session>> {
            Err(anyhow!("termd: connection refused"))
        }
        fn send_keys(&self, _: &str, _: &str) -> Result<()> {
            panic!("typed")
        }
        fn paste_text(&self, _: &str, _: &str) -> Result<()> {
            panic!("typed")
        }
        fn send_special_key(&self, _: &str, _: &str) -> Result<()> {
            panic!("typed")
        }
        fn capture(&self, _: &str, _: i32) -> Result<String> {
            Err(anyhow!("termd: connection refused"))
        }
        fn capture_styled(&self, _: &str, _: i32) -> Result<String> {
            Err(anyhow!("termd: connection refused"))
        }
        fn pane_pids(&self) -> Result<std::collections::HashMap<i32, String>> {
            Err(anyhow!("termd: connection refused"))
        }
    }

    // 2.3 (2): a pane revive can't read (termd didn't answer whether the
    // session exists) is a refusal that says why, never a resume that kills
    // the session once termd answers again.
    #[test]
    fn unreadable_pane_is_refused() {
        let dir = tempfile::Builder::new()
            .prefix("svc-revive")
            .tempdir_in("/var/tmp")
            .unwrap();
        let store = std::sync::Arc::new(agent_store::Store::open(dir.path().join("t.db")).unwrap());
        let cfg = agent_base::config::Config {
            host: "h".into(),
            ..Default::default()
        };
        let s = AgentService::new(
            cfg,
            store.clone(),
            std::sync::Arc::new(DownTerm),
            std::sync::Arc::new(agent_backend::new_registry()),
        );
        store
            .create_agent(&Agent {
                name: "ag".into(),
                session_name: "agent-ag".into(),
                backend: "agy".into(),
                created_at: Time::now(),
                ..Default::default()
            })
            .unwrap();
        s.report(
            "ag",
            StatusReport {
                status: AGENT_STATUS_ENDED,
                end_reason: END_QUIT,
                session_id: "c1".into(),
                ..Default::default()
            },
        )
        .unwrap();
        match s.revive_case("ag").unwrap() {
            ReviveCase::Refuse(p) => assert_eq!(
                p,
                PaneProcess {
                    pid: 0,
                    command: String::new(),
                    why: "checking whether its session agent-ag exists: termd: connection refused"
                        .into()
                }
            ),
            other => panic!("revive_case = {other:?}, want a refusal"),
        }
    }

    #[test]
    fn adds_dir_names_the_agents_dir() {
        let dir = tempfile::Builder::new()
            .prefix("svc-revive")
            .tempdir_in("/var/tmp")
            .unwrap();
        let agent_dir = dir.path().join("agents/ag");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&agent_dir, &link).unwrap();
        let a = agent_dir.to_string_lossy().into_owned();
        let l = link.to_string_lossy().into_owned();
        assert!(adds_dir(&words(&format!("agy --add-dir {a}")), &a));
        assert!(
            adds_dir(&words(&format!("agy --add-dir {l}")), &a),
            "through the symlink spawn makes"
        );
        assert!(adds_dir(&words(&format!("agy --add-dir={a}/")), &a));
        assert!(!adds_dir(&words("agy --add-dir /elsewhere"), &a));
        assert!(!adds_dir(&words("agy"), &a));
    }
}
