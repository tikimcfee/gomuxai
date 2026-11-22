//! Go `internal/service/status.go`: status reports, and resolving an agent's
//! status from its backend CLI, its reports, and its screen.

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

pub use agent_backend::session_name;
use agent_backend::{
    END_GONE, END_QUIT, EndReason, HookReport, PermissionResponse, PromptChange, Provider,
    ProviderConfig, Status,
};
use agent_base::gostd::{filepath, strconv};
use agent_base::types::{END_REASONS, meaning_of};
use agent_discovery as discovery;
use agent_store::{AGENT_STATUS, Agent, AgentEvent, StateEntry, StateUpdate, Time};

use super::agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_ERROR, AGENT_STATUS_IDLE, AGENT_STATUS_PROCESSING,
    AGENT_STATUS_UNKNOWN, AGENT_STATUS_WAITING, AgentService, AgentStatus,
};
use super::trust::trust_starting;
use super::{since, time_from_unix_nano};

// Reserved state keys written by status reporting. The leading underscore
// keeps them clear of keys agents set for their own coordination.
pub const STATE_KEY_STATUS: &str = "_status";
pub const STATE_KEY_EVENT: &str = "_event";
pub const STATE_KEY_DETAIL: &str = "_detail";
pub const STATE_KEY_RESULT: &str = "_result";
pub const STATE_KEY_SESSION_ID: &str = "_session_id";
/// StateKeyEndReason says how the session ended, while the status is
/// ended (backend.EndReason); "" otherwise.
pub const STATE_KEY_END_REASON: &str = "_end_reason";
/// tool the last event concerned, e.g. the one awaiting permission
pub const STATE_KEY_TOOL: &str = "_tool";
/// that tool call's full input (JSON), when it may need permission
pub const STATE_KEY_TOOL_INPUT: &str = "_tool_input";
/// StateKeyTranscript is the path of the CLI's own transcript of the
/// session, from the last hook payload that named one.
pub const STATE_KEY_TRANSCRIPT: &str = "_transcript";
/// StateKeySpawnID identifies one spawn of an agent name. Hook commands
/// carry it, so hooks from a previous agent with the same name (e.g. the
/// SessionEnd fired while cleanup kills it) can't write into a respawn.
pub const STATE_KEY_SPAWN_ID: &str = "_spawn_id";
/// StateKeyHookedSession is the session the global hook (agent hook) last
/// reported for the agent: while it is the agent's _session_id, the CLI
/// running it carries gomuxai's hooks, so a Stop can hand over held
/// messages. A CLI started before agent hooks install, or by hand without
/// them, never sets it.
pub const STATE_KEY_HOOKED_SESSION: &str = "_hooked_session";
/// StateKeySentAt is when a message was last typed into the agent (send,
/// a queued delivery, or spawn's first message), and StateKeyTurnAt when
/// its last turn started (a report that is processing and clears the
/// result). wait --next compares them to catch a turn that started after
/// a send and ended before the wait began.
pub const STATE_KEY_SENT_AT: &str = "_sent_at";
pub const STATE_KEY_TURN_AT: &str = "_turn_at";
/// StateKeyNowAt marks a `send --now` typed into a harness (such as agy with
/// `queuedMessages: "send-immediately"`) that folds queued input into the
/// running turn (`PreInvocation` with `invocationNum >= 1`) without firing a
/// separate prompt-submit hook. Consumed on the next `PreInvocation` /
/// `UserPromptSubmit` so `_turn_at` advances past `_sent_at` for `wait --next`.
pub const STATE_KEY_NOW_AT: &str = "_now_at";
/// StateKeyModel is the model spawn --model started the agent on; resume
/// relaunches on it. Unset for the CLI's default.
pub const STATE_KEY_MODEL: &str = "_model";
/// StateKeyEffort is the effort spawn --effort started the agent at; resume
/// relaunches at it. Unset for the CLI's default.
pub const STATE_KEY_EFFORT: &str = "_effort";
/// StateKeyNamed holds the conversation/session id that this run has already
/// named via `/rename <agent.name>` (or confirmed already carries that title),
/// so an agy session is named at most once per conversation.
pub const STATE_KEY_NAMED: &str = "_named";

// Status sources, from most to least authoritative.
/// the backend CLI's own session status
pub const STATUS_SOURCE_NATIVE: &str = "native";
/// hooks or `agent report`
pub const STATUS_SOURCE_REPORT: &str = "report";
/// regex over captured terminal output
pub const STATUS_SOURCE_SCRAPE: &str = "scrape";

/// StatusReport is a status update pushed by an agent or its backend's hooks.
#[derive(Debug, Clone, Default)]
pub struct StatusReport {
    pub status: AgentStatus,
    /// with a status of ended: how the session ended
    pub end_reason: EndReason,
    pub event: String,
    pub detail: String,
    pub tool: String,
    /// full input (JSON) of the tool call that may need permission
    pub tool_input: String,
    /// only stored when non-empty
    pub result: String,
    /// only stored when non-empty
    pub session_id: String,
    /// Transcript is the path of the CLI's transcript file; only stored
    /// when non-empty.
    pub transcript: String,
    /// ClearTranscript empties the stored transcript path: the session
    /// changed to one whose transcript no hook has named yet.
    pub clear_transcript: bool,
    /// ClearResult empties the stored result (a new turn started), so a
    /// reader never takes the previous turn's reply for this one's.
    pub clear_result: bool,
    /// SpawnID, when non-empty, drops the report unless the agent's
    /// current _spawn_id matches, so a stale exit wrapper can't overwrite
    /// a respawned agent.
    pub spawn_id: String,

    /// Prompt, CallKey, and Agent update the pending permission prompts
    /// (see backend.HookReport).
    pub prompt: PromptChange,
    pub call_key: String,
    pub agent: String,
    /// DropPrompts are pending prompts' state keys to delete: the prompts
    /// approve or deny answered.
    pub drop_prompts: Vec<String>,
    /// Message IDs delivered or reported superseded on `event == "delivered"`.
    pub delivered_ids: Vec<i64>,
    pub superseded_ids: Vec<i64>,
}

/// stateKeyPromptPrefix starts the key of each pending permission prompt,
/// followed by the time it was raised (zero-padded nanoseconds, so keys sort
/// oldest first). Each prompt has its own key, so concurrent hooks add and
/// remove prompts without losing each other's.
pub(crate) const STATE_KEY_PROMPT_PREFIX: &str = "_prompt.";

fn is_empty(s: &str) -> bool {
    s.is_empty()
}

/// pendingPrompt is a permission prompt raised and not yet answered, stored
/// as JSON under its STATE_KEY_PROMPT_PREFIX key.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct PendingPrompt {
    #[serde(skip)]
    pub(crate) key: String,
    #[serde(default)]
    pub(crate) call: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub(crate) agent: String,
    #[serde(default)]
    pub(crate) tool: String,
    #[serde(default, skip_serializing_if = "is_empty")]
    pub(crate) input: String,
    #[serde(default)]
    pub(crate) detail: String,
}

/// StatusInfo describes an agent's status and where it came from.
#[derive(Debug, Clone, Default)]
pub struct StatusInfo {
    pub status: AgentStatus,
    /// with a status of ended: how the session ended
    pub end_reason: EndReason,
    pub source: String,
    pub event: String,
    pub detail: String,
    pub tool: String,
    /// full input (JSON) of the tool call a permission prompt is about
    pub tool_input: String,
    /// time of the last report; zero for other sources
    pub updated_at: Time,

