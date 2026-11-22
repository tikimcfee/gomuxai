//! Go `internal/service/rpc.go`: the bus's RPC ops, served through an
//! AgentService.

use std::sync::Arc;

use anyhow::{Result, anyhow, bail};

use agent_backend::PermissionResponse;
use agent_base::gostd::{filepath, strconv};
use agent_base::types::resumable;
use agent_bus::{
    AgentInfo, CaptureRequest, CaptureResponse, Context, DeliverResponse, FleetHost, FleetRequest,
    FrictionRequest, FrictionResponse, Handler, KeysRequest, ListResponse, LogsRequest,
    LogsResponse, OP_FLEET, OP_LOGS, OP_RESUME, OP_TEMPLATE, RESPONSE_APPROVE,
    RESPONSE_APPROVE_ALWAYS, RESPONSE_DENY, RespondRequest, ResumeRequest, ResumeResponse,
    SendRequest, SendResponse, SpawnRequest, SpawnResponse, StatusRequest, StatusResponse,
    TemplateRequest, TemplateResponse, parse_agent_ref,
};
use agent_store::Agent;

use super::agent_service::{
    AGENT_STATUS_ENDED, AGENT_STATUS_UNKNOWN, AgentService, SpawnOptions, check_cwd,
};
use super::errors::remote_err;
use super::events::bus_time;
use super::held::{ErrNothingHeld, SendOptions};
use super::status::PermissionOptions;
use super::user_home_dir;

/// The logs op's logic, which lives in the CLI.
pub type LogsFn = Box<dyn Fn(&Context, LogsRequest) -> Result<LogsResponse> + Send + Sync>;
/// The resume op's logic, which lives in the CLI.
pub type ResumeFn = Box<dyn Fn(&Context, ResumeRequest) -> Result<ResumeResponse> + Send + Sync>;
/// The fleet op's logic, which lives in `agent-fleet`.
pub type FleetFn = Box<dyn Fn(&Context, FleetRequest) -> Result<FleetHost> + Send + Sync>;
/// The manage op's logic, which lives in the CLI.
pub type ManageFn =
    Box<dyn Fn(&Context, agent_manage::Request) -> Result<agent_manage::HostView> + Send + Sync>;
/// The roster op's logic, which lives in the CLI (over `agent-roster`).
pub type RosterFn = Box<
    dyn Fn(&Context, agent_roster::RosterRequest) -> Result<agent_roster::HostRoster> + Send + Sync,
>;
/// The template op's logic, which lives in `agent-fleet`.
pub type TemplateFn =
    Box<dyn Fn(&Context, TemplateRequest) -> Result<TemplateResponse> + Send + Sync>;

/// RPCExtras serves the ops whose logic lives outside `agent-service`: what
/// `agent logs` prints, `agent resume`, `agent fleet`, and `agent template`.
/// The bus's own process supplies them; a `None` entry refuses its op.
#[derive(Default)]
pub struct RpcExtras {
    pub logs: Option<LogsFn>,
    pub resume: Option<ResumeFn>,
    pub fleet: Option<FleetFn>,
    pub template: Option<TemplateFn>,
    pub manage: Option<ManageFn>,
    pub roster: Option<RosterFn>,
}

/// rpcHandler serves bus RPC ops through an AgentService.
pub struct RpcHandler {
    s: Arc<AgentService>,
    x: RpcExtras,
}

/// NewRPCHandler returns an agent_bus::Handler that runs each op on s, the
/// same way the CLI does for a local agent. It serves no logs or resume:
/// new_rpc_handler_with does.
pub fn new_rpc_handler(s: Arc<AgentService>) -> RpcHandler {
    RpcHandler {
        s,
        x: RpcExtras::default(),
    }
}

/// NewRPCHandlerWith is new_rpc_handler, serving logs, resume, fleet and
/// template through x.
pub fn new_rpc_handler_with(s: Arc<AgentService>, x: RpcExtras) -> RpcHandler {
    RpcHandler { s, x }
}

impl Handler for RpcHandler {
    fn logs(&self, ctx: &Context, req: LogsRequest) -> Result<LogsResponse> {
        let Some(logs) = &self.x.logs else {
            bail!("this host's bus doesn't serve {OP_LOGS}");
        };
        logs(ctx, req).map_err(remote_err)
    }

