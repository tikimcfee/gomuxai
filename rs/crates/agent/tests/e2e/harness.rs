//! What the end-to-end tests share: a host, which is the built `agent` binary
//! hard-linked into a temp dir under /var/tmp (its data dir, the .gomuxai
//! beside it, is the test's), run with a cleared environment:
//!
//! - PATH is a dir of the test's own, holding `sh` and the few tools the
//!   commands and panes need, and a stand-in `claude` (see FAKE_CLAUDE), so
//!   systemd-run is never found and no real Claude Code runs;
//! - HOME and CLAUDE_CONFIG_DIR are temp dirs: `agent hooks install` edits the
//!   stand-in settings, never the real ~/.claude;
//! - termd is a private daemon: the socket dir agent_termd::testing keeps for
//!   this test binary (G3D_TERMD_DIR, and GOMUXAI_TEST_TERMD_DIR, which makes
//!   the child refuse any socket outside it), in a namespace of the test's own
//!   (GOMUXAI_TERMD_NS), whose daemon stops when the host drops;
//! - a bus, when a test starts one, listens on random ports, and is stopped
//!   when the host drops.
//!
//! A termd pane doesn't get G3D_TERMD_DIR (the daemon gives its terminals
//! G3D_TERMD_SOCKET instead). An `agent` run in a pane (the bus's `agent bus
//! run`, the hook a stand-in Claude runs) still has GOMUXAI_TEST_TERMD_DIR,
//! and agent-termd takes a G3D_TERMD_SOCKET in that private dir as the
//! test's: it uses the private dir, and no other.

#![allow(dead_code)]

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use agent_termd::testing::{self, SharedGuard, TestClient};

/// What a run printed and how it exited.
#[derive(Debug)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    /// Panics, saying what ran and what it printed, unless it exited 0.
    pub fn ok(self, what: &str) -> Out {
        assert_eq!(self.code, 0, "agent {what}: {self:?}");
        self
    }
}

/// A Claude Code stand-in. Like Claude Code, it records itself in the
/// session registry ($CLAUDE_CONFIG_DIR/sessions/<pid>.json: its pid, the
/// session it runs (--session-id, or --resume's), and its start time as
/// /proc gives it, which proves the entry is this process's), with no
/// status, so the hooks' reports stand. Then it is a shell (the same
/// process: exec), into which a test types the hook runs Claude Code would
/// make, with CLAUDE_PID=$$ as Claude Code sets it.
pub const FAKE_CLAUDE: &str = r#"#!/bin/sh
sid=
while [ $# -gt 0 ]; do
  case "$1" in --session-id|--resume) sid=$2; shift;; esac
  shift
done
set -- $(cat /proc/$$/stat)
mkdir -p "$CLAUDE_CONFIG_DIR/sessions"
printf '{"pid":%s,"sessionId":"%s","cwd":"%s","kind":"interactive","entrypoint":"cli","procStart":"%s"}\n' \
  $$ "$sid" "$PWD" "${22}" > "$CLAUDE_CONFIG_DIR/sessions/$$.json"
PS1='claude$ ' exec sh
"#;

/// Tools linked into PATH: what the commands run (ps for a pane's
/// processes, env for the bus session), and what the panes' scripts use.
const TOOLS: &[&str] = &[
    "sh", "cat", "env", "ps", "mkdir", "mv", "sleep", "printf", "rm",
];

static SEQ: AtomicUsize = AtomicUsize::new(0);

/// Whether the end-to-end tests can run here: a g3d-termd to drive.
pub fn termd_available() -> bool {
    testing::available()
}

/// Skips (returns from) a test when there is no g3d-termd.
#[macro_export]
macro_rules! require_termd {
    () => {
        if !$crate::harness::termd_available() {
            eprintln!("skipping: g3d-termd not available");
            return;
        }
    };
}

