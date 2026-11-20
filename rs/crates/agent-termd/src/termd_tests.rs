use crate::chord::{caller_env, parse_special_key};
use crate::client::{Client, Options, TEST_SERVER_ENV, resolve_terminal_cwd};
use crate::paths::{
    check_test_server, daemon_binary, is_cargo_test_exe, log_path, socket_dir, socket_path,
    validate_namespace,
};
use crate::testing::{EnvGuard, create_socket_dir};
use crate::wire::{
    WireError, check_socket_dir, connect_unix, daemon_env, dial_namespace, ensure_daemon_with_bin,
    go_exit_text, is_not_running, is_unsafe_socket, is_wire_code, read_owned_regular_file,
    request_raw, stop_socket, verify_peer_creds, verify_peer_creds_for_uid,
};
use g3d_wire::frame::{self, FrameDecoder, ty};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

struct ShortDir(PathBuf);

impl ShortDir {
    fn new(prefix: &str) -> Self {
        Self(create_socket_dir(prefix).expect("short temp dir"))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ShortDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn read_one_frame(sock: &mut UnixStream) -> Option<g3d_wire::Frame> {
    let mut dec = FrameDecoder::default();
    let mut buf = [0u8; 64 << 10];
    loop {
        if let Ok(Some(f)) = dec.next_frame() {
            return Some(f);
        }
        let n = sock.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        dec.feed(&buf[..n]);
    }
}

fn start_mock_welcome_server(sock_path: &Path, ns: &str) -> (UnixListener, mpsc::Receiver<Value>) {
    let ln = UnixListener::bind(sock_path).expect("bind mock socket");
    let ln_clone = ln.try_clone().expect("clone listener");
    let ns = ns.to_owned();
    let (tx, rx) = mpsc::sync_channel(1);

    std::thread::spawn(move || {
        let Ok((mut conn, _)) = ln_clone.accept() else {
            return;
        };
        let Some(f) = read_one_frame(&mut conn) else {
            return;
        };
        if f.ty != ty::HELLO {
            let _ = tx.try_send(Value::Null);
            return;
        }
        let hello: Value = serde_json::from_slice(&f.payload).unwrap_or(Value::Null);
        let _ = tx.try_send(hello);

        let welcome = json!({
            "proto": [1, 5],
            "daemon": "g3d-termd/test",
            "engine": "mock",
            "ns": ns,
            "pid": std::process::id(),
            "features": ["key", "paste", "status"],
            "max_frame": frame::DEFAULT_MAX_FRAME,
            "instance": "inst-test",
            "generation": 2,
            "holder": {"pid": std::process::id(), "version": "test"},
        });
        if conn
            .write_all(&frame::encode_json(ty::WELCOME, 0, &welcome))
            .is_err()
        {
            return;
        }

        while let Some(req_frame) = read_one_frame(&mut conn) {
            if req_frame.ty != ty::REQUEST {
                continue;
            }
            let Ok(req) = serde_json::from_slice::<Value>(&req_frame.payload) else {
                continue;
            };
            let op = req.get("op").and_then(Value::as_str).unwrap_or("");
            let reply = json!({
                "req": req["req"],
                "ok": true,
                "op": op,
            });
            let _ = conn.write_all(&frame::encode_json(ty::REPLY, 0, &reply));
            if op == "stop" {
                break;
            }
        }
    });

    (ln, rx)
}

#[test]
fn test_validate_namespace() {
    for valid in ["agents", "agents-dev", "a1.b_c-2", &"z".repeat(64)] {
        assert!(
            validate_namespace(valid).is_ok(),
            "validate_namespace({valid:?}) failed"
        );
    }
    for reserved in ["default", "wall"] {
        let err = validate_namespace(reserved).unwrap_err();
        assert!(
            err.to_string().contains("reserved"),
            "validate_namespace({reserved:?}) = {err}, want reserved refusal"
        );
    }
    for bad in [
        "",
        "has space",
        "slash/ns",
        "../up",
        "bad\0nul",
        "é",
        &"a".repeat(65),
    ] {
        assert!(
            validate_namespace(bad).is_err(),
            "validate_namespace({bad:?}) succeeded"
        );
    }
}

#[test]
fn test_check_test_server() {
    let mut env = EnvGuard::lock();
    env.remove(TEST_SERVER_ENV);
    env.remove("G3D_TERMD_DIR");
    let err = check_test_server().unwrap_err();
    assert!(
        err.to_string().contains("agent_termd::testing"),
        "check_test_server without {TEST_SERVER_ENV} = {err}"
    );

    let dir = ShortDir::new("gmxt-chk-");
    env.set(TEST_SERVER_ENV, dir.path());
    env.set("G3D_TERMD_DIR", dir.path());
    env.remove("G3D_TERMD_SOCKET");
    check_test_server().expect("matching dir should pass");

    // A socket outside the private dir is refused.
    let other = ShortDir::new("gmxt-oth-");
    env.set("G3D_TERMD_SOCKET", other.path().join("termd-wall.sock"));
    let err = check_test_server().unwrap_err();
    assert!(err.to_string().contains("G3D_TERMD_SOCKET"), "err = {err}");

    // One inside it is what a pane of the test's own daemon gets: with or
    // without G3D_TERMD_DIR, it passes, and without it the socket dir is the
    // private one (an agent a test runs in a pane is still the test's).
    env.set("G3D_TERMD_SOCKET", dir.path().join("termd-wall.sock"));
    check_test_server().expect("a pane's socket in the private dir");
    assert_eq!(
        crate::paths::pane_test_dir(),
        None,
        "G3D_TERMD_DIR decides while set"
    );
    env.remove("G3D_TERMD_DIR");
    check_test_server().expect("a pane's socket in the private dir, no G3D_TERMD_DIR");
    assert_eq!(socket_dir(), dir.path());
    assert_eq!(socket_path("x"), dir.path().join("termd-x.sock"));
    assert_eq!(log_path("x"), dir.path().join("termd-x.log"));
    let daemon_dir = crate::wire::daemon_env_os()
        .into_iter()
        .rfind(|(k, _)| k == "G3D_TERMD_DIR")
        .map(|(_, v)| v);
    assert_eq!(
        daemon_dir.as_deref(),
        Some(dir.path().as_os_str()),
        "a daemon it starts goes there too"
    );

    // Without the private dir's variable, the pane's socket proves nothing.
    env.remove(TEST_SERVER_ENV);
    assert!(check_test_server().is_err());
    assert_eq!(crate::paths::pane_test_dir(), None);
}

#[test]
fn test_parse_special_key() {
    let cases: &[(&str, &str, &str, &[&str], &str)] = &[
        ("Enter", "Enter", "Enter", &[], ""),
        ("Escape", "Escape", "Escape", &[], ""),
        ("Tab", "Tab", "Tab", &[], ""),
        ("BTab", "Tab", "Tab", &["Shift"], ""),
        ("BSpace", "Backspace", "Backspace", &[], ""),
        ("Space", "Space", " ", &[], " "),
        ("Up", "ArrowUp", "ArrowUp", &[], ""),
        ("Down", "ArrowDown", "ArrowDown", &[], ""),
        ("Left", "ArrowLeft", "ArrowLeft", &[], ""),
        ("Right", "ArrowRight", "ArrowRight", &[], ""),
        ("PgUp", "PageUp", "PageUp", &[], ""),
        ("PPage", "PageUp", "PageUp", &[], ""),
        ("PgDn", "PageDown", "PageDown", &[], ""),
        ("NPage", "PageDown", "PageDown", &[], ""),
        ("DC", "Delete", "Delete", &[], ""),
        ("IC", "Insert", "Insert", &[], ""),
        ("Home", "Home", "Home", &[], ""),
        ("End", "End", "End", &[], ""),
        ("F5", "F5", "F5", &[], ""),
        ("C-c", "KeyC", "c", &["Control"], "c"),
        ("c-d", "KeyD", "d", &["Control"], "d"),
        ("M-b", "KeyB", "b", &["Alt"], "b"),
        ("shift+enter", "Enter", "Enter", &["Shift"], ""),
        ("ctrl+c", "KeyC", "c", &["Control"], "c"),
        ("alt+left", "ArrowLeft", "ArrowLeft", &["Alt"], ""),
        ("1", "Digit1", "1", &[], "1"),
        ("!", "Digit1", "!", &["Shift"], "!"),
        ("A", "KeyA", "A", &["Shift"], "A"),
        ("x", "KeyX", "x", &[], "x"),
        ("+", "Equal", "+", &["Shift"], "+"),
        ("ctrl++", "Equal", "+", &["Control", "Shift"], "+"),
    ];
    for &(input, code, key, mods, text) in cases {
        let ev = parse_special_key(input)
            .unwrap_or_else(|| panic!("parse_special_key({input:?}) = None"));
        let want_mods: Vec<String> = mods.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            (&ev.code[..], &ev.key[..], &ev.mods, &ev.text[..]),
            (code, key, &want_mods, text),
            "parse_special_key({input:?})"
        );
    }

    for literal in ["-N2", "not-a-key", ""] {
        assert!(
            parse_special_key(literal).is_none(),
            "parse_special_key({literal:?}) should fall back to literal"
        );
    }
}

#[test]
fn test_caller_env() {
    let huge = format!("HUGE={}", "x".repeat(130 * 1024));
    let entries: Vec<&OsStr> = vec![
        OsStr::new("FOO=bar=baz"),
        OsStr::new("EMPTY="),
        OsStr::new("G3D_BIN=/usr/local/bin/g3d"),
        OsStr::new("GOMUXAI_TERMD_NS=agents"),
        OsStr::new("BASH_FUNC_fn%%=() { :; }"),
        OsStr::new("BAD-NAME=1"),
        OsStr::from_bytes(b"INVALID_UTF8=\xff"),
        OsStr::new("NUL_VAL=abc\0def"),
        OsStr::new(&huge),
        OsStr::new(
            "G3D_RELAY_TOKEN=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        ),
        OsStr::new("G3D_RELAY_TOKEN_DIR=/var/tmp/relay-tokens"),
        OsStr::new("G3D_RELAY_PORT=8080"),
        OsStr::new("G3D_TERMD_DIR=/var/tmp/g3d-501"),
        OsStr::new("G3D_TERMD_SOCKET=/var/tmp/g3d-501/termd-wall.sock"),
        OsStr::new("G3D_TERMD_ID=pane-1"),
        OsStr::new("G3D_TERMD_CONFIG=/var/tmp/termd.toml"),
        OsStr::new("SHLVL=3"),
        OsStr::new("OLDPWD=/var/tmp/prev"),
        OsStr::new("PWD=/var/tmp/curr"),
        OsStr::new("_=/usr/bin/env"),
    ];
    let got = caller_env(entries);
    assert_eq!(got.get("FOO").map(String::as_str), Some("bar=baz"));
    assert_eq!(got.get("EMPTY").map(String::as_str), Some(""));
    assert_eq!(
        got.get("G3D_BIN").map(String::as_str),
        Some("/usr/local/bin/g3d")
    );
    assert_eq!(
        got.get("GOMUXAI_TERMD_NS").map(String::as_str),
        Some("agents")
    );

    for rejected in [
        "BASH_FUNC_fn%%",
        "BAD-NAME",
        "INVALID_UTF8",
        "NUL_VAL",
        "HUGE",
        "G3D_RELAY_TOKEN",
        "G3D_RELAY_TOKEN_DIR",
        "G3D_RELAY_PORT",
        "G3D_TERMD_DIR",
        "G3D_TERMD_SOCKET",
        "G3D_TERMD_ID",
        "G3D_TERMD_CONFIG",
        "SHLVL",
        "OLDPWD",
        "PWD",
        "_",
    ] {
        assert!(
            !got.contains_key(rejected),
            "{rejected} should be excluded from caller_env"
        );
    }
}

#[test]
fn test_create_session_scrubs_relay_and_termd_vars_from_wire() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-env-");
    let ns = "agents";
    let sock_path = dir.path().join(format!("termd-{ns}.sock"));
    let ln = UnixListener::bind(&sock_path).unwrap();
    let ln_clone = ln.try_clone().unwrap();

    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = ln_clone.accept() else {
            return;
        };
        let Some(f) = read_one_frame(&mut conn) else {
            return;
        };
        if f.ty != ty::HELLO {
            return;
        }
        let welcome = json!({
            "proto": [1, 5],
            "daemon": "g3d-termd/mock",
            "engine": "mock",
            "ns": ns,
            "pid": std::process::id(),
            "features": ["status"],
            "max_frame": frame::DEFAULT_MAX_FRAME,
            "instance": "inst-mock",
        });
        if conn
            .write_all(&frame::encode_json(ty::WELCOME, 0, &welcome))
            .is_err()
        {
            return;
        }
        let Some(req_frame) = read_one_frame(&mut conn) else {
            return;
        };
        if req_frame.ty != ty::REQUEST {
            return;
        }
        let req: Value = serde_json::from_slice(&req_frame.payload).unwrap();
        let reply = json!({"req": req["req"], "ok": true});
        let _ = conn.write_all(&frame::encode_json(ty::REPLY, 0, &reply));
        let _ = tx.send(req);
    });

    let bin_dir = ShortDir::new("gmxt-bin-");
    let fake_bin = bin_dir.path().join("fake-g3d-termd");
    std::fs::write(&fake_bin, b"#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o700)).unwrap();

    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");
    env.set("G3D_TERMD_ID", "caller-pane");
    env.set("G3D_RELAY_TOKEN", "secret-capability-token");
    env.set("G3D_RELAY_TOKEN_DIR", "/var/tmp/secret-token-dir");
    env.set("SHLVL", "4");
    env.set("OLDPWD", "/var/tmp/old");

    let c = Client::new(Options {
        namespace: ns.to_owned(),
        daemon_bin: fake_bin.to_string_lossy().into_owned(),
        ..Default::default()
    })
    .unwrap();

    let req_env = HashMap::from([
        ("AGENT_NAME".to_owned(), "worker".to_owned()),
        ("G3D_RELAY_TOKEN".to_owned(), "req-leak".to_owned()),
        ("G3D_RELAY_TOKEN_DIR".to_owned(), "/req-leak".to_owned()),
        ("G3D_TERMD_DIR".to_owned(), "/req-leak".to_owned()),
        ("G3D_TERMD_CONFIG".to_owned(), "/req-leak".to_owned()),
        ("G3D_TERMD_SOCKET".to_owned(), "/req-leak".to_owned()),
        ("G3D_TERMD_ID".to_owned(), "req-leak".to_owned()),
        ("PWD".to_owned(), "/req-leak".to_owned()),
    ]);
    c.create_session("worker", &req_env).unwrap();

    let req = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let env_base = req["env_base"].as_object().unwrap();
    for forbidden in [
        "G3D_RELAY_TOKEN",
        "G3D_RELAY_TOKEN_DIR",
        "G3D_TERMD_DIR",
        "G3D_TERMD_SOCKET",
        "G3D_TERMD_ID",
        "SHLVL",
        "OLDPWD",
        "PWD",
        "_",
    ] {
        assert!(
            !env_base.contains_key(forbidden),
            "create.env_base leaked {forbidden}"
        );
    }

    let sent_env = req["env"].as_object().unwrap();
    assert_eq!(
        sent_env.get("AGENT_NAME").and_then(Value::as_str),
        Some("worker")
    );
    for forbidden in [
        "G3D_RELAY_TOKEN",
        "G3D_RELAY_TOKEN_DIR",
        "G3D_TERMD_DIR",
        "G3D_TERMD_CONFIG",
        "G3D_TERMD_SOCKET",
        "G3D_TERMD_ID",
        "PWD",
    ] {
        assert!(
            !sent_env.contains_key(forbidden),
            "create.env leaked {forbidden}"
        );
    }
    drop(ln);
}

