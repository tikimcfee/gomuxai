//! Package `agent-discovery` finds every agent session this host can see: the
//! transcripts Claude Code, agy and Kimi Code keep in their own directories,
//! whether gomuxai spawned the session or a person ran the CLI in a terminal,
//! and joins the ones a gomuxai agent records to that agent.
//!
//! It reads only the metadata a listing needs (the head and tail of a
//! transcript, and the harnesses' small index files), never a whole
//! conversation; `agent-transcript` parses those.

pub mod agy;
pub mod claude;
pub mod r#continue;
pub mod dirs;
pub mod index;
pub mod kimi;
#[cfg(target_os = "linux")]
mod lock_linux;
#[cfg(not(target_os = "linux"))]
mod lock_other;
pub mod resolve;
pub mod sessions;

#[cfg(test)]
mod continue_test;
#[cfg(test)]
mod discovery_test;
#[cfg(test)]
mod index_test;

pub use r#continue::r#continue;
pub use dirs::{
    Dir, HARNESS_AGY, HARNESS_CLAUDE, HARNESS_KIMI, HARNESSES, ORIGIN_CONFIG, ORIGIN_DEFAULT,
    Overrides, config_key, contract_home, default_candidates, describe_dirs, dirs, harness_of,
    resolve_dirs,
};
pub use index::{Index, Query, new_index};
pub use resolve::{
    Agent, DENIED_LAG, Live, NATIVE_GONE, NATIVE_WINS, Native, RECENT_WINDOW, REPORT_DENIED,
    REPORT_STANDS, Report, STATUS_ENDED, STATUS_ERROR, STATUS_IDLE, STATUS_PROCESSING,
    STATUS_UNKNOWN, STATUS_WAITING, reconcile_native, resolve,
};
pub use sessions::{ParentSession, Session, Workgroup, probe, scan};
