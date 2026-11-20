//! Local NATS client connection (`ConnectLocal` in `daemon.go`) and cross-host
//! RPC client (`internal/natsbus/rpc_client.go`).
//!
//! Uses `async-nats` internally while exposing plain blocking functions and
//! types outside (rule 4).

use std::collections::BTreeMap;
use std::error::Error as StdError;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use agent_base::gostd::strconv::quote;
use agent_base::gostd::time::format_duration;
use agent_base::util::{sanitize_styled, sanitize_terminal};
use async_nats::client::RequestErrorKind;
use async_nats::rustls::client::danger::{
    HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
};
use async_nats::rustls::crypto::{
    WebPkiSupportedAlgorithms, ring as rustls_ring, verify_tls12_signature, verify_tls13_signature,
};
use async_nats::rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use async_nats::rustls::{
    ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme, version,
};
use async_nats::{ConnectErrorKind, ConnectOptions};
use futures::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::contract::{
    AgentInfo, AgentRef, CaptureRequest, CaptureResponse, DeliverResponse, ErrNoBus, FleetAgent,
    FleetFriction, FleetHost, FleetReport, FleetRequest, FocusEvent, FrictionRequest,
    FrictionResponse, KeysRequest, ListRequest, ListResponse, LogsRequest, LogsResponse,
    OP_CAPTURE, OP_FLEET, OP_FRICTION, OP_KEYS, OP_LIST, OP_LOGS, OP_RESPOND, OP_RESUME, OP_SEND,
    OP_SPAWN, OP_STATUS, OP_TEMPLATE, RESPONSE_DELIVER, RPCResponse, RespondRequest, ResumeRequest,
    ResumeResponse, SendRequest, SendResponse, SpawnRequest, SpawnResponse, StatusRequest,
    StatusResponse, TEMPLATE_RESUME, TEMPLATE_START, TemplateRequest, TemplateResponse,
    inbox_prefix, subject_rpc, validate_host,
};
use crate::daemon::BUS_URL_FILE;
use crate::identity::{load_identity, spki_sha256};
use crate::runtime::{Context, block_on, handle};

/// Bounds each RPC request unless overridden.
pub const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(10);

/// Bounds a `fleet` request across linked hosts (`docs/FLEET.md`).
pub const FLEET_TIMEOUT: Duration = Duration::from_secs(3);

/// Bounds a `deliver` request: typing held messages into Claude waits for each
/// message to be taken in.
pub const DELIVER_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds `spawn` and `resume` requests: the far side waits out CLI startup.
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(120);

/// Local connect timeout: hooks call `connect_local` on every agent event.
pub const LOCAL_CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Opens an authenticated client connection to this host's bus.
pub type LocalConnector = Box<dyn Fn() -> anyhow::Result<Conn> + Send + Sync>;

/// An error returned by the remote host's RPC handler. Its display text is the
/// remote message unchanged (sanitized for terminal safety).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{msg}")]
pub struct RemoteError {
    pub host: String,
    pub op: String,
    pub msg: String,
}

/// Returned when no bus answered a request for `host` and `op`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "no bus answering for host {} (is its bus running and linked, and does it allow {}?)",
    quote(.host),
    quote(.op)
)]
pub struct NoRespondersError {
    pub host: String,
    pub op: String,
}

/// Returned when `host` did not reply to `op` before `timeout` elapsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "host {} did not answer {} within {}",
    quote(.host),
    .op,
    format_duration(*.timeout)
)]
pub struct TimeoutError {
    pub host: String,
    pub op: String,
    pub timeout: Duration,
}

/// Reports whether `err` is [`NoRespondersError`] (the host or op has no subscriber).
pub fn is_no_responders(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<NoRespondersError>().is_some())
}

/// Reports whether `err` is [`TimeoutError`] (the request timed out waiting for a reply).
pub fn is_timeout(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|e| e.downcast_ref::<TimeoutError>().is_some())
}

/// Reports whether `err` means `host` did not answer the op at all
/// ([`NoRespondersError`] or [`TimeoutError`]), as opposed to a [`RemoteError`]
/// returned by the handler or a local bus failure. Callers such as `fleet` use
/// this to recognize an older or non-serving host and fall back to `list`.
pub fn is_unanswered(err: &anyhow::Error) -> bool {
    is_no_responders(err) || is_timeout(err)
}

/// A NATS message (`*nats.Msg` in Go).
#[derive(Debug, Clone)]
pub struct Msg {
    pub subject: String,
    pub reply: Option<String>,
    pub data: Vec<u8>,
    client: Option<async_nats::Client>,
}

