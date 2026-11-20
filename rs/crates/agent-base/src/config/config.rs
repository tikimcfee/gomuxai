use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use regex::Regex;
use serde_json::Value;

use super::local_host_name;
use crate::gostd::{filepath, os};

/// Runtime configuration for the agent tool.
///
/// Paths are `String`s, as in the Go, and are built with Go's path rules
/// (`gostd::filepath`). The JSON names are the Go `json:` tags; a duration
/// in JSON is integer nanoseconds, as Go's `time.Duration`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// The name of the backend to use (e.g., "claude", "aider").
    pub backend: String,

    /// The path to the SQLite database file.
    pub database_path: String,

    /// The directory for agent log files.
    pub log_dir: String,

    /// Created by ensure_dirs, but agent state lives in the database;
    /// agent_dir takes the data directory from its parent.
    pub state_dir: String,

    /// The prefix for terminal session names.
    pub session_prefix: String,

    /// The default number of lines to capture.
    pub default_scrollback: i64,

    /// The time to wait after launching a CLI before sending init message.
    /// Default: 2s (can be set to 0 for testing).
    pub startup_delay: Duration,

    /// Bounds each wait while typing a message into a CLI that can take its
    /// Enter for a newline (Claude): for the input box to show the typed text
    /// before Enter, and for it to empty after each Enter. Default: 3s; 0
    /// types and presses Enter without looking.
    pub submit_wait: Duration,

    /// The absolute path of the agent binary that backend hooks call to
    /// report status. Empty disables hook-based status reporting. Default:
    /// the running executable.
    pub agent_binary: String,

    /// Names this machine on the agent bus ("name@host" addressing and bus
    /// subjects). Default: $AGENT_HOST, else the host the bus identity was
    /// created for, else the system's name for this machine (default_host).
    pub host: String,

    /// Where agent sessions looks for each harness's transcripts. Unset
    /// (None) means the app's default locations that exist; a path is the
    /// only place looked; an empty string turns that harness off. A leading
    /// ~ is the home dir.
    pub claude_projects_dir: Option<String>,
    pub agy_brain_dir: Option<String>,
    pub kimi_sessions_dir: Option<String>,

    /// The g3d-termd namespace (default "agents"; never "default" or "wall").
    pub termd_namespace: String,

    /// Optional explicit paths to g3d-termd and g3d.
    pub termd_binary: String,
    pub g3d_binary: String,
}

/// The termd namespace gomuxai uses when none is configured: isolated from
/// interactive ("default") and wall ("wall") terminals. Go's
/// `termd.DefaultNamespace`, defined here because agent-base depends on no
/// other crate; agent-termd reuses it.
pub const DEFAULT_TERMD_NAMESPACE: &str = "agents";

/// The default startup_delay.
pub const DEFAULT_STARTUP_DELAY: Duration = Duration::from_secs(2);

/// The default submit_wait.
pub const DEFAULT_SUBMIT_WAIT: Duration = Duration::from_secs(3);

/// The name of the data directory.
pub const STORAGE_DIR_NAME: &str = ".gomuxai";

/// Go's `termdwire.ValidateName`: `s` must be 1 to 64 bytes of
/// [A-Za-z0-9._-]. `what` names it in the error ("termd namespace").
pub fn validate_termd_name(what: &str, s: &str) -> anyhow::Result<()> {
    let ok = !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-');
    if !ok {
        anyhow::bail!(
            "{what} {} must be 1 to 64 bytes of [A-Za-z0-9._-]",
            crate::gostd::strconv::quote(s)
        );
    }
    Ok(())
}

/// Go's `termd.ValidateNamespace`: a valid name, and not "default" or
/// "wall", which interactive and wall sessions use.
pub fn validate_termd_namespace(ns: &str) -> anyhow::Result<()> {
    validate_termd_name("termd namespace", ns)?;
    if ns == "default" || ns == "wall" {
        anyhow::bail!(
            "termd namespace {} is reserved for interactive and wall sessions; use a dedicated namespace such as {}",
            crate::gostd::strconv::quote(ns),
            crate::gostd::strconv::quote(DEFAULT_TERMD_NAMESPACE)
        );
    }
    Ok(())
}