    fn resume(&self, ctx: &Context, req: ResumeRequest) -> Result<ResumeResponse> {
        let Some(resume) = &self.x.resume else {
            bail!("this host's bus doesn't serve {OP_RESUME}");
        };
        resume(ctx, req).map_err(remote_err)
    }

    fn fleet(&self, ctx: &Context, req: FleetRequest) -> Result<FleetHost> {
        let Some(fleet) = &self.x.fleet else {
            bail!("this host's bus doesn't serve {OP_FLEET}");
        };
        fleet(ctx, req).map_err(remote_err)
    }

    fn template(&self, ctx: &Context, req: TemplateRequest) -> Result<TemplateResponse> {
        let Some(template) = &self.x.template else {
            bail!("this host's bus doesn't serve {OP_TEMPLATE}");
        };
        template(ctx, req).map_err(remote_err)
    }

    fn manage(&self, ctx: &Context, req: agent_manage::Request) -> Result<agent_manage::HostView> {
        let Some(manage) = &self.x.manage else {
            bail!("this host's bus doesn't serve {}", agent_bus::OP_MANAGE);
        };
        manage(ctx, req).map_err(remote_err)
    }

    fn roster(
        &self,
        ctx: &Context,
        req: agent_roster::RosterRequest,
    ) -> Result<agent_roster::HostRoster> {
        let Some(roster) = &self.x.roster else {
            bail!("this host's bus doesn't serve {}", agent_bus::OP_ROSTER);
        };
        roster(ctx, req).map_err(remote_err)
    }

    fn list(&self, _ctx: &Context) -> Result<ListResponse> {
        let agents = self.s.list()?;
        let mut resp = ListResponse {
            agents: Vec::with_capacity(agents.len()),
            agent_binary: self.s.cfg.agent_binary.clone(),
        };
        for a in &agents {
            resp.agents.push(self.s.agent_info(a));
        }
        Ok(resp)
    }

    fn status(&self, _ctx: &Context, req: StatusRequest) -> Result<StatusResponse> {
        let info = self.s.status_info(&req.agent).map_err(remote_err)?;
        Ok(StatusResponse {
            status: info.status.to_string(),
            end_reason: info.end_reason.as_str().to_string(),
            source: info.source,
            event: info.event,
            detail: info.detail,
            tool: info.tool,
            updated_at: bus_time(&info.updated_at),
            tool_input: info.tool_input,

            reported_status: info.reported_status.to_string(),
            reported_at: bus_time(&info.reported_at),
            sent_at: bus_time(&info.sent_at),
            turn_at: bus_time(&info.turn_at),
            held: info.held as i64,
            model: info.model,
            effort: info.effort,
        })
    }

    fn capture(&self, _ctx: &Context, req: CaptureRequest) -> Result<CaptureResponse> {
        let out = self
            .s
            .capture(&req.agent, req.lines as i32, req.raw)
            .map_err(remote_err)?;
        Ok(CaptureResponse { output: out })
    }

    fn send(&self, ctx: &Context, req: SendRequest) -> Result<Option<SendResponse>> {
        let inferred = sender_from_message(&req.message);
        let from = if !req.from.is_empty() {
            // Never let a remote agent spoof `from = "user"` to bypass
            // `check_can_interrupt` when the message header names an agent.
            if req.from == "user" && inferred != "user" && inferred != "remote" {
                inferred
            } else {
                req.from.clone()
            }
        } else {
            inferred
        };
        let res = self
            .s
            .send(
                ctx,
                &req.agent,
                &req.message,
                SendOptions {
                    now: req.now,
                    from,
                    urgency: req.urgency,
                    replace: req.replace,
                    supersede: req.supersede,
                    topic: req.topic,
                },
            )
            .map_err(remote_err)?;
        Ok(Some(SendResponse {
            status: res.status.to_string(),
            detail: res.detail,
            held: res.held,
            held_count: res.held_count as i64,
            held_until: res.held_until,
            typed: res.typed as i64,

            input_blocked: res.input_blocked,
            unchecked: res.unchecked,
            message_id: res.message_id,
            superseded_ids: res.superseded_ids,
            already_delivered_id: res.already_delivered_id,
            already_delivered_via: res.already_delivered_via,
            interrupted: res.interrupted,
        }))
    }

