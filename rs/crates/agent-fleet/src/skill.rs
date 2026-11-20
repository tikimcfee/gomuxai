//! `agent skill install` and `agent skill show` (`docs/FLEET.md`, section
//! `"`agent skill install`"`).
//!
//! Renders and installs the concise `agent` skill from the single shared
//! source [`agent_service::skill_text`]:
//!
//! - **Claude Code**: `<claude config dir>/skills/agent/SKILL.md` (`$CLAUDE_CONFIG_DIR`
//!   when set, else `<home>/.claude`) with YAML frontmatter, plus gomuxai's
//!   global hook in `<claude config dir>/settings.json` via
//!   [`agent_backend::namedagents::install_claude_hooks`].
//! - **Antigravity (`agy`)**: `<home>/.gemini/config/skills/agent/SKILL.md`
//!   (Antigravity's machine-local customization discovery root, shared across
//!   Antigravity CLI and Desktop, as documented in `agy`'s built-in
//!   `agy-customizations` guide and `agy changelog`), with YAML frontmatter.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use agent_backend::namedagents::{
    CLAUDE_HOOK_EVENTS, HookState, HooksReport, install_claude_hooks,
};
use agent_base::gostd::filepath;
use agent_base::util::write_file_atomic;
use anyhow::{Context, Result, bail};

/// Name of the installed skill (`skills/agent/SKILL.md`).
pub const SKILL_NAME: &str = "agent";

/// Trigger description placed in `SKILL.md` frontmatter so Claude Code and
/// Antigravity load the skill when coordinating agents.
pub const SKILL_DESCRIPTION: &str = "Coordinate AI agents on this host and across hosts with gomuxai (agent). Use when listing agents, spawning or reviving helpers, sending messages, waiting on turns or permission prompts, checking fleet status, running heavy scripts, or filing tool friction.";

/// File mode for installed `SKILL.md` files.
pub const SKILL_FILE_MODE: u32 = 0o644;

/// Supported AI CLI harnesses for `agent skill install`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Harness {
    Claude,
    Agy,
}

impl Harness {
    pub const ALL: &[Harness] = &[Harness::Claude, Harness::Agy];

    pub fn as_str(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Agy => "agy",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "claude" => Ok(Harness::Claude),
            "agy" => Ok(Harness::Agy),
            _ => bail!("unknown harness {s:?}: use claude or agy"),
        }
    }

    fn cli_binary(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Agy => "agy",
        }
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Harness {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s)
    }
}

/// Outcome of writing a harness's `SKILL.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkillStatus {
    /// Created a new `SKILL.md`.
    Created,
    /// Rewrote an existing `SKILL.md` whose contents differed.
    Updated,
    /// `SKILL.md` already matched byte-for-byte; nothing was written.
    Unchanged,
}

impl SkillStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SkillStatus::Created => "installed",
            SkillStatus::Updated => "updated",
            SkillStatus::Unchanged => "unchanged",
        }
    }
}