fn start_impostor_listener(sock_path: &Path) -> (UnixListener, mpsc::Receiver<()>) {
    let ln = UnixListener::bind(sock_path).unwrap();
    let ln_clone = ln.try_clone().unwrap();
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        if let Ok((conn, _)) = ln_clone.accept() {
            drop(conn);
            let _ = tx.try_send(());
        }
    });
    (ln, rx)
}

#[test]
fn test_socket_trust_fails_closed_on_unsafe_dir_and_symlink() {
    let mut env = EnvGuard::lock();
    let ns = "agents";
    let bin_dir = ShortDir::new("gmxt-fbin-");
    let spawn_marker = bin_dir.path().join("daemon-spawned");
    let fake_bin = bin_dir.path().join("fake-g3d-termd");
    std::fs::write(
        &fake_bin,
        format!("#!/bin/sh\ntouch '{}'\nexit 0\n", spawn_marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o700)).unwrap();

    let assert_fails_closed = |env: &mut EnvGuard, sock_dir: &str| {
        env.set("G3D_TERMD_DIR", sock_dir);
        env.set(TEST_SERVER_ENV, sock_dir);
        env.remove("G3D_TERMD_SOCKET");

        let c = Client::new(Options {
            namespace: ns.to_owned(),
            daemon_bin: fake_bin.to_string_lossy().into_owned(),
            ..Default::default()
        })
        .unwrap();

        let err = c.status().unwrap_err();
        assert!(
            is_unsafe_socket(&err),
            "status({sock_dir:?}) = {err}, want UnsafeSocketError"
        );

        let start = Instant::now();
        let err = c.create_session("sess", &HashMap::new()).unwrap_err();
        assert!(
            is_unsafe_socket(&err),
            "create_session({sock_dir:?}) = {err}, want UnsafeSocketError"
        );
        assert!(
            start.elapsed() <= Duration::from_secs(1),
            "create_session({sock_dir:?}) took {:?}",
            start.elapsed()
        );
        assert!(
            !spawn_marker.exists(),
            "create_session({sock_dir:?}) spawned daemon despite unsafe socket directory"
        );

        let sock_path = Path::new(sock_dir).join(format!("termd-{ns}.sock"));
        let err = stop_socket(&sock_path).unwrap_err();
        assert!(is_unsafe_socket(&err), "stop_socket({sock_path:?}) = {err}");
    };

    // mode_0755
    {
        let dir = ShortDir::new("gmxt755-");
        let (_ln, accepted) = start_impostor_listener(&dir.path().join(format!("termd-{ns}.sock")));
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_fails_closed(&mut env, &dir.path().to_string_lossy());
        assert!(
            accepted.try_recv().is_err(),
            "connected to impostor socket inside 0755 dir"
        );
    }

    // symlink_and_dangling (including trailing slash!)
    {
        let real_dir = ShortDir::new("gmxtreal-");
        let (_ln, accepted) =
            start_impostor_listener(&real_dir.path().join(format!("termd-{ns}.sock")));

        let parent = ShortDir::new("gmxtlnk-");
        let link_dir = parent.path().join("socklink");
        std::os::unix::fs::symlink(real_dir.path(), &link_dir).unwrap();
        let dangling_dir = parent.path().join("dangling");
        std::os::unix::fs::symlink(parent.path().join("missing"), &dangling_dir).unwrap();

        let candidates = [
            link_dir.to_string_lossy().into_owned(),
            format!("{}/", link_dir.display()),
            dangling_dir.to_string_lossy().into_owned(),
        ];
        for candidate in &candidates {
            assert_fails_closed(&mut env, candidate);
        }
        assert!(
            accepted.try_recv().is_err(),
            "connected to impostor socket through symlinked G3D_TERMD_DIR"
        );
    }
}