    /// The last recorded report, whatever Status was resolved from. Status can
    /// lag it (native status trails hooks) or override it (interrupts).
    pub reported_status: AgentStatus,
    pub reported_at: Time,

    /// When a message was last typed into the agent, and when its last turn
    /// started; zero if never, or if the agent has never reported.
    pub sent_at: Time,
    pub turn_at: Time,

    /// Held is how many messages wait for the agent: held for the end of its
    /// turn (see send), or queued for agent watch.
    pub held: usize,

    /// Model is the model spawn --model started the agent on; "" for the
    /// CLI's default.
    pub model: String,

    /// Effort is the effort spawn --effort started the agent at; "" for the
    /// CLI's default.
    pub effort: String,

    /// NativeErr says why the backend CLI's own status couldn't be read,
    /// when that is why another source answered.
    pub native_err: String,

    /// unanswered permission prompts, oldest first
    pub(crate) pending: Vec<PendingPrompt>,
    /// while waiting: the one on screen, which Tool and Detail describe
    pub(crate) prompt: Option<PendingPrompt>,
}

/// Validates a status name.
pub fn parse_agent_status(s: &str) -> Result<AgentStatus> {
    let st = AgentStatus::from(s);
    for known in [
        AGENT_STATUS_IDLE,
        AGENT_STATUS_PROCESSING,
        AGENT_STATUS_WAITING,
        AGENT_STATUS_ERROR,
        AGENT_STATUS_ENDED,
        AGENT_STATUS_UNKNOWN,
    ] {
        if st == known {
            return Ok(known);
        }
    }
    bail!(
        "invalid status {} (want idle, processing, waiting, error, ended, or unknown; agent statuses says what each means)",
        strconv::quote(s)
    )
}

/// Validates an end reason for status: one of backend's END_REASONS with
/// ended (quit when empty), none with any other.
pub fn parse_end_reason(status: &AgentStatus, reason: &str) -> Result<EndReason> {
    if *status != AGENT_STATUS_ENDED {
        if !reason.is_empty() {
            bail!("an end reason goes only with the status ended, not {status}");
        }
        return Ok(EndReason::default());
    }
    if reason.is_empty() {
        return Ok(END_QUIT);
    }
    if meaning_of(END_REASONS, reason).is_empty() {
        let names: Vec<&str> = END_REASONS.iter().map(|m| m.name).collect();
        bail!(
            "invalid end reason {} (want {}; agent statuses says what each means)",
            strconv::quote(reason),
            names.join(", ")
        );
    }
    Ok(EndReason::from(reason.to_string()))
}

pub(crate) fn from_backend_status(st: Status) -> AgentStatus {
    AgentStatus::from_static(st.as_str())
}

/// turnEndLag is how long a report must stand before the screen can say the
/// turn it describes has ended: right after a hook, the CLI may not have
/// redrawn yet.
const TURN_END_LAG: std::time::Duration = std::time::Duration::from_secs(3);

/// Bounds how long agy's Stop waits for the CLI to flush the turn's closing
/// reply to its transcript before recording `_result`.
const AGY_RESULT_WAIT: std::time::Duration = std::time::Duration::from_millis(200);
const AGY_RESULT_POLL: std::time::Duration = std::time::Duration::from_millis(15);

/// Reads the closing reply of the latest turn from an agy transcript, or `None`
/// if no closing `say` event stands at the end of the transcript yet (the file
/// hasn't been flushed yet, or a prompt/tool/think event came after the last
/// reply).
fn read_turn_final_reply(path: &str, reader: &dyn agent_transcript::Reader) -> Option<String> {
    let file = agent_transcript::read_file(path, Some(reader), 32).ok()?;
    let mut reply: Option<String> = None;
    for e in &file.events {
        if e.op == agent_transcript::OP_ADD {
            match e.kind.as_str() {
                agent_transcript::KIND_SAY => {
                    if let Some(say) = &e.say
                        && say.r#final
                        && !say.text.is_empty()
                    {
                        reply = Some(say.text.clone());
                    } else {
                        reply = None;
                    }
                }
                agent_transcript::KIND_PROMPT
                | agent_transcript::KIND_TOOL
                | agent_transcript::KIND_THINK => {
                    reply = None;
                }
                _ => {}
            }
        } else if e.op == agent_transcript::OP_UPDATE && e.kind == agent_transcript::KIND_TOOL {
            reply = None;
        }
    }
    reply
}

/// Reads the final reply at agy's Stop, retrying briefly if the transcript
/// hasn't flushed the closing `PLANNER_RESPONSE` line yet.
fn read_agy_stop_result(path: &str, reader: &dyn agent_transcript::Reader) -> Option<String> {
    if let Some(reply) = read_turn_final_reply(path, reader) {
        return Some(reply);
    }
    let deadline = std::time::Instant::now() + AGY_RESULT_WAIT;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(AGY_RESULT_POLL);
        if let Some(reply) = read_turn_final_reply(path, reader) {
            return Some(reply);
        }
    }
    None
}

/// EventAdopted is the event of the report that records an agent's pane
/// running a session other than the one on record.
pub const EVENT_ADOPTED: &str = "adopted";

/// EventSessionMoved is the event of the report that moves an agent onto
/// the session its CLI carried its conversation into (follow_moved_session).
pub const EVENT_SESSION_MOVED: &str = "SessionMoved";

/// PermissionOptions guards respond_permission against answering the wrong prompt.
#[derive(Debug, Clone, Default)]
pub struct PermissionOptions {
    /// Force sends the key even if the agent isn't waiting, is waiting on a
    /// dialog the backend won't treat as a permission prompt, or shows no
    /// prompt on screen.
    pub force: bool,
    /// ExpectTool, if set, refuses unless the pending prompt is for this tool,
    /// so a prompt that replaced the one you inspected isn't answered blind.
    pub expect_tool: String,
}

/// promptsIn picks the pending prompts out of an agent's state entries,
/// oldest first.
pub(crate) fn prompts_in(entries: &[StateEntry]) -> Vec<PendingPrompt> {
    let mut pending: Vec<PendingPrompt> = Vec::new();
    for e in entries {
        if !e.key.starts_with(STATE_KEY_PROMPT_PREFIX) {
            continue;
        }
        if let Ok(mut p) = serde_json::from_str::<PendingPrompt>(&e.value) {
            p.key = e.key.clone();
            pending.push(p);
        }
    }
    pending.sort_by(|a, b| a.key.cmp(&b.key));
    pending
}

/// unloadedBackend is the error for an agent whose custom backend this
/// process didn't load: custom backends are only known through the
/// --backend-config they were spawned with.
pub(crate) fn unloaded_backend(agent: &Agent) -> anyhow::Error {
    anyhow!(
        "agent {} runs backend {}, which isn't built in: pass the --backend-config file it was spawned with (e.g. agent --backend-config <file> status {})",
        agent.name,
        strconv::quote(&agent.backend),
        agent.name
    )
}

impl AgentService {
    /// Report records a status update for an agent. All keys are written in one
    /// transaction, so concurrent reports can't leave a mix of both.
    pub fn report(&self, name: &str, r: StatusReport) -> Result<()> {
        self.check_spawn(name, &r.spawn_id, "report")?;
        let now = Time::now();
        let u = self.report_update(name, &r, &now)?;
        self.storage
            .record_report(&u.entries, name, &u.delete, u.event.as_ref())?;
        self.publish_status(name, &r, &now);
        Ok(())
    }

