//! What the Go tests in tests/service share: the test environment (a real
//! store in a temp dir and a real g3d-termd in a private socket dir), and the
//! fake backends that stand in for the CLIs.
//!
//! Rule 9: nothing here touches the live system. `init` points HOME and
//! CLAUDE_CONFIG_DIR at a temp dir, and puts stand-ins for `claude` and
//! `agy` first on PATH, before any termd daemon starts (it inherits that
//! environment), so a test that launches the claude backend in a test
//! session runs `cat`, not Claude Code. Deliverers are never started: every
//! service gets a recording starter.

#![allow(dead_code)]

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once, OnceLock, Weak};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};

use agent_backend::namedagents::{self, AgyBackend, AgyConfig, ClaudeBackend};
use agent_backend::{
    Backend, CommandAllower, Dialog, DialogReader, EffortChooser, GlobalHooker, HookAllower,
    HookDeliverer, HookProvider, HookReport, InputReader, InstructionsProvider, ModelChooser,
    NativeStatusProvider, PaneSession, PaneSessionProvider, PermissionResponder,
    PermissionResponse, PromptFlagProvider, PromptHandover, Provider, ProviderConfig,
    QuestionPrompter, Registry, SpawnHooker, Status, SubmitChecker, TranscriptProvider,
    TrustPrompter, TurnEndReader,
};
use agent_base::await_;
use agent_base::config::Config;
use agent_service::{AgentService, Context, STATE_KEY_HOOKED_SESSION, SpawnOptions};
use agent_store::{Agent, StateEntry, Store, Time};
use agent_termd::Ops;
use agent_termd::testing;

/// The root of everything a test run writes: on disk, never /tmp (RAM here).
pub fn temp_root() -> PathBuf {
    PathBuf::from("/var/tmp")
}

pub fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("agent-service-test-")
        .tempdir_in(temp_root())
        .expect("a temp dir in /var/tmp")
}

/// The stand-in home, Claude config dir, and bin dir of this test process.
pub struct Isolation {
    pub home: PathBuf,
    pub claude_config_dir: PathBuf,
    pub bin: PathBuf,
    /// The Go toolchain's settings from before HOME moved, so building
    /// agent-busd uses the module and build caches it always has, rather than
    /// downloading modules into the stand-in home.
    pub go_env: Vec<(String, String)>,
}

/// GOPATH, GOMODCACHE and GOCACHE as `go env` gives them now.
fn go_env() -> Vec<(String, String)> {
    let keys = ["GOPATH", "GOMODCACHE", "GOCACHE"];
    let Ok(out) = std::process::Command::new("go")
        .arg("env")
        .args(keys)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&out.stdout);
    keys.iter()
        .zip(text.lines())
        .filter(|(_, v)| !v.is_empty())
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

static ISOLATION: OnceLock<Isolation> = OnceLock::new();
static INIT: Once = Once::new();
static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Removes the run's temp root at exit: registered before the termd test
/// module registers its own cleanup, so it runs after that has stopped the
/// daemons.
extern "C" fn remove_root() {
    if let Some(root) = ROOT.get() {
        let _ = std::fs::remove_dir_all(root);
    }
}

