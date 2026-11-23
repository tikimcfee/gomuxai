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
            // agy passes a path inside its workspace as relative to it
            // (the agent's directory): resolve it there, then check it.
            let cwd = self
                .storage
                .get_agent(name)
                .ok()
                .flatten()
                .map(|a| a.cwd)
                .unwrap_or_default();
            let path = resolve(&path, &cwd);
            let mut roots = self.dream_read_roots(name);
            // agy cuts a long tool output and leaves the rest in a file under
            // its own conversation's directory: the agent may read its own.
            let transcript = if hr.transcript.is_empty() {
                self.storage
                    .get_state(name, super::status::STATE_KEY_TRANSCRIPT)
                    .ok()
                    .flatten()
                    .map(|e| e.value)
                    .unwrap_or_default()
            } else {
                hr.transcript.clone()
            };
            roots.extend(own_steps_dir(&transcript));
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

/// `path` made absolute against `cwd` when it is relative and `cwd` is
/// absolute; otherwise as it is (and refused later if still relative).
pub(crate) fn resolve(path: &str, cwd: &str) -> String {
    if path.is_empty() || Path::new(path).is_absolute() || !Path::new(cwd).is_absolute() {
        return path.to_string();
    }
    Path::new(cwd).join(path).to_string_lossy().into_owned()
}

/// The directory where agy keeps the tool outputs it cut short for the
/// conversation whose transcript is `transcript`
/// (`<conversation>/.system_generated/logs/<file>` →
/// `<conversation>/.system_generated/steps`), or None for any other shape.
pub(crate) fn own_steps_dir(transcript: &str) -> Option<String> {
    let p = Path::new(transcript);
    if !p.is_absolute()
        || p.components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    let logs = p.parent()?;
    let generated = logs.parent()?;
    if logs.file_name()? != "logs" || generated.file_name()? != ".system_generated" {
        return None;
    }
    Some(generated.join("steps").to_string_lossy().into_owned())
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
    use super::{own_steps_dir, resolve, under_roots};

    #[test]
    fn an_agent_reads_its_own_cut_outputs_only() {
        let t = "/h/brain/abc/.system_generated/logs/transcript_full.jsonl";
        let steps = own_steps_dir(t).unwrap();
        assert_eq!(steps, "/h/brain/abc/.system_generated/steps");
        let roots = vec![steps];
        assert!(under_roots(
            "/h/brain/abc/.system_generated/steps/11/output.txt",
            &roots
        ));
        assert!(!under_roots(
            "/h/brain/xyz/.system_generated/steps/11/output.txt",
            &roots
        ));
        assert!(!under_roots(
            "/h/brain/abc/.system_generated/logs/transcript_full.jsonl",
            &roots
        ));
        assert_eq!(own_steps_dir(""), None);
        assert_eq!(
            own_steps_dir("relative/.system_generated/logs/t.jsonl"),
            None
        );
        assert_eq!(own_steps_dir("/h/brain/abc/transcript.jsonl"), None);
        assert_eq!(
            own_steps_dir("/h/brain/../.system_generated/logs/t.jsonl"),
            None
        );
    }

    #[test]
    fn relative_paths_resolve_against_the_agent_dir() {
        let roots = vec!["/data/dreams/9".to_string()];
        assert_eq!(
            resolve("9/brief.md", "/data/dreams"),
            "/data/dreams/9/brief.md"
        );
        assert!(under_roots(&resolve("9/brief.md", "/data/dreams"), &roots));
        assert!(!under_roots(&resolve("9/../../x", "/data/dreams"), &roots));
        assert!(
            !under_roots(&resolve("9/brief.md", ""), &roots),
            "no cwd: stays relative"
        );
        assert_eq!(resolve("/abs/x", "/data"), "/abs/x");
    }

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