    fn respond(&self, _ctx: &Context, req: RespondRequest) -> Result<()> {
        let resp = parse_permission_response(&req.response)?;
        self.s
            .respond_permission(
                &req.agent,
                resp,
                PermissionOptions {
                    force: req.force,
                    expect_tool: req.expect_tool,
                },
            )
            .map(|_| ())
            .map_err(remote_err)
    }

    /// Deliver types the messages held for an agent here, for another host's
    /// agent deliver name@host: what agent deliver does here.
    fn deliver(&self, ctx: &Context, req: RespondRequest) -> Result<DeliverResponse> {
        match self.s.deliver_now(ctx, &req.agent) {
            Err(e) if e.downcast_ref::<ErrNothingHeld>().is_some() => {
                Ok(DeliverResponse::default())
            }
            Err(e) => Err(remote_err(e)),
            Ok(res) => Ok(DeliverResponse {
                typed: res.typed as i64,
                held: res.held as i64,
                status: res.status.to_string(),
            }),
        }
    }

    fn keys(&self, _ctx: &Context, req: KeysRequest) -> Result<()> {
        self.s
            .keys(&req.agent, &req.text, &req.key)
            .map_err(remote_err)
    }

    fn friction(&self, _ctx: &Context, req: FrictionRequest) -> Result<FrictionResponse> {
        self.s.serve_friction(&req)
    }

    /// Spawn starts an agent for another host's agent spawn name@host. The
    /// request has no --skip-permissions, --trust-workspace or --resume: a peer
    /// starts an agent on the permission settings of the directory it names, on
    /// a backend this host knows (built in, or the bus's own --backend-config).
    /// Paths are this host's; with no Cwd the agent
    /// starts in the home directory of the user the bus runs as, since the
    /// bus's own working directory is wherever it happened to be started.
    fn spawn(&self, ctx: &Context, req: SpawnRequest) -> Result<SpawnResponse> {
        if let Ok(r) = parse_agent_ref(&req.spawned_by)
            // A former name of this host, or another spelling of its name,
            // is this host too.
            && agent_bus::canonical_host(self.s.cfg.bus_dir(), &self.s.cfg.host, &r.host)
                .eq_ignore_ascii_case(&self.s.cfg.host)
        {
            bail!(
                "spawned_by {} names an agent on this host ({}), but the spawn came over the bus",
                req.spawned_by,
                self.s.cfg.host
            );
        }
        let mut cwd = req.cwd.clone();
        if cwd.is_empty() {
            cwd = user_home_dir().map_err(|e| {
                anyhow!("no working directory given, and no home directory to start in: {e:#}")
            })?;
        }
        check_cwd(&cwd)?;
        let mut task_file = req.task_file.clone();
        if !task_file.is_empty() && !filepath::is_abs(&task_file) {
            task_file = filepath::join(&[cwd.as_str(), task_file.as_str()]);
        }
        // What a local spawn checks before it starts anything (the Go
        // checked none of it for a peer's spawn).
        if !req.task_content.is_empty() && !req.task_file.is_empty() {
            bail!(
                "cannot specify both --task and positional [task-file] {}",
                strconv::quote(&req.task_file)
            );
        }
        if req.backend == "shell" && (!req.task_content.is_empty() || !req.task_file.is_empty()) {
            bail!(
                "backend \"shell\" does not take an initial --task or [task-file]; use agent send or agent keys after spawning"
            );
        }
        if !req.task_file.is_empty() {
            check_task_file(&req.agent, &req.task_file, &task_file)?;
        }
        let agent = self.s.spawn(
            ctx,
            SpawnOptions {
                name: req.agent.clone(),
                backend: req.backend.clone(),
                task_file: task_file.clone(),
                task_content: req.task_content.clone(),
                spawned_by: req.spawned_by.clone(),
                cwd,
                model: req.model.clone(),
                effort: req.effort.clone(),
                ..Default::default()
            },
        )?;
        let mut resp = SpawnResponse {
            name: agent.name.clone(),
            session: agent.session_name.clone(),
            backend: agent.backend.clone(),
            spawned_by: agent.spawned_by.clone(),
            cwd: agent.cwd.clone(),
            task_file,
            model: req.model,
            effort: req.effort,
            status: AGENT_STATUS_UNKNOWN.to_string(),
            ..Default::default()
        };
        if let Ok(info) = self.s.status_info(&agent.name) {
            resp.status = info.status.to_string();
            resp.detail = info.detail;
            resp.end_reason = info.end_reason.as_str().to_string();
        }
        Ok(resp)
    }
}

