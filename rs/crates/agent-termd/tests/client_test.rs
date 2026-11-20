//! Integration tests for `agent-termd` against a real `g3d-termd` in a private
//! `0700` socket directory under `/var/tmp`.
//! Ported from `tests/termd/client_test.go`.

use agent_termd::testing::{SharedGuard, TestClient, new_test_client, require};
use agent_termd::{Client, DEFAULT_NAMESPACE, Options, attach_binary};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static SEQ: AtomicU64 = AtomicU64::new(1);

fn temp_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("termd-it-")
        .tempdir_in("/var/tmp")
        .or_else(|_| tempfile::tempdir())
        .expect("tempdir")
}

fn unique_session(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos() as u64);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}-{nanos}-{n}")
}

fn await_until(desc: &str, mut cond: impl FnMut() -> (String, bool)) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last;
    loop {
        let (state, ok) = cond();
        if ok {
            return state;
        }
        last = state;
        if Instant::now() >= deadline {
            panic!("timed out waiting for {desc}; last state:\n{last}");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn wait_line(c: &Client, session: &str, want: &str) -> String {
    await_until(&format!("line {want:?} on {session}"), || {
        match c.capture(session, 50) {
            Ok(out) => {
                let ok = out.lines().any(|line| line.trim() == want);
                (out, ok)
            }
            Err(err) => (format!("capture error: {err}"), false),
        }
    })
}

struct SessionGuard<'a> {
    client: &'a Client,
    name: String,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let _ = self.client.kill_session(&self.name);
    }
}

fn new_shell<'a>(c: &'a Client, prefix: &str) -> SessionGuard<'a> {
    let sess = unique_session(prefix);
    c.create_session(&sess, &HashMap::new())
        .expect("create_session");
    let guard = SessionGuard {
        client: c,
        name: sess,
    };
    c.send_keys(&guard.name, "echo ready-$((40+2))")
        .expect("send_keys");
    c.send_special_key(&guard.name, "Enter")
        .expect("send_special_key Enter");
    wait_line(c, &guard.name, "ready-42");
    guard
}

#[test]
fn test_namespace_refuses_default_and_wall() {
    let _srv: SharedGuard = require();
    for reserved in ["default", "wall"] {
        assert!(
            Client::new(Options {
                namespace: reserved.to_owned(),
                ..Default::default()
            })
            .is_err(),
            "Client::new({reserved:?}) succeeded; must refuse reserved namespaces"
        );
    }
}

#[test]
fn test_session_lifecycle_status_and_process_tree_kill() {
    let _srv = require();
    let c: TestClient = new_test_client("agents-life");

    // Empty before daemon starts: no error, zero sessions.
    let sessions = c.list_sessions().unwrap();
    assert!(
        sessions.is_empty(),
        "initial list_sessions = {sessions:?}, want empty"
    );

    let dir = temp_dir();
    let dir_str = dir.path().to_string_lossy().into_owned();
    let sess = unique_session("agent-w");
    let env = HashMap::from([
        ("AGENT_NAME".to_owned(), "w".to_owned()),
        ("CUSTOM_KEY".to_owned(), "alpha-beta".to_owned()),
    ]);
    c.create_session_in(&sess, &dir_str, &env).unwrap();

    // Duplicate creation must fail naming "already exists".
    let dup_err = c.create_session(&sess, &HashMap::new()).unwrap_err();
    assert!(
        dup_err.to_string().contains("already exists"),
        "duplicate create_session = {dup_err}, want already exists"
    );

    assert!(c.session_exists(&sess).unwrap());

    // Status document reports daemon + terminal + pid.
    let st = c.status().unwrap();
    assert_eq!(st.daemon.ns, "agents-life");
    assert_eq!(st.terminals.len(), 1);
    assert_eq!(st.terminals[0].id, sess);

    let pane = c.first_pane(&sess).unwrap();
    assert!(pane.pid > 0, "first_pane = {pane:?}");

    let pids = c.pane_pids().unwrap();
    assert_eq!(pids.get(&pane.pid).map(String::as_str), Some(sess.as_str()));
    assert_eq!(c.session_env(&sess, "CUSTOM_KEY"), "alpha-beta");

    // Verify env and cwd inside the running shell, and start a child process
    // so kill_session proves termd's process-tree close rule.
    let pid_file = dir.path().join("child.pid");
    let cmd = format!(
        r#"echo "ENV=[$CUSTOM_KEY]"; pwd -P; sleep 300 & echo $! > {}; echo child-$((40+2))"#,
        pid_file.display()
    );
    c.send_keys(&sess, &cmd).unwrap();
    c.send_special_key(&sess, "Enter").unwrap();
    let out = wait_line(&c, &sess, "child-42");
    assert!(
        out.contains("ENV=[alpha-beta]"),
        "shell missing CUSTOM_KEY:\n{out}"
    );
    let real_dir = std::fs::canonicalize(dir.path()).unwrap();
    assert!(
        out.contains(&real_dir.to_string_lossy().into_owned()),
        "shell cwd missing {real_dir:?}:\n{out}"
    );

    let raw_pid = std::fs::read_to_string(&pid_file).unwrap();
    let child_pid: i32 = raw_pid.trim().parse().expect("valid child pid");
    assert!(child_pid > 0);

    c.kill_session(&sess).unwrap();
    assert!(
        !c.session_exists(&sess).unwrap(),
        "session still exists after kill_session"
    );

    // Both shell and background sleep child must be gone.
    await_until("child sleep killed by termd close rule", || {
        // SAFETY: signal 0 only probes process existence.
        let rc = unsafe { libc::kill(child_pid, 0) };
        if rc != 0 {
            ("gone".to_owned(), true)
        } else {
            (format!("pid {child_pid} still alive"), false)
        }
    });
}