    /// reportUpdate is the state change that records r, reported at now.
    pub(crate) fn report_update(
        &self,
        name: &str,
        r: &StatusReport,
        now: &Time,
    ) -> Result<StateUpdate> {
        let mut values: Vec<(String, String)> = vec![
            (STATE_KEY_STATUS.into(), r.status.to_string()),
            // Always written, so a reason never outlives the end it describes.
            (
                STATE_KEY_END_REASON.into(),
                r.end_reason.as_str().to_string(),
            ),
            (STATE_KEY_EVENT.into(), r.event.clone()),
            (STATE_KEY_DETAIL.into(), r.detail.clone()),
            (STATE_KEY_TOOL.into(), r.tool.clone()),
            // Always written, so an input never outlives the call it describes.
            (STATE_KEY_TOOL_INPUT.into(), r.tool_input.clone()),
        ];
        if !r.result.is_empty() || r.clear_result {
            values.push((STATE_KEY_RESULT.into(), r.result.clone()));
        }
        if !r.session_id.is_empty() {
            values.push((STATE_KEY_SESSION_ID.into(), r.session_id.clone()));
        }
        if !r.transcript.is_empty() || r.clear_transcript {
            values.push((STATE_KEY_TRANSCRIPT.into(), r.transcript.clone()));
        }
        if r.clear_result && r.status == AGENT_STATUS_PROCESSING {
            values.push((STATE_KEY_TURN_AT.into(), now.format_rfc3339_nano()));
        }
        let del = self.prompt_changes(name, r, now, &mut values)?;
        let entries = values
            .into_iter()
            .map(|(key, value)| StateEntry {
                agent_name: name.to_string(),
                key,
                value,
                updated_at: *now,
            })
            .collect();
        let mut ev = AgentEvent {
            at: *now,
            kind: AGENT_STATUS.to_string(),
            agent: name.to_string(),
            status: r.status.to_string(),
            event: r.event.clone(),
            detail: r.detail.clone(),
            tool: r.tool.clone(),
            session_id: r.session_id.clone(),
            transcript: r.transcript.clone(),
            end_reason: r.end_reason.as_str().to_string(),
            ..Default::default()
        };
        if r.status == AGENT_STATUS_WAITING {
            ev.tool_input = r.tool_input.clone(); // only prompts: every tool call's input would bloat the log
        }
        Ok(StateUpdate {
            entries,
            agent_name: name.to_string(),
            delete: del,
            event: Some(ev),
        })
    }

    /// promptChanges applies r's change to the pending permission prompts: a new
    /// prompt goes into values, and the keys of prompts that ended are returned.
    fn prompt_changes(
        &self,
        name: &str,
        r: &StatusReport,
        now: &Time,
        values: &mut Vec<(String, String)>,
    ) -> Result<Vec<String>> {
        let mut del = r.drop_prompts.clone();
        if r.prompt == PromptChange::Opened {
            let data = agent_store::gojson::marshal(&PendingPrompt {
                key: String::new(),
                call: r.call_key.clone(),
                agent: r.agent.clone(),
                tool: r.tool.clone(),
                input: r.tool_input.clone(),
                detail: r.detail.clone(),
            })?;
            values.push((
                format!("{STATE_KEY_PROMPT_PREFIX}{:020}", now.unix_nano()),
                data,
            ));
            return Ok(del);
        }
        if r.prompt == PromptChange::None {
            return Ok(del);
        }
        let pending = self.pending_prompts(name)?;
        for p in pending {
            if r.prompt == PromptChange::AllEnded
                || (r.prompt == PromptChange::MainEnded && p.agent.is_empty())
            {
                del.push(p.key);
            } else if r.prompt == PromptChange::Resolved
                && p.call == r.call_key
                && p.agent == r.agent
            {
                del.push(p.key);
                return Ok(del); // the oldest of identical calls
            }
        }
        Ok(del)
    }

    /// pendingPrompts returns the agent's unanswered permission prompts, oldest
    /// first.
    pub(crate) fn pending_prompts(&self, name: &str) -> Result<Vec<PendingPrompt>> {
        let entries = self
            .storage
            .list_state(name)
            .context("reading pending prompts")?;
        Ok(prompts_in(&entries))
    }

    /// checkSpawn refuses a report (or hook, per what) sent by spawn_id, unless
    /// that is the agent's current spawn. An empty spawn_id passes.
    pub(crate) fn check_spawn(&self, name: &str, spawn_id: &str, what: &str) -> Result<()> {
        if spawn_id.is_empty() {
            return Ok(());
        }
        let current = self.state_value(name, STATE_KEY_SPAWN_ID);
        if current != spawn_id {
            bail!(
                "ignoring {what} from spawn {spawn_id} of {} (current spawn: {})",
                strconv::quote(name),
                strconv::quote(&current)
            );
        }
        Ok(())
    }

    /// ReportHook records the status carried by a backend hook payload.
    /// Events that carry no status change are ignored. spawn_id is the id the hook
    /// command was launched with; hooks from any other spawn of this agent name
    /// are rejected. An empty spawn_id skips the check (hooks wired by hand).
    pub fn report_hook(
        &self,
        name: &str,
        spawn_id: &str,
        event: &str,
        payload: &[u8],
    ) -> Result<()> {
        self.report_hook_inner(name, spawn_id, event, payload, false)
            .1
    }

    /// HandleHook is report_hook for the hook itself. A hook that ends the
    /// agent's turn also hands it the messages held for it (see send), where the
    /// backend can: the output is what the hook must print for that, None if
    /// nothing. As in the Go, there can be output with an error (the report was
    /// recorded, the transcript list wasn't), and the hook prints it anyway.
    pub fn handle_hook(
        &self,
        name: &str,
        spawn_id: &str,
        event: &str,
        payload: &[u8],
    ) -> (Option<Vec<u8>>, Result<()>) {
        self.report_hook_inner(name, spawn_id, event, payload, true)
    }

