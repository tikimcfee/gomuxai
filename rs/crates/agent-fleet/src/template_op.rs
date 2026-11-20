//! Server-side handler for `OP_TEMPLATE` (`list`, `start`, `resume`; see
//! `docs/FLEET.md`, section `"Templates: starting agents across hosts"`).
//!
//! Security boundary: a `TemplateRequest` carries only `action`, `template`,
//! and `from`. Every launch parameter (`backend`, `model`, `cwd`, `brief`, and
//! the spawned agent's `name`) comes strictly from this host's own
//! `<data dir>/templates.json`. `resume` brings back only an ended agent that
//! `start` here recorded as the template's ([`STATE_KEY_TEMPLATE`]).

use std::path::PathBuf;
use std::sync::Arc;

use agent_base::config::Config;
use agent_base::gostd::filepath;
use agent_base::types::resumable;
use agent_bus::{
    AgentInfo, Context, ResumeRequest, ResumeResponse, TEMPLATE_LIST, TEMPLATE_RESUME,
    TEMPLATE_START, TemplateRequest, TemplateResponse, canonical_host, parse_agent_ref,
    validate_agent,
};
use agent_service::{AGENT_STATUS_ENDED, AgentService, SpawnOptions, TemplateFn};
use agent_store::{StateEntry, Time as StoreTime};
use anyhow::{Context as _, Result, anyhow, bail};

use crate::{focus, templates};

/// The state key `start` records on the agent it spawns: the template's name.
/// `resume` brings back only an agent whose key names its template, so the
/// op resumes what this host started from its templates and nothing else
/// (any agent's name could be a template's). Nothing in a request sets it.
/// It is an agent's own key, like `handoff`, not a `_` one: a resume clears
/// the `_` keys of the run that ended, and this one has to outlive resumes.
pub const STATE_KEY_TEMPLATE: &str = "template";

/// Callback executing the `resume` flow (which lives in the CLI alongside
/// `RpcExtras::resume`).
pub type ResumeAgentFn = dyn Fn(&Context, ResumeRequest) -> Result<ResumeResponse> + Send + Sync;

/// Resolves `<data dir>` (`.gomuxai`) from `cfg`, matching `Config::bus_dir`.
pub fn data_dir(cfg: &Config) -> PathBuf {
    if !cfg.database_path.is_empty() {
        PathBuf::from(filepath::dir(&cfg.database_path))
    } else if !cfg.state_dir.is_empty() {
        PathBuf::from(filepath::dir(&cfg.state_dir))
    } else if !cfg.log_dir.is_empty() {
        PathBuf::from(filepath::dir(&cfg.log_dir))
    } else {
        PathBuf::from(".gomuxai")
    }
}

/// Builds a [`TemplateFn`] closure suitable for [`agent_service::RpcExtras::template`].
pub fn rpc_handler(
    service: Arc<AgentService>,
    cfg: Config,
    resume: Option<Arc<ResumeAgentFn>>,
) -> TemplateFn {
    Box::new(move |ctx, req| handle(ctx, &service, &cfg, req, resume.as_deref()))
}

/// Executes a `template` request (`list`, `start`, or `resume`) against this
/// host's `<data dir>/templates.json` and `service`.
pub fn handle(
    ctx: &Context,
    service: &AgentService,
    cfg: &Config,
    req: TemplateRequest,
    resume: Option<&ResumeAgentFn>,
) -> Result<TemplateResponse> {
    match req.action.as_str() {
        TEMPLATE_LIST => handle_list(cfg),
        TEMPLATE_START => handle_start(ctx, service, cfg, &req),
        TEMPLATE_RESUME => handle_resume(ctx, service, cfg, &req, resume),
        other => bail!(
            "unknown template action {:?} (want {}, {}, or {})",
            other,
            TEMPLATE_LIST,
            TEMPLATE_START,
            TEMPLATE_RESUME
        ),
    }
}

fn handle_list(cfg: &Config) -> Result<TemplateResponse> {
    // Expose wire metadata (including `problem` when broken on disk, and only
    // the brief's path string — never the brief file's contents).
    let wire = templates::list_wire(data_dir(cfg))?;
    Ok(TemplateResponse {
        templates: wire,
        agent: None,
    })
}

