//! Go `namedagents/aider.go`.

use agent_base::util::shell_quote;

use crate::backend::Backend;
use crate::provider::{ModelChooser, Provider};

/// Implements [`Provider`] for Aider CLI.
#[derive(Debug, Clone, Default)]
pub struct AiderBackend;

/// Creates a new Aider backend.
pub fn new_aider_backend() -> AiderBackend {
    AiderBackend
}

impl AiderBackend {
    pub fn name(&self) -> &str {
        "aider"
    }

    pub fn launch_command(&self) -> String {
        "aider".to_string()
    }

    pub fn init_message(&self, task_file: &str) -> String {
        if !task_file.is_empty() {
            return format!("/read {task_file}");
        }
        String::new()
    }

    pub fn exit_command(&self) -> String {
        "/exit".to_string()
    }

    // Backend: status scraping, aider's only status source.

    pub fn idle_pattern(&self) -> &str {
        // Aider shows "aider>" or similar prompt when idle
        r"(?m)^aider>\s*$"
    }

    pub fn processing_pattern(&self) -> &str {
        // Aider shows various indicators when processing
        "(Thinking|Working|Applying)"
    }
}

impl ModelChooser for AiderBackend {
    /// Runs aider on `model` (--model).
    fn with_model(&self, launch_cmd: &str, model: &str) -> String {
        format!("{launch_cmd} --model {}", shell_quote(model))
    }
}

impl Backend for AiderBackend {
    fn name(&self) -> &str {
        AiderBackend::name(self)
    }
    fn launch_command(&self) -> String {
        AiderBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        AiderBackend::init_message(self, task_file)
    }
    fn idle_pattern(&self) -> &str {
        AiderBackend::idle_pattern(self)
    }
    fn processing_pattern(&self) -> &str {
        AiderBackend::processing_pattern(self)
    }
}

impl Provider for AiderBackend {
    fn name(&self) -> &str {
        AiderBackend::name(self)
    }
    fn launch_command(&self) -> String {
        AiderBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        AiderBackend::init_message(self, task_file)
    }
    fn exit_command(&self) -> String {
        AiderBackend::exit_command(self)
    }
    fn as_model_chooser(&self) -> Option<&dyn ModelChooser> {
        Some(self)
    }
}