/// Isolates this test process from the live system (see the module comment).
/// Every test calls it first, through the setups below.
pub fn init() -> &'static Isolation {
    INIT.call_once(|| {
        // Kept for the run: the termd daemons started under it outlive any
        // one test.
        let root = tempdir().keep();
        let _ = ROOT.set(root.clone());
        // SAFETY: registering a plain extern "C" function.
        unsafe {
            libc::atexit(remove_root);
        }
        let home = root.join("home");
        let claude_config_dir = root.join("claude");
        let bin = root.join("bin");
        for d in [&home, &claude_config_dir.join("sessions"), &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        for cli in ["claude", "agy"] {
            let p = bin.join(cli);
            // A CLI that takes what is typed and does nothing with it.
            std::fs::write(&p, "#!/bin/sh\nexec cat\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Found before HOME moves (it may be found under the real home).
        let termd_bin = agent_termd::daemon_binary("").ok();
        let go_env = go_env();
        let path = format!(
            "{}:{}",
            bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        // SAFETY: Once runs this before any test thread reads the
        // environment: every test calls init() first, and blocks here.
        unsafe {
            if let Some(b) = &termd_bin {
                std::env::set_var("G3D_TERMD_BIN", b);
            }
            std::env::set_var("HOME", &home);
            std::env::set_var("CLAUDE_CONFIG_DIR", &claude_config_dir);
            std::env::set_var("PATH", path);
            std::env::remove_var("AGENT_NAME");
            std::env::remove_var("AGENT_HOST");
        }
        let _ = ISOLATION.set(Isolation {
            home,
            claude_config_dir,
            bin,
            go_env,
        });
    });
    ISOLATION.get().expect("isolated")
}

/// Reports whether a g3d-termd binary is here to run the termd tests.
pub fn termd_available() -> bool {
    init();
    testing::available()
}

/// Skips (returns from) a test that needs g3d-termd when there is none, as
/// Go's termdtest.Require skips.
#[macro_export]
macro_rules! require_termd {
    () => {
        if !$crate::common::termd_available() {
            eprintln!("skipping: g3d-termd not available");
            return;
        }
    };
}

/// Which termd socket dir an environment holds.
enum Guard {
    Shared(#[allow(dead_code)] testing::SharedGuard),
    Private(testing::TestServer),
}

static ENV_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// testEnv holds test environment resources.
pub struct TestEnv {
    pub service: Arc<AgentService>,
    pub store: Arc<Store>,
    pub tmux: Arc<agent_termd::Client>,
    pub backends: Arc<Registry>,
    /// The agents a deliverer was started for, in order.
    pub delivering: Arc<Mutex<Vec<String>>>,
    initial_prefix: String,
    dir: tempfile::TempDir,
    guard: Guard,
}

/// setupTestEnv creates a full test environment with real dependencies, on
/// the shared private termd socket dir.
pub fn setup_test_env() -> TestEnv {
    init();
    let guard = testing::require();
    new_env(Guard::Shared(guard))
}

/// setup_test_env, alone in a private termd socket dir and holding the
/// environment lock, for a test that sets environment variables
/// (TestEnv::set_env).
pub fn setup_test_env_private() -> TestEnv {
    init();
    let server = testing::setup();
    agent_termd::check_test_server().expect("a private termd dir");
    new_env(Guard::Private(server))
}

fn new_env(guard: Guard) -> TestEnv {
    let dir = tempdir();
    let db = dir.path().join("test.db");
    let initial_prefix = format!(
        "test-{}-{}-",
        std::process::id(),
        ENV_SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    );
    let cfg = Config {
        backend: "claude".into(),
        termd_namespace: agent_termd::DEFAULT_NAMESPACE.into(),
        database_path: db.display().to_string(),
        log_dir: format!("{}/logs", dir.path().display()),
        state_dir: format!("{}/state", dir.path().display()),
        session_prefix: initial_prefix.clone(),
        default_scrollback: 100,
        startup_delay: Duration::ZERO, // No delay in tests for speed
        ..Default::default()
    };
    cfg.ensure_dirs().expect("ensuring dirs");
    let store = Arc::new(Store::open(&db).expect("creating storage"));
    let tmux = Arc::new(
        agent_termd::Client::new(agent_termd::Options {
            namespace: cfg.termd_namespace.clone(),
            ..Default::default()
        })
        .expect("creating termd client"),
    );
    let backends = Arc::new(agent_backend::new_registry());
    let mut svc = AgentService::new(
        cfg,
        Arc::clone(&store),
        tmux.clone() as Arc<dyn Ops>,
        Arc::clone(&backends),
    );
    let delivering = Arc::new(Mutex::new(Vec::new()));
    let d = Arc::clone(&delivering);
    svc.set_deliverer_starter(move |name| {
        d.lock().unwrap().push(name.to_string());
        Ok(())
    });
    TestEnv {
        service: Arc::new(svc),
        store,
        tmux,
        backends,
        delivering,
        initial_prefix,
        dir,
        guard,
    }
}

impl TestEnv {
    pub fn cfg(&self) -> &Config {
        self.service.config()
    }

    /// The config, to change: only while nothing else holds the service.
    pub fn cfg_mut(&mut self) -> &mut Config {
        Arc::get_mut(&mut self.service)
            .expect("the service is shared: change its config before serving it")
            .config_mut()
    }

    pub fn svc_mut(&mut self) -> &mut AgentService {
        Arc::get_mut(&mut self.service).expect("the service is shared")
    }

    pub fn session_name(&self, name: &str) -> String {
        self.cfg().session_name(name)
    }

    pub fn agent_dir(&self, name: &str) -> String {
        self.cfg().agent_dir(name)
    }

    /// Sets an environment variable for this test: only in a private env.
    pub fn set_env(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        match &mut self.guard {
            Guard::Private(s) => s.env().set(key, value),
            Guard::Shared(_) => panic!("set_env needs setup_test_env_private"),
        }
    }

    pub fn delivering(&self) -> Vec<String> {
        self.delivering.lock().unwrap().clone()
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }

    pub fn state_value(&self, name: &str, key: &str) -> String {
        state_of(&self.store, name, key)
    }

    pub fn set_state(&self, name: &str, key: &str, value: &str) {
        self.store
            .set_state(&StateEntry {
                agent_name: name.into(),
                key: key.into(),
                value: value.into(),
                updated_at: Time::now(),
            })
            .unwrap();
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        // Kill every recorded agent's session directly by name before
        // removing the temp dir (so no surviving shell reads a deleted
        // launch.sh).
        if let Ok(agents) = self.store.list_agents() {
            for a in agents {
                if !a.session_name.is_empty() {
                    let _ = self.tmux.kill_session(&a.session_name);
                }
            }
        }
        // Also sweep any sessions matching this test's initial or current prefix.
        let prefix = self.service.config().session_prefix.clone();
        if let Ok(sessions) = self.tmux.list_sessions() {
            for s in sessions {
                if s.name.starts_with(&self.initial_prefix)
                    || (!prefix.is_empty() && s.name.starts_with(&prefix))
                {
                    let _ = self.tmux.kill_session(&s.name);
                }
            }
        }
    }
}

pub fn state_of(store: &Store, name: &str, key: &str) -> String {
    match store.get_state(name, key) {
        Ok(Some(e)) => e.value,
        _ => String::new(),
    }
}

pub fn bg() -> Context {
    Context::background()
}

/// waitForSession waits until a session's screen shows every one of wants.
pub fn wait_for_session(env: &TestEnv, session: &str, wants: &[&str]) {
    await_::until(&format!("session {session} to show {wants:?}"), || {
        let out = env.tmux.capture(session, 20).unwrap_or_default();
        let ok = wants.iter().all(|w| out.contains(w));
        (out, ok)
    });
}

/// waitPane waits until ok holds for a session's visible screen.
pub fn wait_pane(env: &TestEnv, session: &str, what: &str, ok: impl Fn(&str) -> bool) -> String {
    await_::until(what, || match env.tmux.capture(session, 0) {
        Err(e) => (format!("capture failed: {e:#}"), false),
        Ok(screen) => {
            let good = ok(&screen);
            (screen, good)
        }
    })
}

/// hasLine reports whether a line of screen is line, ignoring surrounding
/// space.
pub fn has_line(screen: &str, line: &str) -> bool {
    screen.split('\n').any(|l| l.trim() == line)
}

/// waitForOutput waits for want on the agent's screen and returns the screen.
pub fn wait_for_output(env: &TestEnv, name: &str, want: &str) -> String {
    await_::until(&format!("{want:?} on {name}'s screen"), || {
        let screen = env.service.capture(name, 50, false).unwrap_or_default();
        let ok = screen.contains(want);
        (screen, ok)
    })
}

/// statusIs waits until name's status is want.
pub fn status_is(env: &TestEnv, name: &str, want: &str) -> agent_service::StatusInfo {
    let mut info = None;
    await_::until(&format!("{name} {want}"), || {
        match env.service.status_info(name) {
            Err(e) => (format!("{e:#}"), false),
            Ok(i) => {
                let s = format!("{} ({})", i.status, i.detail);
                let ok = i.status == want;
                info = Some(i);
                (s, ok)
            }
        }
    });
    info.unwrap()
}

/// panePath is the working directory of session's pane, as termd sees it.
pub fn pane_path(env: &TestEnv, session: &str) -> String {
    let pane = env
        .tmux
        .first_pane(session)
        .unwrap_or_else(|e| panic!("reading {session}'s pane path: {e:#}"));
    real_path(&pane.cwd)
}

/// realPath is path with its symlinks resolved.
pub fn real_path(path: &str) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|e| panic!("resolving {path}: {e}"))
        .display()
        .to_string()
}

/// panePID returns the pid of the process a session's pane runs.
pub fn pane_pid(env: &TestEnv, session: &str) -> i32 {
    let panes = env.tmux.pane_pids().unwrap();
    panes
        .into_iter()
        .find_map(|(pid, s)| (s == session).then_some(pid))
        .unwrap_or_else(|| panic!("no pane for {session}"))
}

// ---------------------------------------------------------------------------
// A claude backend with parts replaced.

type Over<A, R> = Option<Box<dyn Fn(A) -> R + Send + Sync>>;

/// What a fake claude backend does instead of the real one; None keeps the
/// real one's (Go's embedded *namedagents.ClaudeBackend).
#[derive(Default)]
pub struct ClaudeOverrides {
    pub name: String,
    pub launch_command: Option<Box<dyn Fn() -> String + Send + Sync>>,
    pub launch_command_with_session: Over<String, String>,
    pub hooks_installed: Over<String, Result<()>>,
    pub native_status: Over<String, (Status, bool, Option<anyhow::Error>)>,
    pub pane_session: Over<i32, Result<Option<PaneSession>>>,
    pub input_text: Over<String, Option<String>>,
    pub typed_as_paste: Over<String, bool>,
    pub permission_prompt_visible: Over<String, bool>,
}

/// A claude backend with the parts its overrides name replaced.
pub struct ClaudeFake {
    pub inner: ClaudeBackend,
    pub o: Arc<ClaudeOverrides>,
}

impl ClaudeFake {
    pub fn new(o: &Arc<ClaudeOverrides>) -> ClaudeFake {
        ClaudeFake {
            inner: namedagents::new_claude_backend(),
            o: Arc::clone(o),
        }
    }
}

/// Registers a claude fake as a backend (for status patterns) and as the
/// factory of its own name.
pub fn register_claude_fake(backends: &Registry, o: ClaudeOverrides) -> Arc<ClaudeOverrides> {
    let o = Arc::new(o);
    register_claude_fake_as(backends, &o.name.clone(), &o);
    backends.register(Arc::new(ClaudeFake::new(&o)));
    o
}

/// Registers a claude fake as the factory of name.
pub fn register_claude_fake_as(backends: &Registry, name: &str, o: &Arc<ClaudeOverrides>) {
    let o = Arc::clone(o);
    backends.register_factory(name, move |_| Box::new(ClaudeFake::new(&o)));
}

impl Backend for ClaudeFake {
    fn name(&self) -> &str {
        &self.o.name
    }
    fn launch_command(&self) -> String {
        Provider::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        Backend::init_message(&self.inner, task_file)
    }
    fn idle_pattern(&self) -> &str {
        Backend::idle_pattern(&self.inner)
    }
    fn processing_pattern(&self) -> &str {
        Backend::processing_pattern(&self.inner)
    }
}

impl Provider for ClaudeFake {
    fn name(&self) -> &str {
        &self.o.name
    }
    fn launch_command(&self) -> String {
        match &self.o.launch_command {
            Some(f) => f(),
            None => Provider::launch_command(&self.inner),
        }
    }
    fn init_message(&self, task_file: &str) -> String {
        Provider::init_message(&self.inner, task_file)
    }
    fn exit_command(&self) -> String {
        Provider::exit_command(&self.inner)
    }
    fn as_hook_provider(&self) -> Option<&dyn HookProvider> {
        Some(self)
    }
    fn as_global_hooker(&self) -> Option<&dyn GlobalHooker> {
        Some(self)
    }
    fn as_hook_deliverer(&self) -> Option<&dyn HookDeliverer> {
        Some(self)
    }
    fn as_prompt_handover(&self) -> Option<&dyn PromptHandover> {
        Some(self)
    }
    fn as_hook_allower(&self) -> Option<&dyn HookAllower> {
        Some(self)
    }
    fn as_input_reader(&self) -> Option<&dyn InputReader> {
        Some(self)
    }
    fn as_submit_checker(&self) -> Option<&dyn SubmitChecker> {
        Some(self)
    }
    fn as_instructions_provider(&self) -> Option<&dyn InstructionsProvider> {
        Some(self)
    }
    fn as_command_allower(&self) -> Option<&dyn CommandAllower> {
        Some(self)
    }
    fn as_trust_prompter(&self) -> Option<&dyn TrustPrompter> {
        Some(self)
    }
    fn as_model_chooser(&self) -> Option<&dyn ModelChooser> {
        Some(self)
    }
    fn as_effort_chooser(&self) -> Option<&dyn EffortChooser> {
        Some(self)
    }
    fn as_dialog_reader(&self) -> Option<&dyn DialogReader> {
        Some(self)
    }
    fn as_native_status_provider(&self) -> Option<&dyn NativeStatusProvider> {
        Some(self)
    }
    fn as_pane_session_provider(&self) -> Option<&dyn PaneSessionProvider> {
        Some(self)
    }
    fn as_permission_responder(&self) -> Option<&dyn PermissionResponder> {
        Some(self)
    }
    fn as_transcript_provider(&self) -> Option<&dyn TranscriptProvider> {
        Some(self)
    }
}

impl HookProvider for ClaudeFake {
    fn parse_hook_event(&self, event: &str, payload: &[u8]) -> Result<Option<HookReport>> {
        self.inner.parse_hook_event(event, payload)
    }
}

impl GlobalHooker for ClaudeFake {
    fn hooks_installed(&self, binary: &str) -> Result<()> {
        match &self.o.hooks_installed {
            Some(f) => f(binary.to_string()),
            None => self.inner.hooks_installed(binary),
        }
    }
    fn launch_command_with_session(&self, session_id: &str) -> String {
        match &self.o.launch_command_with_session {
            Some(f) => f(session_id.to_string()),
            None => self.inner.launch_command_with_session(session_id),
        }
    }
}

impl HookDeliverer for ClaudeFake {
    fn can_deliver(&self, r: &HookReport) -> bool {
        self.inner.can_deliver(r)
    }
    fn delivery_output(&self, messages: &[String]) -> Vec<u8> {
        self.inner.delivery_output(messages)
    }
    fn delivery_output_for(
        &self,
        event: &str,
        batch: &[agent_backend::CoalescedMessage],
    ) -> Vec<u8> {
        self.inner.delivery_output_for(event, batch)
    }
    fn interrupt_output(&self, r: &HookReport, reason: &str) -> Option<Vec<u8>> {
        self.inner.interrupt_output(r, reason)
    }
    fn delivery_point(&self) -> String {
        self.inner.delivery_point()
    }
}

impl PromptHandover for ClaudeFake {
    fn typed_as_paste(&self, text: &str) -> bool {
        match &self.o.typed_as_paste {
            Some(f) => f(text.to_string()),
            None => self.inner.typed_as_paste(text),
        }
    }
    fn submitted_prompt(&self, r: &HookReport) -> Option<String> {
        self.inner.submitted_prompt(r)
    }
    fn handover_output(&self, text: &str) -> Vec<u8> {
        self.inner.handover_output(text)
    }
}

impl HookAllower for ClaudeFake {
    fn allow_output(&self, reason: &str) -> Vec<u8> {
        self.inner.allow_output(reason)
    }
}

impl InputReader for ClaudeFake {
    fn input_text(&self, screen: &str) -> Option<String> {
        match &self.o.input_text {
            Some(f) => f(screen.to_string()),
            None => self.inner.input_text(screen),
        }
    }
}

impl SubmitChecker for ClaudeFake {
    fn enter_can_be_a_newline(&self) {}
}

impl InstructionsProvider for ClaudeFake {
    fn with_instructions(&self, launch_cmd: &str, path: &str) -> Result<String> {
        self.inner.with_instructions(launch_cmd, path)
    }
}

impl CommandAllower for ClaudeFake {
    fn allow_commands(&self, launch_cmd: &str, prefixes: &[String]) -> String {
        self.inner.allow_commands(launch_cmd, prefixes)
    }
}

impl TrustPrompter for ClaudeFake {
    fn trust_prompt(&self, screen: &str) -> Option<String> {
        self.inner.trust_prompt(screen)
    }
    fn trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        self.inner.trust_keys(screen)
    }
    fn decline_trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        self.inner.decline_trust_keys(screen)
    }
}

