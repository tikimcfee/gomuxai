//! The bus on macOS (Go: internal/cli/bus_macos.go, bus_macos_other.go).
//! Declared from bus.rs.
//!
//! macOS lets a process inside a background daemon (g3d-termd) dial the LAN
//! only if the responsible process holds the Local Network permission, which
//! needs an embedded Info.plist (make macos). In the Go build that process
//! was the agent binary itself, re-spawned disclaimed by runBusDisclaimed.
//! Now the process that dials peers is agent-busd: agent_bus::start_daemon
//! spawns it disclaimed when it carries an Info.plist, and agent-busd watches
//! this process (--parent-pid) as the Go's watchBusParent did. What's left
//! here is the warning, which names agent-busd.

use std::path::Path;

use agent_bus::PeerBundle;

use crate::cli::root::Ctx;

/// The note warn_local_network prints.
pub const LOCAL_NETWORK_NOTE: &str = "note: macOS blocks LAN links from background daemon processes unless agent-busd carries its own Local Network permission. If linking fails with \"no route to host\", build with `make macos` on this Mac (see the macOS section of README.md).";

/// Whether the bus will dial a peer over the LAN from inside g3d-termd with
/// an agent-busd that can't hold the Local Network permission (no embedded
/// Info.plist): the link would otherwise fail with only "no route to host"
/// in the log.
pub fn needs_local_network_note(
    busd_has_info_plist: bool,
    in_termd: bool,
    disclaimed: bool,
    listen: &str,
    peers: &[PeerBundle],
) -> bool {
    if busd_has_info_plist || !in_termd || disclaimed || !listen.is_empty() {
        return false;
    }
    peers.iter().any(|p| !p.url.is_empty())
}

/// Points at the fix when needs_local_network_note says so (Go:
/// warnLocalNetwork). busd is the agent-busd the bus runs (None: found as
/// start_daemon finds it). The bus only needs a disclaimed process on macOS
/// (Go: bus_macos_other.go): elsewhere it says nothing. The body compiles
/// everywhere (cfg!, not #[cfg]), so a Linux build checks the macOS path.
pub fn warn_local_network(ctx: &Ctx, busd: Option<&Path>, peers: &[PeerBundle], listen: &str) {
    use std::io::Write as _;
    if !cfg!(target_os = "macos") {
        return;
    }
    let busd = match busd {
        Some(p) => p.to_path_buf(),
        None => match agent_bus::find_busd() {
            Ok(p) => p,
            Err(_) => return,
        },
    };
    let set = |k: &str| std::env::var_os(k).is_some_and(|v| !v.is_empty());
    if needs_local_network_note(
        agent_base::macos::has_info_plist(&busd.to_string_lossy()),
        set("G3D_TERMD_ID"),
        set(agent_bus::DISCLAIMED_ENV),
        listen,
        peers,
    ) {
        let _ = writeln!(ctx.err(), "{LOCAL_NETWORK_NOTE}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_network_note_only_for_a_leaf_dialing_from_termd() {
        let leaf = |url: &str| PeerBundle {
            host: "hub".into(),
            url: url.into(),
            ..Default::default()
        };
        let peers = [leaf("tls://hub.lan:7422")];
        assert!(needs_local_network_note(false, true, false, "", &peers));
        assert!(!needs_local_network_note(true, true, false, "", &peers), "busd has its plist");
        assert!(!needs_local_network_note(false, false, false, "", &peers), "not in termd");
        assert!(!needs_local_network_note(false, true, true, "", &peers), "disclaimed");
        assert!(!needs_local_network_note(false, true, false, "0.0.0.0:7422", &peers), "the hub");
        assert!(!needs_local_network_note(false, true, false, "", &[leaf("")]), "dials no one");
        assert!(LOCAL_NETWORK_NOTE.contains("agent-busd"));
    }
}