/// Skips (returns from) a test when agent-busd can't be built (no go).
#[macro_export]
macro_rules! require_busd {
    () => {
        if $crate::harness::busd_binary().is_none() {
            eprintln!("skipping: no agent-busd (go isn't installed)");
            return;
        }
    };
}

pub struct Host {
    /// The host's name (AGENT_HOST).
    pub name: String,
    /// The termd namespace its agents run in.
    pub ns: String,
    pub bin: PathBuf,
    termd_dir: PathBuf,
    /// Whether a test started this host's bus (stopped on drop).
    pub bus_started: std::cell::Cell<bool>,
    // Dropped in this order: the namespace's daemon stops, then the socket
    // dir's guard goes, then the files.
    _termd: TestClient,
    _guard: SharedGuard,
    pub root: tempfile::TempDir,
}

fn find_tool(name: &str) -> Option<PathBuf> {
    ["/usr/bin", "/bin", "/usr/local/bin"]
        .iter()
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

pub fn write_script(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

impl Host {
    /// A host named name, with the built binary in a temp dir of its own.
    pub fn new(name: &str) -> Host {
        let guard = testing::require();
        let termd_dir = guard.dir().to_path_buf();
        let root = tempfile::Builder::new()
            .prefix("agent-e2e-")
            .tempdir_in("/var/tmp")
            .unwrap();
        let bin_dir = root.path().join("bin");
        for d in ["bin", "path", "home", "claude/sessions", "work"] {
            std::fs::create_dir_all(root.path().join(d)).unwrap();
        }
        let bin = bin_dir.join("agent");
        let built = env!("CARGO_BIN_EXE_agent");
        if std::fs::hard_link(built, &bin).is_err() {
            std::fs::copy(built, &bin).unwrap();
        }
        let path = root.path().join("path");
        for tool in TOOLS {
            if let Some(p) = find_tool(tool) {
                std::os::unix::fs::symlink(p, path.join(tool)).unwrap();
            }
        }
        write_script(&path.join("claude"), FAKE_CLAUDE);
        let ns = format!(
            "e2e-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        Host {
            name: name.to_string(),
            _termd: testing::new_test_client(&ns),
            ns,
            bin,
            termd_dir,
            bus_started: std::cell::Cell::new(false),
            _guard: guard,
            root,
        }
    }

    pub fn work(&self) -> PathBuf {
        self.root.path().join("work")
    }

    pub fn data_dir(&self) -> PathBuf {
        self.root.path().join("bin/.gomuxai")
    }

    pub fn claude_dir(&self) -> PathBuf {
        self.root.path().join("claude")
    }

    pub fn termd_dir(&self) -> &Path {
        &self.termd_dir
    }

    /// The command for words, as a person runs it in work/.
    pub fn command(&self, words: &[&str]) -> Command {
        let mut c = Command::new(&self.bin);
        c.args(words)
            .current_dir(self.work())
            .env_clear()
            .env("PATH", self.root.path().join("path"))
            .env("HOME", self.root.path().join("home"))
            .env("CLAUDE_CONFIG_DIR", self.claude_dir())
            .env("AGENT_HOST", &self.name)
            .env("G3D_TERMD_DIR", &self.termd_dir)
            .env(agent_termd::TEST_SERVER_ENV, &self.termd_dir)
            .env("GOMUXAI_TERMD_NS", &self.ns)
            .env("TZ", "UTC")
            .env("TMPDIR", "/var/tmp")
            .env("LANG", "C.UTF-8");
        if let Ok(b) = agent_termd::daemon_binary("") {
            c.env("G3D_TERMD_BIN", b);
        }
        c
    }

    /// Runs agent with these words in work/, as a person (no AGENT_NAME),
    /// with nothing on stdin.
    pub fn run(&self, words: &[&str]) -> Out {
        self.run_with(words, &[], b"")
    }

    /// run, with more environment and stdin.
    pub fn run_with(&self, words: &[&str], env: &[(&str, &str)], stdin: &[u8]) -> Out {
        let mut c = self.command(words);
        c.envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().expect("running agent");
        {
            let mut i = child.stdin.take().unwrap();
            let _ = i.write_all(stdin);
        }
        let out = child.wait_with_output().expect("running agent");
        Out {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    /// Starts agent with these words in the background, its output going to
    /// <name>.out and <name>.err in the host's root.
    pub fn start(&self, name: &str, words: &[&str]) -> Background {
        let out = self.root.path().join(format!("{name}.out"));
        let err = self.root.path().join(format!("{name}.err"));
        let child = self
            .command(words)
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&out).unwrap())
            .stderr(std::fs::File::create(&err).unwrap())
            .spawn()
            .expect("starting agent");
        Background { child, out, err }
    }

    /// The text agent capture prints for agent, or "" on an error.
    pub fn screen(&self, agent: &str) -> String {
        self.run(&["capture", agent, "--lines", "200"]).stdout
    }

    /// Waits until agent's screen has a line that is want, trimmed.
    pub fn wait_line(&self, agent: &str, want: &str) -> String {
        wait_for(&format!("{want:?} on {agent}'s screen"), || {
            let s = self.screen(agent);
            let ok = s.lines().any(|l| l.trim() == want);
            (s, ok)
        })
    }

    /// Types line into agent's pane (agent keys, which presses Enter).
    pub fn type_line(&self, agent: &str, line: &str) {
        self.run(&["keys", &format!("agent-{agent}"), line])
            .ok("keys");
    }

    /// Runs `agent hook` with payload on stdin in agent's pane, under the
    /// stand-in Claude there (CLAUDE_PID=$$, as Claude Code sets it for its
    /// hooks), and returns its exit code, stdout and stderr.
    pub fn hook_in_pane(&self, agent: &str, payload: &str) -> Out {
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = self.root.path().join("hooks");
        std::fs::create_dir_all(&dir).unwrap();
        let p = |ext: &str| {
            dir.join(format!("{n}.{ext}"))
                .to_string_lossy()
                .into_owned()
        };
        std::fs::write(p("json"), payload).unwrap();
        self.type_line(
            agent,
            &format!(
                "CLAUDE_PID=$$ '{}' hook < '{}' > '{}' 2> '{}'; echo $? > '{}.tmp'; mv '{}.tmp' '{}'",
                self.bin.display(),
                p("json"),
                p("out"),
                p("err"),
                p("code"),
                p("code"),
                p("code"),
            ),
        );
        let code =
            wait_for(
                &format!("the hook to run in {agent}'s pane"),
                || match std::fs::read_to_string(p("code")) {
                    Ok(c) => (c, true),
                    Err(_) => (self.screen(agent), false),
                },
            );
        Out {
            code: code.trim().parse().unwrap_or(-1),
            stdout: std::fs::read_to_string(p("out")).unwrap_or_default(),
            stderr: std::fs::read_to_string(p("err")).unwrap_or_default(),
        }
    }

    /// Copies agent-busd beside the binary, where agent bus finds it.
    pub fn install_busd(&self) {
        let busd = busd_binary().expect("agent-busd");
        let dst = self.root.path().join("bin/agent-busd");
        if std::fs::hard_link(&busd, &dst).is_err() {
            std::fs::copy(&busd, &dst).unwrap();
        }
    }

    /// agent state get name key's value ("" when unset).
    pub fn state(&self, name: &str, key: &str) -> String {
        self.run(&["state", "get", name, key])
            .stdout
            .trim_end()
            .to_string()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        if self.bus_started.get() {
            let _ = self.run(&["bus", "stop"]);
        }
    }
}

/// An agent command running in the background.
pub struct Background {
    pub child: Child,
    out: PathBuf,
    err: PathBuf,
}

impl Background {
    pub fn stdout(&self) -> String {
        std::fs::read_to_string(&self.out).unwrap_or_default()
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.err).unwrap_or_default()
    }

    /// Sends sig, and waits (up to 10s) for the process to exit: its exit
    /// code, or -signal if a signal ended it.
    pub fn stop(&mut self, sig: i32) -> i32 {
        // SAFETY: a plain kill(2) of our own child.
        unsafe {
            libc::kill(self.child.id() as i32, sig);
        }
        self.wait(Duration::from_secs(10))
    }

    /// Waits up to d for the process to exit (killing it after).
    pub fn wait(&mut self, d: Duration) -> i32 {
        let deadline = Instant::now() + d;
        loop {
            if let Some(st) = self.child.try_wait().unwrap() {
                use std::os::unix::process::ExitStatusExt;
                return st.code().unwrap_or_else(|| -st.signal().unwrap_or(0));
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "agent didn't exit within {d:?}; stdout:\n{}\nstderr:\n{}",
                    self.stdout(),
                    self.stderr()
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls cond until it holds (up to 20s: the machine may be loaded), and
/// returns its state; fails saying what it waited for and the last state.
pub fn wait_for(what: &str, mut cond: impl FnMut() -> (String, bool)) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let (state, ok) = cond();
        if ok {
            return state;
        }
        if Instant::now() > deadline {
            panic!("timed out waiting for {what}; last state:\n{state}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A free TCP port on 127.0.0.1 (the kernel's pick; never 7420 or 7422).
pub fn free_port() -> u16 {
    loop {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        if port != 7420 && port != 7422 {
            return port;
        }
    }
}

static BUSD_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

extern "C" fn remove_busd() {
    if let Some(Some(p)) = BUSD_PATH.get() {
        let _ = std::fs::remove_file(p);
    }
}

/// Builds agent-busd from busd/ once per test binary: None when there is
/// no go toolchain.
pub fn busd_binary() -> Option<PathBuf> {
    BUSD_PATH
        .get_or_init(|| {
            let ok = Command::new("go")
                .arg("version")
                .output()
                .is_ok_and(|o| o.status.success());
            if !ok {
                return None;
            }
            let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(3)
                .expect("the repo root")
                .to_path_buf();
            let build_dir = PathBuf::from("/var/tmp/agent-busd-test-bin");
            std::fs::create_dir_all(&build_dir).expect("busd test bin dir");
            let bin = build_dir.join(format!("agent-busd-e2e-{}", std::process::id()));
            let out = Command::new("go")
                .args(["build", "-trimpath", "-o"])
                .arg(&bin)
                .arg(".")
                .current_dir(repo.join("busd"))
                .env("TMPDIR", "/var/tmp")
                .output()
                .expect("running go build");
            assert!(
                out.status.success(),
                "building agent-busd failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
            // SAFETY: registering a plain extern "C" function.
            unsafe {
                libc::atexit(remove_busd);
            }
            Some(bin)
        })
        .clone()
}

/// A Claude Code transcript of one prompt and its reply, as Claude Code
/// writes it (synthesized), for session sid in cwd.
pub fn claude_transcript(sid: &str, cwd: &str, prompt: &str, reply: &str) -> String {
    format!(
        concat!(
            r#"{{"type":"permission-mode","permissionMode":"default","sessionId":"{sid}"}}"#,
            "\n",
            r#"{{"type":"user","sessionId":"{sid}","cwd":"{cwd}","version":"2.1.400","timestamp":"2026-08-01T10:00:00.000Z","message":{{"role":"user","content":"{prompt}"}}}}"#,
            "\n",
            r#"{{"type":"ai-title","aiTitle":"List the files","sessionId":"{sid}"}}"#,
            "\n",
            r#"{{"type":"assistant","sessionId":"{sid}","timestamp":"2026-08-01T10:00:01.000Z","message":{{"id":"m1","role":"assistant","model":"claude-test-1","content":[{{"type":"text","text":"{reply}"}}]}}}}"#,
            "\n"
        ),
        sid = sid,
        cwd = cwd,
        prompt = prompt,
        reply = reply
    )
}