/// A peer's [task-file] is a file on this host, at path (given, as the peer
/// named it, relative to the agent's directory), not a stray word or a
/// directory the agent would be asked to read (the local spawn's
/// checkTaskFile, in its words).
fn check_task_file(agent: &str, given: &str, path: &str) -> Result<()> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!(
            "task file {} not found (omit [task-file] to spawn {agent} with no initial task)",
            strconv::quote(given)
        ),
        Err(e) => bail!(
            "checking task file {}: stat {path}: {}",
            strconv::quote(given),
            agent_base::gostd::os::errno_text(&e)
        ),
        Ok(m) if m.is_dir() => bail!(
            "task file {} is a directory, not a file",
            strconv::quote(given)
        ),
        Ok(_) => Ok(()),
    }
}

impl AgentService {
    /// agentInfo describes an agent as the bus's list serves it: its resolved
    /// status, and what export shows for a local agent.
    pub fn agent_info(&self, a: &Agent) -> AgentInfo {
        let (mut status, mut reason, mut detail) =
            (AGENT_STATUS_UNKNOWN, String::new(), String::new());
        if let Ok(info) = self.status_info(&a.name) {
            status = info.status;
            reason = info.end_reason.as_str().to_string();
            detail = info.detail;
        }
        let session_id = self.last_session_id(&a.name);
        let mut info = AgentInfo {
            name: a.name.clone(),
            session: a.session_name.clone(),
            backend: a.backend.clone(),
            spawned_by: a.spawned_by.clone(),
            adopted: a.adopted,
            created_at: bus_time(&a.created_at),
            status: status.to_string(),
            end_reason: reason,
            detail,
            session_id: session_id.clone(),
            cwd: a.cwd.clone(),
            ..Default::default()
        };
        if let Ok(exists) = self.tmux.session_exists(&a.session_name) {
            info.attached = Some(exists);
        }
        info.resumable = Some(resumable(
            &a.backend,
            a.adopted,
            &session_id,
            status == AGENT_STATUS_ENDED,
        ));
        info
    }

    /// Keys sends text, then the special key, to an agent's session. Either may be
    /// empty. Unlike `agent keys <session>`, it only reaches managed agents.
    pub fn keys(&self, name: &str, text: &str, key: &str) -> Result<()> {
        let agent = self.get_agent(name)?;
        if !text.is_empty() {
            self.tmux.send_keys(&agent.session_name, text)?;
        }
        if !key.is_empty() {
            self.tmux.send_special_key(&agent.session_name, key)?;
        }
        Ok(())
    }
}

const PERMISSION_RESPONSES: &[(&str, PermissionResponse)] = &[
    (RESPONSE_APPROVE, PermissionResponse::Approve),
    (RESPONSE_APPROVE_ALWAYS, PermissionResponse::ApproveAlways),
    (RESPONSE_DENY, PermissionResponse::Deny),
];

/// ParsePermissionResponse maps a RespondRequest's response to a backend response.
pub fn parse_permission_response(s: &str) -> Result<PermissionResponse> {
    if let Some((_, r)) = PERMISSION_RESPONSES.iter().find(|(name, _)| *name == s) {
        return Ok(*r);
    }
    bail!(
        "invalid permission response {} (want {RESPONSE_APPROVE}, {RESPONSE_APPROVE_ALWAYS}, or {RESPONSE_DENY})",
        strconv::quote(s)
    )
}

/// PermissionResponseName is the RespondRequest's response for resp.
pub fn permission_response_name(resp: PermissionResponse) -> Result<String> {
    match PERMISSION_RESPONSES.iter().find(|(_, r)| *r == resp) {
        Some((name, _)) => Ok(name.to_string()),
        None => bail!("unknown permission response {}", resp as i32),
    }
}

/// Extracts the sender identity from a `[from <who>] ` prefix when an older
/// remote peer omits `SendRequest.from`.
fn sender_from_message(msg: &str) -> String {
    if let Some(rest) = msg.strip_prefix("[from ")
        && let Some((who, _)) = rest.split_once("] ")
    {
        if who.starts_with("a person on ") || who.starts_with("a ") || who.starts_with("an ") {
            return "user".to_string();
        }
        if !who.is_empty() && !who.contains(char::is_whitespace) {
            return who.to_string();
        }
    }
    "remote".to_string()
}