#[test]
fn test_resolve_terminal_cwd() {
    let base = ShortDir::new("gmxt-cwd-");
    let spaced = base.path().join("sub dir");
    std::fs::create_dir_all(&spaced).unwrap();

    // macOS zsh OSC 7 reports file://<host>/<escaped-path>.
    let osc7 = format!(
        "file://some-host{}",
        spaced.to_string_lossy().replace(' ', "%20")
    );
    assert_eq!(
        resolve_terminal_cwd(&osc7, "/var/tmp"),
        spaced.to_string_lossy()
    );
    assert_eq!(
        resolve_terminal_cwd(
            "file://some-host/nonexistent/path/xyz",
            &spaced.to_string_lossy()
        ),
        spaced.to_string_lossy()
    );
}

#[test]
fn test_new_client_lazy_daemon_binary() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-lazy-");
    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");
    env.set("G3D_TERMD_BIN", "/nonexistent/g3d-termd");

    // Client::new and read-only status succeed without resolving daemon_binary.
    let c = Client::new(Options {
        namespace: "agents".to_owned(),
        ..Default::default()
    })
    .unwrap();
    let st = c.status().unwrap();
    assert!(st.terminals.is_empty());

    let err = c.create_session("w", &HashMap::new()).unwrap_err();
    assert!(
        err.to_string().contains("G3D_TERMD_BIN"),
        "create_session with bad G3D_TERMD_BIN = {err}"
    );
}

