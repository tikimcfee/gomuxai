//! RPC server (`internal/natsbus/rpc.go`): one subscription per op, at most 32
//! requests in flight, panics turned into error replies, and a [`Handler`]
//! trait the service implements.

use std::any::Any;
use std::sync::{Arc, Condvar, Mutex, Once};

use agent_base::gostd::strconv::quote;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::value::RawValue;

use crate::client::{CallbackSubscription, Conn, Msg};
use crate::contract::{
    CaptureRequest, CaptureResponse, DeliverResponse, FRICTION_ACTIONS, FRICTION_GET,
    FRICTION_RESOLVE, FleetHost, FleetRequest, FrictionRequest, FrictionResponse, KeysRequest,
    ListRequest, ListResponse, LogsRequest, LogsResponse, OP_CAPTURE, OP_FLEET, OP_FRICTION,
    OP_KEYS, OP_LIST, OP_LOGS, OP_MANAGE, OP_RESPOND, OP_RESUME, OP_ROSTER, OP_SEND, OP_SPAWN,
    OP_STATUS, OP_TEMPLATE, RESPONSE_DELIVER, RPCResponse, RespondRequest, ResumeRequest,
    ResumeResponse, SendRequest, SendResponse, SpawnRequest, SpawnResponse, StatusRequest,
    StatusResponse, TEMPLATE_ACTIONS, TEMPLATE_LIST, TemplateRequest, TemplateResponse,
    parse_agent_ref, subject_rpc, validate_agent, validate_host,
};
use crate::runtime::{CancelFn, Context};

/// Bounds the requests a bus serves at once; more get a `"busy"` reply.
pub const MAX_IN_FLIGHT: usize = 32;

/// Executes RPC ops on the local host, one method per op (`RPCHandler` in Go).
pub trait Handler: Send + Sync + 'static {
    fn list(&self, ctx: &Context) -> anyhow::Result<ListResponse>;
    fn status(&self, ctx: &Context, req: StatusRequest) -> anyhow::Result<StatusResponse>;
    fn capture(&self, ctx: &Context, req: CaptureRequest) -> anyhow::Result<CaptureResponse>;
    fn send(&self, ctx: &Context, req: SendRequest) -> anyhow::Result<Option<SendResponse>>;
    fn respond(&self, ctx: &Context, req: RespondRequest) -> anyhow::Result<()>;
    fn deliver(&self, ctx: &Context, req: RespondRequest) -> anyhow::Result<DeliverResponse>;
    fn keys(&self, ctx: &Context, req: KeysRequest) -> anyhow::Result<()>;
    fn spawn(&self, ctx: &Context, req: SpawnRequest) -> anyhow::Result<SpawnResponse>;
    fn friction(&self, ctx: &Context, req: FrictionRequest) -> anyhow::Result<FrictionResponse>;
    fn logs(&self, ctx: &Context, req: LogsRequest) -> anyhow::Result<LogsResponse>;
    fn resume(&self, ctx: &Context, req: ResumeRequest) -> anyhow::Result<ResumeResponse>;
    fn fleet(&self, _ctx: &Context, _req: FleetRequest) -> anyhow::Result<FleetHost> {
        anyhow::bail!("this host's bus doesn't serve {OP_FLEET}")
    }
    fn template(&self, _ctx: &Context, _req: TemplateRequest) -> anyhow::Result<TemplateResponse> {
        anyhow::bail!("this host's bus doesn't serve {OP_TEMPLATE}")
    }
    fn manage(
        &self,
        _ctx: &Context,
        _req: agent_manage::Request,
    ) -> anyhow::Result<agent_manage::HostView> {
        anyhow::bail!("this host's bus doesn't serve {OP_MANAGE}")
    }
    fn roster(
        &self,
        _ctx: &Context,
        _req: agent_roster::RosterRequest,
    ) -> anyhow::Result<agent_roster::HostRoster> {
        anyhow::bail!("this host's bus doesn't serve {OP_ROSTER}")
    }
}

type OpRunner = fn(&Context, &dyn Handler, &[u8]) -> anyhow::Result<Option<Box<RawValue>>>;