// get_storage_dir returns the base directory for all gomuxai data: next to
// the binary, so the tool keeps nothing in ~/, or in the working directory if
// the binary's path is unknown.
fn get_storage_dir() -> String {
    let exe = agent_binary_path();
    if !exe.is_empty() {
        return filepath::join(&[filepath::dir(&exe).as_str(), STORAGE_DIR_NAME]);
    }
    STORAGE_DIR_NAME.to_string()
}

fn getenv(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

// read_trimmed reads a file and trims it, or "" if it can't be read.
fn read_trimmed(path: &str) -> Option<String> {
    let b = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&b).trim().to_string())
}

// default_host returns $AGENT_HOST if set, else the host name the bus
// identity in bus_dir was created for, else system_host's: this machine's
// name on the bus stays the one its peers trust, whatever the network later
// calls it (macOS renames itself "Mac" on some networks).
fn default_host(bus_dir: &str) -> String {
    let host = getenv("AGENT_HOST");
    if !host.is_empty() {
        return current_host(bus_dir, &host);
    }
    // The identity's own record (natsbus's hostNameFile): written by agent
    // bus init, and checked against the host on every load.
    if let Some(host) = read_trimmed(&filepath::join(&[bus_dir, "host"]))
        && !host.is_empty()
    {
        return host;
    }
    system_host()
}

// Go's strings.EqualFold for host names.
fn equal_fold(a: &str, b: &str) -> bool {
    a.chars()
        .flat_map(char::to_lowercase)
        .eq(b.chars().flat_map(char::to_lowercase))
}

// current_host returns host, or the bus identity's name if host is one the
// identity had before (natsbus's host.previous, written by agent bus
// rename): agents spawned before a rename keep the old name in AGENT_HOST,
// and a config file may still name it.
fn current_host(bus_dir: &str, host: &str) -> String {
    let Ok(data) = std::fs::read(filepath::join(&[bus_dir, "host.previous"])) else {
        return host.to_string();
    };
    let data = String::from_utf8_lossy(&data);
    for line in data.split('\n') {
        if equal_fold(line.trim(), host)
            && let Some(name) = read_trimmed(&filepath::join(&[bus_dir, "host"]))
            && !name.is_empty()
        {
            return name;
        }
    }
    host.to_string()
}

static HOST_UNSAFE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^A-Za-z0-9_-]").unwrap());

// system_host returns this machine's name with characters that can't appear
// in a bus subject token replaced, e.g. "Studio-Mac.local" -> "Studio-Mac":
// on macOS the LocalHostName set in Sharing settings, which the network
// doesn't change, else the short hostname, else "localhost".
fn system_host() -> String {
    let mut name = local_host_name();
    if name.is_empty()
        && let Ok(h) = os::hostname()
    {
        name = h;
    }
    let name = name.split('.').next().unwrap_or("");
    if name.is_empty() {
        return "localhost".to_string();
    }
    HOST_UNSAFE.replace_all(name, "-").into_owned()
}

/// The resolved path of the running executable, or "".
pub fn agent_binary_path() -> String {
    let Ok(exe) = os::executable() else {
        return String::new();
    };
    filepath::eval_symlinks(&exe).unwrap_or(exe)
}

/// A Config with sensible defaults. Backend is intentionally empty: it must
/// be explicitly provided. Storage is relative to the binary location for
/// portability.
pub fn default_config() -> Config {
    let base_dir = get_storage_dir();
    let mut cfg = Config {
        backend: String::new(), // Must be explicitly set
        database_path: filepath::join(&[base_dir.as_str(), "gomuxai.db"]),
        log_dir: filepath::join(&[base_dir.as_str(), "logs"]),
        state_dir: filepath::join(&[base_dir.as_str(), "state"]),
        session_prefix: "agent-".to_string(),
        default_scrollback: 500,
        startup_delay: DEFAULT_STARTUP_DELAY,
        submit_wait: DEFAULT_SUBMIT_WAIT,
        agent_binary: agent_binary_path(),
        host: default_host(&filepath::join(&[base_dir.as_str(), "bus"])),
        termd_namespace: DEFAULT_TERMD_NAMESPACE.to_string(),
        ..Default::default()
    };
    cfg.apply_termd_env();
    cfg
}