    fn report_hook_inner(
        &self,
        name: &str,
        spawn_id: &str,
        event: &str,
        payload: &[u8],
        deliver: bool,
    ) -> (Option<Vec<u8>>, Result<()>) {
        let mut agent = match self.get_agent(name) {
            Ok(a) => a,
            Err(e) => return (None, Err(e)),
        };
        if let Err(e) = self.check_spawn(name, spawn_id, "hook") {
            return (None, Err(e));
        }
        if agent.adopted && agent.backend == "shell" && spawn_id.is_empty() {
            self.upgrade_adopted_backend(&mut agent, "claude");
        }
        let provider = match self.provider(&agent) {
            Ok(p) => p,
            Err(e) => return (None, Err(e.context("getting backend"))),
        };
        let Some(hp) = provider.as_hook_provider() else {
            return (
                None,
                Err(anyhow!(
                    "backend {} does not support hooks",
                    strconv::quote(&agent.backend)
                )),
            );
        };
        let hr = match hp.parse_hook_event(event, payload) {
            Ok(Some(hr)) => hr,
            Ok(None) => return (None, Ok(())),
            Err(e) => return (None, Err(e)),
        };
        if self.background_work_running(name, &hr) {
            return (None, Ok(()));
        }
        // Decided first, so a failure to record the report can't cost it.
        let approval = self.hook_approval(name, provider.as_ref(), &hr);
        let is_agy_idle_stop = hr.event == "Stop"
            && hr.status == Status::Idle
            && hr.detail != "USER_CANCELED"
            && provider
                .as_transcript_provider()
                .is_some_and(|tp| tp.transcript_reader().harness() == "agy");
        let mut result = hr.result.clone();
        if result.is_empty()
            && is_agy_idle_stop
            && let Some(tp) = provider.as_transcript_provider()
        {
            let path = self.agy_stop_transcript(name, &hr);
            if !path.is_empty()
                && let Some(reply) = read_agy_stop_result(&path, tp.transcript_reader().as_ref())
            {
                result = reply;
            }
        }
        let consume_now = (hr.event == "PreInvocation" || hr.event == "UserPromptSubmit")
            && !self.state_value(name, STATE_KEY_NOW_AT).is_empty();
        let r = StatusReport {
            status: from_backend_status(hr.status),
            end_reason: hr.end_reason.clone(),
            event: hr.event.clone(),
            detail: hr.detail.clone(),
            tool: hr.tool.clone(),
            tool_input: hr.tool_input.clone(),
            result,
            session_id: hr.session_id.clone(),

            transcript: hr.transcript.clone(),
            clear_result: hr.clear_result || consume_now,

            prompt: hr.prompt,
            call_key: hr.call_key.clone(),
            agent: hr.agent.clone(),
            drop_prompts: if consume_now {
                vec![STATE_KEY_NOW_AT.to_string()]
            } else {
                Vec::new()
            },
            ..Default::default()
        };
        let (out, res) = match provider.as_hook_deliverer() {
            Some(hd) if deliver && hd.can_deliver(&hr) => {
                match self.report_delivering(name, &r, hd, hr.delivery_only) {
                    Ok(out) => (out, Ok(())),
                    Err(e) => (None, Err(e)),
                }
            }
            Some(hd) if deliver && is_agy_idle_stop && self.has_pending_later(name) => {
                // `--later` messages skipped PostInvocation so the current turn
                // could finish first; deliver them as the turn closes.
                match self.report_delivering_filtered(
                    name,
                    &r,
                    hd,
                    false,
                    agent_store::ClaimFilter::All,
                ) {
                    Ok(out) => (out, Ok(())),
                    Err(e) => (None, Err(e)),
                }
            }
            _ if hr.delivery_only => return (None, Ok(())),
            _ => {
                let mut r = r;
                let mut out = approval;
                let mut kick_deliverer = false;
                if deliver
                    && hr.event == "PreToolUse"
                    && let Some(hd) = provider.as_hook_deliverer()
                    && let Ok(Some(urgent)) = self.storage.has_interrupt_pending(name)
                {
                    let reason = format!(
                        "[gomuxai] Tool blocked by urgent message #{} from {} — check incoming messages.",
                        urgent.id, urgent.sender_id
                    );
                    if let Some(deny) = hd.interrupt_output(&hr, &reason) {
                        out = Some(deny);
                        // Claude's PreToolUse interrupt sets `continue: false`,
                        // halting the turn without firing Stop: mark the turn
                        // idle/interrupted so deliver_held types at 0s grace
                        // even when native registry status is unavailable.
                        if provider.as_global_hooker().is_some() {
                            r.status = super::agent_service::AGENT_STATUS_IDLE;
                            r.event = "interrupted".to_string();
                            r.detail = reason;
                            kick_deliverer = true;
                        }
                    }
                }
                if deliver && let Some(h) = self.handover_output(name, provider.as_ref(), &hr) {
                    out = Some(h);
                }
                let rep_res = self.report(name, r);
                if kick_deliverer {
                    let _ = self.start_deliverer(name);
                }
                (out, rep_res)
            }
        };
        if res.is_err() {
            return (out, res);
        }
        let res = if hr.transcript.is_empty() {
            Ok(())
        } else {
            self.record_transcript(name, &agent.backend, &hr.transcript)
        };
        if res.is_ok() && is_agy_idle_stop && out.is_none() {
            self.maybe_name_agy_session(&agent, &hr);
        }
        (out, res)
    }

    /// backgroundWorkRunning reports whether hr is Claude's idle_prompt
    /// Notification arriving while the turn's Stop said background tasks are
    /// still running. The notification fires ~60s into any stay at the prompt,
    /// background work or not; recorded, it would turn "processing, N background
    /// task(s) running" into idle with no result, which a coordinator waiting
    /// for idle takes for done whenever native status can't overrule it. The
    /// task's completion starts a new turn, whose hooks report as usual.
    fn background_work_running(&self, name: &str, hr: &HookReport) -> bool {
        if hr.event != "Notification" || hr.status != Status::Idle {
            return false;
        }
        matches!(self.reported_status(name), Ok(Some(last)) if last.status == AGENT_STATUS_PROCESSING && last.event == "Stop")
    }

    /// Status checks the current status of an agent.
    pub fn status(&self, name: &str) -> Result<AgentStatus> {
        Ok(self.status_info(name)?.status)
    }

    /// StatusInfo resolves an agent's status from, in order: the backend's native
    /// session status, the last reported status, and terminal scraping.
    pub fn status_info(&self, name: &str) -> Result<StatusInfo> {
        let (mut info, reported) = self.resolve_status(name)?;
        info.held = self.held_count(name);
        info.model = self.state_value(name, STATE_KEY_MODEL);
        info.effort = self.state_value(name, STATE_KEY_EFFORT);
        if let Some(reported) = &reported {
            info.reported_status = reported.status.clone();
            info.reported_at = reported.updated_at;
            info.sent_at = reported.sent_at;
            info.turn_at = reported.turn_at;
            info.pending = reported.pending.clone();
        }
        if info.status == AGENT_STATUS_WAITING && !info.pending.is_empty() {
            // Describe the prompt on screen: the oldest unanswered one, as
            // Claude shows them in the order they were raised. The last report
            // may be a later prompt of the same batch, or a hook that fired
            // since (an answered call's PostToolUse, the Stop of a turn whose
            // background subagent is asking).
            let p = info.pending[0].clone();
            info.event = "PermissionRequest".into();
            info.detail = p.detail.clone();
            info.tool = p.tool.clone();
            info.tool_input = p.input.clone();
            let n = info.pending.len() - 1;
            if n > 0 {
                info.detail += &format!("; {n} more queued");
            }
            if let Ok(nanos) = p
                .key
                .trim_start_matches(STATE_KEY_PROMPT_PREFIX)
                .parse::<i64>()
            {
                info.updated_at = time_from_unix_nano(nanos);
            }
            info.prompt = Some(p);
        }
        // A dialog no hook reports (see dialog.rs) leaves the CLI looking idle.
        if let Ok(agent) = self.get_agent(name)
            && let Some(dialog) = self.dialog_status(&agent, &info)
        {
            info = dialog;
        }
        Ok(info)
    }

    /// resolveStatus returns the resolved status and the last report as recorded
    /// (None if none).
    pub(crate) fn resolve_status(&self, name: &str) -> Result<(StatusInfo, Option<StatusInfo>)> {
        let mut native_err: Option<String> = None;
        let (mut info, reported) = self.resolve_status_from(name, &mut native_err)?;
        // The backend CLI couldn't be asked: say so beside what stood in.
        if let Some(e) = native_err
            && info.source != STATUS_SOURCE_NATIVE
        {
            info.native_err = e;
        }
        Ok((info, reported))
    }