const RPC_OPS: &[(&str, OpRunner)] = &[
    (OP_LIST, run_list),
    (OP_STATUS, run_status),
    (OP_CAPTURE, run_capture),
    (OP_SEND, run_send),
    (OP_RESPOND, run_respond),
    (OP_KEYS, run_keys),
    (OP_SPAWN, run_spawn),
    (OP_LOGS, run_logs),
    (OP_RESUME, run_resume),
    (OP_FRICTION, run_friction),
    (OP_FLEET, run_fleet),
    (OP_TEMPLATE, run_template),
    (OP_MANAGE, run_manage),
    (OP_ROSTER, run_roster),
];

fn find_op(op: &str) -> Option<(&'static str, OpRunner)> {
    RPC_OPS.iter().copied().find(|&(name, _)| name == op)
}

fn encode_opt<T: Serialize>(op: &str, v: T) -> anyhow::Result<Option<Box<RawValue>>> {
    let raw = serde_json::value::to_raw_value(&v)
        .map_err(|e| anyhow::anyhow!("encoding {op} response: {e}"))?;
    Ok(Some(raw))
}

fn run_list(ctx: &Context, h: &dyn Handler, data: &[u8]) -> anyhow::Result<Option<Box<RawValue>>> {
    let _req: ListRequest = decode(OP_LIST, data)?;
    encode_opt(OP_LIST, h.list(ctx)?)
}

fn run_status(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: StatusRequest = decode(OP_STATUS, data)?;
    validate_agent(&req.agent)?;
    encode_opt(OP_STATUS, h.status(ctx, req)?)
}

fn run_capture(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: CaptureRequest = decode(OP_CAPTURE, data)?;
    validate_agent(&req.agent)?;
    encode_opt(OP_CAPTURE, h.capture(ctx, req)?)
}

fn run_send(ctx: &Context, h: &dyn Handler, data: &[u8]) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: SendRequest = decode(OP_SEND, data)?;
    validate_agent(&req.agent)?;
    match h.send(ctx, req)? {
        Some(resp) => encode_opt(OP_SEND, resp),
        None => Ok(None),
    }
}

fn run_respond(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: RespondRequest = decode(OP_RESPOND, data)?;
    validate_agent(&req.agent)?;
    if req.response == RESPONSE_DELIVER {
        return encode_opt(OP_RESPOND, h.deliver(ctx, req)?);
    }
    h.respond(ctx, req)?;
    Ok(None)
}

fn run_keys(ctx: &Context, h: &dyn Handler, data: &[u8]) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: KeysRequest = decode(OP_KEYS, data)?;
    validate_agent(&req.agent)?;
    if req.text.is_empty() && req.key.is_empty() {
        anyhow::bail!("keys request has neither text nor key");
    }
    h.keys(ctx, req)?;
    Ok(None)
}

fn run_spawn(ctx: &Context, h: &dyn Handler, data: &[u8]) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: SpawnRequest = decode(OP_SPAWN, data)?;
    validate_agent(&req.agent)?;
    if req.backend.is_empty() {
        anyhow::bail!("spawn request names no backend");
    }
    if !req.spawned_by.is_empty() {
        let r = parse_agent_ref(&req.spawned_by)
            .map_err(|e| anyhow::anyhow!("spawn request's spawned_by: {e}"))?;
        if r.host.is_empty() {
            anyhow::bail!(
                "spawn request's spawned_by {} names no host: want name@host",
                quote(&req.spawned_by)
            );
        }
    }
    encode_opt(OP_SPAWN, h.spawn(ctx, req)?)
}

fn run_logs(ctx: &Context, h: &dyn Handler, data: &[u8]) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: LogsRequest = decode(OP_LOGS, data)?;
    validate_agent(&req.agent)?;
    if req.lines < 1 {
        anyhow::bail!("logs request's lines must be at least 1");
    }
    encode_opt(OP_LOGS, h.logs(ctx, req)?)
}

fn run_resume(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: ResumeRequest = decode(OP_RESUME, data)?;
    validate_agent(&req.agent)?;
    if !req.resumed_by.is_empty() {
        let r = parse_agent_ref(&req.resumed_by)
            .map_err(|e| anyhow::anyhow!("resume request's resumed_by: {e}"))?;
        if r.host.is_empty() {
            anyhow::bail!(
                "resume request's resumed_by {} names no host: want name@host",
                quote(&req.resumed_by)
            );
        }
    }
    encode_opt(OP_RESUME, h.resume(ctx, req)?)
}

fn run_friction(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: FrictionRequest = decode(OP_FRICTION, data)?;
    check_friction_request(&req)?;
    encode_opt(OP_FRICTION, h.friction(ctx, req)?)
}

