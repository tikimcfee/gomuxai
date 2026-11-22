//! The roster (docs/ROSTER.md): which harness and model each host can run,
//! what each is good at, whether it is available now, and what past runs
//! showed. agent-store keeps the rows; this crate holds the rules: the task
//! kinds and other fixed lists, entry keys, availability, this host's view
//! (what the bus will serve), pick's ranking, and adding a note, which checks
//! the session it cites against this host's sessions.

pub mod entry;
pub mod key;
pub mod note;
pub mod pick;
pub mod view;

#[cfg(test)]
mod roster_test;

pub use agent_store::{RosterEntry, RosterNote};
pub use entry::{EntryChanges, remove_entry, set_enabled, set_entry};
pub use key::{
    COSTS, EFFORTS, Key, PREFERS, SPEEDS, TASK_KINDS, VERDICTS, check_cost, check_effort,
    check_prefer, check_speed, check_task, check_verdict, parse_tasks,
};
pub use note::{NoteInput, add_note, add_note_in, resolve_source, session_dirs};
pub use pick::{Pick, PickOptions, pick};
pub use view::{
    EntryShow, EntryView, HostRoster, NoteCounts, NoteView, Tally, host_view, host_view_with,
    on_path, show, show_with,
};