    fn resolve_status_from(
        &self,
        name: &str,
        native_err: &mut Option<String>,
    ) -> Result<(StatusInfo, Option<StatusInfo>)> {
        let mut agent = self.get_agent(name)?;

        let mut reported = self.reported_status(name)?;

        // No termd terminal, no CLI: whatever the last report said, nothing runs
        // now. A reboot or a stopped g3d-termd ends every agent at once and fires
        // no hook, and backends with no native session list (agy) would otherwise
        // keep their last report (idle) forever.
        if let Ok(false) = self.tmux.session_exists(&agent.session_name) {
            if let Some(r) = &reported
                && r.status == AGENT_STATUS_ENDED
            {
                return Ok((r.clone(), reported));
            }
            let mut info = StatusInfo {
                status: AGENT_STATUS_ENDED,
                end_reason: END_GONE,
                source: STATUS_SOURCE_NATIVE.into(),
                detail: format!(
                    "its session {} is gone (a reboot, or g3d-termd was stopped); agent resume {} continues its session, agent resume --all every one that ended so",
                    agent.session_name, agent.name
                ),
                ..Default::default()
            };
            if let Some(r) = &reported {
                info.event = r.event.clone();
                info.updated_at = r.updated_at;
            }
            return Ok((info, reported));
        }

        // A CLI agent revive took over fires no hook as it exits (see
        // revive.rs): its process is what says it has gone.
        if let Some(info) = self.revived_cli_gone(&agent) {
            return Ok((info, reported));
        }

        if let Some(info) = self.trust_prompt_status(&agent, reported.as_ref()) {
            return Ok((info, reported));
        }

        if self.follow_moved_session(&agent) {
            reported = self.reported_status(name)?;
        }

        let mut native = self.native_status(&agent);
        *native_err = native.err.as_ref().map(|e| format!("{e:#}"));
        if (native.queried && !native.listed) || (agent.adopted && agent.backend == "shell") {
            // The session on record isn't running (or an adopted shell pane now
            // runs a CLI). Ask what the pane runs now: a person may have quit
            // the CLI and started another in the same pane, which no hook of the
            // old one can tell us.
            match self.adopt_pane_session(&mut agent) {
                Ok(Some(info)) => {
                    let after = self.reported_status(&agent.name)?;
                    return Ok((info, after));
                }
                Ok(None) => {}
                Err(e) => native.pane_err = Some(e),
            }
        }
        let report_status = reported
            .as_ref()
            .map(|r| r.status.to_string())
            .unwrap_or_default();
        // The same rule agent sessions applies (discovery.ReconcileNative).
        let decision = discovery::reconcile_native(&native, &report_status, || {
            self.denied_since(&agent, reported.as_ref())
        });
        match decision {
            discovery::REPORT_DENIED => {
                // Claude's session list still says waiting for a moment after a
                // deny (which fires no hook), so a send right after deny was refused.
                let r = reported.clone().unwrap_or_default();
                return Ok((r, reported));
            }
            discovery::NATIVE_WINS => {
                let mut info = StatusInfo {
                    status: AgentStatus::from(native.status.clone()),
                    source: STATUS_SOURCE_NATIVE.into(),
                    ..Default::default()
                };
                // Hook detail is only trustworthy while it agrees with the backend;
                // e.g. an interrupted permission prompt fires no hook at all.
                if let Some(r) = &reported
                    && r.status == info.status
                {
                    info.event = r.event.clone();
                    info.detail = r.detail.clone();
                    info.tool = r.tool.clone();
                    info.tool_input = r.tool_input.clone();
                    info.updated_at = r.updated_at;
                }
                return Ok((info, reported));
            }
            _ => {}
        }
        if let Some(r) = &reported {
            // The backend answered but no longer knows the session, yet the last
            // report says it's alive: the CLI died without its SessionEnd hook
            // (crash, kill -9, OOM). Echoing the stale report would let the
            // watcher type queued messages into the bare shell left behind.
            // Just trusted, the CLI may not be listed yet: it is starting.
            if decision == discovery::NATIVE_GONE && !trust_starting(Some(r)) {
                let mut detail =
                    "session not running (exited without a SessionEnd hook)".to_string();
                if let Some(e) = &native.pane_err {
                    detail += &format!("; its pane: {e:#}");
                }
                let info = StatusInfo {
                    status: AGENT_STATUS_ENDED,
                    end_reason: END_GONE,
                    source: STATUS_SOURCE_NATIVE.into(),
                    event: r.event.clone(),
                    detail,
                    updated_at: r.updated_at,
                    ..Default::default()
                };
                return Ok((info, reported));
            }
            if let Some(info) = self.screen_status(&agent, r) {
                return Ok((info, reported));
            }
            // Backends like agy fire PreToolUse before showing a permission
            // dialog and fire nothing while the dialog is up. When the dialog
            // is on screen after PreToolUse, surface the status as waiting so
            // callers and approve/deny see the pending prompt.
            if r.status == AGENT_STATUS_PROCESSING
                && r.event == "PreToolUse"
                && self.permission_prompt_visible(&agent)
            {
                let mut waiting = r.clone();
                waiting.status = AGENT_STATUS_WAITING;
                if !waiting.detail.is_empty() && !waiting.detail.starts_with("permission: ") {
                    waiting.detail = format!("permission: {}", waiting.detail);
                }
                return Ok((waiting, reported));
            }
            return Ok((r.clone(), reported));
        }

        // A backend whose screen gomuxai reads (agy) is read by its prompt and
        // dialogs, not by regexes over the last 50 lines: a "Thinking" or a
        // bare ">" in a reply's scrollback isn't its status, and agy 1.5.8 and
        // on draw a mode hint in the idle prompt's box, which the idle regex
        // never matched (review-3, 2.1 and 2.2). Before its first hook, a
        // spawned agy reads the same way.
        if let Some(info) = self.unreported_screen_status(&agent) {
            return Ok((info, None));
        }
        let status = self.scrape_status(&agent)?;
        Ok((
            StatusInfo {
                status,
                source: STATUS_SOURCE_SCRAPE.into(),
                ..Default::default()
            },
            None,
        ))
    }

    /// unreportedScreenStatus reads the visible pane of a TurnEndReader CLI
    /// with no report to go by (agy before its first hook, or one started
    /// without per-spawn hooks: adopted, or taken over by revive): a
    /// permission or question dialog is waiting, its idle prompt is idle, its
    /// input box under a busy footer is processing, and no input box at all
    /// (something covers it: a survey, a sign-in, the shell) is unknown. None
    /// for other backends, or when the screen can't be read.
    fn unreported_screen_status(&self, agent: &Agent) -> Option<StatusInfo> {
        let provider = self.provider(agent).ok()?;
        let te = provider.as_turn_end_reader()?;
        let screen = self.tmux.capture(&agent.session_name, 0).ok()?;
        if provider
            .as_permission_responder()
            .is_some_and(|pr| pr.permission_prompt_visible(&screen))
        {
            return Some(StatusInfo {
                status: AGENT_STATUS_WAITING,
                source: STATUS_SOURCE_SCRAPE.into(),
                detail: "permission: prompt on screen".into(),
                ..Default::default()
            });
        }
        if provider
            .as_question_prompter()
            .is_some_and(|qp| qp.question_prompt_visible(&screen))
        {
            return Some(StatusInfo {
                status: AGENT_STATUS_WAITING,
                source: STATUS_SOURCE_SCRAPE.into(),
                detail: "question: dialog on screen".into(),
                ..Default::default()
            });
        }
        if let Some(how) = te.at_prompt(&screen) {
            let mut detail = "at its prompt".to_string();
            if !how.is_empty() {
                detail = format!("{how}; at its prompt");
            }
            return Some(StatusInfo {
                status: AGENT_STATUS_IDLE,
                source: STATUS_SOURCE_SCRAPE.into(),
                detail,
                ..Default::default()
            });
        }
        if provider
            .as_input_reader()
            .and_then(|ir| ir.input_text(&screen))
            .is_some()
        {
            return Some(StatusInfo {
                status: AGENT_STATUS_PROCESSING,
                source: STATUS_SOURCE_SCRAPE.into(),
                detail: "a turn runs: its input box shows no idle footer".into(),
                ..Default::default()
            });
        }
        Some(StatusInfo {
            status: AGENT_STATUS_UNKNOWN,
            source: STATUS_SOURCE_SCRAPE.into(),
            detail: format!(
                "its input box isn't on screen: something covers it, which agent capture {} shows",
                agent.name
            ),
            ..Default::default()
        })
    }

