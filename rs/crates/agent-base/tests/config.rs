// Go tests/config/config_test.go: configuration management.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use agent_base::config::{self, Config};
use agent_base::gostd::filepath;

// The environment is process-wide and these tests read it (AGENT_HOST,
// GOMUXAI_TERMD_NS, ...) or set it, so they take turns.
static ENV: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

// set_env is t.Setenv: it sets key until the guard drops, then restores it.
struct EnvGuard(&'static str, Option<String>);

fn set_env(key: &'static str, value: &str) -> EnvGuard {
    let old = std::env::var(key).ok();
    // SAFETY: every test in this binary that touches the environment holds
    // ENV, so no other thread reads or writes it meanwhile.
    unsafe { std::env::set_var(key, value) };
    EnvGuard(key, old)
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: as in set_env.
        unsafe {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }
}

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("config-")
        .tempdir_in("/var/tmp")
        .unwrap()
}

fn s(p: &Path) -> String {
    p.to_str().unwrap().to_string()
}

fn write_temp(dir: &tempfile::TempDir, name: &str, body: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    s(&path)
}

// natsbus.ValidateHost's rule: a host name is a bus subject token.
fn validate_host(host: &str) -> Result<(), String> {
    let ok = host
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid host name {host:?}: use letters, digits, '-' and '_'"
        ))
    }
}

// ===========================================================================
// File Loading Tests
// ===========================================================================

#[test]
fn test_load_from_file_success() {
    let _env = env_lock();
    let dir = temp_dir();
    let valid_config = serde_json::json!({
        "backend":            "claude",
        "database_path":      "/var/tmp/agent-base-test/test.db",
        "log_dir":            "/var/tmp/agent-base-test/logs",
        "state_dir":          "/var/tmp/agent-base-test/state",
        "session_prefix":     "test-",
        "default_scrollback": 1000,
    });
    let path = write_temp(&dir, "config.json", &valid_config.to_string());

    let cfg = config::load_from_file(&path).expect("load_from_file failed");

    assert_eq!(cfg.backend, "claude");
    assert_eq!(cfg.database_path, "/var/tmp/agent-base-test/test.db");
    assert_eq!(cfg.log_dir, "/var/tmp/agent-base-test/logs");
    assert_eq!(cfg.state_dir, "/var/tmp/agent-base-test/state");
    assert_eq!(cfg.session_prefix, "test-");
    assert_eq!(cfg.default_scrollback, 1000);
}

#[test]
fn test_load_from_file_missing_file() {
    let _env = env_lock();
    let err = config::load_from_file("/nonexistent/path/config.json")
        .expect_err("expected error for missing file");
    // Should contain a helpful message about reading the file.
    let msg = format!("{err:#}");
    assert!(
        msg.contains("reading config file"),
        "error should mention 'reading config file', got: {msg}"
    );
    assert_eq!(
        msg,
        "reading config file: open /nonexistent/path/config.json: no such file or directory"
    );
}

#[test]
fn test_load_from_file_invalid_json() {
    let _env = env_lock();
    let dir = temp_dir();
    let path = write_temp(
        &dir,
        "config-invalid.json",
        r#"{"backend": "claude", invalid json"#,
    );
    let err = config::load_from_file(&path).expect_err("expected error for invalid JSON");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("parsing config file"),
        "error should mention 'parsing config file', got: {msg}"
    );
}

#[test]
fn test_load_from_file_empty_file() {
    let _env = env_lock();
    let dir = temp_dir();
    let path = write_temp(&dir, "config-empty.json", "");
    // An empty file is invalid JSON.
    let err = config::load_from_file(&path).expect_err("expected error for empty file");
    assert_eq!(
        format!("{err:#}"),
        "parsing config file: unexpected end of JSON input"
    );
}

