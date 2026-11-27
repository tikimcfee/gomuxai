//! SQLite-based persistence for the agent tool (Go `internal/storage`).
//!
//! The database is shared with the Go binary during rollout, so the schema,
//! migrations, pragmas and every stored value's text are the Go's exactly:
//! see [`gotime`] for timestamps and [`gojson`] for the JSON columns.

pub mod commands;
pub mod dream_proposals;
pub mod dream_workers;
pub mod dreams;
pub mod friction;
pub mod friction_links;
pub mod gojson;
pub mod gotime;
pub mod host_rename;
pub mod host_state;
pub mod jobs;
pub mod manage;
pub mod models;
pub mod roster;
pub mod session_reports;
pub mod sqlite;
pub mod storage;
pub mod tree;

#[cfg(test)]
mod commands_test;
#[cfg(test)]
mod dream_workers_test;
#[cfg(test)]
mod dreams_test;
#[cfg(test)]
mod friction_links_test;
#[cfg(test)]
mod friction_test;
#[cfg(test)]
mod host_rename_test;
#[cfg(test)]
mod roster_test;
#[cfg(test)]
mod tree_test;

pub use dream_proposals::{
    PROPOSAL_ACCEPTED, PROPOSAL_PENDING, PROPOSAL_REJECTED, PROPOSAL_SKIP, Proposal,
};
pub use dream_workers::{
    CALLER_SYNTHESIS, DreamIdea, DreamWorker, IDEA_APPLIED, IDEA_APPROVED, IDEA_PROPOSED,
    IDEA_REJECTED, IDEA_SKIPPED, IdeaVote, ROLE_CRITIC, ROLE_DREAMER, ROLE_GENERATOR, Scores,
    Verdict, WORKER_DONE, WORKER_FAILED, WORKER_RUNNING,
};
pub use dreams::{
    DREAM_ABANDONED, DREAM_COMMITTED, DREAM_KIND_GLOBAL, DREAM_KIND_PROJECT, DREAM_OPEN,
    DREAM_QUIET, Dream, DreamItem, DreamOf,
};
pub use gotime::Time;
pub use host_rename::HostRenamed;
pub use jobs::{Job, PromotedScript};
pub use manage::{Pruned, Untracked, UsageRow};
pub use models::*;
pub use roster::{RosterEntry, RosterNote, RosterPick};
pub use session_reports::SessionReport;
pub use sqlite::{ClaimUpdate, Store, new_sqlite_storage};
pub use storage::SQLiteStorage;
pub use tree::{
    AGENT_REMOVED, AGENT_SPAWNED, AGENT_STATUS, AgentEvent, Run, STATE_REMOVED_WHY, Tree,
    open_read_only,
};
