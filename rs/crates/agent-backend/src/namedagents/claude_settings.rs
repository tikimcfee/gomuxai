//! Go `namedagents/claude_settings.go`.
//!
//! Claude Code's hooks for every session on the machine live in its user
//! settings file. gomuxai's entry runs `<agent binary> hook` on each event
//! (see agent hook): whichever Claude Code session fires it, a workgroup
//! agent's or one a person started, the hook works out which it is from the
//! running processes.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;

use agent_base::gostd::{filepath, os};
use agent_base::util::{shell_quote, split_shell_words, write_file_atomic};
use anyhow::Context;

use super::hooks::HOOK_TIMEOUT_SEC;
use crate::gojson::{self, Value};

/// The events gomuxai's hook is registered on.
pub const CLAUDE_HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PostToolUseFailure",
    "PreCompact",
    "Notification",
    "SubagentStart",
    "SubagentStop",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// Returns Claude Code's user settings file:
/// $CLAUDE_CONFIG_DIR/settings.json, else ~/.claude/settings.json.
pub fn claude_settings_path() -> anyhow::Result<String> {
    if let Ok(d) = std::env::var("CLAUDE_CONFIG_DIR")
        && !d.is_empty()
    {
        return Ok(filepath::join(&[d.as_str(), "settings.json"]));
    }
    let home = os::user_home_dir().context("finding Claude Code's settings")?;
    Ok(filepath::join(&[home.as_str(), ".claude", "settings.json"]))
}

/// The command gomuxai's entry runs.
pub fn claude_hook_command(binary: &str) -> String {
    format!("{} hook", shell_quote(binary))
}

/// gomuxai's entry for one event.
///
/// The discriminants are Go's iota values.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[repr(i32)]
pub enum HookState {
    /// No entry of gomuxai's.
    #[default]
    Missing = 0,
    /// An entry running this binary.
    Current = 1,
    /// An entry running another agent binary (moved, or another build).
    Stale = 2,
}

/// Says, per event, what the settings held (before a change).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HooksReport {
    pub path: String,
    pub events: BTreeMap<String, HookState>,
    /// Names the stale entries' commands, once each, so a person can see
    /// what an install re-pointed or a remove took out.
    pub others: Vec<String>,
    /// Says the file was written; `backup` is where the old one was copied
    /// ("" if unchanged, or there was no file).
    pub changed: bool,
    pub backup: String,
}

/// Reads which events carry gomuxai's entry.
pub fn claude_hooks_status(path: &str, binary: &str) -> anyhow::Result<HooksReport> {
    let settings = read_settings(path)?;
    let (r, _) = scan_hooks(&settings, binary, path)?;
    Ok(r)
}

/// Makes every event in CLAUDE_HOOK_EVENTS run `binary`'s hook, in the
/// settings file at `path`: it points a stale entry at `binary`, adds a
/// missing one, and leaves every other hook and setting as it was. It
/// changes nothing when all are current. A file that isn't a JSON object, or
/// whose hooks aren't shaped as Claude Code reads them, is an error, never
/// overwritten. The old file is copied into `backup_dir` first.
///
/// One hook serves one agent binary: the store beside it records the
/// sessions it places. An entry of another agent binary that still exists is
/// re-pointed only with `force`, since that binary's agents would stop
/// reporting.
pub fn install_claude_hooks(
    path: &str,
    binary: &str,
    backup_dir: &str,
    force: bool,
) -> anyhow::Result<HooksReport> {
    edit_hooks(path, binary, backup_dir, true, force)
}

/// Takes gomuxai's entries out of the settings file, from every event,
/// dropping an event (and the hooks key) left empty.
pub fn remove_claude_hooks(
    path: &str,
    binary: &str,
    backup_dir: &str,
) -> anyhow::Result<HooksReport> {
    edit_hooks(path, binary, backup_dir, false, true)
}

