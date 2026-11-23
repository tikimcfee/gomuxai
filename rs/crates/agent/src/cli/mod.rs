//! The command-line interface for the agent tool (Go: internal/cli).
//!
//! How the frame is put together, for whoever fills in a command:
//!
//! - Each command is a [`command::Def`]: its help text and positional-argument rule
//!   (a [`command::Doc`] const, what cobra.Command held), its flags (a `clap::Args`
//!   struct), and its run fn. A command in a file of its own (send.rs) names these
//!   `DOC`, `Args`, `run` and `def`; any other command in a file uses its Go name
//!   (approve.rs: `DENY`, `DenyArgs`, `run_deny`, `deny_def`).
//! - A run fn is `fn(&Ctx, &Args) -> anyhow::Result<()>`. Positional arguments are in
//!   `args.args`, already checked against the Doc's rule (Go: `args []string`).
//!   [`root::Ctx`] has the global flags, `changed()` (Go: `cmd.Flags().Changed`),
//!   stdout/stderr, `hint` and `notice`, the config setup builds (`cfg()`), and
//!   the store and termd client, opened the first time a command asks (`store()`,
//!   `termd()`), the backends (`backends()`), the agent service (`service()`,
//!   lazy too), the bus (`bus_connect()`, `bus_publisher()`; remote.rs has
//!   `parse_target` and `call_remote` for name@host), and the caller
//!   (`caller_agent()`). Tests give a Ctx their own config, store, termd and
//!   bus (testenv.rs), as Go's tests set its package variables.
//! - Parsing follows cobra: [`root::find`] walks the words to the command, then that
//!   command's clap parser reads its flags and the global ones, anywhere on the line,
//!   with pflag's syntax ([`flags`]). Errors keep cobra's and pflag's words.
//! - Help is rendered in cobra's layout from the Defs and the clap flags ([`help`]).
//!
//! Adding a command: write its file (copy a leaf such as list.rs: a `DOC`, an `Args`
//! struct, `def()` and `run`), then add its module name to its group in the
//! [`commands!`] list below. That one word declares the module and puts the command
//! in `agent help`. A subcommand instead goes in its group's `def()`.

#[macro_use]
pub mod command;
pub mod exec;
pub mod exit;
pub mod flags;
pub mod help;
pub mod output;
pub mod remote;
pub mod remote_ops;
pub mod root;

mod bus_identity;
// The evidence in a dream's digest: the host's side of agent_dream::evidence.
mod dream_evidence;
// `agent dream memory`, wired under `agent dream` by its group's def().
mod dream_memory;
// `agent dream schedule`, wired under `agent dream` by its group's def().
mod dream_schedule;
// The global dream's steps and the idea, ideas, judge and done verbs, wired
// under `agent dream` by its group's def().
mod dream_steps;
mod friction_links;
mod orient;
mod resume_all;
mod spawn_pick;
mod ssh;

#[cfg(test)]
mod adopt_test;
#[cfg(test)]
mod cleanup_test;
#[cfg(test)]
mod commands_test;
#[cfg(test)]
mod history_test;
#[cfg(test)]
mod list_test;
#[cfg(test)]
mod orient_test;
#[cfg(test)]
mod remote_ops_test;
#[cfg(test)]
mod remote_test;
#[cfg(test)]
mod resume_all_test;
#[cfg(test)]
mod revive_test;
#[cfg(test)]
mod spawn_continue_test;
#[cfg(test)]
mod spawn_pick_test;
#[cfg(test)]
mod spawn_test;
#[cfg(test)]
mod ssh_test;
#[cfg(test)]
mod testenv;

pub use root::{execute, execute_with};

/// The error of a command whose body isn't ported yet: "agent send: not ported
/// to Rust yet". `path` is the command below `agent` ("state set"), or "" for
/// `agent` itself.
pub fn not_ported(path: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("agent: not ported to Rust yet")
    }
    anyhow::bail!("agent {path}: not ported to Rust yet")
}

/// Declares the command modules and lists them by group, in the order `agent help`
/// shows them (Go: root.go's commandGroups). A bare name is a module whose `def()`
/// is the command; a `[path]` is the def fn of a command that shares another
/// command's file (its module is declared by that command's name).
macro_rules! commands {
    ($( $id:ident $title:literal { $($entry:tt)* } )*) => {
        commands!(@mods $($($entry)*)*);

        /// The command groups of `agent help`, with their commands.
        pub(crate) fn command_groups() -> Vec<command::Group> {
            vec![$(
                command::Group {
                    id: stringify!($id),
                    title: $title,
                    defs: commands!(@defs [] $($entry)*),
                }
            ),*]
        }
    };
    (@mods) => {};
    (@mods $m:ident $($rest:tt)*) => { pub mod $m; commands!(@mods $($rest)*); };
    (@mods [$($p:tt)*] $($rest:tt)*) => { commands!(@mods $($rest)*); };
    (@defs [$($out:expr),*]) => { vec![$($out),*] };
    (@defs [$($out:expr),*] $m:ident $($rest:tt)*) => {
        commands!(@defs [$($out,)* $m::def()] $($rest)*)
    };
    (@defs [$($out:expr),*] [$p:path] $($rest:tt)*) => {
        commands!(@defs [$($out,)* $p()] $($rest)*)
    };
}

commands! {
    // revive is new in the Rust build, like run below.
    agents "Agents:" { spawn adopt resume revive attach list cleanup export sessions }
    fleet "Fleet:" { fleet focus template manage dream roster }
    messages "Messages:" { send queue deliver watch }
    status "Status and waiting:" {
        status statuses wait approve [approve::deny_def] capture logs transcript events report
        hooks [hooks::hook_def]
    }
    state "State and tasks:" { state task }
    jobs "Jobs:" { run }
    bus "Machines (bus):" { bus }
    tool "About this tool:" { friction history skill version }
    sessions "Low-level sessions (session names, not agent names):" { session keys key }
}

// cobra's own hidden `completion` command (no Go file).
pub mod completion;