impl ModelChooser for ClaudeFake {
    fn with_model(&self, launch_cmd: &str, model: &str) -> String {
        self.inner.with_model(launch_cmd, model)
    }
}

impl EffortChooser for ClaudeFake {
    fn with_effort(&self, launch_cmd: &str, effort: &str) -> String {
        self.inner.with_effort(launch_cmd, effort)
    }
}

impl DialogReader for ClaudeFake {
    fn dialog(&self, screen: &str) -> Option<Dialog> {
        self.inner.dialog(screen)
    }
    fn dialog_key(&self, resp: PermissionResponse) -> Result<String> {
        self.inner.dialog_key(resp)
    }
}

impl NativeStatusProvider for ClaudeFake {
    fn native_status(&self, session_id: &str) -> (Status, bool, Option<anyhow::Error>) {
        match &self.o.native_status {
            Some(f) => f(session_id.to_string()),
            None => self.inner.native_status(session_id),
        }
    }
}

impl PaneSessionProvider for ClaudeFake {
    fn pane_session(&self, pane_pid: i32) -> Result<Option<PaneSession>> {
        match &self.o.pane_session {
            Some(f) => f(pane_pid),
            None => self.inner.pane_session(pane_pid),
        }
    }
}

impl PermissionResponder for ClaudeFake {
    fn permission_key(&self, resp: PermissionResponse, tool: &str) -> Result<String> {
        self.inner.permission_key(resp, tool)
    }
    fn permission_prompt_visible(&self, screen: &str) -> bool {
        match &self.o.permission_prompt_visible {
            Some(f) => f(screen.to_string()),
            None => self.inner.permission_prompt_visible(screen),
        }
    }
}

