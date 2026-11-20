use std::process::Command;

/// The LocalHostName macOS keeps in its system configuration (Sharing
/// settings), or "". The kernel's hostname, which `os::hostname` reads,
/// follows the network instead: DHCP or reverse DNS can make it "Mac" or
/// another machine's name.
pub(super) fn local_host_name() -> String {
    match Command::new("/usr/sbin/scutil")
        .args(["--get", "LocalHostName"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => String::new(),
    }
}