#[test]
fn test_load_from_file_partial_config() {
    let _env = env_lock();
    let dir = temp_dir();
    // Only set backend, the rest should be defaults.
    let path = write_temp(&dir, "config-partial.json", r#"{"backend": "aider"}"#);
    let cfg = config::load_from_file(&path).expect("load_from_file failed");
    assert_eq!(cfg.backend, "aider");

    let defaults = config::default_config();
    assert_eq!(cfg.session_prefix, defaults.session_prefix);
    assert_eq!(cfg.default_scrollback, defaults.default_scrollback);
}

// Go's encoding/json: keys match ignoring case, null leaves a field alone
// (and unsets a pointer), a duration is nanoseconds, a wrong type fails.
#[test]
fn test_load_from_file_json_like_go() {
    let _env = env_lock();
    let dir = temp_dir();
    let path = write_temp(
        &dir,
        "app.json",
        r#"{"Backend": "claude", "session_prefix": null, "startup_delay": 500000000,
            "claude_projects_dir": "", "agy_brain_dir": null, "unknown": [1, 2]}"#,
    );
    let cfg = config::load_from_file(&path).unwrap();
    assert_eq!(cfg.backend, "claude");
    assert_eq!(cfg.session_prefix, "agent-");
    assert_eq!(cfg.startup_delay, std::time::Duration::from_millis(500));
    assert_eq!(cfg.claude_projects_dir.as_deref(), Some(""));
    assert_eq!(cfg.agy_brain_dir, None);
    assert_eq!(cfg.kimi_sessions_dir, None);

    let path = write_temp(&dir, "bad.json", r#"{"default_scrollback": "many"}"#);
    let err = config::load_from_file(&path).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "parsing config file: json: cannot unmarshal string into Go struct field Config.default_scrollback of type int"
    );
    let path = write_temp(&dir, "array.json", "[]");
    let err = config::load_from_file(&path).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "parsing config file: json: cannot unmarshal array into Go value of type config.Config"
    );
    let path = write_temp(&dir, "null.json", "null");
    assert!(config::load_from_file(&path).is_ok());
}

// ===========================================================================
// Validation Tests
// ===========================================================================

#[test]
fn test_validate_backend_required() {
    let _env = env_lock();
    let cfg = config::default_config();
    // Backend is empty by default.
    let err = cfg
        .validate()
        .expect_err("expected validation error for empty backend");
    let msg = err.to_string();
    assert!(
        msg.contains("backend"),
        "error should mention 'backend', got: {msg}"
    );
    assert!(
        msg.contains("--backend"),
        "error should suggest '--backend' flag, got: {msg}"
    );
    assert!(
        msg.contains("--backend-config"),
        "error should suggest '--backend-config' flag, got: {msg}"
    );
}

#[test]
fn test_validate_valid_backend() {
    let _env = env_lock();
    for (name, backend) in [
        ("claude backend", "claude"),
        ("aider backend", "aider"),
        ("custom backend", "custom-backend"),
    ] {
        let mut cfg = config::default_config();
        cfg.backend = backend.to_string();
        assert!(
            cfg.validate().is_ok(),
            "{name}: validation should pass for backend {backend:?}"
        );
    }
}

#[test]
fn test_validate_termd() {
    let mut cfg = Config::default();
    cfg.validate_termd().unwrap();
    assert_eq!(cfg.termd_namespace, config::DEFAULT_TERMD_NAMESPACE);
    for (ns, msg) in [
        (
            "default",
            r#"termd namespace "default" is reserved for interactive and wall sessions; use a dedicated namespace such as "agents""#,
        ),
        (
            "wall",
            r#"termd namespace "wall" is reserved for interactive and wall sessions; use a dedicated namespace such as "agents""#,
        ),
        (
            "a b",
            r#"termd namespace "a b" must be 1 to 64 bytes of [A-Za-z0-9._-]"#,
        ),
    ] {
        cfg.termd_namespace = ns.to_string();
        assert_eq!(cfg.validate_termd().unwrap_err().to_string(), msg);
    }
    cfg.termd_namespace = "my.agents_2-x".to_string();
    cfg.validate_termd().unwrap();
}

// ===========================================================================
// Directory Setup Tests
// ===========================================================================