impl fmt::Display for SkillStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of registering gomuxai's hook in Claude Code's `settings.json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookInstall {
    pub settings_path: PathBuf,
    pub changed: bool,
    pub added: usize,
    pub repointed: usize,
    pub backup: String,
    pub report: HooksReport,
}

impl HookInstall {
    /// Human-readable one-line summary (`"unchanged"`, `"installed (13 events added)"`, etc.).
    pub fn summary(&self) -> String {
        if !self.changed {
            return "unchanged".to_string();
        }
        match (self.added, self.repointed) {
            (a, 0) => format!("installed ({a} event(s) added)"),
            (0, r) => format!("updated ({r} event(s) pointed at this binary)"),
            (a, r) => format!("updated ({a} added, {r} pointed at this binary)"),
        }
    }
}

/// Result of installing the `agent` skill for one harness.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    pub harness: Harness,
    pub skill_path: PathBuf,
    pub status: SkillStatus,
    /// Populated for [`Harness::Claude`]; `None` for [`Harness::Agy`] (whose
    /// lifecycle hooks are wired per spawned agent via `--add-dir`).
    pub hooks: Option<HookInstall>,
}

/// Environment inputs for [`install_with`], allowing hermetic tests without
/// mutating process-wide environment variables.
#[derive(Clone, Debug, Default)]
pub struct InstallEnv {
    /// Overrides `$CLAUDE_CONFIG_DIR` when `Some` (empty = unset).
    pub claude_config_dir: Option<PathBuf>,
    /// Overrides `$PATH` when checking whether `claude` or `agy` is installed.
    pub path_env: Option<OsString>,
    /// Overrides the directory where `settings.json` backups are written.
    pub backup_dir: Option<PathBuf>,
    /// Passes `force = true` to [`install_claude_hooks`].
    pub force_hooks: bool,
}

impl InstallEnv {
    /// Reads `$CLAUDE_CONFIG_DIR` and `$PATH` from the current process.
    pub fn from_process() -> Self {
        let claude_config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let path_env = std::env::var_os("PATH");
        Self {
            claude_config_dir,
            path_env,
            backup_dir: None,
            force_hooks: false,
        }
    }
}

/// Returns the plain `agent` skill text (`agent skill show`).
pub fn show(binary: &str) -> String {
    agent_service::skill_text(binary)
}

/// Returns the complete `SKILL.md` file contents (YAML frontmatter + [`show`]).
pub fn skill_markdown(binary: &str) -> String {
    format!(
        "---\nname: {SKILL_NAME}\ndescription: {SKILL_DESCRIPTION}\n---\n\n{}",
        show(binary)
    )
}

/// Returns Claude Code's config directory (`$CLAUDE_CONFIG_DIR` if set, else
/// `<home>/.claude`).
pub fn claude_config_dir(home: impl AsRef<Path>) -> PathBuf {
    claude_config_dir_with(
        home.as_ref(),
        InstallEnv::from_process().claude_config_dir.as_deref(),
    )
}

fn claude_config_dir_with(home: &Path, override_dir: Option<&Path>) -> PathBuf {
    match override_dir {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => home.join(".claude"),
    }
}

/// Returns where Claude Code's `agent` skill is installed:
/// `<claude config dir>/skills/agent/SKILL.md`.
pub fn claude_skill_path(home: impl AsRef<Path>) -> PathBuf {
    claude_config_dir(home)
        .join("skills")
        .join(SKILL_NAME)
        .join("SKILL.md")
}

/// Returns where Antigravity (`agy`) discovers user-global skills:
/// `<home>/.gemini/config/skills/agent/SKILL.md`.
pub fn agy_skill_path(home: impl AsRef<Path>) -> PathBuf {
    home.as_ref()
        .join(".gemini")
        .join("config")
        .join("skills")
        .join(SKILL_NAME)
        .join("SKILL.md")
}

/// Reports whether `harness` is present on this machine (its CLI executable is
/// on `PATH`, or its configuration directory exists under `home`).
pub fn is_present(harness: Harness, home: impl AsRef<Path>) -> bool {
    is_present_with(harness, home.as_ref(), &InstallEnv::from_process())
}

fn is_present_with(harness: Harness, home: &Path, env: &InstallEnv) -> bool {
    if find_executable_in_path(harness.cli_binary(), env.path_env.as_deref()).is_some() {
        return true;
    }
    match harness {
        Harness::Claude => claude_config_dir_with(home, env.claude_config_dir.as_deref()).is_dir(),
        Harness::Agy => {
            let gemini = home.join(".gemini");
            gemini.join("antigravity-cli").is_dir()
                || gemini.join("config").is_dir()
                || gemini.join("antigravity").is_dir()
        }
    }
}

/// Go's `exec.LookPath(name)` for a bare name, as agent-termd's `look_path`
/// (paths.rs) does it, with `PATH` given: the first `PATH` entry (an empty
/// one meaning `.`) holding an executable `name` decides, and when that
/// entry is relative it is no hit (Go's `exec.ErrDot`). So an executable
/// named `claude` in whatever directory `skill install` runs from never
/// counts as the harness being installed.
fn find_executable_in_path(name: &str, path_env: Option<&OsStr>) -> Option<PathBuf> {
    let path_env = path_env.filter(|v| !v.is_empty())?;
    for dir in std::env::split_paths(path_env) {
        let dir = if dir.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            dir
        };
        let cand = dir.join(name);
        if is_executable(&cand) {
            return cand.is_absolute().then_some(cand);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
}

/// Reports whether [`install_with`] needs a home directory for `harness`
/// (`None`: every harness found): agy's skill lives under it, and finding
/// which harnesses are here looks in it; Claude Code's alone doesn't when
/// `$CLAUDE_CONFIG_DIR` (`env.claude_config_dir`) says where its config is.
pub fn needs_home(harness: Option<Harness>, env: &InstallEnv) -> bool {
    match harness {
        Some(Harness::Claude) => env
            .claude_config_dir
            .as_ref()
            .is_none_or(|d| d.as_os_str().is_empty()),
        _ => true,
    }
}

/// Installs the `agent` skill (and Claude Code's hook) for `harness` (`Some`),
/// or for every harness present on this machine (`None`).
pub fn install(
    harness: Option<Harness>,
    home: impl AsRef<Path>,
    binary: &str,
) -> Result<Vec<Installed>> {
    install_with(harness, home, binary, &InstallEnv::from_process())
}

/// Like [`install`], with explicit [`InstallEnv`] overrides.
pub fn install_with(
    harness: Option<Harness>,
    home: impl AsRef<Path>,
    binary: &str,
    env: &InstallEnv,
) -> Result<Vec<Installed>> {
    let home = home.as_ref();
    if home.as_os_str().is_empty() && needs_home(harness, env) {
        bail!("home directory is empty");
    }
    if binary.is_empty() {
        bail!("can't tell where this agent binary is, so there is nothing to install");
    }

    let targets: Vec<Harness> = match harness {
        Some(h) => vec![h],
        None => Harness::ALL
            .iter()
            .copied()
            .filter(|&h| is_present_with(h, home, env))
            .collect(),
    };

    let rendered = skill_markdown(binary);
    let mut out = Vec::with_capacity(targets.len());
    for target in targets {
        match target {
            Harness::Claude => {
                let cfg_dir = claude_config_dir_with(home, env.claude_config_dir.as_deref());
                let skill_path = cfg_dir.join("skills").join(SKILL_NAME).join("SKILL.md");
                let status = write_skill_if_changed(&skill_path, rendered.as_bytes())?;
                let hooks = install_claude_hook_entry(&cfg_dir, binary, env)?;
                out.push(Installed {
                    harness: Harness::Claude,
                    skill_path,
                    status,
                    hooks: Some(hooks),
                });
            }
            Harness::Agy => {
                let skill_path = agy_skill_path(home);
                let status = write_skill_if_changed(&skill_path, rendered.as_bytes())?;
                out.push(Installed {
                    harness: Harness::Agy,
                    skill_path,
                    status,
                    hooks: None,
                });
            }
        }
    }
    Ok(out)
}

fn write_skill_if_changed(path: &Path, content: &[u8]) -> Result<SkillStatus> {
    let existed = match fs::read(path) {
        Ok(existing) if existing == content => return Ok(SkillStatus::Unchanged),
        Ok(_) => true,
        Err(e) if e.kind() == ErrorKind::NotFound => false,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("creating directory {}", parent.display()))?;
    }
    let path_str = filepath::clean(&path.to_string_lossy());
    write_file_atomic(&path_str, content, SKILL_FILE_MODE)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(if existed {
        SkillStatus::Updated
    } else {
        SkillStatus::Created
    })
}

fn default_backup_dir(claude_cfg_dir: &Path, binary: &str) -> PathBuf {
    let bin_path = Path::new(binary);
    if bin_path.is_absolute()
        && let Some(bin_dir) = bin_path.parent()
        && bin_dir.is_dir()
    {
        return bin_dir.join(".gomuxai").join("backups");
    }
    claude_cfg_dir.join("backups")
}

fn install_claude_hook_entry(
    claude_cfg_dir: &Path,
    binary: &str,
    env: &InstallEnv,
) -> Result<HookInstall> {
    fs::create_dir_all(claude_cfg_dir)
        .with_context(|| format!("creating directory {}", claude_cfg_dir.display()))?;
    let settings_path = claude_cfg_dir.join("settings.json");
    let settings_s = filepath::clean(&settings_path.to_string_lossy());
    let backup_dir = env
        .backup_dir
        .clone()
        .unwrap_or_else(|| default_backup_dir(claude_cfg_dir, binary));
    let backup_s = filepath::clean(&backup_dir.to_string_lossy());

    let report = install_claude_hooks(&settings_s, binary, &backup_s, env.force_hooks)?;
    let (mut added, mut repointed) = (0usize, 0usize);
    for &event in CLAUDE_HOOK_EVENTS {
        match report.events.get(event).copied().unwrap_or_default() {
            HookState::Missing => added += 1,
            HookState::Stale => repointed += 1,
            HookState::Current => {}
        }
    }
    Ok(HookInstall {
        settings_path,
        changed: report.changed,
        added,
        repointed,
        backup: report.backup.clone(),
        report,
    })
}