#[test]
fn test_literal_send_keys_and_special_keys() {
    let _srv = require();
    let c = new_test_client(DEFAULT_NAMESPACE);
    let sess = new_shell(&c, "keys");

    // C-c cancels a half-typed command.
    c.send_keys(&sess.name, "echo never-''runs").unwrap();
    c.send_special_key(&sess.name, "C-c").unwrap();
    c.send_keys(&sess.name, "echo after-$((40+2))").unwrap();
    c.send_special_key(&sess.name, "Enter").unwrap();
    let screen = wait_line(&c, &sess.name, "after-42");
    assert!(
        !screen.contains("\nnever-runs\n"),
        "C-c did not cancel line:\n{screen}"
    );

    // Literal strings that look like tmux flags or key names arrive verbatim.
    c.send_keys(&sess.name, "echo cat-$((40+2)); cat -v")
        .unwrap();
    c.send_special_key(&sess.name, "Enter").unwrap();
    wait_line(&c, &sess.name, "cat-42");

    for lit in ["Escape", "C-c", "-R", "--"] {
        c.send_keys(&sess.name, lit).unwrap();
        c.send_special_key(&sess.name, "Enter").unwrap();
    }
    c.send_special_key(&sess.name, "-N2").unwrap();
    c.send_special_key(&sess.name, "Enter").unwrap();
    wait_line(&c, &sess.name, "-N2");
}