fn run_manage(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: agent_manage::Request = decode(OP_MANAGE, data)?;
    encode_opt(OP_MANAGE, h.manage(ctx, req)?)
}

fn run_roster(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: agent_roster::RosterRequest = decode(OP_ROSTER, data)?;
    encode_opt(OP_ROSTER, h.roster(ctx, req)?)
}

fn run_fleet(ctx: &Context, h: &dyn Handler, data: &[u8]) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: FleetRequest = decode(OP_FLEET, data)?;
    encode_opt(OP_FLEET, h.fleet(ctx, req)?)
}

fn run_template(
    ctx: &Context,
    h: &dyn Handler,
    data: &[u8],
) -> anyhow::Result<Option<Box<RawValue>>> {
    let req: TemplateRequest = decode(OP_TEMPLATE, data)?;
    check_template_request(&req)?;
    encode_opt(OP_TEMPLATE, h.template(ctx, req)?)
}

fn check_template_request(req: &TemplateRequest) -> anyhow::Result<()> {
    if !TEMPLATE_ACTIONS.contains(&req.action.as_str()) {
        anyhow::bail!(
            "unknown template action {} (want {})",
            quote(&req.action),
            TEMPLATE_ACTIONS.join(", ")
        );
    }
    if req.action != TEMPLATE_LIST {
        validate_agent(&req.template)?;
        if !req.from.is_empty() {
            let r = parse_agent_ref(&req.from)
                .map_err(|e| anyhow::anyhow!("template request's from: {e}"))?;
            if r.host.is_empty() {
                anyhow::bail!(
                    "template request's from {} names no host: want name@host",
                    quote(&req.from)
                );
            }
        }
    }
    Ok(())
}

fn check_friction_request(req: &FrictionRequest) -> anyhow::Result<()> {
    if !FRICTION_ACTIONS.contains(&req.action.as_str()) {
        anyhow::bail!(
            "unknown friction action {} (want {})",
            quote(&req.action),
            FRICTION_ACTIONS.join(", ")
        );
    }
    if req.action == FRICTION_GET {
        if req.ids.is_empty() {
            anyhow::bail!("friction get request names no ids");
        }
        return Ok(());
    }
    if req.id < 1 {
        anyhow::bail!("friction {} request names no entry", req.action);
    }
    if req.action == FRICTION_RESOLVE && req.status != "resolved" && req.status != "wontfix" {
        anyhow::bail!(
            "friction resolve request's status {} isn't resolved or wontfix",
            quote(&req.status)
        );
    }
    let Some(from) = &req.from else {
        anyhow::bail!(
            "friction {} request names no entry it comes from",
            req.action
        );
    };
    if from.id < 1 {
        anyhow::bail!(
            "friction {} request names no entry it comes from",
            req.action
        );
    }
    validate_host(&from.host)
        .map_err(|e| anyhow::anyhow!("friction {} request's from: {e}", req.action))?;
    Ok(())
}

fn decode<T: Default + DeserializeOwned>(op: &str, data: &[u8]) -> anyhow::Result<T> {
    if data.is_empty() {
        return Ok(T::default());
    }
    serde_json::from_slice(data).map_err(|e| anyhow::anyhow!("malformed {op} request: {e}"))
}

struct ServerState {
    stopped: bool,
    in_flight: usize,
    active_workers: usize,
}

/// Handle returned by [`serve_rpc`]. Calling [`RpcServer::stop`] (or dropping
/// the handle) unsubscribes, cancels in-flight requests, and waits for their
/// handlers to return. It does not close the NATS connection.
pub struct RpcServer {
    conn: Conn,
    subs: Mutex<Vec<CallbackSubscription>>,
    state: Arc<(Mutex<ServerState>, Condvar)>,
    cancel: CancelFn,
    once: Once,
}

impl RpcServer {
    pub fn stop(&self) {
        self.once.call_once(|| {
            {
                let (lock, _) = &*self.state;
                let mut st = lock.lock().unwrap();
                st.stopped = true;
            }
            let subs = std::mem::take(&mut *self.subs.lock().unwrap());
            for mut sub in subs {
                let _ = sub.unsubscribe();
            }
            let _ = self.conn.flush();
            self.cancel.cancel();

            let (lock, cvar) = &*self.state;
            let mut st = lock.lock().unwrap();
            while st.active_workers > 0 {
                st = cvar.wait(st).unwrap();
            }
        });
    }
}

