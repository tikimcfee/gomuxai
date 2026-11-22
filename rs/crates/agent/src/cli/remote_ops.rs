//! The ops this binary's bus serves to other hosts beyond the service's own
//! (logs, resume), and the commands that ask another host for them (Go:
//! internal/cli/remote_ops.go).

use std::sync::Arc;

use agent_bus::{
    AgentRef, Context, LogsRequest, LogsResponse, RemoteError, ResumeRequest, ResumeResponse,
};
use agent_service::{AgentService, RpcExtras, RpcHandler};

use super::remote::{call_remote, is_no_responders};
use super::root::{Ctx, CtxShare};

/// The RPC handler this binary's bus serves: the service's ops, plus logs
/// and resume, whose logic lives here in the CLI (Go: busHandler). Each
/// request gets a Ctx of its own over ctx's store, termd and service.
pub fn bus_handler(ctx: &Ctx) -> anyhow::Result<RpcHandler> {
    bus_handler_with(ctx, Arc::clone(ctx.service()?))
}

/// bus_handler serving svc's ops (tests: a service with a config of its
/// own); logs and resume still run on ctx's.
pub fn bus_handler_with(ctx: &Ctx, svc: Arc<AgentService>) -> anyhow::Result<RpcHandler> {
    let share = ctx.share()?;
    let logs_share = share.clone();
    let template_share = share.clone();
    let manage_share = share.clone();
    let roster_share = share.clone();
    let fleet = agent_fleet::fleet::rpc_handler(
        Arc::clone(&svc),
        Arc::clone(svc.storage()),
        svc.config().clone(),
    );
    let template_svc = Arc::clone(&svc);
    Ok(agent_service::new_rpc_handler_with(
        svc,
        RpcExtras {
            logs: Some(Box::new(move |c, req| serve_logs(&logs_share, c, req))),
            resume: Some(Box::new(move |c, req| serve_resume(&share, c, req))),
            fleet: Some(fleet),
            template: Some(Box::new(move |c, req| {
                super::template::serve_template(&template_share, &template_svc, c, req)
            })),
            manage: Some(Box::new(move |_c, req| {
                super::manage::local_host(&manage_share.quiet_ctx(), &req)
            })),
            roster: Some(Box::new(move |_c, _req| {
                super::roster::local_roster(&roster_share.quiet_ctx())
            })),
        },
    ))
}

/// One piece of what agent logs prints: a session line (or separator), or
/// an event (Go: logsChunk, in logs.go; here, since fit_logs cuts by them).
#[derive(Debug, Clone, Default)]
pub struct LogsChunk {
    pub text: String,
    pub session: bool,
}

/// Answers another host's agent logs name@host (Go: serveLogs; its body is
/// logs.rs's serve_logs, beside build_logs).
fn serve_logs(share: &CtxShare, c: &Context, req: LogsRequest) -> anyhow::Result<LogsResponse> {
    super::logs::serve_logs(&share.quiet_ctx(), c, req)
}

/// Joins chunks into at most budget bytes once JSON-encoded, keeping the
/// newest: it leaves out whole chunks from the start, and puts back the
/// session line or separator the first one kept belongs to. With cut_text, a
/// single chunk too big on its own (an event printed --full) keeps its end;
/// otherwise it is left out, since half a JSON line is no line. It returns
/// the output and how many bytes were left out (Go: fitLogs).
pub fn fit_logs(chunks: &[LogsChunk], budget: i64, cut_text: bool) -> (String, usize) {
    let sizes: Vec<i64> = chunks.iter().map(|c| json_size(&c.text) as i64).collect();
    let total: i64 = sizes.iter().sum();
    if total <= budget {
        return (join_chunks(chunks), 0);
    }
    // The session chunk chunks[first] belongs to, if it isn't one itself
    // and was left out; None for none.
    let header = |first: usize| -> Option<usize> {
        if first >= chunks.len() || chunks[first].session {
            return None;
        }
        (0..first).rev().find(|&i| chunks[i].session)
    };
    let (mut first, mut kept) = (chunks.len(), 0i64);
    while first > 0 && kept + sizes[first - 1] <= budget {
        first -= 1;
        kept += sizes[first];
    }
    while first < chunks.len() {
        match header(first) {
            Some(h) if kept + sizes[h] > budget => {
                kept -= sizes[first];
                first += 1;
            }
            _ => break,
        }
    }

    let mut b = String::new();
    if let Some(h) = header(first) {
        b.push_str(&chunks[h].text);
    }
    if first == chunks.len() && cut_text && !chunks.is_empty() {
        // The newest chunk alone is too big: keep its end. JSON escaping
        // grows text at most sixfold.
        let last = &chunks[chunks.len() - 1].text;
        let keep = (last.len() as i64).min(budget / 6).max(0) as usize;
        let mut start = last.len() - keep;
        while start < last.len() && !last.is_char_boundary(start) {
            start += 1;
        }
        b.push_str(&last[start..]);
    }
    b.push_str(&join_chunks(&chunks[first..]));
    let left_out = raw_size(chunks).saturating_sub(b.len());
    (b, left_out)
}

