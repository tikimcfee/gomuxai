//! Go `internal/service/agent_service.go`: the service, spawn, send, capture
//! and cleanup.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use regex::Regex;

use agent_backend::{Backend, Provider, ProviderConfig, Registry, TrustPrompter};
use agent_base::config::{self, Config};
use agent_base::gostd::{filepath, os as goos, strconv};
use agent_base::util;
use agent_store::{Agent, MESSAGE_STATUS_PENDING, Message, Store, Time};
use agent_termd::Ops;

use super::ctx::{Ctx, CtxExt};
use super::dialog::{dialog_from_detail, dialog_send_err};
use super::events::EventPublisher;
use super::handover::HandoverError;
use super::held::{SendOptions, SendResult};
use super::model::{model_refusal, validate_model};
use super::remove_all;
use super::slash::is_slash_command;
use super::status::{STATE_KEY_MODEL, STATE_KEY_SENT_AT, STATE_KEY_SESSION_ID, STATE_KEY_SPAWN_ID};
use super::submit::{STARTUP_POLL, UnsubmittedError};
use super::trust::{same_folder, trust_backend_name};

/// AgentStatus represents the current state of an agent. Like the Go string
/// type, it holds any text (a stored or remote status may be one this build
/// doesn't know); the known ones are the `AGENT_STATUS_*` constants, and the
/// default is "".
#[derive(
    Debug,
    Clone,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct AgentStatus(pub Cow<'static, str>);

impl AgentStatus {
    pub const fn from_static(s: &'static str) -> Self {
        AgentStatus(Cow::Borrowed(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl std::fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for AgentStatus {
    fn from(s: String) -> Self {
        AgentStatus(Cow::Owned(s))
    }
}

impl From<&str> for AgentStatus {
    fn from(s: &str) -> Self {
        AgentStatus(Cow::Owned(s.to_string()))
    }
}

impl PartialEq<str> for AgentStatus {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for AgentStatus {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

pub const AGENT_STATUS_IDLE: AgentStatus = AgentStatus::from_static("idle");
pub const AGENT_STATUS_PROCESSING: AgentStatus = AgentStatus::from_static("processing");
/// blocked on a human, e.g. a permission prompt
pub const AGENT_STATUS_WAITING: AgentStatus = AgentStatus::from_static("waiting");
pub const AGENT_STATUS_ERROR: AgentStatus = AgentStatus::from_static("error");
pub const AGENT_STATUS_ENDED: AgentStatus = AgentStatus::from_static("ended");
pub const AGENT_STATUS_UNKNOWN: AgentStatus = AgentStatus::from_static("unknown");

/// CleanupError represents errors that occurred during cleanup.
/// Cleanup continues through errors to maximize resource cleanup.
#[derive(Debug, Default)]
pub struct CleanupError {
    pub errors: Vec<anyhow::Error>,
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.errors.is_empty() {
            return f.write_str("cleanup completed with no errors");
        }
        let msgs: Vec<String> = self.errors.iter().map(|e| format!("{e:#}")).collect();
        write!(f, "cleanup errors: {}", msgs.join("; "))
    }
}

impl std::error::Error for CleanupError {}

impl CleanupError {
    pub fn add(&mut self, err: anyhow::Error) {
        self.errors.push(err);
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }
}

/// The function that starts a deliverer for an agent (see
/// [`AgentService::set_deliverer_starter`]).
pub type DelivererStarter = Box<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// AgentService handles agent lifecycle operations.
pub struct AgentService {
    pub(crate) cfg: Config,
    pub(crate) storage: Arc<Store>,
    pub(crate) tmux: Arc<dyn Ops>,
    pub(crate) backends: Arc<Registry>,
    /// None: no bus
    pub(crate) events: Option<Box<dyn EventPublisher>>,
    /// deliverer, if set, replaces how a deliverer process is started
    /// (start_deliverer).
    pub(crate) deliverer: Option<DelivererStarter>,
}

/// Creates a new agent service.
pub fn new_agent_service(
    cfg: Config,
    store: Arc<Store>,
    mux_client: Arc<dyn Ops>,
    backends: Arc<Registry>,
) -> AgentService {
    AgentService::new(cfg, store, mux_client, backends)
}

/// SpawnOptions holds options for spawning an agent.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub name: String,
    pub backend: String,
    /// Path to task file (read from filesystem)
    pub task_file: String,
    /// Task content directly (from DB task repository)
    pub task_content: String,
    /// Pass --dangerously-skip-permissions to Claude / Agy
    pub skip_permissions: bool,
    /// Confirm the CLI's workspace trust prompt on launch
    pub trust_workspace: bool,
    /// agent that requested the spawn ("" for a user); name@host from another host
    pub spawned_by: String,
    /// Model is the model the CLI runs on (its --model), "" for its
    /// default; backends that take one only (backend.ModelChooser).
    pub model: String,
    /// Cwd is the absolute directory the agent starts in; "" for this
    /// process's working directory.
    pub cwd: String,
    /// Resume is the session the agent continues (Claude's session id, agy's
    /// conversation id) instead of starting a new one; claude and agy only.
    pub resume: String,
    /// Tool rules beyond the pre-allowed agent commands, for backends that
    /// take them (backend.CommandAllower's allow_tools): run without a prompt
    /// (Claude's "Read(//dir/**)"), and refused ("Edit"). A dream's agent is
    /// spawned with them; resume doesn't carry them.
    pub allow_tools: Vec<String>,
    pub deny_tools: Vec<String>,
}

/// The Go SpawnOptions' unexported fields, which only resume and adopt set:
/// kept apart so SpawnOptions can be built with `..Default::default()`
/// outside this crate.
#[derive(Debug, Clone, Default)]
pub(crate) struct SpawnInternal {
    /// replacing is the ended agent's record a resume (AgentService::resume)
    /// takes over, None for a new agent.
    pub(crate) replacing: Option<Agent>,
    /// existingSession is the person's own tmux session Adopt records the
    /// agent in (store_agent marks the record Adopted); Spawn never takes it.
    pub(crate) existing_session: String,
    /// sessionCwd is the adopted session's working directory, for the
    /// stored agent's Cwd.
    pub(crate) session_cwd: String,
}

/// agentNamePattern keeps names safe as tmux session names and as path
/// components: cleanup removes .gomuxai/agents/<name>, so "../.." must not pass.
static AGENT_NAME_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]*$").unwrap());

/// Reports whether name can be used for a new agent.
pub fn validate_agent_name(name: &str) -> Result<()> {
    if !AGENT_NAME_PATTERN.is_match(name) {
        bail!(
            "invalid agent name {}: use letters, digits, '-' and '_', starting with a letter or digit",
            strconv::quote(name)
        );
    }
    Ok(())
}

/// hookedWait bounds how long a new agent's first message waits for its
/// hook to report the session.
const HOOKED_WAIT: Duration = Duration::from_secs(15);

/// cleanupExitGrace is how long cleanup lets an idle agent exit on its own.
const CLEANUP_EXIT_GRACE: Duration = Duration::from_millis(1500);

// How long spawn looks for a workspace trust prompt with --trust-workspace.
const TRUST_PROMPT_ATTEMPTS: usize = 8;
const TRUST_PROMPT_POLL: Duration = Duration::from_millis(500);
// After answering one, how long spawn watches for another trust prompt about
// the same folder under another name, and how many it answers in all.
const TRUST_PROMPT_FOLLOW_UP_POLLS: usize = 3;
const TRUST_PROMPT_MAX_ANSWERS: usize = 3;
// How many polls an answered prompt's frame may stay unchanged before it is
// taken for a prompt still (or again) asking.
const TRUST_PROMPT_STALE_POLLS: usize = 2;

/// maxOneLine bounds one_line's result.
const MAX_ONE_LINE: usize = 120;

/// Shortens a status detail to one line, for messages that quote it.
pub fn one_line(s: &str) -> String {
    let runes: Vec<char> = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .collect();
    if runes.len() > MAX_ONE_LINE {
        let mut out: String = runes[..MAX_ONE_LINE - 1].iter().collect();
        out.push('…');
        return out;
    }
    runes.into_iter().collect()
}

impl AgentService {
    /// Creates a new agent service.
    pub fn new(
        cfg: Config,
        store: Arc<Store>,
        mux_client: Arc<dyn Ops>,
        backends: Arc<Registry>,
    ) -> AgentService {
        AgentService {
            cfg,
            storage: store,
            tmux: mux_client,
            backends,
            events: None,
            deliverer: None,
        }
    }

    /// The service's config.
    pub fn config(&self) -> &Config {
        &self.cfg
    }

    /// The service's config, to change (the Go service holds a pointer its
    /// callers can change).
    pub fn config_mut(&mut self) -> &mut Config {
        &mut self.cfg
    }

    /// The store the service records in.
    pub fn storage(&self) -> &Arc<Store> {
        &self.storage
    }

    /// The terminal multiplexer the service drives.
    pub fn tmux(&self) -> &Arc<dyn Ops> {
        &self.tmux
    }

    /// The backends the service knows.
    pub fn backends(&self) -> &Arc<Registry> {
        &self.backends
    }

    /// Spawn creates and starts a new agent.
    pub fn spawn(&self, ctx: &Ctx, opts: SpawnOptions) -> Result<Agent> {
        self.spawn_internal(ctx, opts, SpawnInternal::default())
    }

    /// spawn, with the Go SpawnOptions' unexported fields.
    pub(crate) fn spawn_internal(
        &self,
        ctx: &Ctx,
        opts: SpawnOptions,
        internal: SpawnInternal,
    ) -> Result<Agent> {
        validate_agent_name(&opts.name)?;
        if !opts.resume.is_empty() && opts.backend != "claude" && opts.backend != "agy" {
            bail!(
                "--resume continues a claude or agy session; backend {} can't resume one",
                opts.backend
            );
        }
        if !opts.cwd.is_empty() {
            check_cwd(&opts.cwd)?;
        }
        if !opts.model.is_empty() {
            validate_model(&opts.model)?;
        }

        // A fresh provider, configured for this agent. Custom JSON backends
        // (--backend-config) have no factory, and spawn from their Backend.
        let mut cwd = opts.cwd.clone();
        if cwd.is_empty() {
            cwd = goos::getwd().unwrap_or_default();
        }
        let provider = match self.backends.create_provider(
            &opts.backend,
            ProviderConfig {
                skip_permissions: opts.skip_permissions,
                agent_dir: self.cfg.agent_dir(&opts.name),
                cwd,
                resume: opts.resume.clone(),
            },
        ) {
            Ok(p) => p,
            Err(_) => {
                let be = self.backends.get(&opts.backend)?;
                if !opts.model.is_empty() {
                    return Err(model_refusal(&opts.backend));
                }
                return self.spawn_with_backend(ctx, &opts, be.as_ref());
            }
        };
        if !opts.model.is_empty() && provider.as_model_chooser().is_none() {
            return Err(model_refusal(&opts.backend));
        }
        self.spawn_with_provider(ctx, &opts, &internal, provider.as_ref())
    }

    /// spawnWithProvider spawns an agent on a built-in backend.
    fn spawn_with_provider(
        &self,
        ctx: &Ctx,
        opts: &SpawnOptions,
        internal: &SpawnInternal,
        provider: &dyn Provider,
    ) -> Result<Agent> {
        let session_name = if internal.replacing.is_some() {
            self.create_session(&opts.name, &opts.cwd)?
        } else {
            self.new_session(&opts.name, &opts.cwd)?
        };

        // Create the agent record before launching: backend hooks report status
        // against it as soon as the CLI starts.
        let agent = match self.store_agent(opts, internal, &session_name) {
            Ok(a) => a,
            Err(e) => {
                // The session was made for this spawn; it goes with it (a ctx
                // canceled mid-spawn, say, fails here as well as later).
                let _ = self.tmux.kill_session(&session_name);
                return Err(e);
            }
        };
        let abort = |err: anyhow::Error| -> Result<Agent> {
            let _ = self.tmux.kill_session(&session_name);
            if internal.replacing.is_none() {
                // Even when ctx is why it failed (a bus stopping mid-spawn
                // cancels its requests), the record must go with the session.
                let _ = self.storage.delete_agent(&opts.name);
                let _ = remove_all(&self.cfg.agent_dir(&opts.name));
            } // a failed resume leaves the agent ended, to resume or clean up
            Err(err)
        };

        if internal.replacing.is_some() {
            // Its old run is closed; what described it would mislead the new one.
            if let Err(e) = self.clear_run_state(&opts.name) {
                return abort(e);
            }
        }

        let init_msg = init_message(opts, |f| provider.init_message(f));
        let (launch_cmd, prompt_in_launch) = match self.launch_command(opts, provider, &init_msg) {
            Ok(v) => v,
            Err(e) => return abort(e),
        };
        if !launch_cmd.is_empty() {
            let run_cmd = match self.write_launch_script(&opts.name, &launch_cmd) {
                Ok(v) => v,
                Err(e) => return abort(e),
            };
            if prompt_in_launch {
                self.mark_sent(&opts.name);
            }
            if let Err(e) = self.send_line(&session_name, &run_cmd) {
                return abort(e.context("launching backend"));
            }
            if let Err(e) = self.wait_for_startup(ctx) {
                return abort(e);
            }

            // Confirm the workspace trust prompt only when opted in via --trust-workspace
            // (trusting a repo also enables its own hooks and settings).
            if let Some(tp) = provider.as_trust_prompter()
                && opts.trust_workspace
            {
                if let Err(e) = self.confirm_trust(ctx, &session_name, &agent.cwd, tp) {
                    return abort(e);
                }
                if let Err(e) = self.wait_for_startup(ctx) {
                    return abort(e);
                }
            }
        }

        // Send initial message if it was not already passed on the launch command line
        if !prompt_in_launch && !init_msg.is_empty() {
            self.mark_sent(&opts.name);
            // At a trust prompt or another dialog, the message would be lost
            // (Claude's ignore a paste) or its Enter answer it: hold it.
            let held = match self.hold_first_message(ctx, opts, &agent, &init_msg) {
                Ok(h) => h,
                Err(e) => return abort(e),
            };
            if !held && let Err(e) = self.type_first_message(ctx, &agent, &init_msg) {
                return abort(e.context("sending init message"));
            }
        }

        Ok(agent)
    }

    /// typeFirstMessage types a new agent's first message. One its CLI would take
    /// as a paste goes as a short line with its text handed over by hook, as
    /// send types it (see handover.rs), once the hook has reported the session.
    /// A handover the hook missed leaves the agent with the short line: spawn
    /// goes on, and says so on stderr.
    fn type_first_message(&self, ctx: &Ctx, agent: &Agent, msg: &str) -> Result<()> {
        let paste = match self.provider(agent) {
            Ok(p) => p
                .as_prompt_handover()
                .is_some_and(|ph| ph.typed_as_paste(msg)),
            Err(_) => false,
        };
        if !paste {
            return self.paste_line(&agent.session_name, msg);
        }
        if !self.await_hooked(ctx, agent) {
            return self.paste_line(&agent.session_name, msg);
        }
        match self.type_handover(ctx, agent, msg, true) {
            Ok(_) => Ok(()),
            Err(err)
                if err.downcast_ref::<HandoverError>().is_some()
                    || err.downcast_ref::<UnsubmittedError>().is_some() =>
            {
                eprintln!("{}'s first message: {err:#}", agent.name);
                Ok(())
            }
            Err(err) => Err(err),
        }
    }

    /// awaitHooked waits until the agent's hook has reported its session
    /// (holds_messages), up to HOOKED_WAIT.
    fn await_hooked(&self, ctx: &Ctx, agent: &Agent) -> bool {
        let deadline = Instant::now() + HOOKED_WAIT;
        while !self.holds_messages(agent) {
            if Instant::now() > deadline || ctx.sleep(STARTUP_POLL).is_err() {
                return false;
            }
        }
        true
    }

    /// spawnWithBackend spawns an agent on a custom JSON backend. It only knows a
    /// launch command, typed into the session as is: no hooks, briefing, or
    /// launch script.
    fn spawn_with_backend(
        &self,
        ctx: &Ctx,
        opts: &SpawnOptions,
        be: &dyn Backend,
    ) -> Result<Agent> {
        let session_name = self.new_session(&opts.name, &opts.cwd)?;
        let abort = |err: anyhow::Error| -> Result<Agent> {
            let _ = self.tmux.kill_session(&session_name);
            Err(err)
        };

        if let Err(e) = self.send_line(&session_name, &be.launch_command()) {
            return abort(e.context("launching backend"));
        }
        if let Err(e) = self.wait_for_startup(ctx) {
            return abort(e);
        }
        let init_msg = init_message(opts, |f| be.init_message(f));
        if !init_msg.is_empty()
            && let Err(e) = self.paste_line(&session_name, &init_msg)
        {
            return abort(e.context("sending init message"));
        }

        self.store_agent(opts, &SpawnInternal::default(), &session_name)
    }

    /// newSession creates a new agent's session, in dir ("" for this process's
    /// working directory), once the name is known to be free.
    fn new_session(&self, name: &str, dir: &str) -> Result<String> {
        self.check_agent_name_free(name)?;
        let session = self.cfg.session_name(name);
        if self.tmux.session_exists(&session).unwrap_or(false) {
            bail!(
                "session {session} already exists, but no agent {name} does (a leftover session?): pick another name, or agent session kill {session} first"
            );
        }
        self.create_session(name, dir)
    }

    pub(crate) fn check_agent_name_free(&self, name: &str) -> Result<()> {
        let existing = self
            .storage
            .get_agent(name)
            .context("checking existing agent")?;
        if let Some(existing) = existing {
            let mut hint = String::new();
            if let Ok((info, _)) = self.resolve_status(name)
                && info.status == super::AGENT_STATUS_ENDED
            {
                hint = format!(" It has ended: agent resume {name} continues its session.");
            }
            bail!(
                "agent {name} already exists (session {}, created {}): pick another name, or agent cleanup {name} first.{hint}",
                existing.session_name,
                existing.created_at.0.format("%Y-%m-%d %H:%M")
            );
        }
        Ok(())
    }

    /// prepareExistingSession checks that agent name and existing sessionName are
    /// both free to adopt.
    pub(crate) fn prepare_existing_session(&self, name: &str, session_name: &str) -> Result<()> {
        self.check_agent_name_free(name)?;
        let exists = self
            .tmux
            .session_exists(session_name)
            .with_context(|| format!("checking session {}", strconv::quote(session_name)))?;
        if !exists {
            bail!("session {} not found", strconv::quote(session_name));
        }
        let agents = self.storage.list_agents().context("listing agents")?;
        for a in &agents {
            if a.session_name == session_name {
                bail!(
                    "session {session_name} is already registered as agent {}",
                    a.name
                );
            }
        }
        Ok(())
    }

    /// createSession creates an agent's session in dir ("" for this process's
    /// working directory).
    pub(crate) fn create_session(&self, name: &str, dir: &str) -> Result<String> {
        let mut ns = self.cfg.termd_namespace.clone();
        if ns.is_empty() {
            ns = config::DEFAULT_TERMD_NAMESPACE.to_string();
        }
        let mut env: HashMap<String, String> = HashMap::from([
            ("AGENT_NAME".to_string(), name.to_string()),
            // Hooks publish status events under this host; without it they'd
            // fall back to the hostname, which AGENT_HOST may override.
            ("AGENT_HOST".to_string(), self.cfg.host.clone()),
            ("GOMUXAI_TERMD_NS".to_string(), ns),
        ]);
        if !self.cfg.termd_binary.is_empty() {
            env.insert("G3D_TERMD_BIN".into(), self.cfg.termd_binary.clone());
        }
        if !self.cfg.g3d_binary.is_empty() {
            env.insert("G3D_BIN".into(), self.cfg.g3d_binary.clone());
        }
        if !self.cfg.agent_binary.is_empty() {
            // So the agent (and its hooks) can just run `agent`.
            env.insert(
                "PATH".into(),
                format!(
                    "{}:{}",
                    filepath::dir(&self.cfg.agent_binary),
                    std::env::var("PATH").unwrap_or_default()
                ),
            );
        }
        let session_name = self.cfg.session_name(name);
        self.tmux
            .create_session_in(&session_name, dir, &env)
            .context("creating session")?;
        // Clear any snapshot log from an earlier cleaned-up run of the same name
        // so `agent logs --terminal` reads live scrollback while this session runs.
        let _ = std::fs::remove_file(self.cfg.log_path(name));
        Ok(session_name)
    }

    /// storeAgent records an agent, killing a newly created session on failure.
    pub(crate) fn store_agent(
        &self,
        opts: &SpawnOptions,
        internal: &SpawnInternal,
        session_name: &str,
    ) -> Result<Agent> {
        let mut cwd = internal.session_cwd.clone(); // an adopted session's
        if cwd.is_empty() {
            cwd = opts.cwd.clone(); // the session started here
        }
        if cwd.is_empty() {
            cwd = goos::getwd().unwrap_or_default();
        }
        let mut agent = Agent {
            cwd,
            name: opts.name.clone(),
            session_name: session_name.to_string(),
            backend: opts.backend.clone(),
            task_file: opts.task_file.clone(),
            log_file: self.cfg.log_path(&opts.name),
            spawned_by: opts.spawned_by.clone(),
            adopted: !internal.existing_session.is_empty(),
            created_at: Time::now(),
        };
        let res = match &internal.replacing {
            Some(replacing) => {
                agent.task_file = replacing.task_file.clone();
                self.storage.replace_agent(&agent)
            }
            None => self.storage.create_agent(&agent),
        };
        if let Err(e) = res {
            if internal.existing_session.is_empty() {
                let _ = self.tmux.kill_session(session_name);
            }
            return Err(e.context("storing agent"));
        }
        Ok(agent)
    }

    /// launchCommand builds the command that starts a built-in backend's CLI,
    /// wired up with each optional capability the backend has. The bool
    /// reports whether init_msg went into the command too.
    fn launch_command(
        &self,
        opts: &SpawnOptions,
        provider: &dyn Provider,
        init_msg: &str,
    ) -> Result<(String, bool)> {
        let name = &opts.name;
        let mut cmd = provider.launch_command();
        if let Some(gh) = provider.as_global_hooker()
            && !self.cfg.agent_binary.is_empty()
        {
            // Its hooks are every session's (agent hook), and place it by its
            // pane, so no spawn id is needed: a dying predecessor's CLI isn't
            // under this agent's pane.
            gh.hooks_installed(&self.cfg.agent_binary)?;
            let mut session_id = uuid::Uuid::new_v4().to_string();
            cmd = gh.launch_command_with_session(&session_id);
            if !opts.resume.is_empty() {
                session_id = opts.resume.clone();
            }
            self.set_state(name, STATE_KEY_SESSION_ID, &session_id)
                .with_context(|| format!("storing {STATE_KEY_SESSION_ID}"))?;
        } else if let Some(hp) = provider.as_spawn_hooker()
            && !self.cfg.agent_binary.is_empty()
        {
            // The spawn-time session id doubles as the spawn id: it is unique per
            // spawn, while _session_id itself moves on /clear and /resume. A
            // resumed agent's session is the one it continues.
            let spawn_id = uuid::Uuid::new_v4().to_string();
            let report_cmd = format!(
                "{} report --hook --as {} --spawn-id {}",
                util::shell_quote(&self.cfg.agent_binary),
                util::shell_quote(name),
                util::shell_quote(&spawn_id)
            );
            cmd = hp.launch_command_with_hooks(&report_cmd, &spawn_id);
            let mut session_id = spawn_id.clone();
            if !opts.resume.is_empty() {
                session_id = opts.resume.clone();
            }
            for (key, value) in [
                (STATE_KEY_SPAWN_ID, &spawn_id),
                (STATE_KEY_SESSION_ID, &session_id),
            ] {
                self.set_state(name, key, value)
                    .with_context(|| format!("storing {key}"))?;
            }
        }
        if let Some(sn) = provider.as_session_namer()
            && !name.is_empty()
        {
            cmd = sn.with_session_name(&cmd, name);
        }
        if let Some(mc) = provider.as_model_chooser()
            && !opts.model.is_empty()
        {
            // Recorded for resume, which relaunches on it.
            self.set_state(name, STATE_KEY_MODEL, &opts.model)
                .with_context(|| format!("storing {STATE_KEY_MODEL}"))?;
            cmd = mc.with_model(&cmd, &opts.model);
        }
        if let Some(ip) = provider.as_instructions_provider() {
            let path = self.write_instructions(name, &opts.spawned_by, provider.name())?;
            cmd = ip.with_instructions(&cmd, &path)?;
        }
        if let Some(ca) = provider.as_command_allower() {
            cmd = ca.allow_tools(
                &cmd,
                &self.allowed_command_prefixes(),
                &opts.allow_tools,
                &opts.deny_tools,
            );
        }
        if let Some(pfp) = provider.as_prompt_flag_provider()
            && !init_msg.is_empty()
        {
            return Ok((pfp.append_prompt_flag(&cmd, init_msg), true));
        }
        Ok((cmd, false))
    }

    /// confirmTrust answers the CLI's workspace trust prompt. The prompt can take
    /// a few seconds to appear, and each CLI answers it differently; if it never
    /// shows up, spawn goes on without it.
    ///
    /// The CLI asks about the folder as its own process sees it, symlinks
    /// resolved (macOS's /var/tmp/x is /private/var/tmp/x), and may ask
    /// again about another name of the same folder (the one it was started
    /// with) once the first is answered. So after an answer spawn watches a
    /// little longer, and answers another prompt too if its folder is dir (or
    /// one already answered) under any name (same_folder); a prompt about
    /// any other folder is left for a person (agent approve).
    fn confirm_trust(
        &self,
        ctx: &Ctx,
        session_name: &str,
        dir: &str,
        tp: &dyn TrustPrompter,
    ) -> Result<()> {
        let mut answered: Vec<String> = Vec::new();
        let mut answered_screen = String::new();
        let mut unchanged = 0;
        let mut polls_left = TRUST_PROMPT_ATTEMPTS;
        while polls_left > 0 {
            polls_left -= 1;
            if let Ok(screen) = self.tmux.capture(session_name, 0)
                && let Some(folder) = tp.trust_prompt(&screen)
                && let Some(keys) = tp.trust_keys(&screen)
            {
                // The frame just answered may not be redrawn yet: answer it
                // again only once it has stayed a while (the keys didn't
                // take, or the CLI asks the same again).
                let stale = !answered.is_empty() && screen == answered_screen && {
                    unchanged += 1;
                    unchanged < TRUST_PROMPT_STALE_POLLS
                };
                if !stale {
                    let ours = answered.is_empty()
                        || folder.trim().is_empty()
                        || same_folder(&folder, dir)
                        || answered.iter().any(|f| same_folder(&folder, f));
                    if !ours {
                        return Ok(());
                    }
                    for key in &keys {
                        self.tmux
                            .send_special_key(session_name, key)
                            .context("confirming workspace trust")?;
                    }
                    answered.push(folder);
                    if answered.len() >= TRUST_PROMPT_MAX_ANSWERS {
                        return Ok(());
                    }
                    answered_screen = screen;
                    unchanged = 0;
                    polls_left = TRUST_PROMPT_FOLLOW_UP_POLLS;
                }
            }
            ctx.sleep(TRUST_PROMPT_POLL)?;
        }
        Ok(())
    }

    /// waitForStartup gives a just-launched CLI the configured time to start.
    pub(crate) fn wait_for_startup(&self, ctx: &Ctx) -> Result<()> {
        if self.cfg.startup_delay.is_zero() {
            return Ok(());
        }
        ctx.sleep(self.cfg.startup_delay)?;
        Ok(())
    }

    /// sendLine types text into a session and presses Enter: a command line
    /// for its shell, or a CLI's exit command.
    pub(crate) fn send_line(&self, session_name: &str, text: &str) -> Result<()> {
        self.tmux.send_keys(session_name, text)?;
        self.tmux.send_special_key(session_name, "Enter")
    }

    /// pasteLine types a message into a session as a paste (see
    /// Client::paste_text), so a long one arrives whole, and presses Enter.
    pub(crate) fn paste_line(&self, session_name: &str, text: &str) -> Result<()> {
        self.tmux.paste_text(session_name, text)?;
        self.tmux.send_special_key(session_name, "Enter")
    }

    /// Send delivers a message to an agent. An agent that holds messages
    /// (Claude, spawned with hooks) gets it typed in if it is at its prompt, and
    /// otherwise holds it for the end of its turn, when its Stop hook hands it
    /// over (see held.rs). Other agents, and opts.now, get it typed in now, even
    /// mid-turn.
    ///
    /// Typing refuses while the agent is at a dialog: Enter there answers the
    /// dialog rather than sending a message (it approves a permission prompt,
    /// or picks "No, exit" at Claude's trust prompt), so a message must never be
    /// how a dialog gets answered. Use approve/deny, or send_unchecked to
    /// override. Send also refuses an agent whose CLI has exited, since the
    /// message would run in the shell left behind. The result says what it
    /// found the agent doing and what it did.
    pub fn send(
        &self,
        ctx: &Ctx,
        name: &str,
        message: &str,
        opts: SendOptions,
    ) -> Result<SendResult> {
        let agent = self.get_agent(name)?;
        // A hook can only hand over text, which wouldn't run a slash command.
        if !opts.now && !is_slash_command(message) && self.holds_messages(&agent) {
            return self.send_held(ctx, &agent, message, &opts.from);
        }
        let status = self.check_can_send(&agent)?;
        let mut res = SendResult {
            status,
            ..Default::default()
        };
        let unchecked = self.send_message(ctx, name, message, true)?;
        res.typed = 1;
        res.unchecked = unchecked;
        Ok(res)
    }

    /// checkCanSend fails if the agent's session has ended, is waiting on a
    /// prompt, or shows a dialog on screen. It returns the agent's status.
    pub(crate) fn check_can_send(&self, agent: &Agent) -> Result<AgentStatus> {
        let mut status = AGENT_STATUS_UNKNOWN;
        if let Ok(info) = self.status_info(&agent.name) {
            status = info.status.clone();
            if info.status == AGENT_STATUS_WAITING {
                let mut prompt = "a prompt".to_string();
                if !info.detail.is_empty() {
                    prompt = format!("a prompt ({})", one_line(&info.detail));
                }
                if info.is_trust_prompt() {
                    return Err(trust_prompt_send_err(agent));
                }
                if info.is_dialog() {
                    return Err(dialog_send_err(agent, &dialog_from_detail(&info.detail)));
                }
                if info.detail.starts_with("question: ") {
                    bail!(
                        "agent {} is waiting on {prompt}, where the message's Enter would pick an answer: {} (a person can send anyway with --force)",
                        agent.name,
                        answer_question_hint(agent)
                    );
                }
                bail!(
                    "agent {} is waiting on {prompt}, where the message's Enter would answer it: see it with agent status {} -v, then agent approve {} or agent deny {} (a person can send anyway with --force)",
                    agent.name,
                    agent.name,
                    agent.name,
                    agent.name
                );
            } else if info.status == AGENT_STATUS_ENDED {
                return Err(exited_err(agent));
            }
        }
        self.check_screen(agent)?;
        Ok(status)
    }

    /// checkScreen fails if the agent shows a permission, question, trust or
    /// other dialog on screen, where a message's Enter would answer it.
    pub(crate) fn check_screen(&self, agent: &Agent) -> Result<()> {
        let Ok(provider) = self.provider(agent) else {
            return Ok(()); // custom backend: nothing to recognize dialogs with
        };
        let Ok(screen) = self.tmux.capture(&agent.session_name, 0) else {
            return Ok(()); // send_unchecked reports a missing session
        };
        if provider
            .as_permission_responder()
            .is_some_and(|pr| pr.permission_prompt_visible(&screen))
        {
            bail!(
                "agent {} has a permission prompt on screen, where the message's Enter would answer it: agent approve {} or agent deny {} first (a person can send anyway with --force)",
                agent.name,
                agent.name,
                agent.name
            );
        }
        if provider
            .as_question_prompter()
            .is_some_and(|qp| qp.question_prompt_visible(&screen))
        {
            bail!(
                "agent {} has a question for the user on screen, where the message's Enter would pick an answer: {} (a person can send anyway with --force)",
                agent.name,
                answer_question_hint(agent)
            );
        }
        if let Some(tp) = provider.as_trust_prompter()
            && tp.trust_prompt(&screen).is_some()
        {
            return Err(trust_prompt_send_err(agent));
        }
        if let Some(dr) = provider.as_dialog_reader()
            && let Some(d) = dr.dialog(&screen)
        {
            return Err(dialog_send_err(agent, &d));
        }
        Ok(())
    }

    /// SendUnchecked types a message into an agent without checking for dialogs.
    /// For a backend whose typed messages need checking (see submit.rs), it
    /// returns why it couldn't check this one was submitted, or "" if it did.
    pub fn send_unchecked(&self, ctx: &Ctx, name: &str, message: &str) -> Result<String> {
        self.send_message(ctx, name, message, false)
    }

    /// sendMessage is send_unchecked; with dialogs, a dialog that comes up while
    /// it waits for a starting CLI's input box stops it (see type_message).
    pub(crate) fn send_message(
        &self,
        ctx: &Ctx,
        name: &str,
        message: &str,
        dialogs: bool,
    ) -> Result<String> {
        let agent = self.get_agent(name)?;

        // Check session exists
        let exists = self
            .tmux
            .session_exists(&agent.session_name)
            .context("checking session")?;
        if !exists {
            return Err(session_gone(&agent));
        }

        self.mark_sent(name);
        self.type_message(ctx, &agent, message, dialogs)
    }

    /// markSent records that a message is about to be typed into the agent
    /// (STATE_KEY_SENT_AT). It's written first, so the turn the message starts is
    /// always later. Best-effort: without it, wait --next only misses a turn
    /// that ends before the wait starts, as it would anyway.
    pub(crate) fn mark_sent(&self, name: &str) {
        let _ = self.set_state(name, STATE_KEY_SENT_AT, &Time::now().format_rfc3339_nano());
    }

    /// Queue queues a message for delivery when agent is idle.
    pub fn queue(&self, sender_id: &str, receiver_id: &str, content: &str) -> Result<()> {
        self.get_agent(receiver_id)?;

        let mut msg = Message {
            sender_id: sender_id.to_string(),
            receiver_id: receiver_id.to_string(),
            content: content.to_string(),
            status: MESSAGE_STATUS_PENDING,
            created_at: Time::now(),
            ..Default::default()
        };

        self.storage.create_message(&mut msg)
    }

    /// Capture captures the current output from an agent's session.
    pub fn capture(&self, name: &str, lines: i32, raw: bool) -> Result<String> {
        let agent = self.get_agent(name)?;

        if let Ok(false) = self.tmux.session_exists(&agent.session_name) {
            return Err(session_gone(&agent));
        }
        let mut lines = lines;
        if lines <= 0 {
            lines = self.cfg.default_scrollback as i32;
        }

        // Plain text by default; raw keeps the colors and styling.
        if raw {
            return self.tmux.capture_styled(&agent.session_name, lines);
        }
        self.tmux.capture(&agent.session_name, lines)
    }

    /// List returns all managed agents.
    pub fn list(&self) -> Result<Vec<Agent>> {
        self.storage.list_agents()
    }

    /// Cleanup removes an agent and its resources.
    /// Returns a CleanupError if any non-fatal errors occurred during cleanup.
    pub fn cleanup(&self, name: &str) -> Result<()> {
        let agent = self.get_agent(name)?;

        let mut cleanup_errs = CleanupError::default();

        // Snapshot scrollback first (unless adopted), so `agent logs --terminal`
        // works after cleanup, as cleanup says: termd holds it in-daemon, and the
        // exit command below can end the pane, and the session with it, before
        // a later snapshot. (The Go took it after the exit, and so saved no log
        // for an idle agent whose CLI quit.)
        if !agent.adopted && matches!(self.tmux.session_exists(&agent.session_name), Ok(true)) {
            let lines = self.cfg.default_scrollback.max(2000) as i32;
            if let Ok(out) = self.tmux.capture(&agent.session_name, lines)
                && !out.is_empty()
                && goos::mkdir_all(&self.cfg.log_dir, 0o700).is_ok()
            {
                let _ = goos::write_file(&self.cfg.log_path(name), out.as_bytes(), 0o600);
            }
        }

        // Ask an idle spawned agent to exit cleanly before the session is killed: its
        // hooks then record the exit, and some CLIs lose saved state when killed
        // (agy's maintainers advise /exit). Only when idle (and not an adopted
        // pre-existing user session, which cleanup leaves running): typed into a
        // dialog, the exit command's Enter would answer the dialog instead.
        if !agent.adopted
            && let Ok(info) = self.status_info(name)
            && info.status == AGENT_STATUS_IDLE
            && let Ok(p) = self.provider(&agent)
            && !p.exit_command().is_empty()
            && self
                .send_line(&agent.session_name, &p.exit_command())
                .is_ok()
        {
            std::thread::sleep(CLEANUP_EXIT_GRACE);
        }

        // (Unless adopted) kill the session, unless it's already gone
        match self.tmux.session_exists(&agent.session_name) {
            Err(e) => cleanup_errs.add(e.context("checking session exists")),
            Ok(true) if !agent.adopted => {
                if let Err(e) = self.tmux.kill_session(&agent.session_name) {
                    cleanup_errs.add(e.context("killing session"));
                }
            }
            Ok(_) => {}
        }

        // Delete messages
        if let Err(e) = self.storage.delete_messages_by_agent(name) {
            cleanup_errs.add(e.context("deleting messages"));
        }

        // Remove the agent's directory (briefing, launch script, agy hooks)
        let dir = self.cfg.agent_dir(name);
        if let Err(e) = remove_all(&dir) {
            cleanup_errs.add(
                anyhow!(goos::PathError::new("unlinkat", &dir, e)).context("removing agent dir"),
            );
        }

        // Delete agent record - this is the critical operation
        self.storage
            .delete_agent(name)
            .context("deleting agent record")?;

        // Return cleanup errors if any occurred (non-fatal)
        if cleanup_errs.has_errors() {
            return Err(cleanup_errs.into());
        }

        Ok(())
    }
}

/// checkCwd says why dir can't be a new agent's working directory, or Ok.
pub(crate) fn check_cwd(dir: &str) -> Result<()> {
    if !filepath::is_abs(dir) {
        bail!(
            "working directory {} is not an absolute path",
            strconv::quote(dir)
        );
    }
    let info = std::fs::metadata(dir)
        .map_err(|e| anyhow!(goos::PathError::new("stat", dir, e)))
        .with_context(|| format!("working directory {dir}"))?;
    if !info.is_dir() {
        bail!("working directory {dir} is not a directory");
    }
    Ok(())
}

/// initMessage is a new agent's first message: the task itself, or the
/// backend's request to read the task file.
fn init_message(opts: &SpawnOptions, from_file: impl Fn(&str) -> String) -> String {
    if !opts.task_content.is_empty() {
        return opts.task_content.clone();
    }
    if !opts.task_file.is_empty() {
        return from_file(&opts.task_file);
    }
    String::new()
}

/// trustPromptSendErr is the error for typing into an agent at its CLI's
/// workspace trust prompt.
pub(crate) fn trust_prompt_send_err(agent: &Agent) -> anyhow::Error {
    anyhow!(
        "agent {} is at {}'s workspace trust prompt, where the message's Enter would answer it: agent approve {} trusts the folder, agent deny {} declines it (its CLI exits), or clean it up and respawn with --trust-workspace",
        agent.name,
        trust_backend_name(&agent.backend),
        agent.name,
        agent.name
    )
}

/// answerQuestionHint says how to answer the question dialog agent shows.
pub(crate) fn answer_question_hint(agent: &Agent) -> String {
    format!(
        "see it with agent capture {}, then answer it in its session (agent key {} 2 picks option 2)",
        agent.name, agent.session_name
    )
}

/// exitedErr is the error for typing into an agent whose CLI has exited.
pub(crate) fn exited_err(agent: &Agent) -> anyhow::Error {
    anyhow!(
        "agent {}'s session has ended, so the message would be typed into the shell left behind: agent logs {} shows how it ended, and agent cleanup {} removes it",
        agent.name,
        agent.name,
        agent.name
    )
}

/// sessionGone is the error for an agent whose terminal session no longer exists.
pub(crate) fn session_gone(agent: &Agent) -> anyhow::Error {
    anyhow!(
        "agent {}'s session {} is gone (its CLI exited or the session was killed): agent logs {} shows its last output, and agent cleanup {} removes the record",
        agent.name,
        agent.session_name,
        agent.name,
        agent.name
    )
}
