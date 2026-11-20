use std::collections::HashMap;
use std::fs::DirEntry;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use agent_base::gostd::{filepath, json};
use agent_transcript::agy_conversation_dir;
use serde::{Deserialize, Serialize};

/// Harnesses whose sessions discovery reads.
pub const HARNESS_CLAUDE: &str = "claude";
pub const HARNESS_AGY: &str = "agy";
pub const HARNESS_KIMI: &str = "kimi";

/// Every harness, in scan and report order.
pub const HARNESSES: &[&str] = &[HARNESS_CLAUDE, HARNESS_AGY, HARNESS_KIMI];

/// Dir origins: why a directory is scanned.
pub const ORIGIN_CONFIG: &str = "config"; // named in the app config
pub const ORIGIN_DEFAULT: &str = "default"; // the app's own default location, found on this machine

/// One directory to scan for one harness's sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dir {
    pub harness: String,
    pub path: String,
    pub origin: String,
}

/// The per-harness paths from the app config, as written (a leading `~` not yet
/// expanded). `None` is unset, so the app defaults are used; `Some("")` turns
/// the harness off.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub claude: Option<String>, // claude_projects_dir
    pub agy: Option<String>,    // agy_brain_dir
    pub kimi: Option<String>,   // kimi_sessions_dir
}

impl Overrides {
    fn get(&self, harness: &str) -> Option<&str> {
        match harness {
            HARNESS_CLAUDE => self.claude.as_deref(),
            HARNESS_AGY => self.agy.as_deref(),
            HARNESS_KIMI => self.kimi.as_deref(),
            _ => None,
        }
    }
}

/// Names the app config field that overrides `harness`'s location.
pub fn config_key(harness: &str) -> &'static str {
    match harness {
        HARNESS_CLAUDE => "claude_projects_dir",
        HARNESS_AGY => "agy_brain_dir",
        HARNESS_KIMI => "kimi_sessions_dir",
        _ => "",
    }
}

/// Returns the app-default locations of `harness`'s sessions under `home`, in
/// scan order. `claude_config_dir` is `$CLAUDE_CONFIG_DIR`, which Claude Code
/// honours in place of `~/.claude`.
pub fn default_candidates(harness: &str, home: &str, claude_config_dir: &str) -> Vec<String> {
    match harness {
        HARNESS_CLAUDE => {
            if !claude_config_dir.is_empty() {
                vec![filepath::join(&[claude_config_dir, "projects"])]
            } else {
                vec![filepath::join(&[home, ".claude", "projects"])]
            }
        }
        HARNESS_AGY => vec![
            filepath::join(&[home, ".gemini", "antigravity", "brain"]), // the desktop app
            filepath::join(&[home, ".gemini", "antigravity-cli", "brain"]), // the CLI
        ],
        HARNESS_KIMI => vec![filepath::join(&[home, ".kimi-code", "sessions"])],
        _ => Vec::new(),
    }
}

/// Resolves the scan list against this machine's home directory and
/// `$CLAUDE_CONFIG_DIR`.
pub fn dirs(o: &Overrides) -> Vec<Dir> {
    let home = user_home_dir();
    let claude_cfg = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
    resolve_dirs(o, &home, &claude_cfg)
}

fn user_home_dir() -> String {
    if let Ok(h) = std::env::var("HOME")
        && !h.is_empty()
    {
        return h;
    }
    String::new()
}

