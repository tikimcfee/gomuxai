use std::ffi::CString;
use std::ptr;

use libc::{c_char, c_int, pid_t, posix_spawnattr_t};

use crate::gostd::os::Errno;

unsafe extern "C" {
    // Private libSystem call, available since macOS 10.14 and used by
    // Chromium and iTerm2 for the same purpose: the spawned process becomes
    // its own responsible process.
    fn responsibility_spawnattrs_setdisclaim(
        attrs: *mut posix_spawnattr_t,
        disclaim: c_int,
    ) -> c_int;
}

/// Whether spawn_disclaimed works in this build.
pub const SUPPORTED: bool = true;

/// Starts `path` with `argv` and `env` as its own responsible process and
/// returns its pid. The child inherits stdin, stdout, stderr, and the
/// caller's process group (so a terminal's Ctrl-C reaches it), but none of
/// this process's other file descriptors, which Rust opens close-on-exec.
pub fn spawn_disclaimed<A: AsRef<str>, E: AsRef<str>>(
    path: &str,
    argv: &[A],
    env: &[E],
) -> anyhow::Result<i32> {
    // Go's C.CString cuts a string at a NUL; do the same.
    let cstr = |s: &str| CString::new(s.split('\0').next().unwrap_or("")).unwrap_or_default();
    let c_path = cstr(path);
    let c_argv: Vec<CString> = argv.iter().map(|s| cstr(s.as_ref())).collect();
    let c_env: Vec<CString> = env.iter().map(|s| cstr(s.as_ref())).collect();
    // NULL-terminated arrays of pointers into the CStrings above, which
    // outlive the call.
    let mut argv_ptrs: Vec<*mut c_char> =
        c_argv.iter().map(|s| s.as_ptr() as *mut c_char).collect();
    argv_ptrs.push(ptr::null_mut());
    let mut env_ptrs: Vec<*mut c_char> = c_env.iter().map(|s| s.as_ptr() as *mut c_char).collect();
    env_ptrs.push(ptr::null_mut());

    let mut pid: pid_t = 0;
    // SAFETY: attr is initialized by posix_spawnattr_init before any other
    // use and destroyed once; the sigsets are initialized by sigemptyset;
    // every pointer passed lives until the call returns.
    let r = unsafe {
        spawn(
            &mut pid,
            c_path.as_ptr(),
            argv_ptrs.as_ptr(),
            env_ptrs.as_ptr(),
        )
    };
    if r != 0 {
        return Err(anyhow::Error::new(Errno::from_raw(r)).context(format!("spawning {path}")));
    }
    Ok(pid)
}

unsafe fn spawn(
    pid: *mut pid_t,
    path: *const c_char,
    argv: *const *mut c_char,
    envp: *const *mut c_char,
) -> c_int {
    unsafe {
        let mut attr: posix_spawnattr_t = std::mem::zeroed();
        let r = libc::posix_spawnattr_init(&mut attr);
        if r != 0 {
            return r;
        }
        // The child inherits the calling thread's signal mask and any ignored
        // signals; start it with neither, as exec from a shell would.
        let mut none: libc::sigset_t = std::mem::zeroed();
        let mut defaults: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::sigemptyset(&mut defaults);
        for sig in [
            libc::SIGHUP,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGTERM,
            libc::SIGPIPE,
        ] {
            libc::sigaddset(&mut defaults, sig);
        }
        libc::posix_spawnattr_setsigmask(&mut attr, &none);
        libc::posix_spawnattr_setsigdefault(&mut attr, &defaults);
        let mut r = libc::posix_spawnattr_setflags(
            &mut attr,
            (libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short,
        );
        if r == 0 {
            r = responsibility_spawnattrs_setdisclaim(&mut attr, 1);
        }
        if r == 0 {
            r = libc::posix_spawn(pid, path, ptr::null(), &attr, argv, envp);
        }
        libc::posix_spawnattr_destroy(&mut attr);
        r
    }
}
