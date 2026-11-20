//! A job's process: telling it from a reused pid, its exit code, keeping
//! the runner alive through a Ctrl-C meant for the job, and noting the
//! SIGTERM or SIGHUP that tells the runner to stop it.

use std::process::{Command, ExitStatus};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};

/// process_start is the process's start time as the OS tells it, which with
/// the pid names one process for good: on Linux the clock ticks since boot
/// (/proc/<pid>/stat field 22), elsewhere what `ps -o lstart=` prints. None
/// when there is no such process (or it can't be read).
pub fn process_start(pid: i64) -> Option<String> {
    if pid <= 0 {
        return None;
    }
    if cfg!(target_os = "linux") {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        return stat_start_time(&stat);
    }
    let out = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() || s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// stat_start_time reads the start time from a /proc/<pid>/stat line. The
/// command name (field 2) is in parentheses and may hold spaces or
/// parentheses itself, so fields are counted from the last ')'.
pub(crate) fn stat_start_time(stat: &str) -> Option<String> {
    let rest = &stat[stat.rfind(')')? + 1..];
    // rest starts at field 3 (state); starttime is field 22.
    rest.split_whitespace().nth(22 - 3).map(str::to_string)
}

/// pid_alive says whether the process recorded as pid, started at start, is
/// still running. Without a recorded start time it can only say whether some
/// process has that pid.
pub fn pid_alive(pid: i64, start: &str) -> bool {
    if pid <= 0 {
        return false;
    }
    if !start.is_empty() {
        return process_start(pid).as_deref() == Some(start);
    }
    // SAFETY: kill with signal 0 only checks that the process exists.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// exit_code is a finished process's code as a shell reports it: its exit
/// status, or 128 plus the signal that killed it.
pub fn exit_code(status: ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(c), _) => c,
        (None, Some(sig)) => 128 + sig,
        (None, None) => 1,
    }
}

/// The signals JobSignals changes: the two a terminal sends the job (the
/// runner ignores them, as system(3) does while it waits), and the two that
/// tell the runner to stop (it stops the job, then records the end).
const INTERRUPTS: [libc::c_int; 2] = [libc::SIGINT, libc::SIGQUIT];
const STOPS: [libc::c_int; 2] = [libc::SIGTERM, libc::SIGHUP];

/// The process's dispositions before the first JobSignals, for put_back and
/// for the jobs' children (child_signals). Written under GUARDS before
/// INSTALLED is set, so a child forked meanwhile reads them whole.
static SAVED: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];
static INSTALLED: AtomicBool = AtomicBool::new(false);
/// The stop signal caught since the first JobSignals; 0 for none.
static CAUGHT: AtomicI32 = AtomicI32::new(0);
/// How many JobSignals are alive. The first saves and installs, the last
/// puts back: concurrent runs in one process share one installation, so
/// none restores another's SIG_IGN as if it were the original.
static GUARDS: Mutex<usize> = Mutex::new(0);

extern "C" fn on_stop(sig: libc::c_int) {
    CAUGHT.store(sig, Ordering::SeqCst);
}

fn all_signals() -> [libc::c_int; 4] {
    [INTERRUPTS[0], INTERRUPTS[1], STOPS[0], STOPS[1]]
}

/// handler reads sig's current handler.
fn handler(sig: libc::c_int) -> libc::sighandler_t {
    // SAFETY: a zeroed sigaction is a valid out-parameter; a null new action
    // only reads.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
            return libc::SIG_DFL;
        }
        old.sa_sigaction
    }
}

/// set_handler sets sig's handler, with an empty mask and no flags but
/// SA_RESTART. Async-signal-safe (a child calls it between fork and exec).
fn set_handler(sig: libc::c_int, h: libc::sighandler_t) {
    // SAFETY: as handler; h is SIG_DFL, SIG_IGN, on_stop, or a handler the
    // process had.
    unsafe {
        let mut act: libc::sigaction = std::mem::zeroed();
        act.sa_sigaction = h;
        act.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut act.sa_mask);
        libc::sigaction(sig, &act, std::ptr::null_mut());
    }
}