fn join_chunks(chunks: &[LogsChunk]) -> String {
    chunks.iter().map(|c| c.text.as_str()).collect()
}

fn raw_size(chunks: &[LogsChunk]) -> usize {
    chunks.iter().map(|c| c.text.len()).sum()
}

/// How many bytes s takes in a JSON string, quotes left out (Go: jsonSize,
/// with encoding/json's escaping of <, > and &).
pub fn json_size(s: &str) -> usize {
    agent_store::gojson::marshal(s).map_or(2, |b| b.len()) - 2
}

/// Prints what agent logs prints on r.host for r.name (Go: runRemoteLogs,
/// with logsOptions as the request's fields).
pub fn run_remote_logs(ctx: &Ctx, r: &AgentRef, req: LogsRequest) -> anyhow::Result<()> {
    let res = call_remote(ctx, &r.host, |c| c.logs(&Context::background(), r, req));
    let resp = match res {
        Ok(resp) => resp,
        Err(e) => {
            if is_no_responders(&e) {
                ctx.hint(format!(
                    "{} serves logs only if its bus allows them (agent bus status there shows what it allows; agent capture {} shows its screen)",
                    r.host, r
                ));
            }
            return Err(e);
        }
    };
    if resp.truncated > 0 {
        ctx.notice(format!(
            "{}'s logs from {} were cut to fit a bus message: the oldest {} left out (--lines asks for fewer).",
            r,
            r.host,
            byte_size(resp.truncated)
        ));
    }
    if !resp.hint.is_empty() {
        ctx.hint(&resp.hint);
    }
    ctx.write_out(&resp.output)
}

/// Go: byteSize (export.go).
fn byte_size(n: i64) -> String {
    if n >= 1 << 20 {
        format!("{:.1} MB", n as f64 / (1 << 20) as f64)
    } else if n >= 1 << 10 {
        format!("{:.1} KB", n as f64 / (1 << 10) as f64)
    } else {
        format!("{n} B")
    }
}

/// Answers another host's agent resume name@host: this host's own resume,
/// with its own refusals (Go: serveResume).
fn serve_resume(
    share: &CtxShare,
    c: &Context,
    req: ResumeRequest,
) -> anyhow::Result<ResumeResponse> {
    let ctx = share.quiet_ctx();
    // This host under any spelling, or a name it had before agent bus
    // rename: name@host resolves those to this host's own agents.
    if let Ok(r) = agent_bus::parse_agent_ref(&req.resumed_by)
        && !r.host.is_empty()
        && super::remote::canonical_host(&ctx, &r.host) == ctx.cfg().host
    {
        anyhow::bail!(
            "resumed_by {} names an agent on this host ({}), but the resume came over the bus",
            req.resumed_by,
            ctx.cfg().host
        );
    }
    super::resume::resume_agent(&ctx, c, &req.agent, &req.resumed_by, false, false)
}

/// Resumes r.name on r.host through the agent bus (Go: runRemoteResume).
pub fn run_remote_resume(
    ctx: &Ctx,
    r: &AgentRef,
    skip_permissions: bool,
    trust_workspace: bool,
    attach: bool,
) -> anyhow::Result<()> {
    for (set, flag) in [
        (skip_permissions, "--skip-permissions"),
        (trust_workspace, "--trust-workspace"),
    ] {
        if set {
            anyhow::bail!(
                "{flag} works only for an agent on this host: to resume {} with it, run agent resume on {}",
                r.name,
                r.host
            );
        }
    }
    if attach {
        // The bus can't carry a terminal: that host's own resume --attach
        // runs over ssh.
        return super::ssh::ssh_attach(ctx, r, "resume", &["--attach"]);
    }
    let mut req = ResumeRequest::default();
    let me = ctx.non_interactive_caller_agent();
    if !me.is_empty() {
        req.resumed_by = AgentRef {
            name: me,
            host: ctx.cfg().host.clone(),
        }
        .to_string();
    }
    let res = call_remote(ctx, &r.host, |c| c.resume(&Context::background(), r, req));
    match res {
        Ok(resp) => super::resume::print_resumed(ctx, &r.to_string(), &resp),
        Err(e) if e.chain().any(|c| c.downcast_ref::<RemoteError>().is_some()) => {
            Err(e.context(format!("{} couldn't resume {}", r.host, r.name)))
        }
        Err(e) => {
            if is_no_responders(&e) {
                ctx.hint(format!(
                    "{} serves resume to other hosts only if its bus runs with --remote-ops listing resume (agent bus status there shows what it allows)",
                    r.host
                ));
            }
            Err(e)
        }
    }
}