    /// screenStatus reads off the screen what a processing agent's hooks can't
    /// report (agy): a turn that ended with no hook (Esc, Ctrl+C), which makes it
    /// idle, and a question dialog after PreToolUse, which makes it waiting, so
    /// send holds rather than types into it and approve/deny refuse it. None
    /// if the screen changes nothing.
    fn screen_status(&self, agent: &Agent, reported: &StatusInfo) -> Option<StatusInfo> {
        if reported.status != AGENT_STATUS_PROCESSING {
            return None;
        }
        let provider = self.provider(agent).ok()?;
        let te = provider.as_turn_end_reader();
        let qp = provider.as_question_prompter();
        // A Stop that says processing left background work running: the
        // prompt is up, but the agent isn't done.
        let ends_silently =
            te.is_some() && reported.event != "Stop" && since(&reported.updated_at) >= TURN_END_LAG;
        let asks = qp.is_some() && reported.event == "PreToolUse";
        if !ends_silently && !asks {
            return None;
        }
        let screen = self.tmux.capture(&agent.session_name, 0).ok()?;
        if ends_silently
            && let Some(te) = te
            && let Some(how) = te.at_prompt(&screen)
        {
            let mut detail = "at its prompt; no hook reported the turn's end".to_string();
            if !how.is_empty() {
                detail = format!("{how}; at its prompt");
            }
            return Some(StatusInfo {
                status: AGENT_STATUS_IDLE,
                source: STATUS_SOURCE_SCRAPE.into(),
                detail,
                ..Default::default()
            });
        }
        if asks
            && let Some(qp) = qp
            && qp.question_prompt_visible(&screen)
        {
            let mut waiting = reported.clone();
            waiting.status = AGENT_STATUS_WAITING;
            waiting.detail = format!("question: {}", waiting.detail);
            return Some(waiting);
        }
        None
    }

    /// reportedStatus returns the last reported status, or None if none was reported.
    /// It reads all keys in one query so it sees a single report, not a mix.
    pub(crate) fn reported_status(&self, name: &str) -> Result<Option<StatusInfo>> {
        let entries = self
            .storage
            .list_state(name)
            .context("reading reported status")?;
        let mut values = std::collections::HashMap::with_capacity(entries.len());
        let mut info: Option<StatusInfo> = None;
        let mut sent_at = Time::zero();
        let mut turn_at = Time::zero();
        for e in &entries {
            values.insert(e.key.as_str(), e.value.as_str());
            match e.key.as_str() {
                STATE_KEY_STATUS => {
                    info = Some(StatusInfo {
                        status: AgentStatus::from(e.value.clone()),
                        source: STATUS_SOURCE_REPORT.into(),
                        updated_at: e.updated_at,
                        ..Default::default()
                    })
                }
                STATE_KEY_SENT_AT => sent_at = e.updated_at,
                STATE_KEY_TURN_AT => turn_at = e.updated_at,
                _ => {}
            }
        }
        let Some(mut info) = info else {
            return Ok(None);
        };
        let get = |k: &str| values.get(k).map(|v| v.to_string()).unwrap_or_default();
        info.sent_at = sent_at;
        info.turn_at = turn_at;
        info.end_reason = EndReason::from(get(STATE_KEY_END_REASON));
        info.event = get(STATE_KEY_EVENT);
        info.detail = get(STATE_KEY_DETAIL);
        info.tool = get(STATE_KEY_TOOL);
        info.tool_input = get(STATE_KEY_TOOL_INPUT);
        info.pending = prompts_in(&entries);
        Ok(Some(info))
    }

    /// adoptPaneSession records the session the agent's pane runs, when it isn't
    /// the one on record: the CLI's own registry, proven by pid and start time,
    /// placed in the pane by the process tree (backend.PaneSessionProvider). It
    /// returns None when the pane runs no CLI of the agent's backend, or the same
    /// session; Err says why the pane couldn't be read.
    fn adopt_pane_session(&self, agent: &mut Agent) -> Result<Option<StatusInfo>> {
        let Ok(provider) = self.provider(agent) else {
            return Ok(None);
        };
        let claude: Option<Box<dyn Provider>> = if provider.as_pane_session_provider().is_none()
            && agent.adopted
            && agent.backend == "shell"
        {
            self.backends
                .create_provider("claude", ProviderConfig::default())
                .ok()
        } else {
            None
        };
        let pp = match provider.as_pane_session_provider() {
            Some(pp) => pp,
            None => match claude.as_ref().and_then(|c| c.as_pane_session_provider()) {
                Some(pp) => pp,
                None => return Ok(None),
            },
        };
        let pane_pid = self.pane_pid(&agent.session_name)?;
        if pane_pid == 0 {
            return Ok(None); // no pane: the session is gone, which status reports on its own
        }
        let Some(ps) = pp.pane_session(pane_pid)? else {
            return Ok(None);
        };
        if agent.adopted && agent.backend == "shell" {
            self.upgrade_adopted_backend(agent, "claude");
        }
        let recorded = self.state_value(&agent.name, STATE_KEY_SESSION_ID);
        if ps.id == recorded {
            return Ok(None);
        }
        let before = if recorded.is_empty() {
            "(none recorded)".to_string()
        } else {
            recorded
        };
        let r = StatusReport {
            status: from_backend_status(ps.status),
            event: EVENT_ADOPTED.into(),
            detail: format!(
                "its pane runs session {} now (pid {}, {}); session {before} before it ended",
                ps.id, ps.pid, ps.detail
            ),
            // A different conversation: its reply and transcript are not the old one's.
            session_id: ps.id.clone(),
            clear_result: true,
            clear_transcript: true,
            ..Default::default()
        };
        let info = StatusInfo {
            status: r.status.clone(),
            source: STATUS_SOURCE_NATIVE.into(),
            event: r.event.clone(),
            detail: r.detail.clone(),
            updated_at: Time::now(),
            ..Default::default()
        };
        self.report(&agent.name, r)
            .context("recording the session its pane runs")?;
        Ok(Some(info))
    }