#[test]
fn test_ensure_dirs_creates_all_directories() {
    let tmp = temp_dir();
    let root = s(tmp.path());
    let cfg = Config {
        database_path: filepath::join(&[root.as_str(), "db", "gomuxai.db"]),
        log_dir: filepath::join(&[root.as_str(), "logs"]),
        state_dir: filepath::join(&[root.as_str(), "state"]),
        ..Default::default()
    };
    cfg.ensure_dirs().expect("ensure_dirs failed");

    for dir in ["db", "logs", "state"] {
        let p = tmp.path().join(dir);
        let meta =
            std::fs::metadata(&p).unwrap_or_else(|e| panic!("directory {p:?} should exist: {e}"));
        assert!(meta.is_dir(), "{p:?} should be a directory");
    }
}

// The data dir holds terminal logs, captured output, and agent state, so
// other local users must not be able to read it.
#[test]
fn test_ensure_dirs_private_to_user() {
    let tmp = temp_dir();
    let root = s(tmp.path());
    let cfg = Config {
        database_path: filepath::join(&[root.as_str(), ".gomuxai", "gomuxai.db"]),
        log_dir: filepath::join(&[root.as_str(), ".gomuxai", "logs"]),
        state_dir: filepath::join(&[root.as_str(), ".gomuxai", "state"]),
        ..Default::default()
    };
    cfg.ensure_dirs().expect("ensure_dirs failed");
    for dir in [".gomuxai", ".gomuxai/logs", ".gomuxai/state"] {
        let perm = std::fs::metadata(tmp.path().join(dir))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            perm & 0o077,
            0,
            "{dir} mode {perm:o}: readable by other users"
        );
    }
}

#[test]
fn test_ensure_dirs_existing_directories() {
    let tmp = temp_dir();
    let log_dir = tmp.path().join("logs");
    let state_dir = tmp.path().join("state");
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::create_dir_all(&state_dir).unwrap();
    let cfg = Config {
        database_path: s(&tmp.path().join("gomuxai.db")),
        log_dir: s(&log_dir),
        state_dir: s(&state_dir),
        ..Default::default()
    };
    // Should not error on existing directories.
    cfg.ensure_dirs()
        .expect("ensure_dirs should handle existing directories");
}

#[test]
fn test_ensure_dirs_nested_directories() {
    let tmp = temp_dir();
    let root = s(tmp.path());
    let cfg = Config {
        database_path: filepath::join(&[root.as_str(), "deep", "nested", "path", "gomuxai.db"]),
        log_dir: filepath::join(&[root.as_str(), "a", "b", "c", "logs"]),
        state_dir: filepath::join(&[root.as_str(), "x", "y", "z", "state"]),
        ..Default::default()
    };
    cfg.ensure_dirs()
        .expect("ensure_dirs should create nested directories");
    for dir in ["deep/nested/path", "a/b/c/logs", "x/y/z/state"] {
        assert!(
            tmp.path().join(dir).is_dir(),
            "nested directory {dir:?} should exist"
        );
    }
}

#[test]
fn test_ensure_dirs_names_the_dir_it_could_not_make() {
    let tmp = temp_dir();
    let file = tmp.path().join("file");
    std::fs::write(&file, "x").unwrap();
    let cfg = Config {
        database_path: s(&tmp.path().join("gomuxai.db")),
        log_dir: s(&file.join("logs")),
        state_dir: s(&tmp.path().join("state")),
        ..Default::default()
    };
    let err = cfg.ensure_dirs().unwrap_err();
    let f = s(&file);
    assert_eq!(
        format!("{err:#}"),
        format!("creating directory {f}/logs: mkdir {f}: not a directory")
    );
}

// ===========================================================================
// Path Construction Tests
// ===========================================================================

#[test]
fn test_session_name() {
    for (name, prefix, agent, expected) in [
        ("default prefix", "agent-", "researcher", "agent-researcher"),
        (
            "custom prefix",
            "myproject-",
            "builder",
            "myproject-builder",
        ),
        ("empty prefix", "", "worker", "worker"),
        (
            "agent name with dashes",
            "test-",
            "sub-agent-1",
            "test-sub-agent-1",
        ),
    ] {
        let cfg = Config {
            session_prefix: prefix.to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.session_name(agent), expected, "{name}");
    }
}