/// Loads configuration from a JSON file, over the defaults.
pub fn load_from_file(path: &str) -> anyhow::Result<Config> {
    let data = os::read_file(path).context("reading config file")?;

    let mut cfg = default_config();
    cfg.termd_namespace = DEFAULT_TERMD_NAMESPACE.to_string();
    cfg.termd_binary = String::new();
    cfg.g3d_binary = String::new();

    let host_set = cfg.unmarshal_json(&data).context("parsing config file")?;
    // A database_path elsewhere has its own bus dir, unless the file names
    // host itself.
    if !host_set {
        cfg.host = default_host(&cfg.bus_dir());
    } else if !cfg.host.is_empty() {
        cfg.host = current_host(&cfg.bus_dir(), &cfg.host);
    }
    cfg.apply_termd_env();

    Ok(cfg)
}

// The fields as Go's encoding/json sees them, by json tag.
const FIELDS: &[&str] = &[
    "backend",
    "database_path",
    "log_dir",
    "state_dir",
    "session_prefix",
    "default_scrollback",
    "startup_delay",
    "submit_wait",
    "agent_binary",
    "host",
    "claude_projects_dir",
    "agy_brain_dir",
    "kimi_sessions_dir",
    "termd_namespace",
    "termd_binary",
    "g3d_binary",
];

impl Config {
    /// Go's `json.Unmarshal(data, cfg)`: the fields `data` names are set,
    /// the rest keep their values. Keys match the json tags exactly, else
    /// ignoring case, as Go matches them; unknown keys are ignored; null
    /// leaves a field as it is (and unsets an Option). It returns whether
    /// `data` set "host" to a string.
    pub fn unmarshal_json(&mut self, data: &[u8]) -> anyhow::Result<bool> {
        let v: Value = serde_json::from_slice(data).map_err(json_syntax_error)?;
        let obj = match v {
            Value::Null => return Ok(false),
            Value::Object(obj) => obj,
            other => anyhow::bail!(
                "json: cannot unmarshal {} into Go value of type config.Config",
                json_kind(&other)
            ),
        };
        let mut host_set = false;
        let mut first_err: Option<anyhow::Error> = None;
        for (key, value) in &obj {
            let field = FIELDS
                .iter()
                .find(|f| **f == key)
                .or_else(|| FIELDS.iter().find(|f| equal_fold(f, key)));
            let Some(&field) = field else {
                continue;
            };
            if let Err(e) = self.set_field(field, value) {
                first_err.get_or_insert(e);
                continue;
            }
            if field == "host" {
                host_set = !value.is_null();
            }
        }
        match first_err {
            Some(e) => Err(e),
            None => Ok(host_set),
        }
    }

    fn set_field(&mut self, field: &str, value: &Value) -> anyhow::Result<()> {
        let type_err = |go_type: &str| {
            anyhow::anyhow!(
                "json: cannot unmarshal {} into Go struct field Config.{field} of type {go_type}",
                json_kind(value)
            )
        };
        let string = |dst: &mut String| -> anyhow::Result<()> {
            match value {
                Value::Null => Ok(()),
                Value::String(s) => {
                    *dst = s.clone();
                    Ok(())
                }
                _ => Err(type_err("string")),
            }
        };
        let opt_string = |dst: &mut Option<String>| -> anyhow::Result<()> {
            match value {
                Value::Null => {
                    *dst = None;
                    Ok(())
                }
                Value::String(s) => {
                    *dst = Some(s.clone());
                    Ok(())
                }
                _ => Err(type_err("string")),
            }
        };
        let int = |go_type: &str| -> anyhow::Result<Option<i64>> {
            match value {
                Value::Null => Ok(None),
                Value::Number(n) => n.as_i64().map(Some).ok_or_else(|| type_err(go_type)),
                _ => Err(type_err(go_type)),
            }
        };
        // A negative duration acts as zero wherever Go reads one (<= 0).
        let duration = |n: i64| Duration::from_nanos(n.max(0) as u64);
        match field {
            "backend" => string(&mut self.backend),
            "database_path" => string(&mut self.database_path),
            "log_dir" => string(&mut self.log_dir),
            "state_dir" => string(&mut self.state_dir),
            "session_prefix" => string(&mut self.session_prefix),
            "default_scrollback" => {
                if let Some(n) = int("int")? {
                    self.default_scrollback = n;
                }
                Ok(())
            }
            "startup_delay" => {
                if let Some(n) = int("time.Duration")? {
                    self.startup_delay = duration(n);
                }
                Ok(())
            }
            "submit_wait" => {
                if let Some(n) = int("time.Duration")? {
                    self.submit_wait = duration(n);
                }
                Ok(())
            }
            "agent_binary" => string(&mut self.agent_binary),
            "host" => string(&mut self.host),
            "claude_projects_dir" => opt_string(&mut self.claude_projects_dir),
            "agy_brain_dir" => opt_string(&mut self.agy_brain_dir),
            "kimi_sessions_dir" => opt_string(&mut self.kimi_sessions_dir),
            "termd_namespace" => string(&mut self.termd_namespace),
            "termd_binary" => string(&mut self.termd_binary),
            "g3d_binary" => string(&mut self.g3d_binary),
            _ => Ok(()),
        }
    }

