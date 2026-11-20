//! The cross-host agent bus (`internal/natsbus`).
//!
//! The embedded NATS server itself runs in the small Go sidecar `agent-busd`
//! (`busd/`); everything else — contract, identity and pinned TLS, local client
//! connection, RPC client and server, status and focus events, `wait`, and
//! `agent-busd` child lifecycle — lives here behind plain blocking functions.

pub mod client;
pub mod contract;
pub mod daemon;
pub mod events;
pub mod gotime;
pub mod identity;
pub mod rpc;
mod runtime;

pub use client::{
    CallbackSubscription, Conn, DEFAULT_RPC_TIMEOUT, DELIVER_TIMEOUT, FLEET_TIMEOUT,
    LOCAL_CONNECT_TIMEOUT, LocalConnector, Msg, NoRespondersError, RPCClient, RemoteError,
    RpcClient, SPAWN_TIMEOUT, Subscription, TimeoutError, connect_local, connect_raw,
    is_no_responders, is_timeout, is_unanswered,
};
pub use contract::*;
pub use daemon::{
    BUS_LOG_FILE, BUS_URL_FILE, BUSD_BINARY_NAME, BUSD_ERR_FILE, BUSD_LINKS_FILE, BUSD_READY_FILE,
    BUSD_READY_TIMEOUT, DEFAULT_CLIENT_PORT, DISCLAIMED_ENV, Daemon, DaemonConfig,
    LOCAL_NETWORK_WARNING, LaunchOptions, find_busd, find_busd_with, local_network_warning,
    start_daemon, start_daemon_pipeless, start_daemon_with, stop_pid,
};
pub use events::{
    CheckFn, DEFAULT_FLUSH_TIMEOUT, FocusSubscription, Observation, Publisher, WaitOptions,
    decode_focus_event, decode_status_event, publish_focus, publish_status, subscribe_focus, wait,
    wait_observation,
};
pub use gotime::Time;
pub use identity::{
    ErrNoIdentity, Identity, PeerBundle, SameKeysError, canonical_host, check_rename,
    init_identity, is_no_identity, load_identity, load_peers, previous_hosts, rename_identity,
    rename_peer, spki_sha256, trust_peer, untrust_peer,
};
pub use rpc::{Handler, MAX_IN_FLIGHT, RpcServer, serve_rpc};
pub use runtime::{CancelFn, Context, ContextError, is_canceled, is_deadline_exceeded};
