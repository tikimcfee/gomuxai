use crate::proof::{stat_fields, stat_start_time};
use std::sync::OnceLock;

/// Reads the `starttime` field of `/proc/<pid>/stat` (clock ticks since boot),
/// which is what Claude Code writes as `procStart` on Linux. Read as bytes:
/// the command name in it needn't be valid UTF-8 ([`stat_start_time`]).
pub(crate) fn proc_start(pid: i32) -> Option<String> {
    let data = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    stat_start_time(&data)
}

static DOMAIN_ONCE: OnceLock<String> = OnceLock::new();

/// `"linux:<machine id>:<pid namespace>"`, as Claude Code writes `pidDomain`.
pub(crate) fn pid_domain() -> String {
    DOMAIN_ONCE
        .get_or_init(|| {
            let Ok(id) = std::fs::read_to_string("/etc/machine-id") else {
                return String::new();
            };
            let Ok(ns) = std::fs::read_link("/proc/self/ns/pid") else {
                return String::new();
            };
            format!("linux:{}:{}", id.trim(), ns.to_string_lossy())
        })
        .clone()
}

/// Reports whether `pid` has exited and waits for its parent to reap it
/// (state `Z` in `/proc/<pid>/stat`): signal 0 still reaches it, but it runs
/// nothing.
pub(crate) fn zombie(pid: i32) -> bool {
    let Ok(stat) = std::fs::read(format!("/proc/{pid}/stat")) else {
        return false;
    };
    stat_fields(&stat).and_then(|mut f| f.next()) == Some(b"Z".as_slice())
}
