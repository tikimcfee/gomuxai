//! Implements gomuxai's terminal multiplexer backend over `g3d-termd`'s Unix
//! socket protocol (version 1.5), built on the `g3d` and `g3d-wire` crates.

pub mod chord;
pub mod client;
pub mod ops;
pub mod paths;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod wire;

#[cfg(test)]
mod termd_tests;

pub use chord::{KeyEvent, caller_env, caller_env_os, parse_special_key};
pub use client::{
    Client, DEFAULT_NAMESPACE, DaemonStatus, Options, StatusDoc, SubscriberStatus, TEST_SERVER_ENV,
    TerminalStatus, new_client, resolve_terminal_cwd,
};
pub use ops::{Ops, PaneInfo, Session};
pub use paths::{
    attach_binary, check_test_server, daemon_binary, is_cargo_test_exe, is_test_binary, log_path,
    socket_dir, socket_path, validate_namespace, validate_terminal_id,
};
pub use wire::{
    NotRunningError, UnsafeSocketError, WireError, check_socket_dir, daemon_env, is_not_running,
    is_unsafe_socket, is_wire_code, read_owned_regular_file, stop_socket, verify_peer_creds,
    verify_peer_creds_for_uid,
};
