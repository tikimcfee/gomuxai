//! Go `internal/util`: utility functions for the agent tool.

mod ansi;
mod files;
mod proctree;
mod shell;
mod suggest;

#[cfg(target_os = "linux")]
mod ancestors_linux;
#[cfg(not(target_os = "linux"))]
mod ancestors_other;
#[cfg(target_os = "linux")]
mod procargs_linux;
#[cfg(not(target_os = "linux"))]
mod procargs_other;

#[cfg(target_os = "linux")]
pub use ancestors_linux::proc_ancestors;
#[cfg(not(target_os = "linux"))]
pub use ancestors_other::proc_ancestors;
pub use ansi::{sanitize_styled, sanitize_terminal, strip_ansi, terminal_lines};
pub use files::write_file_atomic;
#[cfg(target_os = "linux")]
pub use procargs_linux::proc_args;
#[cfg(not(target_os = "linux"))]
pub use procargs_other::proc_args;
pub use proctree::{ProcTree, parse_proc_tree, read_proc_tree};
pub use shell::{shell_join, shell_quote, split_shell_words};
pub use suggest::{or_list, similar};