impl TranscriptProvider for ClaudeFake {
    fn transcript_reader(&self) -> Box<dyn agent_transcript::Reader> {
        self.inner.transcript_reader()
    }
}

// ---------------------------------------------------------------------------
// fakeHookBackend: the claude backend with the parts that would launch or
// query a real claude replaced, so tests control what the CLI "says".

#[derive(Default)]
pub struct HookState {
    /// the session spawn pinned
    pub session_id: String,
    /// what HooksInstalled says; None = installed
    pub hooks_err: Option<String>,
    /// the CLI lists the session, with status native
    pub native_found: bool,
    /// the CLI answers but doesn't list the session (it died)
    pub native_gone: bool,
    pub native: Status,
    /// with native_found: why the status is unknown
    pub native_err: Option<String>,
    /// no permission dialog on screen (the test shell shows none)
    pub prompt_gone: bool,
    /// what the pane runs, per the registry; None = no CLI there
    pub pane_session: Option<PaneSession>,
    /// why the pane couldn't be read
    pub pane_err: Option<String>,
    /// what a person has typed in the input line
    pub input: String,
    /// if set, when they clear it
    pub input_until: Option<Instant>,
}

/// The test's handle on a fakeHookBackend's state.
#[derive(Clone, Default)]
pub struct FakeHook(pub Arc<Mutex<HookState>>);

impl FakeHook {
    pub fn set(&self, f: impl FnOnce(&mut HookState)) {
        f(&mut self.0.lock().unwrap());
    }
    pub fn session_id(&self) -> String {
        self.0.lock().unwrap().session_id.clone()
    }
}

/// The overrides that make a claude backend the fakeHookBackend over st.
pub fn fake_hook_overrides(name: &str, st: &FakeHook) -> ClaudeOverrides {
    let s = |st: &FakeHook| Arc::clone(&st.0);
    let (a, b, c, d, e, f) = (s(st), s(st), s(st), s(st), s(st), s(st));
    ClaudeOverrides {
        name: name.to_string(),
        // Typed into the test shell; keep it harmless.
        launch_command: Some(Box::new(|| "true".to_string())),
        launch_command_with_session: Some(Box::new(move |id| {
            a.lock().unwrap().session_id = id;
            "true".to_string()
        })),
        // Stands in for reading Claude Code's settings.
        hooks_installed: Some(Box::new(move |_| match &b.lock().unwrap().hooks_err {
            Some(e) => Err(anyhow!("{e}")),
            None => Ok(()),
        })),
        native_status: Some(Box::new(move |session_id| {
            let st = c.lock().unwrap();
            if !st.native_found && !st.native_gone {
                return (
                    Status::Unknown,
                    false,
                    Some(anyhow!("fake: native status unavailable")),
                );
            }
            if st.native_gone || session_id != st.session_id {
                return (Status::Unknown, false, None);
            }
            (
                st.native,
                true,
                st.native_err.as_ref().map(|e| anyhow!("{e}")),
            )
        })),
        // Stands in for reading the registry and the process tree.
        pane_session: Some(Box::new(move |pane_pid| {
            let st = d.lock().unwrap();
            if pane_pid <= 0 {
                bail!("fake: no pane pid");
            }
            if let Some(e) = &st.pane_err {
                bail!("{e}");
            }
            Ok(st.pane_session.clone())
        })),
        // Stands in for reading Claude's input line, which the test shell
        // doesn't draw.
        input_text: Some(Box::new(move |_| {
            let st = e.lock().unwrap();
            if st.input_until.is_some_and(|u| Instant::now() > u) {
                return Some(String::new());
            }
            Some(st.input.clone())
        })),
        // The test session is a shell, which runs no UserPromptSubmit hook to
        // hand a message over, so messages are typed as they are.
        typed_as_paste: Some(Box::new(|_| false)),
        // Stands in for the screen check: the test session is a plain shell,
        // so the real check would never see a dialog.
        permission_prompt_visible: Some(Box::new(move |_| !f.lock().unwrap().prompt_gone)),
    }
}

/// Registers a fakeHookBackend ("fakehooks") on env's registry.
pub fn register_fake_hook(env: &TestEnv) -> FakeHook {
    let fake = FakeHook::default();
    register_claude_fake(&env.backends, fake_hook_overrides("fakehooks", &fake));
    fake
}

/// spawnFake registers a fake hook backend and spawns an agent on it.
pub fn spawn_fake(env: &TestEnv, name: &str) -> FakeHook {
    spawn_fake_with(env, name, SpawnOptions::default())
}

/// spawnFakeWith is spawn_fake with extra spawn options.
pub fn spawn_fake_with(env: &TestEnv, name: &str, mut opts: SpawnOptions) -> FakeHook {
    let fake = register_fake_hook(env);
    opts.name = name.to_string();
    opts.backend = "fakehooks".to_string();
    env.service
        .spawn(&bg(), opts)
        .unwrap_or_else(|e| panic!("spawn: {e:#}"));
    // As the global hook does at the CLI's SessionStart: it reaches this
    // session, so a Stop can deliver.
    let sid = fake.session_id();
    if !sid.is_empty() {
        env.set_state(name, STATE_KEY_HOOKED_SESSION, &sid);
    }
    fake
}

/// hookAs sends a Claude hook payload for agent name, with the fake's session
/// id, so native status applies.
pub fn hook_as(env: &TestEnv, fake: &FakeHook, name: &str, payload: &str) {
    let payload = payload.replacen(
        '{',
        &format!(
            "{{\"session_id\":{},",
            serde_json::to_string(&fake.session_id()).unwrap()
        ),
        1,
    );
    env.service
        .report_hook(name, "", "", payload.as_bytes())
        .unwrap_or_else(|e| panic!("ReportHook {payload}: {e:#}"));
}

// ---------------------------------------------------------------------------
// An agy backend with parts replaced.

/// An override that also gets the inner backend, as Go's methods reach their
/// embedded one.
type AgyOver<A, R> = Option<Box<dyn Fn(&AgyBackend, A) -> R + Send + Sync>>;

#[derive(Default)]
pub struct AgyOverrides {
    pub name: String,
    pub launch_command_with_hooks: AgyOver<(String, String), String>,
    pub with_model: AgyOver<(String, String), String>,
    pub with_effort: AgyOver<(String, String), String>,
    pub append_prompt_flag: AgyOver<(String, String), String>,
    pub at_prompt: AgyOver<String, Option<String>>,
    pub question_prompt_visible: AgyOver<String, bool>,
    pub permission_prompt_visible: AgyOver<String, bool>,
    pub input_text: AgyOver<String, Option<String>>,
}

/// An agy backend with the parts its overrides name replaced.
pub struct AgyFake {
    pub inner: Arc<AgyBackend>,
    built: Arc<AgyOverrides>,
}

impl AgyFake {
    pub fn new(inner: AgyBackend, o: &Arc<AgyOverrides>) -> AgyFake {
        AgyFake {
            inner: Arc::new(inner),
            built: Arc::clone(o),
        }
    }
}

impl Backend for AgyFake {
    fn name(&self) -> &str {
        &self.built.name
    }
    fn launch_command(&self) -> String {
        Backend::launch_command(self.inner.as_ref())
    }
    fn init_message(&self, task_file: &str) -> String {
        Backend::init_message(self.inner.as_ref(), task_file)
    }
    fn idle_pattern(&self) -> &str {
        Backend::idle_pattern(self.inner.as_ref())
    }
    fn processing_pattern(&self) -> &str {
        Backend::processing_pattern(self.inner.as_ref())
    }
}