#[test]
fn test_session_env_prefers_request_and_redials_on_busy() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-senv-");
    let ns = "agents";
    let sock_path = dir.path().join(format!("termd-{ns}.sock"));
    let ln = UnixListener::bind(&sock_path).unwrap();
    let ln_clone = ln.try_clone().unwrap();

    std::thread::spawn(move || {
        // Attempt 1: simulate transient daemon busy/restarting frame on handshake.
        if let Ok((mut c1, _)) = ln_clone.accept() {
            let _ = read_one_frame(&mut c1);
            let busy = json!({"code": "restarting", "message": "brain restarting"});
            let _ = c1.write_all(&frame::encode_json(ty::ERROR, 0, &busy));
        }
        // Attempt 2: serve WELCOME and op "env" with base PATH before request PATH.
        let Ok((mut c2, _)) = ln_clone.accept() else {
            return;
        };
        if read_one_frame(&mut c2).is_none() {
            return;
        }
        let welcome = json!({
            "proto": [1, 5],
            "daemon": "g3d-termd/mock",
            "engine": "mock",
            "ns": ns,
            "pid": std::process::id(),
            "features": ["status", "env"],
            "max_frame": frame::DEFAULT_MAX_FRAME,
            "instance": "inst-mock",
        });
        let _ = c2.write_all(&frame::encode_json(ty::WELCOME, 0, &welcome));
        let Some(req_frame) = read_one_frame(&mut c2) else {
            return;
        };
        let req: Value = serde_json::from_slice(&req_frame.payload).unwrap();
        let reply = json!({
            "req": req["req"],
            "ok": true,
            "env": [
                {"name": "PATH", "value": "/usr/bin:/bin", "source": "caller"},
                {"name": "BASE_ONLY", "value": "from-base", "source": "caller"},
                {"name": "PATH", "value": "/opt/worktree/bin:/usr/bin:/bin", "source": "request"},
            ],
        });
        let _ = c2.write_all(&frame::encode_json(ty::REPLY, 0, &reply));
    });

    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");

    let c = Client::new(Options {
        namespace: ns.to_owned(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        c.session_env("agent-x", "PATH"),
        "/opt/worktree/bin:/usr/bin:/bin"
    );
    drop(ln);
}

#[test]
fn test_respawn_pane_preserves_base_and_request_env() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-resp-");
    let ns = "agents";
    let sock_path = dir.path().join(format!("termd-{ns}.sock"));
    let ln = UnixListener::bind(&sock_path).unwrap();
    let ln_clone = ln.try_clone().unwrap();

    let work_dir = ShortDir::new("gmxt-work-");
    let work_dir_str = work_dir.path().to_string_lossy().into_owned();
    let (tx, rx) = mpsc::sync_channel(1);

    std::thread::spawn(move || {
        for _ in 0..4 {
            let Ok((mut conn, _)) = ln_clone.accept() else {
                return;
            };
            if read_one_frame(&mut conn).is_none() {
                continue;
            }
            let welcome = json!({
                "proto": [1, 5],
                "daemon": "g3d-termd/mock",
                "engine": "mock",
                "ns": ns,
                "pid": std::process::id(),
                "features": ["status", "env"],
                "max_frame": frame::DEFAULT_MAX_FRAME,
                "instance": "inst-mock",
            });
            let _ = conn.write_all(&frame::encode_json(ty::WELCOME, 0, &welcome));
            let Some(req_frame) = read_one_frame(&mut conn) else {
                continue;
            };
            let req: Value = serde_json::from_slice(&req_frame.payload).unwrap();
            let reply = match req["op"].as_str() {
                Some("status") => json!({
                    "req": req["req"],
                    "ok": true,
                    "status": {
                        "daemon": {"ns": ns},
                        "terminals": [
                            {"id": "gomux-bus", "handle": 1, "pid": 4321, "cwd": work_dir_str},
                        ],
                    },
                }),
                Some("env") => json!({
                    "req": req["req"],
                    "ok": true,
                    "env": [
                        {"name": "AQUA_CONTEXT", "value": "kept-from-gui", "source": "caller"},
                        {"name": "AGENT_HOST", "value": "mac", "source": "request"},
                        {"name": "G3D_TERMD_ID", "value": "gomux-bus", "source": "termd"},
                    ],
                }),
                Some("kill") => json!({"req": req["req"], "ok": true}),
                Some("create") => {
                    let _ = tx.send(req.clone());
                    json!({"req": req["req"], "ok": true})
                }
                _ => json!({"req": req["req"], "ok": true}),
            };
            let _ = conn.write_all(&frame::encode_json(ty::REPLY, 0, &reply));
        }
    });

    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");
    env.set("SSH_ONLY_VAR", "from-ssh-caller");

    let c = Client::new(Options {
        namespace: ns.to_owned(),
        ..Default::default()
    })
    .unwrap();
    c.respawn_pane("gomux-bus").unwrap();

    let created = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let env_base = created["env_base"].as_object().unwrap();
    assert_eq!(
        env_base.get("AQUA_CONTEXT").and_then(Value::as_str),
        Some("kept-from-gui")
    );
    assert!(
        !env_base.contains_key("SSH_ONLY_VAR"),
        "env_base replaced with SSH caller's environment: {env_base:?}"
    );
    assert!(
        !env_base.contains_key("G3D_TERMD_ID"),
        "env_base leaked layer-6 G3D_TERMD_ID: {env_base:?}"
    );
    let req_env = created["env"].as_object().unwrap();
    assert_eq!(
        req_env.get("AGENT_HOST").and_then(Value::as_str),
        Some("mac")
    );
    drop(ln);
}