#[test]
fn test_log_path() {
    for (name, log_dir, agent, expected) in [
        (
            "simple path",
            "/var/log/gomuxai",
            "researcher",
            "/var/log/gomuxai/researcher.log",
        ),
        ("relative path", "logs", "builder", "logs/builder.log"),
        (
            "agent name with special chars",
            "/tmp/logs",
            "agent-v2",
            "/tmp/logs/agent-v2.log",
        ),
    ] {
        let cfg = Config {
            log_dir: log_dir.to_string(),
            ..Default::default()
        };
        assert_eq!(cfg.log_path(agent), expected, "{name}");
    }
}

#[test]
fn test_derived_paths() {
    let cfg = Config {
        database_path: "/srv/.gomuxai/gomuxai.db".to_string(),
        log_dir: "/srv/.gomuxai/logs".to_string(),
        state_dir: "/srv/.gomuxai/state".to_string(),
        ..Default::default()
    };
    assert_eq!(
        cfg.transcripts_path("helper"),
        "/srv/.gomuxai/logs/helper.transcripts"
    );
    assert_eq!(cfg.bus_dir(), "/srv/.gomuxai/bus");
    assert_eq!(cfg.agent_dir("helper"), "/srv/.gomuxai/agents/helper");
    let only_db = Config {
        database_path: "/srv/data/gomuxai.db".to_string(),
        ..Default::default()
    };
    assert_eq!(only_db.agent_dir("helper"), "/srv/data/agents/helper");
    assert_eq!(
        Config::default().agent_dir("helper"),
        ".gomuxai/agents/helper"
    );
    assert_eq!(Config::default().bus_dir(), "bus");
}

// ===========================================================================
// DefaultConfig Tests
// ===========================================================================

#[test]
fn test_default_config() {
    let _env = env_lock();
    let cfg = config::default_config();
    // Backend should be empty (required to be explicitly set).
    assert_eq!(cfg.backend, "", "default backend should be empty");
    assert!(
        !cfg.session_prefix.is_empty(),
        "default session_prefix should not be empty"
    );
    assert!(
        cfg.default_scrollback > 0,
        "default scrollback should be positive, got {}",
        cfg.default_scrollback
    );
    assert!(
        !cfg.database_path.is_empty(),
        "default database_path should not be empty"
    );
    assert!(
        !cfg.log_dir.is_empty(),
        "default log_dir should not be empty"
    );
    assert!(
        !cfg.state_dir.is_empty(),
        "default state_dir should not be empty"
    );
    assert_eq!(cfg.startup_delay, config::DEFAULT_STARTUP_DELAY);
    assert_eq!(cfg.submit_wait, config::DEFAULT_SUBMIT_WAIT);
}

#[test]
fn test_default_config_portable_storage() {
    let _env = env_lock();
    let cfg = config::default_config();

    // CRITICAL: Storage must NOT be in the home directory. This ensures the
    // tool is portable.
    let Ok(home) = std::env::var("HOME") else {
        return; // cannot determine home directory
    };
    for (name, p) in [
        ("database_path", &cfg.database_path),
        ("log_dir", &cfg.log_dir),
        ("state_dir", &cfg.state_dir),
    ] {
        assert!(
            !(p.len() > home.len() && p.starts_with(&home)),
            "{name} should NOT be in the home directory for portability, got {p:?}"
        );
    }
    // All paths should contain the storage dir name.
    assert!(
        cfg.database_path.contains(config::STORAGE_DIR_NAME),
        "database_path should contain {:?}, got {:?}",
        config::STORAGE_DIR_NAME,
        cfg.database_path
    );
}