#[test]
fn test_large_bracketed_paste_and_capture_styled() {
    let _srv = require();
    let c = new_test_client(DEFAULT_NAMESPACE);
    let sess = new_shell(&c, "paste");

    // 66 KB paste (over 60x macOS's 1024-byte PTY MAX_CANON) with embedded
    // bracket-end sequence stripped so it arrives as one bracketed paste.
    let text = format!(
        "{}\x1b[201~{}\nlast line",
        "0123456789".repeat(3300),
        "abcdefghij".repeat(3300)
    );
    let stripped = text.replace("\x1b[201~", "");
    let want = format!("\x1b[200~{stripped}\x1b[201~");

    let dir = temp_dir();
    let out_path = dir.path().join("pasted.bin");
    c.send_keys(
        &sess.name,
        &format!(
            r#"stty raw -echo; printf '\033[?2004h'; echo RE''ADY; cat > {}"#,
            out_path.display()
        ),
    )
    .unwrap();
    c.send_special_key(&sess.name, "Enter").unwrap();
    await_until("app READY", || {
        let s = c.capture(&sess.name, 0).unwrap_or_default();
        let ok = s.contains("READY");
        (s, ok)
    });

    c.paste_text(&sess.name, &text).unwrap();
    await_until("full 66KB bracketed paste", || {
        let len = std::fs::metadata(&out_path).map_or(0, |m| m.len() as usize);
        (format!("{len}/{} bytes", want.len()), len >= want.len())
    });

    let raw_bytes = std::fs::read(&out_path).unwrap();
    assert_eq!(
        raw_bytes,
        want.as_bytes(),
        "got {} bytes, want {}",
        raw_bytes.len(),
        want.len()
    );

    // Styled capture preserves dim SGR with \n line separators.
    let sess2 = new_shell(&c, "styled");
    let box_cmd = r#"printf '────────────────────\n❯ \033[2mTry "fix lint"\033[0m\n────────────────────\nDONE-%d\n' $((40+2))"#;
    c.send_keys(&sess2.name, box_cmd).unwrap();
    c.send_special_key(&sess2.name, "Enter").unwrap();
    wait_line(&c, &sess2.name, "DONE-42");

    let plain = c.capture(&sess2.name, 0).unwrap();
    assert!(
        !plain.contains("\x1b["),
        "plain capture should not contain ANSI escapes: {plain:?}"
    );

    let styled = c.capture_styled(&sess2.name, 0).unwrap();
    assert!(
        !styled.contains("\r\n"),
        "capture_styled must normalize \\r\\n to \\n: {styled:?}"
    );
    let idx = styled.find("DONE-42").expect("DONE-42 in styled capture");
    let box_slice = &styled[..idx];
    assert!(
        box_slice.contains("\x1b[") && box_slice.contains(r#"Try "fix lint""#),
        "styled capture should preserve SGR around dim placeholder:\n{box_slice:?}"
    );
}

#[test]
fn test_respawn_preserves_env_and_attach_command() {
    let _srv = require();
    let c = new_test_client(DEFAULT_NAMESPACE);
    let sess_name = unique_session("respawn");
    let dir = temp_dir();
    let dir_str = dir.path().to_string_lossy().into_owned();
    let req_env = HashMap::from([
        ("AGENT_NAME".to_owned(), "respawn-agent".to_owned()),
        ("CUSTOM_KEY".to_owned(), "preserved-42".to_owned()),
    ]);
    c.create_session_in(&sess_name, &dir_str, &req_env).unwrap();
    let sess = SessionGuard {
        client: &c,
        name: sess_name,
    };

    // respawn_pane replaces the shell PID while keeping the session, cwd, and request env.
    let before = c.first_pane(&sess.name).unwrap();
    c.respawn_pane(&sess.name).unwrap();
    let after = c.first_pane(&sess.name).unwrap();
    assert_ne!(
        after.pid, before.pid,
        "respawn_pane kept same PID {}",
        after.pid
    );
    assert_eq!(c.session_env(&sess.name, "CUSTOM_KEY"), "preserved-42");

    c.send_keys(&sess.name, r#"echo "AFTER=[$AGENT_NAME:$CUSTOM_KEY]""#)
        .unwrap();
    c.send_special_key(&sess.name, "Enter").unwrap();
    wait_line(&c, &sess.name, "AFTER=[respawn-agent:preserved-42]");

    // attach_command builds g3d --ns <ns> attach --take <session>.
    if attach_binary("", "").is_ok() {
        let (bin, argv) = c.attach_command(&sess.name).unwrap();
        assert!(!bin.is_empty());
        assert_eq!(
            argv,
            vec![
                "g3d".to_owned(),
                "--ns".to_owned(),
                DEFAULT_NAMESPACE.to_owned(),
                "attach".to_owned(),
                "--take".to_owned(),
                sess.name.clone(),
            ]
        );
    }
}

// Go termdwire's TestFrameRoundTripAndMaxFrameUnsent. The Rust client frames
// through the g3d crates (g3d-wire's encoder and decoder, g3d::Client's
// send_request), so this checks them as agent-termd uses them: a frame
// round-trips, a length under the header is refused, and a request over the
// daemon's max_frame is refused before a byte of it reaches the real daemon,
// whose connection then goes on serving (a daemon given an oversized frame
// ends the whole connection).
#[test]
fn test_frame_round_trip_and_max_frame_unsent() {
    use g3d_wire::frame::{self, FrameDecoder, ty};

    let raw = frame::encode(ty::OUTPUT, 42, b"hello world");
    let mut dec = FrameDecoder::default();
    dec.feed(&raw);
    let f = dec
        .next_frame()
        .expect("a valid frame")
        .expect("a whole frame");
    assert_eq!(
        (f.ty, f.stream, f.payload.as_slice()),
        (ty::OUTPUT, 42, &b"hello world"[..])
    );

    // A frame length under 8 is refused.
    let mut bad_short = vec![0u8; 12];
    bad_short[0] = 4;
    let mut dec = FrameDecoder::default();
    dec.feed(&bad_short);
    assert!(
        dec.next_frame().is_err(),
        "the decoder took a frame length < 8"
    );

    // A payload over the decoder's max_frame is refused.
    let mut dec = FrameDecoder::new(1024);
    dec.feed(&frame::encode(ty::OUTPUT, 1, &[b'x'; 2048]));
    assert!(
        dec.next_frame().is_err(),
        "the decoder took a frame over max_frame"
    );

    // Against a real daemon in the private socket dir.
    let _srv = require();
    let c = new_test_client("agents-frame");
    let sess = new_shell(&c, "frame");
    let mut g = g3d::Client::connect(&agent_termd::socket_path("agents-frame"), "frame-test")
        .expect("connecting to the test daemon");
    assert_eq!(g.welcome.max_frame, frame::DEFAULT_MAX_FRAME as u64);
    for feature in ["key", "paste"] {
        assert!(
            g.welcome.features.iter().any(|f| f == feature),
            "the daemon lacks {feature}: {:?}",
            g.welcome.features
        );
    }
    assert!(!g.welcome.features.iter().any(|f| f == "no-such-feature"));

    let big = "a".repeat(frame::DEFAULT_MAX_FRAME);
    let err = g
        .send_request(g3d_wire::Op::Paste(g3d_wire::Paste {
            handle: 1,
            text: big,
            allow_unsafe: Some(true),
            more: None,
        }))
        .expect_err("a request over max_frame was taken");
    assert_eq!(err.code(), Some(g3d_wire::code::TOO_LARGE), "{err}");
    assert!(err.to_string().contains("max_frame"), "{err}");

    // Nothing of it was written: the same connection still answers.
    g.request_raw(g3d_wire::Op::List)
        .unwrap_or_else(|e| panic!("the connection after the refused request: {e}"));
    // And agent-termd's client, on connections of its own, still types.
    c.send_keys(&sess.name, "echo after-$((40+2))").unwrap();
    c.send_special_key(&sess.name, "Enter").unwrap();
    wait_line(&c, &sess.name, "after-42");
}