/// [`dirs`] with the machine-dependent inputs passed in. A configured path is
/// the only one scanned for its harness, and is kept even if it doesn't exist,
/// so a report can say so; a default is kept only if it exists.
pub fn resolve_dirs(o: &Overrides, home: &str, claude_config_dir: &str) -> Vec<Dir> {
    let mut out = Vec::new();
    for &h in HARNESSES {
        match o.get(h) {
            Some("") => {}
            Some(p) => out.push(Dir {
                harness: h.to_string(),
                path: expand_home(p, home),
                origin: ORIGIN_CONFIG.to_string(),
            }),
            None if home.is_empty() && (h != HARNESS_CLAUDE || claude_config_dir.is_empty()) => {}
            None => {
                for path in default_candidates(h, home, claude_config_dir) {
                    if is_dir(&path) {
                        out.push(Dir {
                            harness: h.to_string(),
                            path,
                            origin: ORIGIN_DEFAULT.to_string(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// Says in one line what is scanned, e.g.
/// `"claude ~/.claude/projects (default); kimi ~/k (config, missing)"`.
pub fn describe_dirs(dirs: &[Dir], home: &str) -> String {
    if dirs.is_empty() {
        return format!(
            "no session directories: none configured and no app default found (set {}, {}, {} in the app config)",
            config_key(HARNESS_CLAUDE),
            config_key(HARNESS_AGY),
            config_key(HARNESS_KIMI),
        );
    }
    dirs.iter()
        .map(|d| {
            let missing = if is_dir(&d.path) { "" } else { ", missing" };
            format!(
                "{} {} ({}{})",
                d.harness,
                contract_home(&d.path, home),
                d.origin,
                missing
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Writes a path under `home` as `~/...`.
pub fn contract_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    let prefix = format!("{}/", home.trim_end_matches('/'));
    if let Some(rest) = path.strip_prefix(&prefix) {
        return format!("~/{rest}");
    }
    path.to_string()
}

/// Expands a leading `~` or `~/` to `home`.
fn expand_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == "~" {
        return home.to_string();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return filepath::join(&[home, rest]);
    }
    path.to_string()
}

pub(crate) fn is_dir(path: &str) -> bool {
    std::fs::metadata(os_path(path)).is_ok_and(|m| m.is_dir())
}

/// Reads `dir` sorted by entry name, matching Go's `os.ReadDir`.
pub(crate) fn read_dir_sorted(dir: &str) -> std::io::Result<Vec<DirEntry>> {
    let mut entries: Vec<DirEntry> = std::fs::read_dir(os_path(dir))?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

/// The paths listed under a name that isn't UTF-8 (or under a directory
/// whose name isn't), by the text they are listed as.
///
/// Go's strings hold any bytes, so the Go lists such a file by its name and
/// reads it. A path here is a `String`, which holds the name as Go's JSON
/// encoder writes it, each bad byte a U+FFFD ([`entry_name`]); this maps that
/// text back to the path on disk for the reads discovery makes itself
/// ([`os_path`]). It holds only such paths, so it stays empty on a host that
/// has none. (`agent-transcript`'s readers of the files beside a transcript
/// take the text as it is, and find nothing there.)
static OS_PATHS: LazyLock<Mutex<HashMap<String, PathBuf>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The name of `e`, an entry of the directory listed as `dir`, as Go's string
/// of it reads in its output: each byte that isn't UTF-8 a U+FFFD.
pub(crate) fn entry_name(dir: &str, e: &DirEntry) -> String {
    let os = e.file_name();
    let name = json::lossy(os.as_bytes()).into_owned();
    let real_dir = os_path(dir);
    if os.to_str().is_none() || real_dir.as_os_str() != dir {
        let listed = filepath::join(&[dir, &name]);
        if let Ok(mut m) = OS_PATHS.lock() {
            m.insert(listed, real_dir.join(&os));
        }
    }
    name
}

/// The path on disk that `path`, as discovery lists it, names: itself, unless
/// it or a directory above it was listed under a name that isn't UTF-8.
pub(crate) fn os_path(path: &str) -> PathBuf {
    let Ok(m) = OS_PATHS.lock() else {
        return PathBuf::from(path);
    };
    if m.is_empty() {
        return PathBuf::from(path);
    }
    let mut prefix = path;
    loop {
        if let Some(real) = m.get(prefix) {
            let rest = path[prefix.len()..].trim_start_matches('/');
            return if rest.is_empty() {
                real.clone()
            } else {
                real.join(rest)
            };
        }
        match prefix.rfind('/') {
            Some(i) if i > 0 => prefix = &prefix[..i],
            _ => return PathBuf::from(path),
        }
    }
}

/// Names the harness that writes a transcript at `path`, from the shape of the
/// path alone, or `""`.
pub fn harness_of(path: &str) -> &'static str {
    if path.ends_with("/agents/main/wire.jsonl") || path.contains("/.kimi-code/") {
        HARNESS_KIMI
    } else if !agy_conversation_dir(path).is_empty()
        || path.contains("/antigravity-cli/brain/")
        || path.contains("/antigravity/brain/")
    {
        HARNESS_AGY
    } else if path.ends_with(".jsonl") {
        HARNESS_CLAUDE
    } else {
        ""
    }
}