// The default host becomes a bus subject token, so it must be subject-safe.
#[test]
fn test_default_config_host_is_subject_safe() {
    let _env = env_lock();
    let host = config::default_config().host;
    if let Err(e) = validate_host(&host) {
        panic!("default host {host:?}: {e}");
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

// The host is $AGENT_HOST, else the app config's "host", else the name the
// bus identity was created for, so a machine the network renames (macOS's
// "Mac") keeps the name its peers trust.
#[test]
fn test_load_from_file_host_order() {
    let _env = env_lock();
    let dir = temp_dir();
    let bus_dir = dir.path().join("bus");
    std::fs::create_dir_all(&bus_dir).unwrap();
    let db = s(&dir.path().join("gomuxai.db"));
    let load = |extra: &str| -> Config {
        let files = temp_dir();
        let path = write_temp(
            &files,
            "app.json",
            &format!(r#"{{"database_path": {}{extra}}}"#, json_string(&db)),
        );
        config::load_from_file(&path).unwrap()
    };
    let _host = set_env("AGENT_HOST", "");

    let system = load("").host; // no identity yet: the system's name
    if let Err(e) = validate_host(&system) {
        panic!("with no identity, host {system:?}: {e}");
    }
    std::fs::write(bus_dir.join("host"), "laptop\n").unwrap();
    assert_eq!(load("").host, "laptop", "with an identity for laptop");
    assert_eq!(
        load(r#", "host": "from-file""#).host,
        "from-file",
        r#"with "host" in the app config"#
    );
    let _host = set_env("AGENT_HOST", "from-env");
    assert_eq!(load("").host, "from-env", "with AGENT_HOST set");
}

// After agent bus rename, a former name (in AGENT_HOST, which agents spawned
// before the rename keep, or in the app config) means the identity's current
// name, so those agents keep reaching their own bus.
#[test]
fn test_load_from_file_former_host_name_means_the_current_one() {
    let _env = env_lock();
    let dir = temp_dir();
    let bus_dir = dir.path().join("bus");
    std::fs::create_dir_all(&bus_dir).unwrap();
    std::fs::write(bus_dir.join("host"), "new-name\n").unwrap();
    std::fs::write(bus_dir.join("host.previous"), "older\nold-name\n").unwrap();
    let db = s(&dir.path().join("gomuxai.db"));
    let load = |extra: &str| -> String {
        let files = temp_dir();
        let path = write_temp(
            &files,
            "app.json",
            &format!(r#"{{"database_path": {}{extra}}}"#, json_string(&db)),
        );
        config::load_from_file(&path).unwrap().host
    };
    let _host = set_env("AGENT_HOST", "OLD-NAME");
    assert_eq!(load(""), "new-name", "AGENT_HOST naming a former name");
    let _host = set_env("AGENT_HOST", "elsewhere");
    assert_eq!(load(""), "elsewhere", "AGENT_HOST naming another host");
    let _host = set_env("AGENT_HOST", "");
    assert_eq!(
        load(r#", "host": "older""#),
        "new-name",
        r#""host" naming a former name"#
    );
}

// GOMUXAI_TERMD_NS, G3D_TERMD_BIN and G3D_BIN override the defaults and the
// file.
#[test]
fn test_termd_env_overrides() {
    let _env = env_lock();
    let dir = temp_dir();
    let path = write_temp(
        &dir,
        "app.json",
        r#"{"termd_namespace": "from-file", "termd_binary": "/opt/termd"}"#,
    );
    let _clear = [
        set_env("GOMUXAI_TERMD_NS", ""),
        set_env("G3D_TERMD_BIN", ""),
        set_env("G3D_BIN", ""),
    ];
    let cfg = config::load_from_file(&path).unwrap();
    assert_eq!(cfg.termd_namespace, "from-file");
    assert_eq!(cfg.termd_binary, "/opt/termd");
    let _ns = set_env("GOMUXAI_TERMD_NS", "from-env");
    let _bin = set_env("G3D_BIN", "/opt/g3d");
    let cfg = config::load_from_file(&path).unwrap();
    assert_eq!(cfg.termd_namespace, "from-env");
    assert_eq!(cfg.g3d_binary, "/opt/g3d");
    assert_eq!(config::default_config().termd_namespace, "from-env");
}
