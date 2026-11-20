//! Go `internal/service`: the business logic of agent operations (spawn,
//! send, status, held messages, permission prompts, resume, the bus's RPC
//! ops), as module [`service`], re-exported at the crate root.
//!
//! `watcher` and `export` (the other Go packages this crate replaces) are
//! ported separately.

pub mod service;
pub mod watcher;

pub use service::*;
