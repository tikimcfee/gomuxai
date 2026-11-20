use crate::registry::Entry;

#[cfg(target_os = "macos")]
use crate::proof_darwin as os_imp;
#[cfg(target_os = "linux")]
use crate::proof_linux as os_imp;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
use crate::proof_other as os_imp;

/// What the running processes say about an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Its pid is running, and that process started when the entry says.
    /// The entry describes a running session.
    Live,
    /// Its pid isn't running, or now belongs to a process that started at
    /// another time. The CLI exited without removing its file (a crash, a
    /// `kill -9`); the session isn't running.
    Stale,
    /// Written in another pid space (another container or host sharing the
    /// directory); its pid says nothing about this host.
    Foreign,
    /// Its pid is running, but whether it is the same process can't be told
    /// (the entry has no `procStart`, or this OS doesn't give a start time
    /// gomuxai can read).
    Unproven,
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Verdict::Live => "live",
            Verdict::Stale => "stale",
            Verdict::Foreign => "foreign",
            Verdict::Unproven => "unproven",
        })
    }
}

/// What [`proof`] asks of the running system.
pub trait Host {
    /// Reports whether `pid` names a running process.
    fn running(&self, pid: i32) -> bool;
    /// Returns the process's start time in the form Claude Code records it,
    /// or `None` if it can't be read.
    fn proc_start(&self, pid: i32) -> Option<String>;
    /// Returns this process's pid space in the form Claude Code records it,
    /// or `""` if it can't be told.
    fn pid_domain(&self) -> String;
}

/// Checks an entry against the running processes.
pub fn proof(e: &Entry, h: &(impl Host + ?Sized)) -> Verdict {
    if !e.pid_domain.is_empty() {
        let d = h.pid_domain();
        if !d.is_empty() && d != e.pid_domain {
            return Verdict::Foreign;
        }
    }
    if !h.running(e.pid) {
        return Verdict::Stale;
    }
    if e.proc_start.is_empty() {
        return Verdict::Unproven;
    }
    let Some(start) = h.proc_start(e.pid) else {
        return Verdict::Unproven;
    };
    if start != e.proc_start {
        if e.proc_start_ft {
            return Verdict::Unproven;
        }
        return Verdict::Stale;
    }
    Verdict::Live
}

/// The [`Host`] of this machine.
#[derive(Clone, Copy, Debug, Default)]
pub struct System;

impl Host for System {
    /// Reports whether `pid` names a running process: signal 0 reaches it, or
    /// it exists but belongs to another user.
    fn running(&self, pid: i32) -> bool {
        if pid <= 1 {
            return false;
        }
        // SAFETY: `kill(pid, 0)` sends no signal and only probes process existence.
        let rc = unsafe { libc::kill(pid, 0) };
        let exists = rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        exists && !os_imp::zombie(pid)
    }

    fn proc_start(&self, pid: i32) -> Option<String> {
        os_imp::proc_start(pid)
    }

    fn pid_domain(&self) -> String {
        os_imp::pid_domain()
    }
}

/// Returns the `starttime` field (the 22nd) of a `/proc/<pid>/stat` line. The
/// command name (the 2nd) is in parentheses and may hold spaces or parentheses
/// itself, so fields are counted from the last `")"`.
///
/// It takes bytes, as Go's `string(data)` does: the kernel cuts the command
/// name at 15 bytes, which can split a UTF-8 character, so a real stat line
/// need not be valid UTF-8.
pub fn stat_start_time(stat: impl AsRef<[u8]>) -> Option<String> {
    // nth(0) is field 3 (state), so field 22 is nth(19).
    let field = stat_fields(stat.as_ref())?.nth(19)?;
    Some(String::from_utf8_lossy(field).into_owned())
}

/// The fields of a `/proc/<pid>/stat` line after the command name (field 3,
/// the state, first), split on whitespace as Go's `strings.Fields`; `None`
/// without a `")"`.
pub(crate) fn stat_fields(stat: &[u8]) -> Option<impl Iterator<Item = &[u8]>> {
    let i = stat.iter().rposition(|&b| b == b')')?;
    Some(
        stat[i + 1..]
            .split(u8::is_ascii_whitespace)
            .filter(|f| !f.is_empty()),
    )
}
