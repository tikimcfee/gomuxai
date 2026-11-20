//! Package `agent-transcript` reads the transcripts agent CLIs keep of their
//! own sessions (Claude Code's session JSONL, agy's `transcript_full.jsonl`,
//! Kimi Code's `wire.jsonl`) into one harness-neutral stream of events
//! (`events.rs`; `docs/EVENTS.md` is the contract): a prompt, a thinking block,
//! a reply, a tool call and its result, a harness record, each where it
//! happened and tagged with the turn it belongs to.
//!
//! A [`Reader`]'s [`Parser`] reads a transcript line by line, so the same code
//! reads a whole file, a window of one, and the lines a live session appends.
//! The formats are the CLIs' internal files, not documented APIs: a line that
//! isn't a JSON object is skipped and counted (`Parser::skipped`), and a record
//! of an unknown shape is ignored.

pub mod agy;
pub mod claude;
mod claude_rewind;
pub mod events;
pub mod r#final;
pub mod kimi;
pub mod meta;
pub mod tools;
pub mod transcript;

#[cfg(test)]
mod corpus_test;
#[cfg(test)]
mod final_test;
#[cfg(test)]
mod review_test;
#[cfg(test)]
mod tools_test;
#[cfg(test)]
mod transcript_test;

pub use agy::{Agy, agy_conversation_dir};
pub use claude::{Claude, TaskNotification, claude_subagent_turns, task_notifications};
pub use events::*;
pub use r#final::{Finals, new_finals};
pub use kimi::Kimi;
pub use meta::{
    AgyFiles, AgySubagent, ClaudeSubagentMeta, claude_meta_record, claude_subagent_session,
    harvest_claude, harvest_kimi, project_of, read_agy_history, read_agy_subagent,
    read_claude_subagent_meta,
};
pub use tools::{ACTION_NOISE, FileText, ToolSummary, describe_tool, summarize_tool};
pub use transcript::{ErrUnparseable, File, detect, file_options, read_file, session_id_from_path};