// --- Ported tests from internal/termdwire/termdwire_test.go ---

#[test]
fn test_socket_dir_and_paths() {
    let mut env = EnvGuard::lock();

    assert_eq!(
        g3d::paths::socket_dir_for(Some(PathBuf::from("/run/user/1000")), 1000),
        PathBuf::from("/run/user/1000/g3d")
    );
    assert_eq!(
        g3d::paths::socket_dir_for(None, 501),
        PathBuf::from("/tmp/g3d-501")
    );

    env.remove("G3D_TERMD_DIR");
    env.set("XDG_RUNTIME_DIR", "/nonexistent/runtime");
    env.set("TMPDIR", "/nonexistent/tmp");
    let desktop = socket_dir();
    env.remove("XDG_RUNTIME_DIR");
    env.remove("TMPDIR");
    let stripped = socket_dir();
    assert_eq!(
        desktop, stripped,
        "socket_dir varied with XDG_RUNTIME_DIR/TMPDIR"
    );

    env.set("G3D_TERMD_DIR", "/var/tmp/custom-g3d");
    assert_eq!(
        socket_path("agents"),
        PathBuf::from("/var/tmp/custom-g3d/termd-agents.sock")
    );
    assert_eq!(
        log_path("agents"),
        PathBuf::from("/var/tmp/custom-g3d/termd-agents.log")
    );

    env.remove("G3D_TERMD_DIR");
    env.set("XDG_STATE_HOME", "/var/tmp/xdg-state");
    assert_eq!(
        log_path("agents"),
        PathBuf::from("/var/tmp/xdg-state/g3d/termd-agents.log")
    );

    env.remove("XDG_STATE_HOME");
    env.set("HOME", "/var/tmp/fakehome");
    assert_eq!(
        log_path("agents"),
        PathBuf::from("/var/tmp/fakehome/.local/state/g3d/termd-agents.log")
    );
}

#[test]
fn test_daemon_env() {
    let input = [
        "PATH=/usr/bin:/bin",
        "G3D_TERMD_DIR=/var/tmp/g3d-keep",
        "G3D_TERMD_SOCKET=/var/tmp/g3d-keep/termd-wall.sock",
        "G3D_TERMD_ID=pane-1",
        "G3D_RELAY_TOKEN=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "G3D_RELAY_PORT=8080",
        "KEEP_VAR=G3D_RELAY_TOKEN=value_not_key",
    ];
    let got = daemon_env(input);
    let want = vec![
        "PATH=/usr/bin:/bin".to_owned(),
        "G3D_TERMD_DIR=/var/tmp/g3d-keep".to_owned(),
        "KEEP_VAR=G3D_RELAY_TOKEN=value_not_key".to_owned(),
    ];
    assert_eq!(got, want);
}

#[test]
fn test_security_rejects_0755_socket_dir() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("tw755-");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let ns = "sec755";
    let sock_path = dir.path().join(format!("termd-{ns}.sock"));
    let (_ln, got_hello) = start_mock_welcome_server(&sock_path, ns);

    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");

    assert!(is_unsafe_socket(&check_socket_dir(dir.path()).unwrap_err()));
    assert!(is_unsafe_socket(&dial_namespace(ns).err().unwrap()));
    assert!(is_unsafe_socket(&stop_socket(&sock_path).unwrap_err()));

    let marker = dir.path().join("spawned");
    let fake_bin = dir.path().join("fake-termd");
    std::fs::write(
        &fake_bin,
        format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&fake_bin, std::fs::Permissions::from_mode(0o700)).unwrap();

    let start = Instant::now();
    let err = ensure_daemon_with_bin(ns, &fake_bin).err().unwrap();
    assert!(is_unsafe_socket(&err), "ensure_daemon(0755) = {err}");
    assert!(start.elapsed() <= Duration::from_secs(1));
    assert!(
        !marker.exists(),
        "ensure_daemon executed binary despite 0755 socket dir"
    );
    assert!(
        got_hello.try_recv().is_err(),
        "connected to socket inside 0755 dir"
    );
}

#[test]
fn test_security_rejects_symlink_socket_dir_and_missing_is_not_running() {
    let mut env = EnvGuard::lock();
    let real_dir = ShortDir::new("twreal-");
    let parent = ShortDir::new("twlnk-");
    let link_dir = parent.path().join("socklink");
    std::os::unix::fs::symlink(real_dir.path(), &link_dir).unwrap();
    let dangling_dir = parent.path().join("dangling");
    std::os::unix::fs::symlink(parent.path().join("does-not-exist"), &dangling_dir).unwrap();

    let ns = "seclnk";
    let (_ln, got_hello) =
        start_mock_welcome_server(&real_dir.path().join(format!("termd-{ns}.sock")), ns);
    let fake_bin = parent.path().join("never-run");

    let candidates = [
        link_dir.to_string_lossy().into_owned(),
        format!("{}/", link_dir.display()),
        dangling_dir.to_string_lossy().into_owned(),
    ];
    for candidate in &candidates {
        env.set("G3D_TERMD_DIR", candidate);
        env.set(TEST_SERVER_ENV, candidate);
        env.remove("G3D_TERMD_SOCKET");
        assert!(is_unsafe_socket(
            &check_socket_dir(Path::new(candidate)).unwrap_err()
        ));
        assert!(is_unsafe_socket(&dial_namespace(ns).err().unwrap()));
        assert!(is_unsafe_socket(
            &ensure_daemon_with_bin(ns, &fake_bin).err().unwrap()
        ));
    }
    assert!(
        got_hello.try_recv().is_err(),
        "connected through symlinked socket directory"
    );

    // Missing directory must report NotFound -> NotRunningError (not UnsafeSocketError).
    let missing_dir = parent.path().join("missing-subdir");
    let err = check_socket_dir(&missing_dir).unwrap_err();
    assert!(!is_unsafe_socket(&err));
    assert!(err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .is_some_and(|ioe| ioe.kind() == std::io::ErrorKind::NotFound)
    }));

    env.set("G3D_TERMD_DIR", &missing_dir);
    env.set(TEST_SERVER_ENV, &missing_dir);
    assert!(is_not_running(&dial_namespace(ns).err().unwrap()));
}