fn edit_hooks(
    path: &str,
    binary: &str,
    backup_dir: &str,
    install: bool,
    force: bool,
) -> anyhow::Result<HooksReport> {
    let mut settings = read_settings(path)?;
    let (mut report, mut hooks) = scan_hooks(&settings, binary, path)?;
    if install && !force {
        let live = live_others(&report.others);
        if !live.is_empty() {
            anyhow::bail!(
                "Claude Code's hook in {path} runs {}, which is still there: one hook serves one agent binary (its store records the sessions it places), so re-pointing it here would stop that binary's agents reporting. Run agent hooks install from that binary, or pass --force to point it here",
                live.join(", ")
            );
        }
    }
    let mut changed = false;
    let want = claude_hook_command(binary);
    for groups in hooks.values_mut() {
        let mut kept = Vec::new();
        for mut g in groups.drain(..) {
            let Value::Object(group) = &mut g else {
                kept.push(g); // scan_hooks let only objects through
                continue;
            };
            let mut group = std::mem::take(group);
            let inner = match group.get("hooks") {
                Some(Value::Array(a)) => a.clone(),
                _ => Vec::new(),
            };
            let mut keep_inner = Vec::new();
            for h in &inner {
                let c = match h {
                    Value::Object(cmd) => cmd.get("command").and_then(Value::as_str).unwrap_or(""),
                    _ => "",
                };
                if !is_our_hook(c, binary) {
                    keep_inner.push(h.clone());
                    continue;
                }
                changed = changed || !install || c != want;
                if install {
                    let mut cmd = h.clone();
                    if let Value::Object(m) = &mut cmd {
                        m.insert("command".to_string(), Value::String(want.clone()));
                    }
                    keep_inner.push(cmd);
                }
            }
            if !inner.is_empty() && keep_inner.is_empty() {
                continue; // the group held only our entry
            }
            group.insert("hooks".to_string(), Value::Array(keep_inner));
            kept.push(Value::Object(group));
        }
        *groups = kept;
    }
    if install {
        for event in CLAUDE_HOOK_EVENTS {
            if report.events.get(*event).copied().unwrap_or_default() == HookState::Missing {
                let mut cmd = BTreeMap::new();
                cmd.insert("type".to_string(), Value::String("command".to_string()));
                cmd.insert("command".to_string(), Value::String(want.clone()));
                cmd.insert(
                    "timeout".to_string(),
                    Value::Number(HOOK_TIMEOUT_SEC.to_string()),
                );
                let mut group = BTreeMap::new();
                group.insert("matcher".to_string(), Value::String(String::new()));
                group.insert("hooks".to_string(), Value::Array(vec![Value::Object(cmd)]));
                hooks
                    .entry(event.to_string())
                    .or_default()
                    .push(Value::Object(group));
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(report);
    }
    let out: BTreeMap<String, Value> = hooks
        .into_iter()
        .filter(|(_, groups)| !groups.is_empty())
        .map(|(event, groups)| (event, Value::Array(groups)))
        .collect();
    if !out.is_empty() {
        settings.insert("hooks".to_string(), Value::Object(out));
    } else {
        settings.remove("hooks");
    }
    report.changed = true;
    report.backup = backup(path, backup_dir)?;
    let data = marshal_settings(settings);
    write_file_atomic(path, data.as_bytes(), 0o600).with_context(|| format!("writing {path}"))?;
    Ok(report)
}

/// Reads the settings file; a missing or empty one is {}.
fn read_settings(path: &str) -> anyhow::Result<BTreeMap<String, Value>> {
    let data = match os::read_file(path) {
        Ok(d) => d,
        Err(e) if e.err.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("reading {path}"))),
    };
    if String::from_utf8_lossy(&data).trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    // Numbers come back as written, not through float64.
    match gojson::decode_object(&data) {
        Ok(Some(settings)) => Ok(settings),
        Ok(None) => anyhow::bail!(
            "{path} isn't a JSON object, so it was left alone (fix it, then run this again): <nil>"
        ),
        Err(e) => anyhow::bail!(
            "{path} isn't a JSON object, so it was left alone (fix it, then run this again): {e}"
        ),
    }
}