impl Msg {
    pub fn new(subject: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self {
            subject: subject.into(),
            reply: None,
            data: data.into(),
            client: None,
        }
    }

    pub(crate) fn from_async(m: async_nats::Message, client: Option<async_nats::Client>) -> Self {
        Self {
            subject: m.subject.to_string(),
            reply: m.reply.map(|r| r.to_string()),
            data: m.payload.to_vec(),
            client,
        }
    }

    /// Publishes `data` to this message's `reply` subject, if present.
    pub fn respond(&self, data: impl Into<Vec<u8>>) -> anyhow::Result<()> {
        let Some(ref reply) = self.reply else {
            return Ok(());
        };
        let Some(ref client) = self.client else {
            anyhow::bail!("message has no associated connection");
        };
        let reply_subj = reply.clone();
        let payload = data.into();
        let client = client.clone();
        block_on(async move { client.publish(reply_subj, payload.into()).await })
            .map_err(|e| anyhow::anyhow!("{e}"))
    }
}

/// A synchronous NATS subscription (`*nats.Subscription` from `SubscribeSync`).
pub struct Subscription {
    sub: Option<async_nats::Subscriber>,
    client: async_nats::Client,
}

impl Subscription {
    /// Waits up to `timeout` for the next message.
    pub fn next_msg(&mut self, timeout: Duration) -> anyhow::Result<Msg> {
        let Some(sub) = self.sub.as_mut() else {
            anyhow::bail!("nats: invalid subscription");
        };
        let client = self.client.clone();
        if timeout.is_zero() {
            return self
                .try_next()
                .ok_or_else(|| anyhow::anyhow!("nats: timeout"));
        }
        block_on(async {
            match tokio::time::timeout(timeout, sub.next()).await {
                Ok(Some(m)) => Ok(Msg::from_async(m, Some(client))),
                Ok(None) => anyhow::bail!("nats: subscription closed"),
                Err(_) => anyhow::bail!("nats: timeout"),
            }
        })
    }

    /// Pops an already-buffered message without waiting, if any.
    pub fn try_next(&mut self) -> Option<Msg> {
        let sub = self.sub.as_mut()?;
        let client = self.client.clone();
        block_on(async {
            futures::future::poll_fn(|cx| match sub.poll_next_unpin(cx) {
                std::task::Poll::Ready(opt) => std::task::Poll::Ready(opt),
                std::task::Poll::Pending => std::task::Poll::Ready(None),
            })
            .await
            .map(|m| Msg::from_async(m, Some(client)))
        })
    }

    pub fn unsubscribe(&mut self) -> anyhow::Result<()> {
        if let Some(mut sub) = self.sub.take() {
            block_on(async move { sub.unsubscribe().await }).map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let _ = self.unsubscribe();
    }
}

/// A callback-driven subscription (`nc.Subscribe(subj, fn)` in Go).
pub struct CallbackSubscription {
    stop_tx: Option<tokio::sync::oneshot::Sender<()>>,
    done_rx: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl CallbackSubscription {
    pub fn unsubscribe(&mut self) -> anyhow::Result<()> {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(rx) = self.done_rx.take() {
            block_on(async move {
                let _ = tokio::time::timeout(Duration::from_secs(2), rx).await;
            });
        }
        Ok(())
    }
}

impl Drop for CallbackSubscription {
    fn drop(&mut self) {
        let _ = self.unsubscribe();
    }
}

/// A blocking client connection to a NATS bus (`*nats.Conn` in Go).
#[derive(Clone)]
pub struct Conn {
    client: async_nats::Client,
    url: Arc<str>,
}

impl std::fmt::Debug for Conn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conn").field("url", &self.url).finish()
    }
}

impl Conn {
    pub(crate) fn inner(&self) -> &async_nats::Client {
        &self.client
    }

    pub fn connected_url(&self) -> &str {
        &self.url
    }

