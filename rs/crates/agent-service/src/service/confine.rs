//! Confined roles on a backend whose PreToolUse hook decides every tool call
//! (agy): an agent with an open dream runs only what its dream allows, and
//! every other call is denied, whatever the CLI's own permission settings
//! say. Claude's dream agents are confined by dontAsk and their launch rules
//! instead (docs/DREAMING.md).

use std::path::{Component, Path};

use agent_backend::{HookReport, Provider};

use super::agent_service::AgentService;

/// The state key holding a dream agent's read roots: a JSON list of absolute
/// paths (a directory covers what is below it). Set by `agent dream run`.
pub const STATE_KEY_DREAM_READ: &str = "dream_read";

/// The tools a confined agy agent may call, each still checked: reading a
/// file under its read roots, and running a command its dream allows.
const READ_TOOLS: &[&str] = &["read_file", "view_file"];
const RUN_TOOL: &str = "run_command";

impl AgentService {
    /// The PreToolUse output for an agent in a confined role, or None for any
    /// other agent or event (left to the CLI, as before). An agent is in a
    /// confined role while it has an open dream.
    pub(crate) fn confined_decision(
        &self,
        name: &str,
        provider: &dyn Provider,
        hr: &HookReport,
    ) -> Option<Vec<u8>> {
        if hr.event != "PreToolUse" {
            return None;
        }
        let ch = provider.as_confined_hook()?;
        let dream = match self.storage.open_dream_of(name) {
            Ok(Some(d)) => d,
            Ok(None) => return None,
            // Can't tell: refuse rather than let it through.
            Err(e) => {
                return Some(ch.confined_deny(&format!(
                    "gomuxai couldn't check this agent's dream ({e:#}), so the call is refused"
                )));
            }
        };
        match self.confined_allows(name, dream.id, hr) {
            Ok(reason) => Some(ch.confined_allow(reason)),
            Err(why) => Some(ch.confined_deny(&why)),
        }
    }

    fn confined_allows(
        &self,
        name: &str,
        dream_id: i64,
        hr: &HookReport,
    ) -> Result<&'static str, String> {
        let tool = hr.tool.as_str();
        if tool == RUN_TOOL {
            return self
                .dream_command_allowed(dream_id, &hr.command, "")
                .ok_or_else(|| {
                    format!(
                        "a dream's agent runs only reading agent commands, its dream verbs, and git -C <repo> log|show in its projects, not {:?}",
                        hr.command
                    )
                });
        }
        if READ_TOOLS.contains(&tool) {
            let path = ["Path", "AbsolutePath", "File"]
                .iter()
                .map(|k| agent_backend::namedagents::agy_arg(hr.tool_input.as_bytes(), k))
                .find(|p| !p.is_empty())
                .unwrap_or_default();
            let roots = self.dream_read_roots(name);
            if under_roots(&path, &roots) {
                return Ok("gomuxai: reading a file the dream may read");
            }
            return Err(format!(
                "a dream's agent reads only under its dream's read roots, not {path:?}"
            ));
        }
        Err(format!(
            "{tool} isn't allowed for a dream's agent: it reads with read_file and runs only its dream's commands"
        ))
    }

    fn dream_read_roots(&self, name: &str) -> Vec<String> {
        match self.storage.get_state(name, STATE_KEY_DREAM_READ) {
            Ok(Some(e)) => serde_json::from_str(&e.value).unwrap_or_default(),
            _ => Vec::new(),
        }
    }
}

/// Whether `path` is absolute, has no `..` or `.` parts, and is one of
/// `roots` or below one (by path components).
pub(crate) fn under_roots(path: &str, roots: &[String]) -> bool {
    let p = Path::new(path);
    if !p.is_absolute()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    roots
        .iter()
        .filter(|r| Path::new(r.as_str()).is_absolute())
        .any(|r| p.starts_with(r))
}

#[cfg(test)]
mod tests {
    use super::under_roots;

    #[test]
    fn roots_go_by_components() {
        let roots = vec!["/srv/app".to_string(), "/data/memory/".to_string()];
        assert!(under_roots("/srv/app/src/main.rs", &roots));
        assert!(under_roots("/data/memory/a.md", &roots));
        assert!(!under_roots("/srv/app2/x", &roots));
        assert!(!under_roots("/srv/app/../etc/passwd", &roots));
        assert!(!under_roots("srv/app/x", &roots));
        assert!(!under_roots("", &roots));
        assert!(!under_roots("/srv/app/x", &["relative".to_string()]));
    }
}
