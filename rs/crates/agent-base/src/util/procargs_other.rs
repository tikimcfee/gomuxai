use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A process's command line, as ps prints it split on spaces (so a word
/// holding a space comes back as two), or None if it can't be read.
pub fn proc_args(pid: i32) -> Option<Vec<String>> {
    let mut child = Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    // Go gives ps a second.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    let f: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if f.is_empty() { None } else { Some(f) }
}