impl Provider for AgyFake {
    fn name(&self) -> &str {
        &self.built.name
    }
    fn launch_command(&self) -> String {
        Provider::launch_command(self.inner.as_ref())
    }
    fn init_message(&self, task_file: &str) -> String {
        Provider::init_message(self.inner.as_ref(), task_file)
    }
    fn exit_command(&self) -> String {
        Provider::exit_command(self.inner.as_ref())
    }
    fn as_hook_provider(&self) -> Option<&dyn HookProvider> {
        Some(self)
    }
    fn as_spawn_hooker(&self) -> Option<&dyn SpawnHooker> {
        Some(self)
    }
    fn as_hook_deliverer(&self) -> Option<&dyn HookDeliverer> {
        Some(self)
    }
    fn as_input_reader(&self) -> Option<&dyn InputReader> {
        Some(self)
    }
    fn as_turn_end_reader(&self) -> Option<&dyn TurnEndReader> {
        Some(self)
    }
    fn as_question_prompter(&self) -> Option<&dyn QuestionPrompter> {
        Some(self)
    }
    fn as_instructions_provider(&self) -> Option<&dyn InstructionsProvider> {
        Some(self)
    }
    fn as_trust_prompter(&self) -> Option<&dyn TrustPrompter> {
        Some(self)
    }
    fn as_model_chooser(&self) -> Option<&dyn ModelChooser> {
        Some(self)
    }
    fn as_effort_chooser(&self) -> Option<&dyn EffortChooser> {
        Some(self)
    }
    fn as_prompt_flag_provider(&self) -> Option<&dyn PromptFlagProvider> {
        Some(self)
    }
    fn as_permission_responder(&self) -> Option<&dyn PermissionResponder> {
        Some(self)
    }
    fn as_transcript_provider(&self) -> Option<&dyn TranscriptProvider> {
        Some(self)
    }
}

impl HookProvider for AgyFake {
    fn parse_hook_event(&self, event: &str, payload: &[u8]) -> Result<Option<HookReport>> {
        self.inner.parse_hook_event(event, payload)
    }
}

impl SpawnHooker for AgyFake {
    fn launch_command_with_hooks(&self, report_cmd: &str, session_id: &str) -> String {
        match &self.built.launch_command_with_hooks {
            Some(f) => f(
                &self.inner,
                (report_cmd.to_string(), session_id.to_string()),
            ),
            None => self.inner.launch_command_with_hooks(report_cmd, session_id),
        }
    }
}

impl HookDeliverer for AgyFake {
    fn can_deliver(&self, r: &HookReport) -> bool {
        self.inner.can_deliver(r)
    }
    fn delivery_output(&self, messages: &[String]) -> Vec<u8> {
        self.inner.delivery_output(messages)
    }
    fn delivery_output_for(
        &self,
        event: &str,
        batch: &[agent_backend::CoalescedMessage],
    ) -> Vec<u8> {
        self.inner.delivery_output_for(event, batch)
    }
    fn interrupt_output(&self, r: &HookReport, reason: &str) -> Option<Vec<u8>> {
        self.inner.interrupt_output(r, reason)
    }
    fn delivery_point(&self) -> String {
        self.inner.delivery_point()
    }
}

impl InputReader for AgyFake {
    fn input_text(&self, screen: &str) -> Option<String> {
        match &self.built.input_text {
            Some(f) => f(&self.inner, screen.to_string()),
            None => self.inner.input_text(screen),
        }
    }
}

impl TurnEndReader for AgyFake {
    fn at_prompt(&self, screen: &str) -> Option<String> {
        match &self.built.at_prompt {
            Some(f) => f(&self.inner, screen.to_string()),
            None => self.inner.at_prompt(screen),
        }
    }
}

impl QuestionPrompter for AgyFake {
    fn question_prompt_visible(&self, screen: &str) -> bool {
        match &self.built.question_prompt_visible {
            Some(f) => f(&self.inner, screen.to_string()),
            None => self.inner.question_prompt_visible(screen),
        }
    }
}

impl InstructionsProvider for AgyFake {
    fn with_instructions(&self, launch_cmd: &str, path: &str) -> Result<String> {
        self.inner.with_instructions(launch_cmd, path)
    }
}

impl TrustPrompter for AgyFake {
    fn trust_prompt(&self, screen: &str) -> Option<String> {
        self.inner.trust_prompt(screen)
    }
    fn trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        self.inner.trust_keys(screen)
    }
    fn decline_trust_keys(&self, screen: &str) -> Option<Vec<String>> {
        self.inner.decline_trust_keys(screen)
    }
}

impl ModelChooser for AgyFake {
    fn with_model(&self, launch_cmd: &str, model: &str) -> String {
        match &self.built.with_model {
            Some(f) => f(&self.inner, (launch_cmd.to_string(), model.to_string())),
            None => self.inner.with_model(launch_cmd, model),
        }
    }
}

impl EffortChooser for AgyFake {
    fn with_effort(&self, launch_cmd: &str, effort: &str) -> String {
        match &self.built.with_effort {
            Some(f) => f(&self.inner, (launch_cmd.to_string(), effort.to_string())),
            None => self.inner.with_effort(launch_cmd, effort),
        }
    }
    fn model_at_effort(&self, model: &str, effort: &str) -> Option<String> {
        self.inner.model_at_effort(model, effort)
    }
}

impl PromptFlagProvider for AgyFake {
    fn append_prompt_flag(&self, launch_cmd: &str, prompt: &str) -> String {
        match &self.built.append_prompt_flag {
            Some(f) => f(&self.inner, (launch_cmd.to_string(), prompt.to_string())),
            None => self.inner.append_prompt_flag(launch_cmd, prompt),
        }
    }
}

impl PermissionResponder for AgyFake {
    fn permission_key(&self, resp: PermissionResponse, tool: &str) -> Result<String> {
        self.inner.permission_key(resp, tool)
    }
    fn permission_prompt_visible(&self, screen: &str) -> bool {
        match &self.built.permission_prompt_visible {
            Some(f) => f(&self.inner, screen.to_string()),
            None => self.inner.permission_prompt_visible(screen),
        }
    }
}

impl TranscriptProvider for AgyFake {
    fn transcript_reader(&self) -> Box<dyn agent_transcript::Reader> {
        self.inner.transcript_reader()
    }
}

// fakeAgyBackend wraps AgyBackend so tests don't launch the real agy binary
// while still exercising LaunchCommandWithHooks bundle generation and
// ParseHookEvent.

/// What one fakeAgyBackend instance was asked to launch.
#[derive(Default, Clone)]
pub struct AgyRecord {
    pub report_cmd: String,
    pub launched_cmd: String,
}

pub type AgyRec = Arc<Mutex<AgyRecord>>;

/// A fakeAgyBackend over agy configured by cfg, recording into rec.
pub fn fake_agy(name: &str, cfg: AgyConfig, rec: &AgyRec) -> AgyFake {
    let (r1, r2, r3, r4) = (
        Arc::clone(rec),
        Arc::clone(rec),
        Arc::clone(rec),
        Arc::clone(rec),
    );
    let o = Arc::new(AgyOverrides {
        name: name.to_string(),
        launch_command_with_hooks: Some(Box::new(
            move |inner: &AgyBackend, (report_cmd, session_id)| {
                let launched = inner.launch_command_with_hooks(&report_cmd, &session_id);
                let mut r = r1.lock().unwrap();
                r.report_cmd = report_cmd;
                r.launched_cmd = launched;
                "true".to_string()
            },
        )),
        with_model: Some(Box::new(move |inner: &AgyBackend, (_, model)| {
            let mut r = r2.lock().unwrap();
            r.launched_cmd = inner.with_model(&r.launched_cmd, &model);
            "true".to_string()
        })),
        with_effort: Some(Box::new(move |inner: &AgyBackend, (_, effort)| {
            let mut r = r4.lock().unwrap();
            r.launched_cmd = inner.with_effort(&r.launched_cmd, &effort);
            "true".to_string()
        })),
        append_prompt_flag: Some(Box::new(move |inner: &AgyBackend, (_, prompt)| {
            let mut r = r3.lock().unwrap();
            r.launched_cmd = inner.append_prompt_flag(&r.launched_cmd, &prompt);
            "true".to_string()
        })),
        ..Default::default()
    });
    AgyFake::new(namedagents::new_agy_backend_with_config(cfg), &o)
}