/// JobSignals is the runner's signal handling while a job runs, until it is
/// dropped. SIGINT and SIGQUIT are ignored, as system(3) does while it
/// waits: the terminal's Ctrl-C goes to the job (same process group), which
/// decides what to do with it, and the runner lives to record how it ended.
/// SIGTERM and SIGHUP are caught (unless the process ignored them, as under
/// nohup) and only noted: the runner polls [`JobSignals::caught`], stops the
/// job and records its end. It is process-wide and counted, so concurrent
/// runs in one process (threads) share it safely, and a stop signal stops
/// them all. Jobs' children must call [`child_signals`] before exec, so they
/// start with the dispositions the process had, not the runner's.
pub struct JobSignals(());

impl JobSignals {
    pub fn new() -> JobSignals {
        let mut n = GUARDS.lock().unwrap_or_else(|e| e.into_inner());
        if *n == 0 {
            CAUGHT.store(0, Ordering::SeqCst);
            for (i, sig) in all_signals().into_iter().enumerate() {
                SAVED[i].store(handler(sig), Ordering::SeqCst);
            }
            INSTALLED.store(true, Ordering::SeqCst);
            for sig in INTERRUPTS {
                set_handler(sig, libc::SIG_IGN);
            }
            for (i, sig) in STOPS.into_iter().enumerate() {
                if SAVED[2 + i].load(Ordering::SeqCst) != libc::SIG_IGN {
                    set_handler(
                        sig,
                        on_stop as extern "C" fn(libc::c_int) as libc::sighandler_t,
                    );
                }
            }
        }
        *n += 1;
        JobSignals(())
    }

    /// caught is the stop signal (SIGTERM or SIGHUP) the process got while
    /// any JobSignals was alive, if one came.
    pub fn caught(&self) -> Option<i32> {
        match CAUGHT.load(Ordering::SeqCst) {
            0 => None,
            s => Some(s),
        }
    }
}