    /// followMovedSession moves the agent onto the session Claude Code carried
    /// its conversation into, when it has: Claude Code can hand an interactive
    /// session over to a background session its daemon runs, with an id of its
    /// own, in a process that may run anywhere (claudereg.Continuation). The
    /// agent's transcript then says which (continued-in), and its pane shows that
    /// session. Hooks from it reach the agent from then on (find_hook_session), but
    /// the agent's record still names the old session, whose registry entry keeps
    /// its last status, so status would read that until the new session's next
    /// hook. It reports true if it moved the agent.
    fn follow_moved_session(&self, agent: &Agent) -> bool {
        let Ok(provider) = self.provider(agent) else {
            return false;
        };
        let Some(pp) = provider.as_pane_session_provider() else {
            return false; // Claude's
        };
        let transcript = self.state_value(&agent.name, STATE_KEY_TRANSCRIPT);
        if transcript.is_empty() {
            return false;
        }
        let Some(next) = agent_claudereg::continued_in(&transcript) else {
            return false;
        };
        if next.is_empty() || next == self.state_value(&agent.name, STATE_KEY_SESSION_ID) {
            return false;
        }
        let pane_pid = match self.pane_pid(&agent.session_name) {
            Ok(p) if p != 0 => p,
            _ => return false,
        };
        let ps = match pp.pane_session(pane_pid) {
            Ok(Some(ps)) if ps.id == next => ps,
            _ => return false, // the pane doesn't run it: not this agent's to follow
        };
        let path = filepath::join(&[filepath::dir(&transcript), format!("{next}.jsonl")]);
        let r = StatusReport {
            status: from_backend_status(ps.status),
            event: EVENT_SESSION_MOVED.into(),
            detail: format!(
                "Claude Code moved its conversation into background session {next} (pid {}), which its pane shows",
                ps.pid
            ),
            session_id: next.clone(),
            transcript: path.clone(),
            ..Default::default()
        };
        if self.report(&agent.name, r).is_err() {
            return false;
        }
        let _ = self.record_transcript(&agent.name, &agent.backend, &path);
        true
    }

    /// upgradeAdoptedBackend switches an adopted shell agent's backend in place
    /// without closing its open run.
    pub(crate) fn upgrade_adopted_backend(&self, agent: &mut Agent, backend_name: &str) {
        agent.backend = backend_name.to_string();
        let _ = self.storage.update_agent_backend(&agent.name, backend_name);
    }

    /// panePID returns the pid of the process the tmux session's pane runs, or
    /// 0 if there is no such session.
    pub(crate) fn pane_pid(&self, session: &str) -> Result<i32> {
        let panes = self.tmux.pane_pids()?;
        Ok(panes
            .into_iter()
            .find_map(|(pid, name)| (name == session).then_some(pid))
            .unwrap_or(0))
    }

    /// nativeStatus asks the backend CLI what its own session list says about
    /// the agent's session, if it can: not queried when there is no session id,
    /// no such CLI, or it didn't answer.
    fn native_status(&self, agent: &Agent) -> discovery::Native {
        let session_id = self.state_value(&agent.name, STATE_KEY_SESSION_ID);
        if session_id.is_empty() {
            return discovery::Native::default();
        }
        let Ok(provider) = self.provider(agent) else {
            return discovery::Native::default();
        };
        let Some(np) = provider.as_native_status_provider() else {
            return discovery::Native::default();
        };
        let (st, known, err) = np.native_status(&session_id);
        if let Some(err) = err {
            if known {
                // Running, in a state the backend can't name: the report stands, and
                // status -v says why.
                return discovery::Native {
                    queried: true,
                    listed: true,
                    status: discovery::STATUS_UNKNOWN.to_string(),
                    err: Some(err),
                    ..Default::default()
                };
            }
            return discovery::Native {
                err: Some(err),
                ..Default::default()
            };
        }
        discovery::Native {
            queried: true,
            listed: known,
            status: from_backend_status(st).to_string(),
            ..Default::default()
        }
    }

    /// deniedSince reports whether the last report is a deny recorded within
    /// discovery.DeniedLag that left no prompt pending or on screen: a native
    /// "waiting" then is the CLI catching up, not a prompt.
    fn denied_since(&self, agent: &Agent, reported: Option<&StatusInfo>) -> bool {
        reported.is_some_and(|r| {
            r.event == "denied"
                && since(&r.updated_at) < discovery::DENIED_LAG
                && r.pending.is_empty()
                && !self.permission_prompt_visible(agent)
        })
    }

    pub(crate) fn permission_prompt_visible(&self, agent: &Agent) -> bool {
        let Ok(provider) = self.provider(agent) else {
            return false;
        };
        let Some(pr) = provider.as_permission_responder() else {
            return false;
        };
        let Ok(screen) = self.tmux.capture(&agent.session_name, 0) else {
            return false;
        };
        pr.permission_prompt_visible(&screen)
    }

    /// scrapeStatus infers status from terminal output. Last resort for backends
    /// that neither report through hooks nor expose a native status.
    fn scrape_status(&self, agent: &Agent) -> Result<AgentStatus> {
        let Ok(be) = self.backends.get(&agent.backend) else {
            return Err(unloaded_backend(agent));
        };

        let output = self
            .tmux
            .capture(&agent.session_name, 50)
            .context("capturing output")?;

        for (status, pattern) in [
            (AGENT_STATUS_IDLE, be.idle_pattern().to_string()),
            (AGENT_STATUS_PROCESSING, be.processing_pattern().to_string()),
        ] {
            if pattern.is_empty() {
                continue;
            }
            let re = regex::Regex::new(&pattern).map_err(|e| {
                anyhow!("invalid {status} pattern {}: {e}", strconv::quote(&pattern))
            })?;
            if re.is_match(&output) {
                return Ok(status);
            }
        }
        Ok(AGENT_STATUS_UNKNOWN)
    }

    /// RespondPermission answers an agent's pending permission prompt. It
    /// returns the status it found the agent in, which describes the prompt it
    /// answered.
    pub fn respond_permission(
        &self,
        name: &str,
        resp: PermissionResponse,
        opts: PermissionOptions,
    ) -> Result<StatusInfo> {
        let agent = self.get_agent(name)?;
        let info = self.status_info(name)?;
        if !opts.force && info.status != AGENT_STATUS_WAITING {
            bail!(
                "agent {name} is {}, not waiting on a permission prompt, so there is nothing to answer (agent status {name} -v shows what it's doing; --force sends the key anyway)",
                info.status
            );
        }
        if !opts.expect_tool.is_empty() && info.tool != opts.expect_tool {
            bail!(
                "agent {name} is waiting on {}, not {}: check the new prompt with agent status {name} -v before answering it",
                strconv::quote(&info.tool),
                strconv::quote(&opts.expect_tool)
            );
        }
        if info.is_trust_prompt() {
            return self.respond_trust(&agent, info, resp);
        }
        if info.is_dialog() {
            return self.respond_dialog(&agent, info, resp);
        }

        let cant = || {
            anyhow!(
                "agent {name} runs backend {}, which can't answer permission prompts: use agent keys {name}@{} to press the keys yourself",
                strconv::quote(&agent.backend),
                self.cfg.host
            )
        };
        let provider = self.provider(&agent).map_err(|_| cant())?;
        let Some(pr) = provider.as_permission_responder() else {
            return Err(cant());
        };
        let mut tool = info.tool.as_str();
        if opts.force {
            tool = ""; // answer whatever is on screen as a plain permission prompt
        }
        let key = pr
            .permission_key(resp, tool)
            .map_err(|e| anyhow!("agent {name}: {e:#}"))?;
        // The status above can be hundreds of ms old (native status runs the
        // backend CLI), and the prompt may have been answered since. For Claude
        // that turns deny's Escape into an interrupt of the now-running turn, and
        // no key means "No" without that risk (the "No" option acts like Escape,
        // and its number varies). So confirm the prompt is on screen right before
        // the keypress. This narrows the race to the gap between the capture and
        // the keypress (a few tmux calls, milliseconds) but can't close it: a
        // prompt answered inside that gap still gets the key.
        if !opts.force {
            let screen = self
                .tmux
                .capture(&agent.session_name, 0)
                .context("capturing screen")?;
            // A trust prompt has an Esc and an Enter of its own: never answer
            // it with a permission prompt's key.
            if let Some(tp) = provider.as_trust_prompter()
                && tp.trust_prompt(&screen).is_some()
            {
                bail!(
                    "agent {name} shows its workspace trust prompt, not the permission prompt its status described: agent status {name} -v shows it now"
                );
            }
            if !pr.permission_prompt_visible(&screen) {
                bail!(
                    "agent {name} has no permission prompt on screen; it may have just been answered (agent status {name} -v shows what it's doing; --force sends the key anyway)"
                );
            }
        }
        self.tmux.send_special_key(&agent.session_name, &key)?;

        // Record the expected outcome now: a denied prompt fires no hook, and an
        // approved one only reports once the tool finishes.
        let mut report = StatusReport {
            status: AGENT_STATUS_PROCESSING,
            event: "approved".into(),
            tool: info.tool.clone(),
            ..Default::default()
        };
        if resp == PermissionResponse::Deny {
            report = StatusReport {
                status: AGENT_STATUS_IDLE,
                event: "denied".into(),
                ..Default::default()
            };
        }
        if let Some(p) = &info.prompt {
            // The answered prompt leaves the queue now, so the next one is
            // described while the approved call runs.
            report.drop_prompts = vec![p.key.clone()];
            if resp == PermissionResponse::Deny {
                // Escape cancels the rest of that agent's batch too. Denying a
                // subagent's call ends only that call: the turn goes on.
                for q in info.pending.iter().skip(1) {
                    if q.agent == p.agent {
                        report.drop_prompts.push(q.key.clone());
                    }
                }
                if !p.agent.is_empty() {
                    report.status = AGENT_STATUS_PROCESSING;
                }
            }
        }
        self.report(name, report)?;
        Ok(info)
    }