    fn apply_termd_env(&mut self) {
        let v = getenv("GOMUXAI_TERMD_NS");
        if !v.is_empty() {
            self.termd_namespace = v;
        }
        let v = getenv("G3D_TERMD_BIN");
        if !v.is_empty() {
            self.termd_binary = v;
        }
        let v = getenv("G3D_BIN");
        if !v.is_empty() {
            self.g3d_binary = v;
        }
    }

    /// Checks that required configuration is present.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.backend.is_empty() {
            anyhow::bail!("backend is required: use --backend <name> or --backend-config <file>");
        }
        Ok(())
    }

    /// Checks termd_namespace (defaulting empty to "agents").
    pub fn validate_termd(&mut self) -> anyhow::Result<()> {
        if self.termd_namespace.is_empty() {
            self.termd_namespace = DEFAULT_TERMD_NAMESPACE.to_string();
        }
        validate_termd_namespace(&self.termd_namespace)
    }

    /// Creates necessary directories if they don't exist.
    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        let dirs = [
            filepath::dir(&self.database_path),
            self.log_dir.clone(),
            self.state_dir.clone(),
        ];

        // Private: logs and the database hold terminal output and agent state.
        for dir in &dirs {
            os::mkdir_all(dir, 0o700).with_context(|| format!("creating directory {dir}"))?;
        }

        Ok(())
    }

    /// The full terminal session name for an agent.
    pub fn session_name(&self, agent_name: &str) -> String {
        format!("{}{agent_name}", self.session_prefix)
    }

    /// The terminal scrollback snapshot log path for an agent. External
    /// viewers tail this path, so changing it breaks them.
    pub fn log_path(&self, agent_name: &str) -> String {
        filepath::join(&[self.log_dir.as_str(), &format!("{agent_name}.log")])
    }

    /// The file, beside the agent's log, listing the CLI transcripts its
    /// sessions were recorded in.
    pub fn transcripts_path(&self, agent_name: &str) -> String {
        filepath::join(&[self.log_dir.as_str(), &format!("{agent_name}.transcripts")])
    }

    /// The agent bus directory (identity, trusted peers, runtime files),
    /// alongside the database.
    pub fn bus_dir(&self) -> String {
        filepath::join(&[filepath::dir(&self.database_path).as_str(), "bus"])
    }

    /// The per-agent bundle directory under the gomuxai data dir (e.g.
    /// .gomuxai/agents/<name>).
    pub fn agent_dir(&self, agent_name: &str) -> String {
        let base_dir = if !self.state_dir.is_empty() {
            filepath::dir(&self.state_dir)
        } else if !self.database_path.is_empty() {
            filepath::dir(&self.database_path)
        } else {
            STORAGE_DIR_NAME.to_string()
        };
        filepath::join(&[base_dir.as_str(), "agents", agent_name])
    }
}

// json_kind names a JSON value as Go's UnmarshalTypeError does: "string",
// "number 1.5", "bool", "array", "object".
fn json_kind(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(_) => "bool".to_string(),
        Value::Number(n) => format!("number {n}"),
        Value::String(_) => "string".to_string(),
        Value::Array(_) => "array".to_string(),
        Value::Object(_) => "object".to_string(),
    }
}

// json_syntax_error words a parse failure as Go's encoding/json does where
// they agree on what went wrong (input that ends early); other failures keep
// serde_json's words.
fn json_syntax_error(e: serde_json::Error) -> anyhow::Error {
    if e.is_eof() {
        return anyhow::anyhow!("unexpected end of JSON input");
    }
    anyhow::Error::new(e)
}