/// Reports gomuxai's entry per event, and returns the hooks as event ->
/// groups (each {"matcher", "hooks": [{"type", "command", ...}]}). Hooks
/// shaped otherwise are an error: editing what Claude Code wouldn't read
/// could only make it worse.
#[allow(clippy::type_complexity)]
fn scan_hooks(
    settings: &BTreeMap<String, Value>,
    binary: &str,
    path: &str,
) -> anyhow::Result<(HooksReport, BTreeMap<String, Vec<Value>>)> {
    let mut report = HooksReport {
        path: path.to_string(),
        ..Default::default()
    };
    let mut hooks = BTreeMap::new();
    let bad = |at: &str, what: &str| {
        anyhow::anyhow!(
            "{path}: {at} isn't {what}, so the file was left alone (fix it, then run this again)"
        )
    };
    let raw = match settings.get("hooks") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m),
        Some(_) => return Err(bad("hooks", "an object")),
    };
    let want = claude_hook_command(binary);
    let mut seen = std::collections::HashSet::new();
    for (event, v) in raw.into_iter().flatten() {
        let Value::Array(groups) = v else {
            return Err(bad(&format!("hooks.{event}"), "an array"));
        };
        hooks.insert(event.clone(), groups.clone());
        for (i, g) in groups.iter().enumerate() {
            let Value::Object(group) = g else {
                return Err(bad(&format!("hooks.{event}[{i}]"), "an object"));
            };
            let inner: &[Value] = match group.get("hooks") {
                None | Some(Value::Null) => &[],
                Some(Value::Array(a)) => a,
                Some(_) => return Err(bad(&format!("hooks.{event}[{i}].hooks"), "an array")),
            };
            for (j, h) in inner.iter().enumerate() {
                let Value::Object(cmd) = h else {
                    return Err(bad(&format!("hooks.{event}[{i}].hooks[{j}]"), "an object"));
                };
                let c = cmd.get("command").and_then(Value::as_str).unwrap_or("");
                if c == want {
                    report.events.insert(event.clone(), HookState::Current);
                } else if is_our_hook(c, binary) {
                    if report.events.get(event) != Some(&HookState::Current) {
                        report.events.insert(event.clone(), HookState::Stale);
                    }
                    if seen.insert(c.to_string()) {
                        report.others.push(c.to_string());
                    }
                }
            }
        }
    }
    Ok((report, hooks))
}

/// Reports whether a hook command is an agent binary's hook: two words, an
/// executable named agent (or as this binary is named) and "hook".
fn is_our_hook(command: &str, binary: &str) -> bool {
    let f = split_shell_words(command);
    if f.len() != 2 || f[1] != "hook" {
        return false;
    }
    let name = filepath::base(&f[0]);
    name == "agent" || name == filepath::base(binary)
}

/// Returns the binaries of the stale hook commands that still exist as
/// executables.
pub(crate) fn live_others(commands: &[String]) -> Vec<String> {
    let mut live = Vec::new();
    for c in commands {
        let f = split_shell_words(c);
        let Some(bin) = f.first() else {
            continue;
        };
        if let Ok(info) = std::fs::metadata(bin)
            && info.is_file()
            && info.permissions().mode() & 0o111 != 0
        {
            live.push(bin.clone());
        }
    }
    live
}

fn backup(path: &str, dir: &str) -> anyhow::Result<String> {
    let data = match os::read_file(path) {
        Ok(d) => d,
        Err(e) if e.err.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(e) => return Err(e.into()),
    };
    os::mkdir_all(dir, 0o700).context("making the backup dir")?;
    let stamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%.9fZ");
    let dst = filepath::join(&[dir.to_string(), format!("{}.{stamp}", filepath::base(path))]);
    os::write_file(&dst, &data, 0o600).with_context(|| format!("backing up {path}"))?;
    Ok(dst)
}

/// Writes settings the way Claude Code does: two-space indents, keys
/// sorted, slashes and other HTML characters unescaped.
fn marshal_settings(settings: BTreeMap<String, Value>) -> String {
    gojson::encode_indent(&Value::Object(settings), "  ", false)
}

/// A HookState as status prints it.
pub fn hook_state_word(s: HookState) -> &'static str {
    match s {
        HookState::Current => "installed",
        HookState::Stale => "another agent binary's",
        HookState::Missing => "missing",
    }
}

impl HooksReport {
    /// Returns CLAUDE_HOOK_EVENTS, then any other event gomuxai's entry sits
    /// on, for printing.
    pub fn sorted_events(&self) -> Vec<String> {
        let mut out: Vec<String> = CLAUDE_HOOK_EVENTS.iter().map(|e| e.to_string()).collect();
        let mut extra: Vec<String> = self
            .events
            .keys()
            .filter(|e| !CLAUDE_HOOK_EVENTS.contains(&e.as_str()))
            .cloned()
            .collect();
        extra.sort();
        out.append(&mut extra);
        out
    }
}