impl Drop for RpcServer {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Answers requests for each op in `allowed_ops` on `subject_rpc(host, op)`,
/// running them through `handler`.
pub fn serve_rpc<S: AsRef<str>>(
    nc: &Conn,
    host: &str,
    handler: Arc<dyn Handler>,
    allowed_ops: &[S],
) -> anyhow::Result<RpcServer> {
    validate_host(host)?;
    let mut ops = Vec::with_capacity(allowed_ops.len());
    for op in allowed_ops {
        let op_str = op.as_ref();
        let Some((static_op, runner)) = find_op(op_str) else {
            anyhow::bail!("unknown RPC op {}", quote(op_str));
        };
        ops.push((static_op, runner));
    }

    let (ctx, cancel) = Context::with_cancel(&Context::background());
    let state = Arc::new((
        Mutex::new(ServerState {
            stopped: false,
            in_flight: 0,
            active_workers: 0,
        }),
        Condvar::new(),
    ));

    let mut subs: Vec<CallbackSubscription> = Vec::with_capacity(ops.len());
    for (op, runner) in ops {
        let subj = subject_rpc(host, op);
        let state_cl = Arc::clone(&state);
        let handler_cl = Arc::clone(&handler);
        let ctx_cl = ctx.clone();

        let sub = match nc.subscribe_callback(&subj, move |msg| {
            {
                let (lock, _) = &*state_cl;
                let mut st = lock.lock().unwrap();
                if st.in_flight >= MAX_IN_FLIGHT {
                    drop(st);
                    reply(
                        &msg,
                        RPCResponse {
                            error: format!("busy: {MAX_IN_FLIGHT} requests already in flight"),
                            data: None,
                        },
                    );
                    return;
                }
                if st.stopped {
                    return;
                }
                st.in_flight += 1;
                st.active_workers += 1;
            }

            let state_worker = Arc::clone(&state_cl);
            let handler_worker = Arc::clone(&handler_cl);
            let ctx_worker = ctx_cl.clone();
            std::thread::spawn(move || {
                let resp = serve_one(&ctx_worker, &*handler_worker, op, runner, &msg.data);
                reply(&msg, resp);
                let (lock, cvar) = &*state_worker;
                let mut st = lock.lock().unwrap();
                st.in_flight -= 1;
                st.active_workers -= 1;
                if st.active_workers == 0 {
                    cvar.notify_all();
                }
            });
        }) {
            Ok(s) => s,
            Err(e) => {
                for mut s in subs {
                    let _ = s.unsubscribe();
                }
                cancel.cancel();
                anyhow::bail!("subscribing to {subj}: {e}");
            }
        };
        subs.push(sub);
    }

    if let Err(e) = nc.flush() {
        for mut s in subs {
            let _ = s.unsubscribe();
        }
        cancel.cancel();
        anyhow::bail!("flushing subscriptions: {e}");
    }

    Ok(RpcServer {
        conn: nc.clone(),
        subs: Mutex::new(subs),
        state,
        cancel,
        once: Once::new(),
    })
}

fn serve_one(
    ctx: &Context,
    h: &dyn Handler,
    op: &'static str,
    runner: OpRunner,
    data: &[u8],
) -> RPCResponse {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runner(ctx, h, data)));
    match outcome {
        Ok(Ok(raw)) => RPCResponse {
            error: String::new(),
            data: raw,
        },
        Ok(Err(err)) => RPCResponse {
            error: format!("{err:#}"),
            data: None,
        },
        Err(payload) => RPCResponse {
            error: format!("internal error in {op}: {}", panic_message(&*payload)),
            data: None,
        },
    }
}

fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

fn reply(msg: &Msg, resp: RPCResponse) {
    if msg.reply.is_none() {
        return;
    }
    let data =
        serde_json::to_vec(&resp).unwrap_or_else(|_| br#"{"error":"encoding response"}"#.to_vec());
    let _ = msg.respond(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ALL_OPS;

    // Port of internal/natsbus/ops_test.go: TestAllOpsMatchesRPCOps.
    #[test]
    fn test_all_ops_matches_rpc_ops() {
        let mut all: Vec<&str> = ALL_OPS.to_vec();
        all.sort_unstable();
        let mut served: Vec<&str> = RPC_OPS.iter().map(|&(name, _)| name).collect();
        served.sort_unstable();
        assert_eq!(
            all, served,
            "ALL_OPS = {all:?}, RPC_OPS has {served:?}: list each op in both"
        );
    }
}
