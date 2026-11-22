//! Decides whether a shell command line is exactly one run of a given program
//! with a safe subcommand (Go `internal/allowcmd`).

mod allowcmd;

pub use allowcmd::{Checker, LookPathFn, literal_words, look_path};