    pub fn publish(&self, subject: &str, payload: impl Into<Vec<u8>>) -> anyhow::Result<()> {
        let subj = subject.to_string();
        let data = payload.into();
        let client = self.client.clone();
        block_on(async move { client.publish(subj, data.into()).await })
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub fn publish_with_reply(
        &self,
        subject: &str,
        reply: &str,
        payload: impl Into<Vec<u8>>,
    ) -> anyhow::Result<()> {
        let subj = subject.to_string();
        let rep = reply.to_string();
        let data = payload.into();
        let client = self.client.clone();
        block_on(async move { client.publish_with_reply(subj, rep, data.into()).await })
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    pub fn request(
        &self,
        subject: &str,
        payload: impl Into<Vec<u8>>,
        timeout: Duration,
    ) -> anyhow::Result<Msg> {
        let subj = subject.to_string();
        let data = payload.into();
        let client = self.client.clone();
        block_on(async move {
            let req = async_nats::Request::new()
                .payload(data.into())
                .timeout(Some(timeout));
            let msg = client
                .send_request(subj, req)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Msg::from_async(msg, Some(client)))
        })
    }

    pub fn subscribe(&self, subject: &str) -> anyhow::Result<Subscription> {
        let subj = subject.to_string();
        let client = self.client.clone();
        let sub = block_on(async move { client.subscribe(subj).await })
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Subscription {
            sub: Some(sub),
            client: self.client.clone(),
        })
    }

    pub fn subscribe_callback(
        &self,
        subject: &str,
        cb: impl Fn(Msg) + Send + Sync + 'static,
    ) -> anyhow::Result<CallbackSubscription> {
        let subj = subject.to_string();
        let client = self.client.clone();
        let mut sub = block_on({
            let client = client.clone();
            async move { client.subscribe(subj).await }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;

        let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        let cb = Arc::new(cb);
        handle().spawn(async move {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stop_rx => {
                        let _ = sub.unsubscribe().await;
                        break;
                    }
                    maybe_msg = sub.next() => {
                        match maybe_msg {
                            Some(m) => {
                                let msg = Msg::from_async(m, Some(client.clone()));
                                cb(msg);
                            }
                            None => break,
                        }
                    }
                }
            }
            let _ = done_tx.send(());
        });

        Ok(CallbackSubscription {
            stop_tx: Some(stop_tx),
            done_rx: Some(done_rx),
        })
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        self.flush_timeout(Duration::from_secs(10))
    }

    pub fn flush_timeout(&self, timeout: Duration) -> anyhow::Result<()> {
        let client = self.client.clone();
        block_on(async move { server_flush(&client, timeout).await })
    }

    pub fn close(&self) {
        let client = self.client.clone();
        block_on(async move {
            let _ = tokio::time::timeout(Duration::from_millis(100), client.drain()).await;
        });
    }
}