fn handle_start(
    ctx: &Context,
    service: &AgentService,
    cfg: &Config,
    req: &TemplateRequest,
) -> Result<TemplateResponse> {
    validate_agent(&req.template)?;
    validate_from(cfg, &req.from)?;

    let Some(tmpl) = templates::get(data_dir(cfg), &req.template)? else {
        bail!("unknown template {:?}{}", req.template, on_host_suffix(cfg));
    };
    tmpl.validate()?;

    if let Some(existing) = service.storage().get_agent(&tmpl.name)? {
        let status = service
            .status_info(&existing.name)
            .map(|s| s.status)
            .unwrap_or_default();
        if status != AGENT_STATUS_ENDED {
            let label = if status.is_empty() {
                "unknown"
            } else {
                status.as_str()
            };
            bail!(
                "template {:?} is already running (status: {label})",
                tmpl.name
            );
        }
        bail!(
            "template {:?} already has an ended agent record; use `agent template resume {}` to resume it",
            tmpl.name,
            tmpl.name
        );
    }

    // Nothing from `req` except `req.template` (above, to pick `tmpl`) and
    // `req.from` (`spawned_by`) may reach `service.spawn`.
    let task_file = tmpl
        .brief_path()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let spawned = service.spawn(
        ctx,
        SpawnOptions {
            name: tmpl.name.clone(),
            backend: tmpl.backend.clone(),
            model: tmpl.model.clone(),
            cwd: tmpl.cwd.clone(),
            task_file,
            task_content: String::new(),
            spawned_by: req.from.clone(),
            ..Default::default()
        },
    )?;
    // What resume checks: this host started the agent from this template.
    service
        .storage()
        .set_state(&StateEntry {
            agent_name: spawned.name.clone(),
            key: STATE_KEY_TEMPLATE.to_string(),
            value: tmpl.name.clone(),
            updated_at: StoreTime::now(),
        })
        .with_context(|| {
            format!(
                "started agent {} from template {:?}, but couldn't record that it was (template resume won't resume it)",
                spawned.name, tmpl.name
            )
        })?;

    Ok(TemplateResponse {
        templates: Vec::new(),
        agent: Some(service.agent_info(&spawned)),
    })
}

fn handle_resume(
    ctx: &Context,
    service: &AgentService,
    cfg: &Config,
    req: &TemplateRequest,
    resume: Option<&ResumeAgentFn>,
) -> Result<TemplateResponse> {
    validate_agent(&req.template)?;
    validate_from(cfg, &req.from)?;

    let Some(tmpl) = templates::get(data_dir(cfg), &req.template)? else {
        bail!("unknown template {:?}{}", req.template, on_host_suffix(cfg));
    };

    let Some(existing) = service.storage().get_agent(&tmpl.name)? else {
        return Err(focus::agent_not_found(service.storage(), &cfg.host, &tmpl.name).into());
    };
    // Only an agent this host's own records show it started from this
    // template: whoever asks may resume that one, but an agent of the same
    // name started any other way (agent spawn, from another directory) is
    // not the template's to bring back, even where the bus doesn't serve
    // resume itself.
    let from_template = service
        .storage()
        .get_state(&existing.name, STATE_KEY_TEMPLATE)?
        .is_some_and(|e| e.value == tmpl.name);
    if !from_template {
        bail!(
            "agent {:?} wasn't started from template {:?} here, so template resume leaves it (agent resume {} on {} resumes it)",
            existing.name,
            tmpl.name,
            existing.name,
            cfg.host
        );
    }
    let status = service
        .status_info(&existing.name)
        .map(|s| s.status)
        .unwrap_or_default();
    if status != AGENT_STATUS_ENDED {
        let label = if status.is_empty() {
            "unknown"
        } else {
            status.as_str()
        };
        bail!(
            "template {:?} is already running (status: {label}); nothing to resume",
            tmpl.name
        );
    }

    let Some(resume_fn) = resume else {
        bail!("this host's bus doesn't serve resume");
    };
    let resumed = resume_fn(
        ctx,
        ResumeRequest {
            agent: tmpl.name.clone(),
            resumed_by: req.from.clone(),
        },
    )?;

    let info = match service.storage().get_agent(&tmpl.name)? {
        Some(agent) => service.agent_info(&agent),
        None => agent_info_from_resume(&resumed, &req.from),
    };
    Ok(TemplateResponse {
        templates: Vec::new(),
        agent: Some(info),
    })
}

fn validate_from(cfg: &Config, from: &str) -> Result<()> {
    if from.is_empty() {
        return Ok(());
    }
    let r = parse_agent_ref(from).map_err(|e| anyhow!("template request's from: {e}"))?;
    // This host under any spelling, or a name it had before agent bus
    // rename: name@host resolves those to this host's own agents.
    if !r.host.is_empty()
        && !cfg.host.is_empty()
        && canonical_host(cfg.bus_dir(), &cfg.host, &r.host) == cfg.host
    {
        bail!(
            "template request's from {} names an agent on this host ({}), but came with a cross-host reference",
            from,
            cfg.host
        );
    }
    Ok(())
}

fn on_host_suffix(cfg: &Config) -> String {
    if cfg.host.is_empty() {
        String::new()
    } else {
        format!(" on {}", cfg.host)
    }
}

fn agent_info_from_resume(r: &ResumeResponse, spawned_by: &str) -> AgentInfo {
    let ended = r.status == AGENT_STATUS_ENDED.as_str();
    AgentInfo {
        name: r.name.clone(),
        session: r.session.clone(),
        backend: r.backend.clone(),
        spawned_by: spawned_by.to_string(),
        status: r.status.clone(),
        end_reason: r.end_reason.clone(),
        detail: r.detail.clone(),
        session_id: r.session_id.clone(),
        cwd: r.cwd.clone(),
        resumable: Some(resumable(&r.backend, false, &r.session_id, ended)),
        ..Default::default()
    }
}