    /// provider returns an instance of agent's backend for the optional
    /// capabilities it implements, or an error for a custom JSON backend.
    pub(crate) fn provider(&self, agent: &Agent) -> Result<Box<dyn Provider>> {
        self.backends
            .create_provider(&agent.backend, ProviderConfig::default())
    }

    pub(crate) fn get_agent(&self, name: &str) -> Result<Agent> {
        match self.storage.get_agent(name).context("getting agent")? {
            Some(a) => Ok(a),
            None => Err(self.not_found(name)),
        }
    }

    /// Agent returns the named agent's record, or an AgentNotFoundError.
    pub fn agent(&self, name: &str) -> Result<Agent> {
        self.get_agent(name)
    }

    pub(crate) fn set_state(&self, name: &str, key: &str, value: &str) -> Result<()> {
        self.storage.set_state(&StateEntry {
            agent_name: name.to_string(),
            key: key.to_string(),
            value: value.to_string(),
            updated_at: Time::now(),
        })
    }

    /// stateValue returns a state value, or "" if unset or unreadable.
    pub(crate) fn state_value(&self, name: &str, key: &str) -> String {
        match self.storage.get_state(name, key) {
            Ok(Some(e)) => e.value,
            _ => String::new(),
        }
    }

    /// Resolves the transcript path for an agy Stop report: the report's own
    /// `transcriptPath` first, else the agent's recorded `_transcript`, else
    /// the newest recorded transcript for the same conversation.
    fn agy_stop_transcript(&self, name: &str, hr: &HookReport) -> String {
        if !hr.transcript.is_empty() {
            return hr.transcript.clone();
        }
        let recorded = self.state_value(name, STATE_KEY_TRANSCRIPT);
        if !recorded.is_empty() {
            return recorded;
        }
        if let Some(last) = self.transcripts(name).last()
            && (hr.session_id.is_empty()
                || agent_transcript::session_id_from_path(&last.path) == hr.session_id)
        {
            return last.path.clone();
        }
        String::new()
    }

    /// Names an agy conversation after its first completed turn by typing
    /// `/rename <agent.name>` once per conversation id, when its recorded title
    /// isn't already the agent's name, no held messages are waiting to go out
    /// first, no dialog is on screen, and its input box is visible and empty.
    /// If any check blocks typing now, `_named` stays unset so the next idle
    /// `Stop` tries again without looping.
    fn maybe_name_agy_session(&self, agent: &Agent, hr: &HookReport) {
        let session_id = if !hr.session_id.is_empty() {
            hr.session_id.clone()
        } else {
            self.state_value(&agent.name, STATE_KEY_SESSION_ID)
        };
        if session_id.is_empty() || agent.name.is_empty() || agent.name.contains(['\n', '\r']) {
            return;
        }
        if self.state_value(&agent.name, STATE_KEY_NAMED) == session_id {
            return;
        }
        let transcript = self.agy_stop_transcript(&agent.name, hr);
        if session_name("agy", &session_id, &transcript) == agent.name {
            let _ = self.set_state(&agent.name, STATE_KEY_NAMED, &session_id);
            return;
        }
        let cmd = format!("/rename {}", agent.name);
        if !super::slash::is_slash_command(&cmd) {
            return;
        }
        // Never type `/rename` ahead of held messages waiting to be delivered:
        // let the deliverer deliver them first, then name at the next Stop.
        if self.held_count(&agent.name) > 0 {
            return;
        }
        if !self
            .tmux
            .session_exists(&agent.session_name)
            .unwrap_or(false)
        {
            return;
        }
        let Ok(st) = self.check_can_send(agent) else {
            return;
        };
        if st != AGENT_STATUS_IDLE {
            return;
        }
        if !self.input_blocked_of(agent).is_empty() {
            return;
        }
        let Ok(provider) = self.provider(agent) else {
            return;
        };
        let Some(ir) = provider.as_input_reader() else {
            return;
        };
        let Ok(styled) = self.tmux.capture_styled(&agent.session_name, 0) else {
            return;
        };
        if ir.input_text(&styled).as_deref() != Some("") {
            return;
        }
        // Type directly without mark_sent: `/rename` is an internal slash
        // command that fires no PreInvocation hook, so advancing _sent_at
        // after Stop would leave _sent_at > _turn_at and stall wait --next.
        if let Ok(why) = self.type_text(&super::ctx::Context::background(), agent, &cmd, true)
            && why.is_empty()
        {
            let _ = self.set_state(&agent.name, STATE_KEY_NAMED, &session_id);
        }
    }

    /// Returns the CLI's own recorded session name or conversation title for
    /// `name`'s current session (see [`session_name`]), or `""` if none is
    /// recorded.
    pub fn cli_session_name(&self, name: &str) -> String {
        let Ok(agent) = self.get_agent(name) else {
            return String::new();
        };
        let mut session_id = self.state_value(name, STATE_KEY_SESSION_ID);
        if session_id.is_empty() {
            session_id = self.last_session_id(name);
        }
        let mut transcript = self.state_value(name, STATE_KEY_TRANSCRIPT);
        if transcript.is_empty()
            && let Some(last) = self.transcripts(name).last()
            && (session_id.is_empty()
                || agent_transcript::session_id_from_path(&last.path) == session_id)
        {
            transcript = last.path.clone();
        }
        let backend = self
            .provider(&agent)
            .ok()
            .and_then(|p| {
                p.as_transcript_provider()
                    .map(|tp| tp.transcript_reader().harness().to_string())
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| agent.backend.clone());
        session_name(&backend, &session_id, &transcript)
    }
}
