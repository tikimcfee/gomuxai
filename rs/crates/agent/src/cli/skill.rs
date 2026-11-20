//! `agent skill install`, `agent skill show` (no Go file: new in the Rust
//! build; docs/FLEET.md, "`agent skill install`"; the work is
//! agent_fleet::skill).
use std::path::PathBuf;

use agent_base::gostd::filepath;
use agent_fleet::skill::{self, Harness, InstallEnv, SkillStatus};

use super::command::{ArgRule, Def, Doc};
use super::root::Ctx;

// `agent skill`

pub const DOC: Doc = Doc {
    use_line: "skill",
    short: "Teach Claude Code and agy how to use agent (a skill)",
    long: r#"Skill installs a short text that teaches a harness how to use agent: a map
to agent help, not a manual. It is the same text a spawned agent's
briefing starts with, made from the commands agent help lists.

  install  writes it where each harness found on this machine (its CLI on
           PATH, or its config dir) reads skills: Claude Code's
           <config dir>/skills/agent/SKILL.md, also registering gomuxai's
           hook as agent hooks install does, and agy's
           ~/.gemini/config/skills/agent/SKILL.md. --harness picks one.
           A file that already says the same is left as it is.
  show     prints the text."#,
    example: r#"  agent skill install
  agent skill install --harness claude
  agent skill show"#,
    args: ArgRule::Any,
    ..Doc::DEFAULT
};

pub fn def() -> Def {
    Def::group(&DOC, vec![skill_install_def(), skill_show_def()])
}

// `agent skill install`

pub const SKILL_INSTALL: Doc = Doc {
    use_line: "install",
    short: "Install the agent skill for every harness here (or one)",
    long: r#"Install writes the agent skill for each harness found on this machine (its
CLI on PATH, or its config dir): Claude Code gets
<config dir>/skills/agent/SKILL.md ($CLAUDE_CONFIG_DIR, else ~/.claude)
and gomuxai's hook in its user settings (as agent hooks install; the
settings file is copied to backups/ in the data dir first), and agy gets
~/.gemini/config/skills/agent/SKILL.md. --harness installs for that one,
found or not. Files that already say the same are left as they are."#,
    example: r#"  agent skill install
  agent skill install --harness agy"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent skill install`.
#[derive(clap::Args, Debug, Clone)]
pub struct SkillInstallArgs {
    #[arg(
        long = "harness",
        value_name = "string",
        default_value = "",
        help = "Harness to install for: claude or agy (default: every one found)",
    )]
    pub harness: String,
    /// The positional arguments, checked against SKILL_INSTALL.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn skill_install_def() -> Def {
    leaf!(SKILL_INSTALL, SkillInstallArgs, run_skill_install)
}

pub fn run_skill_install(ctx: &Ctx, args: &SkillInstallArgs) -> anyhow::Result<()> {
    let harness = if args.harness.is_empty() {
        None
    } else {
        Some(Harness::parse(&args.harness)?)
    };
    let bin = ctx.cfg().agent_binary.clone();
    let env = InstallEnv {
        // Where agent hooks install keeps its backups: the data dir's backups/.
        backup_dir: Some(PathBuf::from(filepath::join(&[
            filepath::dir(&ctx.cfg().database_path).as_str(),
            "backups",
        ]))),
        ..InstallEnv::from_process()
    };
    // Claude Code's skill alone, with $CLAUDE_CONFIG_DIR set, needs no home.
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    let home = match home {
        Some(h) => h,
        None if !skill::needs_home(harness, &env) => PathBuf::new(),
        None => anyhow::bail!("$HOME isn't set, so there is no home to install the skill in"),
    };
    let installed = skill::install_with(harness, &home, &bin, &env)?;
    if installed.is_empty() {
        return ctx.write_out(
            "Found neither claude nor agy here (on PATH, or their config dirs), so nothing was installed: --harness claude|agy installs for one anyway.\n",
        );
    }
    let mut out = String::new();
    let mut hook_changed = false;
    for i in &installed {
        let did = match i.status {
            SkillStatus::Created => "Installed",
            SkillStatus::Updated => "Updated",
            SkillStatus::Unchanged => "Already installed",
        };
        out += &format!("{did} the agent skill for {}: {}\n", i.harness, i.skill_path.display());
        if let Some(h) = &i.hooks {
            hook_changed |= h.changed;
            let what = if h.changed {
                let mut s = format!("{} event(s) added, {} pointed at this binary", h.added, h.repointed);
                if !h.backup.is_empty() {
                    s += &format!(" (backup: {})", h.backup);
                }
                s
            } else {
                "already installed".to_string()
            };
            out += &format!("  its hook in {}: {what}\n", h.settings_path.display());
        }
    }
    ctx.write_out(&out)?;
    if hook_changed || installed.iter().any(|i| i.status != SkillStatus::Unchanged) {
        ctx.hint("sessions already running pick it up when restarted");
    }
    Ok(())
}

// `agent skill show`

pub const SKILL_SHOW: Doc = Doc {
    use_line: "show",
    short: "Print the agent skill's text",
    example: r#"  agent skill show"#,
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent skill show`.
#[derive(clap::Args, Debug, Clone)]
pub struct SkillShowArgs {
    /// The positional arguments, checked against SKILL_SHOW.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn skill_show_def() -> Def {
    leaf!(SKILL_SHOW, SkillShowArgs, run_skill_show)
}

pub fn run_skill_show(ctx: &Ctx, _args: &SkillShowArgs) -> anyhow::Result<()> {
    let text = skill::show(&ctx.cfg().agent_binary);
    if text.ends_with('\n') {
        ctx.write_out(&text)
    } else {
        ctx.write_out(&format!("{text}\n"))
    }
}

#[cfg(test)]
#[path = "skill_test.rs"]
mod skill_test;
