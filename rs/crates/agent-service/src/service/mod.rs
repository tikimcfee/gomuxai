//! Go `internal/service`: business logic for agent operations.
//!
//! [`AgentService`] keeps its Go shape: one struct, the Go methods as
//! methods, split over the same files (`impl AgentService` blocks per
//! file). Where the Go takes a `context.Context` only to end its waits, the
//! methods that can wait take a [`Ctx`] (see ctx.rs); the rest take none.

mod adopt;
mod agent_service;
mod caller;
mod ctx;
mod dialog;
mod errors;
mod events;
mod friction;
mod handover;
mod held;
mod hook;
mod instructions;
mod model;
mod resume;
pub mod revive;
mod rpc;
mod slash;
mod status;
mod submit;
mod transcripts;
mod trust;

pub use adopt::*;
pub use agent_service::*;
pub use ctx::{CancelFn, Context, ContextError};
pub use dialog::*;
pub use errors::AgentNotFoundError;
pub use events::*;
pub use friction::*;
pub use handover::*;
pub use held::*;
pub use hook::*;
pub use instructions::skill_text;
pub use model::validate_model;
pub use resume::*;
pub use rpc::*;
pub use slash::*;
pub use status::*;
pub use submit::*;
pub use transcripts::*;
pub use trust::*;

use std::time::Duration;

use agent_store::Time;

/// How long ago t was (Go's `time.Since(t)`); zero for a time in the future.
pub(crate) fn since(t: &Time) -> Duration {
    let d = Time::now().unix_nano().saturating_sub(t.unix_nano());
    if d <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(d as u64)
    }
}

/// Go's `time.Unix(0, nanos)`: that instant, in the local zone.
pub(crate) fn time_from_unix_nano(nanos: i64) -> Time {
    use chrono::TimeZone;
    Time(chrono::Local.timestamp_nanos(nanos).fixed_offset())
}

/// Go's `os.RemoveAll`: a path that doesn't exist is no error.
pub(crate) fn remove_all(path: &str) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
        Ok(m) if m.is_dir() => match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
        Ok(_) => match std::fs::remove_file(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        },
    }
}

/// Go's `os.UserHomeDir` error: "$HOME is not defined".
pub(crate) fn user_home_dir() -> anyhow::Result<String> {
    agent_base::gostd::os::user_home_dir()
}
