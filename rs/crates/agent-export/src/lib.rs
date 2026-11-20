//! Package `agent-export` builds the agent tree that `agent export` prints: a
//! snapshot of every agent in one or more stores, and the changes after it,
//! read from the stores' agent events. It only reads: stores open read-only,
//! and nothing runs a backend CLI.

pub mod export;

#[cfg(test)]
mod export_test;

pub use export::*;