impl Default for JobSignals {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for JobSignals {
    fn drop(&mut self) {
        let mut n = GUARDS.lock().unwrap_or_else(|e| e.into_inner());
        *n -= 1;
        if *n == 0 {
            for (i, sig) in all_signals().into_iter().enumerate() {
                set_handler(sig, SAVED[i].load(Ordering::SeqCst));
            }
            INSTALLED.store(false, Ordering::SeqCst);
        }
    }
}

/// child_signals gives a job's process, between fork and exec, the
/// dispositions the runner's process had before JobSignals: SIGINT and
/// SIGQUIT as they were (ignored under a `&` in a script, say), SIGTERM and
/// SIGHUP ignored if they were, else the default. Async-signal-safe: it
/// reads atomics and calls sigaction.
pub fn child_signals() {
    if !INSTALLED.load(Ordering::SeqCst) {
        return;
    }
    for (i, sig) in all_signals().into_iter().enumerate() {
        let h = SAVED[i].load(Ordering::SeqCst);
        // A handler doesn't survive exec; only "ignored" is inherited.
        set_handler(
            sig,
            if h == libc::SIG_IGN {
                libc::SIG_IGN
            } else {
                libc::SIG_DFL
            },
        );
    }
}

/// signal_name is how a person knows sig ("SIGTERM").
pub fn signal_name(sig: i32) -> String {
    match sig {
        libc::SIGTERM => "SIGTERM".into(),
        libc::SIGHUP => "SIGHUP".into(),
        libc::SIGINT => "SIGINT".into(),
        libc::SIGKILL => "SIGKILL".into(),
        libc::SIGQUIT => "SIGQUIT".into(),
        s => format!("signal {s}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_start_time_counts_from_the_last_paren() {
        let stat = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 10";
        assert_eq!(stat_start_time(stat).as_deref(), Some("987654"));
        assert_eq!(stat_start_time("garbage"), None);
        assert_eq!(stat_start_time("1 (x) S 1"), None);
    }

    #[test]
    fn this_process_is_alive_and_a_wrong_start_is_not() {
        let me = std::process::id() as i64;
        let start = process_start(me).expect("own start time");
        assert!(pid_alive(me, &start));
        assert!(pid_alive(me, ""));
        assert!(!pid_alive(me, "1"));
        assert!(!pid_alive(0, ""));
    }

    #[test]
    fn a_reaped_child_is_gone() {
        let mut c = Command::new("true").spawn().unwrap();
        let pid = c.id() as i64;
        let start = process_start(pid).unwrap_or_default();
        c.wait().unwrap();
        assert!(!pid_alive(pid, &start) || start.is_empty());
        assert_eq!(
            process_start(pid).filter(|s| *s == start && !s.is_empty()),
            None
        );
    }

    /// review-3 1.5: JobSignals in many threads at once leaves the process's
    /// dispositions as they were; while one lives, a job's child starts with
    /// them (not the runner's SIG_IGN); a SIGTERM is noted, not fatal. One
    /// test, so nothing else here changes dispositions meanwhile.
    #[test]
    fn job_signals_are_shared_restored_and_not_inherited() {
        let before: Vec<libc::sighandler_t> = all_signals().into_iter().map(handler).collect();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    for _ in 0..300 {
                        let g = JobSignals::new();
                        assert_eq!(handler(libc::SIGINT), libc::SIG_IGN);
                        drop(g);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let after: Vec<libc::sighandler_t> = all_signals().into_iter().map(handler).collect();
        assert_eq!(after, before, "dispositions not restored");

        let g = JobSignals::new();
        assert_eq!(handler(libc::SIGINT), libc::SIG_IGN);
        assert_eq!(g.caught(), None);
        if cfg!(target_os = "linux") {
            use std::os::unix::process::CommandExt;
            // SigIgn: the ignored signals' mask, bit sig-1.
            let ignored = |pre: bool| -> u64 {
                let mut c = Command::new("cat");
                c.arg("/proc/self/status");
                if pre {
                    // SAFETY: child_signals is async-signal-safe.
                    unsafe {
                        c.pre_exec(|| {
                            child_signals();
                            Ok(())
                        });
                    }
                }
                let out = String::from_utf8(c.output().unwrap().stdout).unwrap();
                let hex = out
                    .lines()
                    .find_map(|l| l.strip_prefix("SigIgn:"))
                    .unwrap()
                    .trim()
                    .to_string();
                u64::from_str_radix(&hex, 16).unwrap()
            };
            let int_bit = 1u64 << (libc::SIGINT - 1);
            assert_ne!(
                ignored(false) & int_bit,
                0,
                "without child_signals the child inherits SIG_IGN"
            );
            let want = if before[0] == libc::SIG_IGN {
                int_bit
            } else {
                0
            };
            assert_eq!(
                ignored(true) & int_bit,
                want,
                "child_signals gives the child the original SIGINT"
            );
        }
        // SAFETY: raising a signal this process catches now.
        unsafe { libc::raise(libc::SIGTERM) };
        assert_eq!(g.caught(), Some(libc::SIGTERM));
        drop(g);
        let after: Vec<libc::sighandler_t> = all_signals().into_iter().map(handler).collect();
        assert_eq!(after, before);
        // A new first JobSignals starts with nothing caught.
        assert_eq!(JobSignals::new().caught(), None);
    }

    #[test]
    fn exit_codes_as_shells_report_them() {
        let st = |script: &str| Command::new("sh").args(["-c", script]).status().unwrap();
        assert_eq!(exit_code(st("exit 0")), 0);
        assert_eq!(exit_code(st("exit 7")), 7);
        assert_eq!(exit_code(st("kill -KILL $$")), 137);
        assert_eq!(exit_code(st("kill -TERM $$")), 143);
    }
}