/// A spawned fakeAgyBackend: what it launched.
#[derive(Clone)]
pub struct SpawnedAgy {
    pub rec: AgyRec,
}

impl SpawnedAgy {
    pub fn report_cmd(&self) -> String {
        self.rec.lock().unwrap().report_cmd.clone()
    }
    pub fn launched_cmd(&self) -> String {
        self.rec.lock().unwrap().launched_cmd.clone()
    }
}

/// spawnFakeAgy registers the fake agy backend ("fakeagy") and spawns an
/// agent on it; it returns the spawn's instance.
pub fn spawn_fake_agy(env: &TestEnv, mut opts: SpawnOptions) -> SpawnedAgy {
    let last: Arc<Mutex<Option<SpawnedAgy>>> = Arc::new(Mutex::new(None));
    let l = Arc::clone(&last);
    env.backends
        .register_factory("fakeagy", move |cfg: ProviderConfig| {
            let acfg = AgyConfig {
                skip_permissions: cfg.skip_permissions,
                agent_dir: cfg.agent_dir,
                ..Default::default()
            };
            let rec: AgyRec = Arc::default();
            *l.lock().unwrap() = Some(SpawnedAgy {
                rec: Arc::clone(&rec),
            });
            Box::new(fake_agy("fakeagy", acfg, &rec))
        });
    opts.backend = "fakeagy".into();
    env.service
        .spawn(&bg(), opts)
        .unwrap_or_else(|e| panic!("Spawn: {e:#}"));
    // the spawn's instance: later lookups make new ones
    last.lock().unwrap().clone().expect("an instance")
}

/// registerFakeAgy makes "agy" in env's registry a fake that launches
/// nothing (resume takes only claude and agy agents). It returns the last
/// instance created with a session to resume.
pub fn register_fake_agy(env: &TestEnv) -> Arc<Mutex<Option<SpawnedAgy>>> {
    let resumed: Arc<Mutex<Option<SpawnedAgy>>> = Arc::new(Mutex::new(None));
    let r = Arc::clone(&resumed);
    env.backends
        .register_factory("agy", move |cfg: ProviderConfig| {
            let acfg = AgyConfig {
                skip_permissions: cfg.skip_permissions,
                agent_dir: cfg.agent_dir,
                resume: cfg.resume.clone(),
                ..Default::default()
            };
            let rec: AgyRec = Arc::default();
            if !cfg.resume.is_empty() {
                *r.lock().unwrap() = Some(SpawnedAgy {
                    rec: Arc::clone(&rec),
                });
            }
            Box::new(fake_agy("fakeagy", acfg, &rec))
        });
    resumed
}

static SPAWN_ID_FLAG: OnceLock<regex::Regex> = OnceLock::new();

/// spawnIDOf extracts the spawn id that spawn baked into the hook command.
pub fn spawn_id_of(report_cmd: &str) -> String {
    let re = SPAWN_ID_FLAG.get_or_init(|| regex::Regex::new(r"--spawn-id '([^']+)'").unwrap());
    match re.captures(report_cmd) {
        Some(m) => m[1].to_string(),
        None => panic!("no --spawn-id in hook command {report_cmd:?}"),
    }
}

// ---------------------------------------------------------------------------
// claudeTerminal stands in for termd and a Claude pane behind it, as far as
// typing a message goes: what is written to the pane waits there until
// Claude reads it, and Claude reads everything that has arrived at once. A
// read that brings text and an Enter together is a paste, where the Enter is
// a newline, and Claude then eats the next Enter (both as seen with Claude
// Code v2.1.310). Typed text longer than the pty's input queue arrives in
// pieces, of which Claude keeps only the last; text pasted with paste
// brackets is one paste however it arrives (both as seen with Claude Code on
// macOS, 2026-11). Screens are synthesized like those in tests/backend's
// TestClaudeInputText.

/// ptyQueue is how much of what is written to a macOS pty its reader gets
/// in one read: its input queue's 1024 bytes, less 2.
pub const PTY_QUEUE: usize = 1022;

const PASTE_START: &str = "\x1b[200~";
const PASTE_END: &str = "\x1b[201~";

#[derive(Default)]
pub struct TermState {
    /// written to the pane, not read by Claude yet
    pub unread: String,
    /// captures Claude sits out before reading again
    pub stalled: i32,
    /// its input box's lines
    pub boxl: Vec<String>,
    /// the next Enter does nothing
    pub eat_enter: bool,
    /// no Enter submits (to see what send says then)
    pub deaf_enter: bool,
    /// the Enter after which a dialog replaces the box (1-based)
    pub dialog_on: i32,
    pub dialog: bool,
    /// a screen with no input box agent recognizes
    pub strange: bool,
    /// captures Claude is still starting for: no box drawn, nothing read
    pub starting: i32,
    pub enters: i32,
    pub submitted: Vec<String>,
    pub pastes: i32,
    /// a paste's placeholder: what was pasted
    pub pasted: HashMap<String, String>,

    /// offer is a dialog drawn above the input box, as Claude's fullscreen
    /// renderer draws its auto-mode offer after a turn: it takes every key,
    /// Enter picks its highlighted "Yes", Escape closes it. offer_on_read
    /// brings it up as typed text arrives (it came up after send looked).
    pub offer: bool,
    pub offer_on_read: bool,
    /// what the offer was answered with
    pub offer_answers: Vec<String>,
    /// typed text goes nowhere, and no dialog shows
    pub deaf_type: bool,

    /// submitted prompts whose hook hasn't run yet
    hooks: Vec<String>,
    context: Vec<String>,
}

/// Claude's UserPromptSubmit hook: it runs for each submitted prompt and
/// returns the hook's output.
pub type OnSubmit = Box<dyn Fn(&str) -> Option<Vec<u8>> + Send + Sync>;

#[derive(Default)]
pub struct ClaudeTerminal {
    pub mu: Mutex<TermState>,
    on_submit: Mutex<Option<Arc<OnSubmit>>>,
}

impl ClaudeTerminal {
    pub fn new(f: impl FnOnce(&mut TermState)) -> Arc<ClaudeTerminal> {
        let t = ClaudeTerminal::default();
        f(&mut t.mu.lock().unwrap());
        Arc::new(t)
    }

    pub fn set(&self, f: impl FnOnce(&mut TermState)) {
        f(&mut self.mu.lock().unwrap());
    }

    pub fn set_on_submit(&self, f: OnSubmit) {
        *self.on_submit.lock().unwrap() = Some(Arc::new(f));
    }