#[test]
fn test_security_accepts_0700_same_uid_and_verifies_peer_creds() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("twok-");
    let ns = "secok";
    let sock_path = dir.path().join(format!("termd-{ns}.sock"));
    let (_ln, got_hello) = start_mock_welcome_server(&sock_path, ns);

    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");

    let mut c = dial_namespace(ns).unwrap();
    assert_eq!(c.welcome.daemon, "g3d-termd/test");
    assert_eq!(c.welcome.instance.as_deref(), Some("inst-test"));
    let hello = got_hello.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(hello["client"].as_str(), Some("gomuxai"));

    let reply: Value = c.request(g3d_wire::Op::Ping).unwrap();
    assert_eq!(reply["op"].as_str(), Some("ping"));
    drop(c);

    // stop_socket on non-existent socket returns Ok(()).
    stop_socket(dir.path().join("termd-absent.sock")).unwrap();

    // verify_peer_creds and verify_peer_creds_for_uid(uid + 1) on live Unix socket.
    let mismatch_sock = dir.path().join("peercred.sock");
    let peer_ln = UnixListener::bind(&mismatch_sock).unwrap();
    let peer_ln_clone = peer_ln.try_clone().unwrap();
    let accept_handle = std::thread::spawn(move || peer_ln_clone.accept().ok());
    let raw_conn = UnixStream::connect(&mismatch_sock).unwrap();

    verify_peer_creds(&raw_conn).unwrap();
    let uid = rustix::process::getuid().as_raw();
    let err = verify_peer_creds_for_uid(&raw_conn, uid.wrapping_add(1)).unwrap_err();
    assert!(is_unsafe_socket(&err));
    drop(raw_conn);
    drop(accept_handle.join());
    drop(peer_ln);
}

#[test]
fn test_handshake_early_brain_failed() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("twerr-");
    let ns = "earlyerr";
    let sock_path = dir.path().join(format!("termd-{ns}.sock"));
    let ln = UnixListener::bind(&sock_path).unwrap();
    let ln_clone = ln.try_clone().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut conn, _)) = ln_clone.accept() {
            let err_payload = WireError::new("brain_failed", "brain crashed");
            let _ = conn.write_all(&frame::encode_json(ty::ERROR, 0, &err_payload));
        }
    });

    env.set("G3D_TERMD_DIR", dir.path());
    env.set(TEST_SERVER_ENV, dir.path());
    env.remove("G3D_TERMD_SOCKET");

    let err = dial_namespace(ns).err().unwrap();
    assert!(
        is_wire_code(&err, "brain_failed"),
        "dial(early ERROR) = {err}"
    );
    drop(ln);
}

#[test]
fn test_security_ensure_daemon_strips_relay_and_termd_vars() {
    let mut env = EnvGuard::lock();
    let base = ShortDir::new("twenv-");
    let sock_dir = base.path().join("run");
    let ns = "envscrub";
    let env_out = base.path().join("daemon.env");
    let fake_daemon = base.path().join("fake-g3d-termd");

    // Dump environment to `env_out`; ensure_daemon will fail fast once fake_daemon exits,
    // after we verify the spawned child's environment was scrubbed.
    let script = format!(
        "#!/bin/sh\nmkdir -m 0700 -p '{}'\nenv > '{}'\nexit 1\n",
        sock_dir.display(),
        env_out.display()
    );
    std::fs::write(&fake_daemon, script).unwrap();
    std::fs::set_permissions(&fake_daemon, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Start a thread that brings up the mock welcome socket right after fake_daemon creates sock_dir,
    // so ensure_daemon succeeds immediately without waiting the full 3s timeout!
    let sock_dir_bg = sock_dir.clone();
    let sock_path_bg = sock_dir.join(format!("termd-{ns}.sock"));
    let server_thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !sock_dir_bg.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        if !sock_dir_bg.exists() {
            return None;
        }
        let (ln, _rx) = start_mock_welcome_server(&sock_path_bg, ns);
        Some(ln)
    });

    let secret = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef";
    env.set("G3D_TERMD_DIR", &sock_dir);
    env.set(TEST_SERVER_ENV, &sock_dir);
    env.set("G3D_RELAY_TOKEN", secret);
    env.set("G3D_RELAY_EXTRA", "relay-control");
    env.set("G3D_TERMD_SOCKET", "/var/tmp/outer.sock");
    env.set("G3D_TERMD_ID", "outer-pane");
    env.set("G3D_TEST_KEEP_MARKER", "kept-ok");

    let c = ensure_daemon_with_bin(ns, &fake_daemon).unwrap();
    drop(c);
    let _ln = server_thread.join().unwrap();

    let env_dump = std::fs::read_to_string(&env_out).unwrap();
    for forbidden in [
        "G3D_RELAY_TOKEN=",
        secret,
        "G3D_RELAY_EXTRA=",
        "G3D_TERMD_SOCKET=",
        "G3D_TERMD_ID=",
    ] {
        assert!(
            !env_dump.contains(forbidden),
            "spawned daemon inherited {forbidden:?}:\n{env_dump}"
        );
    }
    assert!(
        env_dump.contains("G3D_TEST_KEEP_MARKER=kept-ok"),
        "env dump:\n{env_dump}"
    );
    assert!(
        env_dump.contains(&format!("G3D_TERMD_DIR={}", sock_dir.display())),
        "env dump:\n{env_dump}"
    );
}