/// Flushes buffered client commands AND performs an in-order round-trip through
/// `nats-server` (equivalent to `nats.go`'s `FlushTimeout` PING/PONG), so once
/// it returns the server has processed every preceding `SUB`, `UNSUB`, and `PUB`
/// on `client`.
pub(crate) async fn server_flush(
    client: &async_nats::Client,
    timeout: Duration,
) -> anyhow::Result<()> {
    tokio::time::timeout(timeout, async {
        let inbox = client.new_inbox();
        let mut sub = client
            .subscribe(inbox.clone())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        sub.unsubscribe_after(1)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        client
            .publish(inbox, Vec::new().into())
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        client.flush().await.map_err(|e| anyhow::anyhow!("{e}"))?;
        match sub.next().await {
            Some(_) => Ok(()),
            None => Err(anyhow::anyhow!("nats: connection closed")),
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("nats: timeout"))?
}

#[derive(Debug)]
struct PinVerifier {
    host: String,
    pin: String,
    algs: WebPkiSupportedAlgorithms,
}

impl PinVerifier {
    fn new(host: impl Into<String>, pin: impl Into<String>) -> Self {
        let provider = rustls_ring::default_provider();
        Self {
            host: host.into(),
            pin: pin.into(),
            algs: provider.signature_verification_algorithms,
        }
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let got = spki_sha256(end_entity.as_ref())
            .map_err(|e| TlsError::General(format!("parsing peer certificate: {e}")))?;
        if got != self.pin {
            return Err(TlsError::General(format!(
                "peer {} certificate key {} doesn't match pinned {}",
                self.host, got, self.pin
            )));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

fn pinned_tls_config(host_label: &str, pin: &str) -> anyhow::Result<ClientConfig> {
    let provider = Arc::new(rustls_ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&version::TLS13])
        .map_err(|e| anyhow::anyhow!("configuring TLS 1.3: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinVerifier::new(host_label, pin)))
        .with_no_client_auth();
    Ok(cfg)
}

/// Opens an authenticated client connection to the bus running on this host
/// (`ConnectLocal` in Go). Returns [`ErrNoBus`] immediately when `bus.url` does
/// not exist or when the recorded port refuses connections (stale `bus.url`).
pub fn connect_local(dir: impl AsRef<Path>, host: &str) -> anyhow::Result<Conn> {
    let dir = dir.as_ref();
    let url_path = dir.join(BUS_URL_FILE);
    let url_str = match std::fs::read_to_string(&url_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(ErrNoBus.into()),
        Err(e) => {
            return Err(agent_base::gostd::os::PathError::new(
                "open",
                &url_path.to_string_lossy(),
                e,
            )
            .into());
        }
    };
    let id = load_identity(dir, host)?;
    let url = url_str.trim().to_string();

    let tls_cfg = pinned_tls_config(&format!("{host} (local bus)"), &id.cert_sha)?;
    let opts = ConnectOptions::new()
        .nkey(id.local_seed)
        .connection_timeout(LOCAL_CONNECT_TIMEOUT)
        .name(format!("agent {host}"))
        .custom_inbox_prefix(inbox_prefix(host))
        .require_tls(true)
        .tls_client_config(tls_cfg)
        .ignore_discovered_servers();

    let connect_url = url.clone();
    let res = block_on(async move { opts.connect(connect_url).await });
    match res {
        Ok(client) => Ok(Conn {
            client,
            url: Arc::from(url),
        }),
        Err(err) => {
            if is_no_server_connect_err(&err) {
                return Err(ErrNoBus.into());
            }
            Err(anyhow::anyhow!("{err}"))
        }
    }
}

/// Connects to `url` with optional nkey seed and optional pinned TLS cert SHA
/// (used by security tests to verify server rejections).
pub fn connect_raw(
    url: &str,
    cert_sha: Option<&str>,
    nkey_seed: Option<&str>,
    timeout: Duration,
) -> anyhow::Result<Conn> {
    let mut opts = ConnectOptions::new()
        .connection_timeout(timeout)
        .ignore_discovered_servers();
    if let Some(seed) = nkey_seed {
        opts = opts.nkey(seed.to_string());
    }
    if let Some(pin) = cert_sha {
        let tls_cfg = pinned_tls_config("server", pin)?;
        opts = opts.require_tls(true).tls_client_config(tls_cfg);
    }
    let u = url.trim().to_string();
    let target = u.clone();
    let client =
        block_on(async move { opts.connect(target).await }).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(Conn {
        client,
        url: Arc::from(u),
    })
}

fn is_no_server_connect_err(err: &async_nats::ConnectError) -> bool {
    if err.kind() != ConnectErrorKind::Io {
        return false;
    }
    let mut cur: Option<&(dyn StdError + 'static)> = Some(err);
    while let Some(e) = cur {
        if let Some(io_err) = e.downcast_ref::<std::io::Error>()
            && matches!(
                io_err.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::AddrNotAvailable
            )
        {
            return true;
        }
        cur = e.source();
    }
    false
}

/// Calls the RPC ops of other hosts' buses (`RPCClient` in Go).
#[derive(Clone, Debug)]
pub struct RpcClient {
    nc: Conn,
    timeout: Duration,
    custom_timeout: bool,
}

pub type RPCClient = RpcClient;

impl RpcClient {
    /// Returns an RPC client over `nc`. Zero `timeout` uses [`DEFAULT_RPC_TIMEOUT`].
    pub fn new(nc: Conn, timeout: Duration) -> Self {
        let custom_timeout = !timeout.is_zero();
        let timeout = if custom_timeout {
            timeout
        } else {
            DEFAULT_RPC_TIMEOUT
        };
        Self {
            nc,
            timeout,
            custom_timeout,
        }
    }

    fn with_min_timeout(&self, min: Duration) -> Self {
        Self {
            nc: self.nc.clone(),
            timeout: self.timeout.max(min),
            custom_timeout: self.custom_timeout,
        }
    }

    fn with_fleet_timeout(&self) -> Self {
        let timeout = if self.custom_timeout {
            self.timeout.min(FLEET_TIMEOUT)
        } else {
            FLEET_TIMEOUT
        };
        Self {
            nc: self.nc.clone(),
            timeout,
            custom_timeout: self.custom_timeout,
        }
    }

    /// Returns the agents on `host`.
    pub fn list(&self, ctx: &Context, host: &str) -> anyhow::Result<Vec<AgentInfo>> {
        Ok(self.list_host(ctx, host)?.agents)
    }

    /// Returns `host`'s `list` reply: its agents and `agent_binary`.
    pub fn list_host(&self, ctx: &Context, host: &str) -> anyhow::Result<ListResponse> {
        let mut resp: ListResponse = self.call(ctx, host, OP_LIST, &ListRequest {})?;
        resp.agent_binary = clean(&resp.agent_binary);
        for a in &mut resp.agents {
            clean_agent_info(a);
        }
        Ok(resp)
    }

    /// Returns the status of a remote agent.
    pub fn status(&self, ctx: &Context, ref_: &AgentRef) -> anyhow::Result<StatusResponse> {
        let req = StatusRequest {
            agent: ref_.name.clone(),
        };
        let mut resp: StatusResponse = self.call_agent(ctx, ref_, OP_STATUS, &req)?;
        resp.status = clean(&resp.status);
        resp.end_reason = clean(&resp.end_reason);
        resp.source = clean(&resp.source);
        resp.event = clean(&resp.event);
        resp.detail = clean(&resp.detail);
        resp.tool = clean(&resp.tool);
        resp.tool_input = clean_tool_input(&resp.tool_input);
        resp.reported_status = clean(&resp.reported_status);
        resp.model = clean(&resp.model);
        Ok(resp)
    }

    /// Returns a remote agent's terminal output, sanitized.
    pub fn capture(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        lines: i64,
        raw: bool,
    ) -> anyhow::Result<String> {
        let req = CaptureRequest {
            agent: ref_.name.clone(),
            lines,
            raw,
        };
        let resp: CaptureResponse = self.call_agent(ctx, ref_, OP_CAPTURE, &req)?;
        if raw {
            Ok(sanitize_styled(&resp.output))
        } else {
            Ok(clean(&resp.output))
        }
    }

    /// Sends `message` to a remote agent.
    pub fn send(&self, ctx: &Context, ref_: &AgentRef, message: &str) -> anyhow::Result<()> {
        let _ = self.send_message(
            ctx,
            ref_,
            SendRequest {
                agent: String::new(),
                message: message.to_string(),
                now: false,
            },
        )?;
        Ok(())
    }

    /// Sends a message to a remote agent, returning `None` if the serving host
    /// predates `SendResponse`.
    pub fn send_message(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        mut req: SendRequest,
    ) -> anyhow::Result<Option<SendResponse>> {
        req.agent = ref_.name.clone();
        if ref_.host.is_empty() {
            anyhow::bail!("{OP_SEND}: no host given for agent {}", quote(&ref_.name));
        }
        let Some(mut resp): Option<SendResponse> =
            self.call_optional(ctx, &ref_.host, OP_SEND, &req)?
        else {
            return Ok(None);
        };
        resp.status = clean(&resp.status);
        resp.detail = clean(&resp.detail);
        resp.held_until = clean(&resp.held_until);
        resp.input_blocked = clean(&resp.input_blocked);
        resp.unchecked = clean(&resp.unchecked);
        Ok(Some(resp))
    }

    /// Answers a remote agent's permission prompt.
    pub fn respond(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        mut req: RespondRequest,
    ) -> anyhow::Result<()> {
        req.agent = ref_.name.clone();
        self.call_agent_unit(ctx, ref_, OP_RESPOND, &req)
    }

    /// Types the messages held for a remote agent if it is at its prompt.
    pub fn deliver(&self, ctx: &Context, ref_: &AgentRef) -> anyhow::Result<DeliverResponse> {
        let long = self.with_min_timeout(DELIVER_TIMEOUT);
        let req = RespondRequest {
            agent: ref_.name.clone(),
            response: RESPONSE_DELIVER.to_string(),
            force: false,
            expect_tool: String::new(),
        };
        let mut resp: DeliverResponse = long.call_agent(ctx, ref_, OP_RESPOND, &req)?;
        resp.status = clean(&resp.status);
        Ok(resp)
    }

    /// Sends raw `text` and/or `key` to a remote agent's terminal.
    pub fn keys(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        text: &str,
        key: &str,
    ) -> anyhow::Result<()> {
        let req = KeysRequest {
            agent: ref_.name.clone(),
            text: text.to_string(),
            key: key.to_string(),
        };
        self.call_agent_unit(ctx, ref_, OP_KEYS, &req)
    }

    /// Starts an agent on `ref_.host`.
    pub fn spawn(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        mut req: SpawnRequest,
    ) -> anyhow::Result<SpawnResponse> {
        req.agent = ref_.name.clone();
        let long = self.with_min_timeout(SPAWN_TIMEOUT);
        let mut resp: SpawnResponse = long.call_agent(ctx, ref_, OP_SPAWN, &req)?;
        resp.name = clean(&resp.name);
        resp.session = clean(&resp.session);
        resp.backend = clean(&resp.backend);
        resp.spawned_by = clean(&resp.spawned_by);
        resp.cwd = clean(&resp.cwd);
        resp.task_file = clean(&resp.task_file);
        resp.model = clean(&resp.model);
        resp.status = clean(&resp.status);
        resp.detail = clean(&resp.detail);
        resp.end_reason = clean(&resp.end_reason);
        Ok(resp)
    }

    /// Returns what `agent logs` prints for a remote agent, sanitized.
    pub fn logs(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        mut req: LogsRequest,
    ) -> anyhow::Result<LogsResponse> {
        req.agent = ref_.name.clone();
        let mut resp: LogsResponse = self.call_agent(ctx, ref_, OP_LOGS, &req)?;
        resp.output = clean(&resp.output);
        resp.hint = clean(&resp.hint);
        Ok(resp)
    }

    /// Resumes an ended agent on `ref_.host`.
    pub fn resume(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        mut req: ResumeRequest,
    ) -> anyhow::Result<ResumeResponse> {
        req.agent = ref_.name.clone();
        let long = self.with_min_timeout(SPAWN_TIMEOUT);
        let mut resp: ResumeResponse = long.call_agent(ctx, ref_, OP_RESUME, &req)?;
        resp.name = clean(&resp.name);
        resp.session = clean(&resp.session);
        resp.backend = clean(&resp.backend);
        resp.session_id = clean(&resp.session_id);
        resp.cwd = clean(&resp.cwd);
        resp.status = clean(&resp.status);
        resp.end_reason = clean(&resp.end_reason);
        resp.detail = clean(&resp.detail);
        Ok(resp)
    }

    /// Sends a friction request to `host` and returns its answer, sanitized.
    pub fn friction(
        &self,
        ctx: &Context,
        host: &str,
        req: FrictionRequest,
    ) -> anyhow::Result<FrictionResponse> {
        let mut resp: FrictionResponse = self.call(ctx, host, OP_FRICTION, &req)?;
        for e in &mut resp.entries {
            e.host = clean(&e.host);
            e.uid = clean(&e.uid);
            e.status = clean(&e.status);
            e.kind = clean(&e.kind);
            e.summary = clean(&e.summary);
            e.resolved_by = clean(&e.resolved_by);
            e.resolution = clean(&e.resolution);
            if let Some(ref mut req_ref) = e.requested {
                req_ref.host = clean(&req_ref.host);
                req_ref.uid = clean(&req_ref.uid);
            }
            for link in &mut e.links {
                link.host = clean(&link.host);
                link.uid = clean(&link.uid);
            }
        }
        Ok(resp)
    }

    /// Queries `host`'s `fleet` view (3s default timeout), sanitizing remote text.
    /// When `host` is an older Go build or doesn't allow `fleet`, fails with
    /// [`NoRespondersError`] or [`TimeoutError`] (see [`is_unanswered`]).
    pub fn fleet(&self, ctx: &Context, host: &str, req: FleetRequest) -> anyhow::Result<FleetHost> {
        let client = self.with_fleet_timeout();
        let mut resp: FleetHost = client.call(ctx, host, OP_FLEET, &req)?;
        clean_fleet_host(&mut resp);
        Ok(resp)
    }

    /// Sends a `template` request (`list`, `start`, or `resume`) to `host` and
    /// returns its sanitized response.
    pub fn template(
        &self,
        ctx: &Context,
        host: &str,
        req: TemplateRequest,
    ) -> anyhow::Result<TemplateResponse> {
        let client = if req.action == TEMPLATE_START || req.action == TEMPLATE_RESUME {
            self.with_min_timeout(SPAWN_TIMEOUT)
        } else {
            self.clone()
        };
        let mut resp: TemplateResponse = client.call(ctx, host, OP_TEMPLATE, &req)?;
        for t in &mut resp.templates {
            t.name = clean(&t.name);
            t.backend = clean(&t.backend);
            t.model = clean(&t.model);
            t.cwd = clean(&t.cwd);
            t.brief = clean(&t.brief);
            t.problem = clean(&t.problem);
        }
        if let Some(ref mut a) = resp.agent {
            clean_agent_info(a);
        }
        Ok(resp)
    }

    fn call_agent<Req: Serialize, Out: Default + DeserializeOwned>(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        op: &str,
        req: &Req,
    ) -> anyhow::Result<Out> {
        if ref_.host.is_empty() {
            anyhow::bail!("{op}: no host given for agent {}", quote(&ref_.name));
        }
        self.call(ctx, &ref_.host, op, req)
    }

    fn call_agent_unit<Req: Serialize>(
        &self,
        ctx: &Context,
        ref_: &AgentRef,
        op: &str,
        req: &Req,
    ) -> anyhow::Result<()> {
        if ref_.host.is_empty() {
            anyhow::bail!("{op}: no host given for agent {}", quote(&ref_.name));
        }
        self.call_unit(ctx, &ref_.host, op, req)
    }

    fn call<Req: Serialize, Out: Default + DeserializeOwned>(
        &self,
        ctx: &Context,
        host: &str,
        op: &str,
        req: &Req,
    ) -> anyhow::Result<Out> {
        let resp = self.send_rpc(ctx, host, op, req)?;
        let Some(raw) = resp.data else {
            anyhow::bail!("empty {op} reply from host {}", quote(host));
        };
        let s = raw.get().trim();
        if s.is_empty() {
            anyhow::bail!("empty {op} reply from host {}", quote(host));
        }
        if s == "null" {
            return Ok(Out::default());
        }
        serde_json::from_str(s)
            .map_err(|e| anyhow::anyhow!("malformed {op} reply from host {}: {e}", quote(host)))
    }

    fn call_optional<Req: Serialize, Out: DeserializeOwned>(
        &self,
        ctx: &Context,
        host: &str,
        op: &str,
        req: &Req,
    ) -> anyhow::Result<Option<Out>> {
        let resp = self.send_rpc(ctx, host, op, req)?;
        let Some(raw) = resp.data else {
            return Ok(None);
        };
        let s = raw.get();
        if s.is_empty() || s == "null" {
            return Ok(None);
        }
        let out = serde_json::from_str(s)
            .map_err(|e| anyhow::anyhow!("malformed {op} reply from host {}: {e}", quote(host)))?;
        Ok(Some(out))
    }

    fn call_unit<Req: Serialize>(
        &self,
        ctx: &Context,
        host: &str,
        op: &str,
        req: &Req,
    ) -> anyhow::Result<()> {
        let _ = self.send_rpc(ctx, host, op, req)?;
        Ok(())
    }

    fn send_rpc<Req: Serialize>(
        &self,
        ctx: &Context,
        host: &str,
        op: &str,
        req: &Req,
    ) -> anyhow::Result<RPCResponse> {
        validate_host(host)?;
        let data =
            serde_json::to_vec(req).map_err(|e| anyhow::anyhow!("encoding {op} request: {e}"))?;

        let (call_ctx, cancel) = Context::with_timeout(ctx, self.timeout);
        let subj = subject_rpc(host, op);
        let client = self.nc.inner().clone();
        let req_timeout = self.timeout;

        let res = block_on(async move {
            let nats_req = async_nats::Request::new()
                .payload(data.into())
                .timeout(Some(req_timeout));
            tokio::select! {
                biased;
                cerr = call_ctx.done() => Err(RpcTransportErr::Ctx(cerr)),
                r = client.send_request(subj, nats_req) => r.map_err(RpcTransportErr::Nats),
            }
        });
        cancel.cancel();

        let msg = match res {
            Ok(m) => m,
            Err(RpcTransportErr::Nats(ref e)) if e.kind() == RequestErrorKind::NoResponders => {
                return Err(NoRespondersError {
                    host: host.to_string(),
                    op: op.to_string(),
                }
                .into());
            }
            Err(RpcTransportErr::Nats(ref e)) if e.kind() == RequestErrorKind::TimedOut => {
                return Err(TimeoutError {
                    host: host.to_string(),
                    op: op.to_string(),
                    timeout: self.timeout,
                }
                .into());
            }
            Err(RpcTransportErr::Ctx(crate::runtime::ContextError::DeadlineExceeded)) => {
                return Err(TimeoutError {
                    host: host.to_string(),
                    op: op.to_string(),
                    timeout: self.timeout,
                }
                .into());
            }
            Err(RpcTransportErr::Ctx(cerr)) => {
                anyhow::bail!("{op} request to host {}: {cerr}", quote(host));
            }
            Err(RpcTransportErr::Nats(e)) => {
                anyhow::bail!("{op} request to host {}: {e}", quote(host));
            }
        };

        let resp: RPCResponse = serde_json::from_slice(&msg.payload)
            .map_err(|e| anyhow::anyhow!("malformed {op} reply from host {}: {e}", quote(host)))?;
        if !resp.error.is_empty() {
            return Err(RemoteError {
                host: host.to_string(),
                op: op.to_string(),
                msg: clean(&resp.error),
            }
            .into());
        }
        Ok(resp)
    }
}

enum RpcTransportErr {
    Ctx(crate::runtime::ContextError),
    Nats(async_nats::RequestError),
}

fn clean(s: &str) -> String {
    sanitize_terminal(s)
}

/// Sanitizes `tool_input` both as raw text and — when it is JSON — inside every
/// decoded string value/key, so JSON-escaped control sequences (`\u001b]52;...`)
/// cannot survive into `formatToolInput` when the CLI decodes `tool_input`.
fn clean_tool_input(s: &str) -> String {
    let cleaned = clean(s);
    if cleaned.is_empty() {
        return cleaned;
    }
    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&cleaned) {
        clean_json_value(&mut val);
        if let Ok(reencoded) = serde_json::to_string(&val) {
            return reencoded;
        }
    }
    cleaned
}

fn clean_json_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => *s = clean(s),
        serde_json::Value::Array(arr) => {
            for item in arr {
                clean_json_value(item);
            }
        }
        serde_json::Value::Object(map) => {
            let entries: Vec<(String, serde_json::Value)> = std::mem::take(map)
                .into_iter()
                .map(|(k, mut val)| {
                    clean_json_value(&mut val);
                    (clean(&k), val)
                })
                .collect();
            for (k, val) in entries {
                map.insert(k, val);
            }
        }
        _ => {}
    }
}

fn clean_agent_info(a: &mut AgentInfo) {
    a.name = clean(&a.name);
    a.session = clean(&a.session);
    a.backend = clean(&a.backend);
    a.spawned_by = clean(&a.spawned_by);
    a.status = clean(&a.status);
    a.end_reason = clean(&a.end_reason);
    a.detail = clean(&a.detail);
    a.session_id = clean(&a.session_id);
    a.cwd = clean(&a.cwd);
}

fn clean_focus_event(ev: &mut FocusEvent) {
    ev.host = clean(&ev.host);
    ev.agent = clean(&ev.agent);
    ev.session = clean(&ev.session);
    ev.by = clean(&ev.by);
}

fn clean_fleet_agent(a: &mut FleetAgent) {
    a.name = clean(&a.name);
    a.session = clean(&a.session);
    a.spawned_by = clean(&a.spawned_by);
    a.backend = clean(&a.backend);
    a.model = clean(&a.model);
    a.status = clean(&a.status);
    a.end_reason = clean(&a.end_reason);
    a.detail = clean(&a.detail);
    a.session_id = clean(&a.session_id);
    a.session_name = clean(&a.session_name);
    a.cwd = clean(&a.cwd);
    for f in &mut a.flags {
        *f = clean(f);
    }
    for c in &mut a.children {
        clean_fleet_agent(c);
    }
}

fn clean_map(m: &BTreeMap<String, i64>) -> BTreeMap<String, i64> {
    m.iter().map(|(k, &v)| (clean(k), v)).collect()
}

fn clean_fleet_friction(f: &mut FleetFriction) {
    f.by_kind = clean_map(&f.by_kind);
}

fn clean_fleet_report(r: &mut FleetReport) {
    r.agents_by_status = clean_map(&r.agents_by_status);
    r.agents_by_backend = clean_map(&r.agents_by_backend);
    r.sessions_7d_by_harness = clean_map(&r.sessions_7d_by_harness);
    r.messages_per_day_7d = clean_map(&r.messages_per_day_7d);
    r.friction_open_by_kind = clean_map(&r.friction_open_by_kind);
    r.friction_resolved_by_kind = clean_map(&r.friction_resolved_by_kind);
    for s in &mut r.long_sessions {
        s.agent = clean(&s.agent);
        s.session_id = clean(&s.session_id);
        s.backend = clean(&s.backend);
    }
}

fn clean_fleet_host(h: &mut FleetHost) {
    h.host = clean(&h.host);
    h.build = clean(&h.build);
    h.error = clean(&h.error);
    for a in &mut h.agents {
        clean_fleet_agent(a);
    }
    if let Some(ref mut f) = h.focus {
        clean_focus_event(f);
    }
    for j in &mut h.jobs {
        j.script = clean(&j.script);
        j.by = clean(&j.by);
        j.unit = clean(&j.unit);
        j.memory_max = clean(&j.memory_max);
    }
    for ho in &mut h.handoffs {
        ho.agent = clean(&ho.agent);
        ho.path = clean(&ho.path);
    }
    if let Some(ref mut fr) = h.friction {
        clean_fleet_friction(fr);
    }
    if let Some(ref mut rep) = h.report {
        clean_fleet_report(rep);
    }
}
