//! Reads Claude Code's session registry: one file per running CLI process,
//! `<config dir>/sessions/<pid>.json`, which Claude Code writes at start,
//! rewrites as its status moves, and removes at exit.
//!
//! The registry is Claude Code's own record of which session each of its
//! processes holds, so it is how gomuxai knows what runs where. Its format is
//! not a published contract (it is what `claude agents --json` is built on), so
//! this crate reads only the fields it names, and treats a value outside the
//! vocabulary here as a problem to report, never as a guess.
//!
//! An entry is trusted only when it is proven: its pid is running and that
//! process started when the entry says (`proc_start`), the same check Claude Code
//! makes before it believes an entry. An entry's `tmux` field is NOT used to
//! place it: Claude copies it from its environment, so a `claude -p` run from
//! inside an agent's pane names that agent's pane too. Where a session runs is
//! read from the process tree instead (see [`Snapshot::under_pane`]).

pub mod continued;
pub mod proof;
#[cfg(target_os = "macos")]
mod proof_darwin;
#[cfg(target_os = "linux")]
mod proof_linux;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod proof_other;
pub mod registry;
pub mod running;

pub use continued::{CONTINUED_TAIL, continued_in, find_transcript};
pub use proof::{Host, System, Verdict, proof, stat_start_time};
pub use registry::{
    Entry, KIND_BACKGROUND, KIND_DAEMON, KIND_DAEMON_WORKER, KIND_INTERACTIVE, Problem,
    STATUS_BUSY, STATUS_IDLE, STATUS_SHELL, STATUS_WAITING, dir, parse_entry, read_dir,
};
pub use running::{Proven, Snapshot, Tree, read, read_pid, read_system};
