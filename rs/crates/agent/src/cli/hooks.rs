//! `agent hooks`, `agent hook` (Go: internal/cli/hooks.go).
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use agent_backend::namedagents::{
    CLAUDE_HOOK_EVENTS, HookState, claude_hook_command, claude_hooks_status,
    claude_settings_path, hook_state_word, install_claude_hooks, remove_claude_hooks,
};
use agent_base::gostd::filepath;
use agent_base::gostd::strconv::quote;
use agent_store::Time;
use anyhow::Context as _;

use super::command::{ArgRule, Def, Doc};
use super::output::usage_error;
use super::root::{Ctx, command};

// `agent hooks`

pub const DOC: Doc = Doc {
    use_line: "hooks <install|remove|status>",
    short: "Register gomuxai's hook with Claude Code for every session",
    long: r#"Hooks manages gomuxai's entry in Claude Code's user settings
($CLAUDE_CONFIG_DIR/settings.json, else ~/.claude/settings.json): for each
hook event, a command running this binary's "agent hook".

  install  adds the entry where it is missing. Every other hook and
           setting stays as it was; the file is copied to backups/ in the
           data dir first, and written atomically. A file that isn't JSON,
           or whose hooks Claude Code couldn't read, is left alone. It
           changes nothing when all are current.
  remove   takes gomuxai's entries out.
  status   prints, per event, whether the entry is installed, and the last
           errors the hook logged.

One hook serves one agent binary: the hook records the sessions it places
in the store beside the binary it runs, so a Claude agent another binary
spawned isn't heard. install points the entry of an agent binary that is
gone (moved, deleted) at this one, but refuses to take over one that still
exists unless --force: that binary's agents would stop reporting. Spawn
refuses a Claude agent while the hook runs another binary.

Claude Code reads hooks when a session starts, so sessions already running
pick the change up when restarted (or through /hooks)."#,
    example: r#"  agent hooks install
  agent hooks install --force
  agent hooks status
  agent hooks remove"#,
    args: ArgRule::Exact(1),
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent hooks`.
#[derive(clap::Args, Debug, Clone)]
pub struct Args {
    #[arg(
        long = "force",
        help = "Point the hook at this binary even if it runs another agent binary that still exists",
    )]
    pub force: bool,
    #[arg(
        long = "settings",
        value_name = "string",
        default_value = "",
        help = "Settings file to edit (default: Claude Code's user settings)",
    )]
    pub settings: String,
    /// The positional arguments, checked against DOC.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn def() -> Def {
    leaf!(DOC, Args, run)
}

pub fn run(ctx: &Ctx, args: &Args) -> anyhow::Result<()> {
    let path = if args.settings.is_empty() {
        claude_settings_path()?
    } else {
        args.settings.clone()
    };
    let bin = ctx.cfg().agent_binary.clone();
    if bin.is_empty() {
        anyhow::bail!("can't tell where this agent binary is, so there is nothing to register");
    }
    let backups = filepath::join(&[filepath::dir(&ctx.cfg().database_path).as_str(), "backups"]);
    match args.args[0].as_str() {
        "install" => {
            let r = install_claude_hooks(&path, &bin, &backups, args.force)?;
            let (mut added, mut repointed) = (0, 0);
            for e in CLAUDE_HOOK_EVENTS {
                match r.events.get(*e).copied().unwrap_or_default() {
                    HookState::Missing => added += 1,
                    HookState::Stale => repointed += 1,
                    HookState::Current => {}
                }
            }
            if !r.changed {
                return ctx.write_out(&format!(
                    "Already installed in {path}: every event runs {}.\n",
                    claude_hook_command(&bin)
                ));
            }
            ctx.write_out(&format!(
                "Installed in {path}: {added} event(s) added, {repointed} pointed at this binary (was: {}). Backup: {}\n",
                or_none(&r.others),
                or_none(&non_empty(&r.backup))
            ))?;
            ctx.hint("sessions already running pick it up when restarted, or through /hooks");
            Ok(())
        }
        "remove" => {
            let r = remove_claude_hooks(&path, &bin, &backups)?;
            if !r.changed {
                return ctx.write_out(&format!("Nothing to remove from {path}.\n"));
            }
            ctx.write_out(&format!(
                "Removed gomuxai's hook from {path}. Backup: {}\n",
                or_none(&non_empty(&r.backup))
            ))
        }
        "status" => {
            let r = claude_hooks_status(&path, &bin)?;
            let mut out = format!("{path} (this binary: {bin})\n");
            for e in r.sorted_events() {
                let state = r.events.get(&e).copied().unwrap_or_default();
                out += &format!("  {e:<19} {}\n", hook_state_word(state));
            }
            if !r.others.is_empty() {
                out += &format!("Other agent binaries' entries: {}\n", r.others.join(", "));
            }
            out += &hook_errors(ctx);
            ctx.write_out(&out)
        }
        other => Err(usage_error(
            &command("hooks"),
            anyhow::anyhow!("unknown action {}: install, remove, or status", quote(other)),
        )),
    }
}

/// The last lines of the hook's error log, as status prints them.
fn hook_errors(ctx: &Ctx) -> String {
    let path = hook_errors_path(ctx);
    let data = std::fs::read(&path).unwrap_or_default();
    if data.is_empty() {
        return "No hook errors logged.\n".to_string();
    }
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<&str> = text.trim_end_matches('\n').split('\n').collect();
    let lines = &lines[lines.len().saturating_sub(5)..];
    let mut out = format!("Last hook errors ({path}):\n");
    for l in lines {
        out += &format!("  {l}\n");
    }
    out
}

fn non_empty(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        vec![s.to_string()]
    }
}

fn or_none(list: &[String]) -> String {
    if list.is_empty() {
        "none".to_string()
    } else {
        list.join(", ")
    }
}

// `agent hook`

pub const HOOK: Doc = Doc {
    use_line: "hook",
    short: "The hook Claude Code runs on each event (installed by agent hooks install)",
    long: r#"Hook is what Claude Code runs for every hook event of every session, once
"agent hooks install" has registered it in Claude Code's user settings. It
reads the event's payload on stdin and works out, from the running
processes, which session fired it: the Claude Code CLI above it (proven by
Claude's session registry: pid and start time), and the workgroup agent
whose session runs that CLI, if any.

Every session's report is recorded (agent sessions shows how each ended).
An agent's session also gets the agent's report, as agent status reads it,
and only there does hook print anything: held messages for Claude's Stop,
and the approval of a safe agent command for PreToolUse. A session no agent
owns, such as one a person started, never sees output from it.

It always exits 0, so a reporting problem can't disturb a session; what went
wrong goes to stderr and to logs/hook-errors.log in the data dir, which
"agent hooks status" shows."#,
    example: "  agent hook < payload.json",
    args: ArgRule::None,
    ..Doc::DEFAULT
};

/// The flags and arguments of `agent hook`.
#[derive(clap::Args, Debug, Clone)]
pub struct HookArgs {
    /// The positional arguments, checked against HOOK.args.
    #[arg(hide = true)]
    pub args: Vec<String>,
}

pub fn hook_def() -> Def {
    leaf!(HOOK, HookArgs, run_hook)
}

/// The hook's error log, in the log dir.
fn hook_errors_path(ctx: &Ctx) -> String {
    filepath::join(&[ctx.cfg().log_dir.as_str(), "hook-errors.log"])
}

pub fn run_hook(ctx: &Ctx, _args: &HookArgs) -> anyhow::Result<()> {
    // Go's setup opens the store before the hook runs, so a tool that can't
    // start (no database) exits 1; everything after exits 0. Opening the
    // service connects to nothing: the bus only on a report that publishes.
    ctx.service()?;
    let at = Time::now();
    let res = ctx
        .read_stdin()
        .context("reading the payload")
        .and_then(|payload| handle_hook(ctx, &payload, at));
    if let Err(e) = res {
        let msg = format!("{e:#}");
        let _ = writeln!(ctx.err(), "agent hook: {msg}");
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(hook_errors_path(ctx))
        {
            let _ = writeln!(f, "{} {}", at.utc().format_rfc3339(), msg.replace('\n', " "));
        }
    }
    Ok(())
}

/// Places the session that fired the hook and records its report (Go:
/// handleHook, after the payload is read).
pub(crate) fn handle_hook(ctx: &Ctx, payload: &[u8], at: Time) -> anyhow::Result<()> {
    let svc = ctx.service()?;
    let claude_pid = super::root::getenv("CLAUDE_PID");
    let (hs, placed) = svc.find_hook_session(std::process::id() as i32, &hook_session_id(payload), &claude_pid);
    if let Err(err) = placed {
        // The session's own report needs no owner: record it, then say why no
        // agent got one.
        let (_, rerr) = svc.handle_claude_hook(&hs, payload, at);
        let err = err.context("placing the session");
        return Err(match rerr {
            Ok(()) => err,
            Err(rerr) => anyhow::anyhow!("{err:#}\n{rerr:#}"),
        });
    }
    let (out, mut res) = svc.handle_claude_hook(&hs, payload, at);
    if let Some(out) = out {
        let mut stdout = ctx.out();
        if let Err(werr) = stdout.write_all(&out).and_then(|()| stdout.flush()) {
            let werr = anyhow::Error::new(werr).context("writing the hook's output");
            res = Err(match res {
                Ok(()) => werr,
                Err(e) => anyhow::anyhow!("{e:#}\n{werr:#}"),
            });
        }
    }
    res
}

/// The payload's session_id, as Go's json.Unmarshal into a struct of that
/// one field reads it, which places the session: a payload that doesn't
/// decode gives "" (and fails later, where it is recorded); one that does
/// gives its session whatever else it holds (a mistyped or null member, half
/// a surrogate pair anywhere, "Session_ID" for the name).
fn hook_session_id(payload: &[u8]) -> String {
    #[derive(serde::Deserialize, Default)]
    struct Head {
        #[serde(default)]
        session_id: String,
    }
    let (head, _): (Head, _) = agent_base::gostd::json::from_slice_partial(payload);
    head.session_id
}

#[cfg(test)]
#[path = "hooks_test.rs"]
mod hooks_test;