#[test]
fn test_security_read_owned_regular_file_rejects_symlink_and_fifo() {
    let dir = ShortDir::new("twlog-");
    let reg = dir.path().join("real.log");
    std::fs::write(&reg, b"alpha\nbeta\ngamma\n").unwrap();

    let got = read_owned_regular_file(&reg, 64 << 10).unwrap();
    assert_eq!(got, b"alpha\nbeta\ngamma\n");
    let tail = read_owned_regular_file(&reg, 6).unwrap();
    assert_eq!(tail, b"gamma\n");

    let link = dir.path().join("symlink.log");
    std::os::unix::fs::symlink(&reg, &link).unwrap();
    assert!(read_owned_regular_file(&link, 64 << 10).is_err());

    let fifo = dir.path().join("pipe.log");
    let c_fifo = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
    // SAFETY: `c_fifo` is a valid NUL-terminated path inside our private temp dir.
    let rc = unsafe { libc::mkfifo(c_fifo.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo failed");

    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(read_owned_regular_file(&fifo, 64 << 10));
    });
    let res = rx
        .recv_timeout(Duration::from_secs(1))
        .expect("read_owned_regular_file blocked on FIFO without a writer");
    assert!(res.is_err(), "read_owned_regular_file(fifo) succeeded");
}

// --- Regression tests for rs/reports/review-termd-claudereg.md ---

/// Points the guard and `G3D_TERMD_DIR` at `dir`, as `testing::setup` does.
fn use_private_dir(env: &mut EnvGuard, dir: &Path) {
    env.set("G3D_TERMD_DIR", dir);
    env.set(TEST_SERVER_ENV, dir);
    env.remove("G3D_TERMD_SOCKET");
}

fn write_script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}

/// `{:#}` (how the CLI prints errors) gives Go's `%v` text, each cause once.
#[test]
fn test_wrapped_errors_print_their_cause_once() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-wrap-");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    use_private_dir(&mut env, dir.path());
    let ns = "wrap";
    let sock = dir.path().join(format!("termd-{ns}.sock"));
    let cause = format!(
        "unsafe termd socket: socket directory {} has unsafe mode 0755 (group/other access must be 0)",
        dir.path().display()
    );

    let err = stop_socket(&sock).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!("termd at {}: {cause}", sock.display())
    );
    assert!(is_unsafe_socket(&err), "the cause left the chain: {err:#}");

    // A wrap of a wrap.
    let c = Client::new(Options {
        namespace: ns.to_owned(),
        ..Default::default()
    })
    .unwrap();
    let err = c.capture("x", 0).unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!("capturing pane: termd at {}: {cause}", sock.display())
    );
    assert!(is_unsafe_socket(&err));
}

/// A daemon that is alive but doesn't accept (hung, SIGSTOP) must not hang
/// the dial: Go's `net.DialTimeout(..., 2s)`.
#[test]
fn test_dial_gives_up_on_a_daemon_that_does_not_accept() {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-full-");
    let ns = "full";
    let sock = dir.path().join(format!("termd-{ns}.sock"));
    let listener = rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None).unwrap();
    rustix::net::bind(&listener, &SocketAddrUnix::new(&sock).unwrap()).unwrap();
    rustix::net::listen(&listener, 0).unwrap();
    use_private_dir(&mut env, dir.path());

    // Fill the accept queue; nothing ever accepts.
    let mut held = Vec::new();
    for _ in 0..16 {
        match connect_unix(&sock, Duration::from_millis(200)) {
            Ok(s) => held.push(s),
            Err(_) => break,
        }
    }

    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    std::thread::spawn(move || {
        let _ = tx.send(dial_namespace(ns).map(|_| ()));
    });
    let res = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("dial hung on a daemon that doesn't accept");
    let err = res.expect_err("dial succeeded with nothing accepting");
    assert!(
        start.elapsed() <= Duration::from_secs(3),
        "dial took {:?}",
        start.elapsed()
    );
    assert!(
        !is_not_running(&err),
        "a full queue isn't 'not running': {err:#}"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        format!("{err:#}"),
        format!(
            "termd at {0}: dial unix {0}: connect: resource temporarily unavailable",
            sock.display()
        )
    );
    drop(held);
}

/// Go's `exec.LookPath` refuses a `g3d-termd` found through a relative `PATH`
/// entry (`exec.ErrDot`), and `findTool` then goes to the install dirs.
#[test]
fn test_daemon_binary_refuses_relative_path_entries() {
    let mut env = EnvGuard::lock();
    let base = ShortDir::new("gmxt-path-");
    let make_tool = |dir: &Path| {
        std::fs::create_dir_all(dir).unwrap();
        write_script(&dir.join("g3d-termd"), "exit 0");
    };
    let rel_target = base.path().join("rel").join("bin");
    let abs_dir = base.path().join("abs").join("bin");
    let home = base.path().join("home");
    make_tool(&rel_target);
    make_tool(&abs_dir);
    make_tool(&home.join(".local").join("bin"));

    // `rel_target` spelled relative to the current directory.
    let cwd = std::env::current_dir().unwrap();
    let ups = cwd.components().count() - 1;
    let rel: PathBuf = std::iter::repeat_n("..", ups)
        .collect::<PathBuf>()
        .join(rel_target.strip_prefix("/").unwrap());
    assert!(
        rel.is_relative() && rel.join("g3d-termd").exists(),
        "{rel:?}"
    );

    env.remove("G3D_TERMD_BIN");
    env.set("HOME", &home);
    env.set("PATH", std::env::join_paths([&rel, &abs_dir]).unwrap());
    assert_eq!(
        daemon_binary("").unwrap(),
        home.join(".local").join("bin").join("g3d-termd"),
        "a relative PATH entry was used, or the search went on past it"
    );

    env.set("PATH", std::env::join_paths([&abs_dir, &rel]).unwrap());
    assert_eq!(daemon_binary("").unwrap(), abs_dir.join("g3d-termd"));
}