    /// runHooks runs the UserPromptSubmit hook for each prompt submitted since
    /// it last ran. Callers hold no lock.
    fn run_hooks(&self) {
        let prompts = std::mem::take(&mut self.mu.lock().unwrap().hooks);
        let Some(on_submit) = self.on_submit.lock().unwrap().clone() else {
            return;
        };
        for p in prompts {
            let out = on_submit(&p).unwrap_or_default();
            let ctx = serde_json::from_slice::<serde_json::Value>(&out)
                .ok()
                .and_then(|v| {
                    v["hookSpecificOutput"]["additionalContext"]
                        .as_str()
                        .map(str::to_string)
                })
                .unwrap_or_default();
            self.mu.lock().unwrap().context.push(ctx);
        }
    }

    /// got is what the model got for each submitted prompt: the prompt, then
    /// the hook's additional context, if any.
    pub fn got(&self) -> Vec<String> {
        let st = self.mu.lock().unwrap();
        st.submitted
            .iter()
            .enumerate()
            .map(|(i, p)| match st.context.get(i) {
                Some(c) if !c.is_empty() => format!("{p}\n{c}"),
                _ => p.clone(),
            })
            .collect()
    }

    pub fn state(&self) -> (i32, Vec<String>, String) {
        let st = self.mu.lock().unwrap();
        (
            st.enters,
            st.submitted.clone(),
            st.boxl.join("\n").trim().to_string(),
        )
    }

    pub fn offer_answers(&self) -> Vec<String> {
        self.mu.lock().unwrap().offer_answers.clone()
    }

    pub fn capture_now(&self) -> String {
        self.capture_styled("", 0).unwrap()
    }
}

