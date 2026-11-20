use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs `ps -o lstart=` in the C locale and UTC, which is how Claude Code reads
/// a process's start time on macOS.
pub(crate) fn proc_start(pid: i32) -> Option<String> {
    let mut child = Command::new("ps")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .env("LC_ALL", "C")
        .env("TZ", "UTC")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });

    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let out = reader.join().ok()?;
                let s = out.trim();
                return (!s.is_empty()).then(|| s.to_owned());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Claude Code records no `pidDomain` gomuxai can rebuild on macOS, so an
/// entry's domain is not checked there.
pub(crate) fn pid_domain() -> String {
    String::new()
}

/// Not checked on macOS, where it would cost a `ps` per entry; a CLI's parent
/// shell reaps it at once.
pub(crate) fn zombie(_pid: i32) -> bool {
    false
}