/// The test-binary decision, on this executable and on made-up layouts.
#[test]
fn test_is_cargo_test_exe() {
    let exe = std::env::current_exe().unwrap();
    assert!(
        is_cargo_test_exe(&exe),
        "this test executable isn't recognised: {exe:?}"
    );

    let base = ShortDir::new("gmxt-exe-");
    let profile = base.path().join("target").join("debug");
    std::fs::create_dir_all(profile.join(".fingerprint")).unwrap();
    std::fs::create_dir_all(profile.join("deps")).unwrap();
    for test_exe in [
        "agent_service-0123456789abcdef",
        "agent-0123456789abcdef (deleted)",
    ] {
        assert!(
            is_cargo_test_exe(&profile.join("deps").join(test_exe)),
            "{test_exe}"
        );
    }
    let vendor_deps = base.path().join("vendor").join("deps");
    std::fs::create_dir_all(&vendor_deps).unwrap();
    for real in [
        profile.join("agent"),
        profile.join("deps").join("agent"),
        profile.join("deps").join("agent-0123456789ABCDEF"),
        profile.join("deps").join("agent-0123abcd"),
        vendor_deps.join("tool-0123456789abcdef"),
        base.path()
            .join("home")
            .join(".local")
            .join("bin")
            .join("agent"),
        base.path()
            .join("home")
            .join(".cargo")
            .join("bin")
            .join("agent"),
        PathBuf::from("/usr/local/bin/agent"),
    ] {
        assert!(
            !is_cargo_test_exe(&real),
            "{real:?} taken for a test binary"
        );
    }
}

/// A test can't name the user's real socket directory as its private one.
#[test]
fn test_check_test_server_refuses_the_live_socket_dir() {
    let mut env = EnvGuard::lock();
    let live = g3d::paths::socket_dir_for(
        g3d::paths::user_runtime_dir(),
        rustix::process::getuid().as_raw(),
    );
    use_private_dir(&mut env, &live);
    let err = check_test_server().unwrap_err();
    assert!(err.to_string().contains("real socket directory"), "{err:#}");
    let err = Client::new(Options::default()).unwrap_err();
    assert!(err.to_string().contains("real socket directory"), "{err:#}");
}

/// Every connection (stop_socket, the daemon start) stays inside the
/// private dir, and needs one.
#[test]
fn test_dial_refuses_a_socket_outside_the_private_dir() {
    let mut env = EnvGuard::lock();
    let private = ShortDir::new("gmxt-priv-");
    let other = ShortDir::new("gmxt-othr-");
    let ns = "outside";
    let other_sock = other.path().join(format!("termd-{ns}.sock"));
    let (_ln, got_hello) = start_mock_welcome_server(&other_sock, ns);
    use_private_dir(&mut env, private.path());

    let err = stop_socket(&other_sock).unwrap_err();
    assert!(
        format!("{err:#}").contains("outside its private socket directory"),
        "{err:#}"
    );

    let marker = private.path().join("spawned");
    let fake_bin = private.path().join("fake-termd");
    write_script(&fake_bin, &format!("touch '{}'", marker.display()));
    env.set("G3D_TERMD_DIR", other.path());
    let err = ensure_daemon_with_bin(ns, &fake_bin).err().unwrap();
    assert!(
        format!("{err:#}").contains("outside its private socket directory"),
        "{err:#}"
    );
    assert!(!marker.exists(), "the daemon start ran despite the guard");

    env.remove(TEST_SERVER_ENV);
    let err = stop_socket(&other_sock).unwrap_err();
    assert!(
        format!("{err:#}").contains("without a private socket directory"),
        "{err:#}"
    );
    assert!(
        got_hello.try_recv().is_err(),
        "connected to a socket outside the private dir"
    );
}

/// Go's `termd: no reply to <op> in 30s`, not the OS's EAGAIN text.
#[test]
fn test_request_without_reply_says_no_reply() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-mute-");
    let ns = "mute";
    let ln = UnixListener::bind(dir.path().join(format!("termd-{ns}.sock"))).unwrap();
    std::thread::spawn(move || {
        let Ok((mut conn, _)) = ln.accept() else {
            return;
        };
        if read_one_frame(&mut conn).is_none() {
            return;
        }
        let welcome = json!({"proto": [1, 5], "daemon": "g3d-termd/mute", "engine": "mock",
            "features": [], "max_frame": frame::DEFAULT_MAX_FRAME});
        let _ = conn.write_all(&frame::encode_json(ty::WELCOME, 0, &welcome));
        while read_one_frame(&mut conn).is_some() {}
    });
    use_private_dir(&mut env, dir.path());

    let mut c = dial_namespace(ns).unwrap();
    c.set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    let err = request_raw(&mut c, g3d_wire::Op::Ping).unwrap_err();
    assert_eq!(format!("{err:#}"), "termd: no reply to ping in 30s");
}

/// Go's `%v` of `cmd.CombinedOutput()`'s error in the daemon start failure.
#[test]
fn test_daemon_start_failure_text() {
    let status = |script: &str| {
        std::process::Command::new("/bin/sh")
            .args(["-c", script])
            .status()
            .unwrap()
    };
    assert_eq!(go_exit_text(status("exit 0")), "<nil>");
    assert_eq!(go_exit_text(status("exit 1")), "exit status 1");
    assert_eq!(go_exit_text(status("kill -9 $$")), "signal: killed");

    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-fail-");
    use_private_dir(&mut env, dir.path());
    let fake_bin = dir.path().join("fake-termd");
    write_script(&fake_bin, "echo boom\nexit 3");
    let err = ensure_daemon_with_bin("quiet", &fake_bin).err().unwrap();
    let text = format!("{err:#}");
    assert!(
        text.starts_with(&format!(
            "termd did not come up for namespace \"quiet\" (termd is not running at {}); \
             {} --ns quiet --daemon: exit status 3 boom; log ",
            socket_path("quiet").display(),
            fake_bin.display()
        )),
        "{text}"
    );
}

/// Names in errors are quoted as Go's `%q` quotes them.
#[test]
fn test_errors_quote_names_as_go_does() {
    let mut env = EnvGuard::lock();
    let dir = ShortDir::new("gmxt-qt-");
    use_private_dir(&mut env, dir.path());

    let err = Client::new(Options {
        namespace: "bad\x01ns".to_owned(),
        ..Default::default()
    })
    .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "termd namespace \"bad\\x01ns\" must be 1 to 64 bytes of [A-Za-z0-9._-]"
    );
    let err = Client::new(Options {
        namespace: "wall".to_owned(),
        ..Default::default()
    })
    .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "termd namespace \"wall\" is reserved for interactive and wall sessions; \
         use a dedicated namespace such as \"agents\""
    );

    let c = Client::new(Options {
        namespace: "quote".to_owned(),
        ..Default::default()
    })
    .unwrap();
    let err = c.attach_command("bad\x01id").unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        "terminal id \"bad\\x01id\" must be 1 to 64 bytes of [A-Za-z0-9._-]"
    );
    let err = c.kill_session("it's\x1b").unwrap_err();
    assert_eq!(format!("{err:#}"), "session \"it's\\x1b\" not found");
}
