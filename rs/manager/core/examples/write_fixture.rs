//! Writes the fixture view as JSON, for `AGENT_MANAGER_FIXTURE`:
//!
//!     cargo run -p agent-manager-core --example write_fixture -- fixtures/view.json
//!
//! With `--now`, the view is read at this moment rather than the fixture's fixed clock, so
//! ages in screenshots look like a live fleet's.

use agent_manager_core::fixture;

fn main() {
    let mut path = None;
    let mut now = fixture::default_now();
    for a in std::env::args().skip(1) {
        if a == "--now" {
            now = chrono::Utc::now();
        } else {
            path = Some(a);
        }
    }
    let Some(path) = path else {
        eprintln!("usage: write_fixture [--now] PATH");
        std::process::exit(2);
    };
    let v = fixture::view(now);
    let json = serde_json::to_string_pretty(&v).expect("serializes");
    std::fs::write(&path, json).unwrap_or_else(|e| {
        eprintln!("{path}: {e}");
        std::process::exit(1);
    });
    let sessions: usize = v.hosts.iter().map(|h| h.sessions.len()).sum();
    println!("wrote {path}: {} hosts, {sessions} sessions", v.hosts.len());
}