impl TermState {
    /// read has Claude read what has arrived, unless it is stalled.
    fn read(&mut self) {
        if self.starting > 0 || self.stalled > 0 || self.unread.is_empty() {
            return;
        }
        let mut chunk = std::mem::take(&mut self.unread);
        if self.offer_on_read && !chunk.trim_matches('\r').is_empty() {
            self.offer = true;
            self.offer_on_read = false;
        }
        if self.offer {
            if chunk.contains('\r') {
                self.offer = false;
                self.offer_answers.push("Yes".into());
            }
            return; // what was typed went to the offer
        }
        if self.deaf_type {
            chunk = "\r".repeat(chunk.matches('\r').count());
            if chunk.is_empty() {
                return;
            }
        }
        if chunk.trim_matches('\r').is_empty() {
            for _ in chunk.chars() {
                self.enter();
            }
            return;
        }
        if chunk.contains('\r') {
            self.eat_enter = true; // the Enter came with the text: a newline
        }
        let (start, end) = (chunk.find(PASTE_START), chunk.find(PASTE_END));
        if let (Some(start), Some(end)) = (start, end)
            && end > start
        {
            // One paste, however the pty split it.
            chunk = format!(
                "{}{}{}",
                &chunk[..start],
                &chunk[start + PASTE_START.len()..end],
                &chunk[end + PASTE_END.len()..]
            );
        } else {
            let typed = chunk.trim_end_matches('\r');
            if typed.len() > PTY_QUEUE {
                // Read in pieces, each taken for a paste of its own, of which the
                // last is kept.
                let tb = typed.as_bytes();
                let from = (tb.len() - 1) / PTY_QUEUE * PTY_QUEUE;
                let rest = chunk[typed.len()..].to_string();
                chunk = format!("{}{rest}", String::from_utf8_lossy(&tb[from..]));
            }
        }
        let text = chunk.replace('\r', "\n");
        if text.trim_end_matches('\n').contains('\n') || text.len() > 200 {
            self.pastes += 1;
            let placeholder = format!(
                "[Pasted text #{} +{} lines]",
                self.pastes,
                text.matches('\n').count()
            );
            self.pasted
                .insert(placeholder.clone(), text.trim_end_matches('\n').to_string());
            self.boxl.push(placeholder);
            return;
        }
        if self.boxl.is_empty() {
            self.boxl = vec![String::new()];
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let last = self.boxl.len() - 1;
        self.boxl[last] += lines[0];
        self.boxl.extend(lines[1..].iter().map(|s| s.to_string()));
    }

    /// enter is Claude reading an Enter on its own.
    fn enter(&mut self) {
        if self.dialog_on > 0 && self.enters >= self.dialog_on {
            self.dialog = true;
        }
        if self.eat_enter {
            self.eat_enter = false;
        } else if self.deaf_enter || self.dialog {
        } else if !self.boxl.join("").trim().is_empty() {
            let mut prompt = self.boxl.join("\n");
            for (placeholder, text) in &self.pasted {
                prompt = prompt.replace(placeholder, text);
            }
            let p = prompt.trim_end_matches('\n').to_string();
            self.submitted.push(p.clone());
            self.hooks.push(p);
            self.boxl.clear();
        }
    }

    fn screen(&self) -> String {
        if self.strange {
            return format!("● ok\n\n> {}\n", self.boxl.join("\n"));
        }
        if self.dialog {
            return "Do you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel\n".to_string();
        }
        let rule = format!("\x1b[38;5;244m{}", "─".repeat(60));
        let mut lines: Vec<String> = vec!["● ok".into(), String::new(), rule.clone()];
        if self.offer {
            for l in [
                "  Teach auto mode about your environment?",
                "",
                "  ❯ 1. Yes",
                "    2. Not now",
                "    3. Don't show again",
                "",
                "  Enter to confirm · Esc to cancel",
            ] {
                lines.push(l.into());
            }
            lines.push(rule.clone());
        }
        let shown: Vec<String> = if self.boxl.is_empty() {
            vec![String::new()]
        } else {
            self.boxl.clone()
        };
        for (i, l) in shown.into_iter().enumerate() {
            lines.push(if i == 0 {
                format!("\x1b[39m❯ {l}")
            } else {
                format!("  {l}")
            });
        }
        lines.push(rule);
        lines.push("\x1b[39m  \x1b[38;5;220m⏵⏵ auto mode on\x1b[39m".into());
        lines.join("\n") + "\n"
    }
}

impl Ops for ClaudeTerminal {
    fn create_session(&self, _: &str, _: &HashMap<String, String>) -> Result<()> {
        bail!("claudeTerminal: create_session")
    }
    fn create_session_in(&self, _: &str, _: &str, _: &HashMap<String, String>) -> Result<()> {
        bail!("claudeTerminal: create_session_in")
    }
    fn kill_session(&self, _: &str) -> Result<()> {
        bail!("claudeTerminal: kill_session")
    }
    fn respawn_pane(&self, _: &str) -> Result<()> {
        bail!("claudeTerminal: respawn_pane")
    }
    fn session_exists(&self, _: &str) -> Result<bool> {
        Ok(true)
    }
    fn list_sessions(&self) -> Result<Vec<agent_termd::Session>> {
        bail!("claudeTerminal: list_sessions")
    }
    fn send_keys(&self, _: &str, text: &str) -> Result<()> {
        let mut st = self.mu.lock().unwrap();
        st.unread += text;
        st.read();
        Ok(())
    }
    /// Pastes text with paste brackets: Claude asks for them.
    fn paste_text(&self, _: &str, text: &str) -> Result<()> {
        let mut st = self.mu.lock().unwrap();
        st.unread += &format!("{PASTE_START}{text}{PASTE_END}");
        st.read();
        Ok(())
    }
    fn send_special_key(&self, _: &str, key: &str) -> Result<()> {
        {
            let mut st = self.mu.lock().unwrap();
            if key == "Escape" && st.offer {
                st.offer = false;
                st.offer_answers.push("Esc".into());
                return Ok(());
            }
            if key != "Enter" {
                bail!("unexpected key {key:?}");
            }
            st.enters += 1;
            st.unread += "\r";
            st.read();
        }
        self.run_hooks();
        Ok(())
    }
    fn capture(&self, session: &str, lines: i32) -> Result<String> {
        self.capture_styled(session, lines)
    }
    fn capture_styled(&self, _: &str, _: i32) -> Result<String> {
        let screen = {
            let mut st = self.mu.lock().unwrap();
            if st.starting > 0 {
                st.starting -= 1;
                " ▐▛███▜▌   Claude Code v2.1.319\n".to_string()
            } else {
                if st.stalled > 0 {
                    st.stalled -= 1;
                }
                st.read();
                st.screen()
            }
        };
        self.run_hooks();
        Ok(screen)
    }
    fn pane_pids(&self) -> Result<HashMap<i32, String>> {
        bail!("claudeTerminal: pane_pids")
    }
}

/// setupClaudeTerminal returns a service whose one agent, "worker", is a
/// Claude behind term, its store, and the agents deliverers were started for.
pub fn setup_claude_terminal(term: &Arc<ClaudeTerminal>, wait: Duration) -> ClaudeEnv {
    init();
    let dir = tempdir();
    let db = dir.path().join("test.db");
    let store = Arc::new(Store::open(&db).unwrap());
    let cfg = Config {
        backend: "claude".into(),
        database_path: db.display().to_string(),
        log_dir: dir.path().join("logs").display().to_string(),
        state_dir: dir.path().join("state").display().to_string(),
        session_prefix: "test-".into(),
        submit_wait: wait,
        ..Default::default()
    };
    let backends = Arc::new(agent_backend::new_registry());
    backends.register_factory("claude", |_| Box::new(namedagents::new_claude_backend()));
    store
        .create_agent(&Agent {
            name: "worker".into(),
            session_name: "test-worker".into(),
            backend: "claude".into(),
            created_at: Time::now(),
            ..Default::default()
        })
        .unwrap();
    let mut svc = AgentService::new(
        cfg,
        Arc::clone(&store),
        Arc::clone(term) as Arc<dyn Ops>,
        backends,
    );
    let delivering = Arc::new(Mutex::new(Vec::new()));
    let d = Arc::clone(&delivering);
    svc.set_deliverer_starter(move |name| {
        d.lock().unwrap().push(name.to_string());
        Ok(())
    });
    ClaudeEnv {
        svc: Arc::new(svc),
        store,
        delivering,
        _dir: dir,
    }
}

pub struct ClaudeEnv {
    pub svc: Arc<AgentService>,
    pub store: Arc<Store>,
    pub delivering: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

impl ClaudeEnv {
    pub fn set_state(&self, key: &str, value: &str) {
        self.store
            .set_state(&StateEntry {
                agent_name: "worker".into(),
                key: key.into(),
                value: value.into(),
                updated_at: Time::now(),
            })
            .unwrap();
    }
}

pub const SUBMIT_TEST_WAIT: Duration = Duration::from_millis(300);

/// turnStarted records that a turn of name's CLI has started, as the hook
/// reporting it does.
pub fn turn_started(store: &Store, name: &str) {
    let t = Time(chrono::Local::now().fixed_offset() - chrono::Duration::minutes(1));
    store
        .set_state(&StateEntry {
            agent_name: name.into(),
            key: agent_service::STATE_KEY_TURN_AT.into(),
            value: t.format_rfc3339_nano(),
            updated_at: t,
        })
        .unwrap();
}

/// promptSubmitHook runs Claude's UserPromptSubmit hook for worker, as the
/// agent's hook does, and returns its output.
pub fn prompt_submit_hook(svc: &Arc<AgentService>, spawn_id: &str) -> OnSubmit {
    let svc: Weak<AgentService> = Arc::downgrade(svc);
    let spawn_id = spawn_id.to_string();
    Box::new(move |prompt: &str| {
        let payload = serde_json::json!({"hook_event_name": "UserPromptSubmit", "prompt": prompt})
            .to_string();
        let svc = svc.upgrade()?;
        let (out, err) = svc.handle_hook("worker", &spawn_id, "", payload.as_bytes());
        if let Err(e) = err {
            panic!("UserPromptSubmit hook: {e:#}");
        }
        out
    })
}

// ---------------------------------------------------------------------------
// A throwaway bus (Go's natstest.Start): agent-busd, built from busd/ with
// the Go toolchain once per test binary, in a temp bus dir.

pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .expect("repo root")
        .to_path_buf()
}

fn has_go() -> bool {
    std::process::Command::new("go")
        .arg("version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Builds busd/ once and returns its path, or None without a Go toolchain.
pub fn busd_binary() -> Option<PathBuf> {
    static BUSD: OnceLock<Option<PathBuf>> = OnceLock::new();
    BUSD.get_or_init(|| {
        if !has_go() {
            eprintln!("skipping bus tests: no `go` on PATH to build agent-busd");
            return None;
        }
        init();
        // In the run's temp root, which goes at exit.
        let bin = ROOT.get().expect("a temp root").join("agent-busd");
        let out = std::process::Command::new("go")
            .args(["build", "-trimpath", "-o"])
            .arg(&bin)
            .arg(".")
            .current_dir(repo_root().join("busd"))
            .env("TMPDIR", temp_root())
            .envs(init().go_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .output()
            .expect("running go build");
        if !out.status.success() {
            panic!(
                "building agent-busd failed:\n{}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Some(bin)
    })
    .clone()
}

/// A single-host bus with its own identity, on a random port.
pub struct TestBus {
    pub host: String,
    pub dir: tempfile::TempDir,
    pub daemon: agent_bus::Daemon,
    pub conn: agent_bus::Conn,
}

impl TestBus {
    pub fn start() -> Option<TestBus> {
        let bin = busd_binary()?;
        let dir = tempdir();
        let host = "test".to_string();
        agent_bus::init_identity(dir.path(), &host).expect("bus identity");
        let daemon = agent_bus::start_daemon(agent_bus::DaemonConfig {
            dir: dir.path().to_path_buf(),
            host: host.clone(),
            client_port: -1,
            leaf_listen: String::new(),
            remote_ops: None,
            busd_path: Some(bin),
        })
        .expect("starting the test bus");
        let conn = agent_bus::connect_local(dir.path(), &host).expect("connecting to the test bus");
        Some(TestBus {
            host,
            dir,
            daemon,
            conn,
        })
    }

    pub fn connect(&self) -> agent_bus::Conn {
        agent_bus::connect_local(self.dir.path(), &self.host).expect("connecting to the test bus")
    }

    /// A publisher connected to this bus, as the CLI's is to the local bus.
    pub fn publisher(&self) -> agent_bus::Publisher {
        let dir = self.dir.path().to_path_buf();
        let host = self.host.clone();
        agent_bus::Publisher::new(move || agent_bus::connect_local(&dir, &host))
    }
}

/// Skips (returns from) a bus test when no bus can be built.
#[macro_export]
macro_rules! require_bus {
    () => {
        match $crate::common::TestBus::start() {
            Some(b) => b,
            None => {
                eprintln!("skipping: no agent-busd");
                return;
            }
        }
    };
}

/// Serves svc's RPC ops on bus as host, returning the server and a client on
/// a separate connection.
pub fn serve(
    bus: &TestBus,
    svc: &Arc<AgentService>,
    host: &str,
    ops: &[&str],
) -> (agent_bus::RpcServer, agent_bus::RpcClient) {
    let handler: Arc<dyn agent_bus::Handler> =
        Arc::new(agent_service::new_rpc_handler(Arc::clone(svc)));
    let server = agent_bus::serve_rpc(&bus.conn, host, handler, ops).expect("ServeRPC");
    let client = agent_bus::RpcClient::new(bus.connect(), Duration::from_secs(5));
    (server, client)
}
