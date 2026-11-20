//! Go `namedagents/shell.go`.

use crate::backend::Backend;
use crate::provider::Provider;

/// Implements [`Provider`] for a plain shell or terminal session with no AI
/// CLI wrapper: the terminal starts the shell directly, so `launch_command`
/// is empty.
#[derive(Debug, Clone, Default)]
pub struct ShellBackend;

/// Creates a new shell backend.
pub fn new_shell_backend() -> ShellBackend {
    ShellBackend
}

impl ShellBackend {
    pub fn name(&self) -> &str {
        "shell"
    }

    pub fn launch_command(&self) -> String {
        String::new()
    }

    pub fn init_message(&self, _task_file: &str) -> String {
        String::new()
    }

    pub fn exit_command(&self) -> String {
        "exit".to_string()
    }

    pub fn idle_pattern(&self) -> &str {
        ""
    }

    pub fn processing_pattern(&self) -> &str {
        ""
    }
}

impl Backend for ShellBackend {
    fn name(&self) -> &str {
        ShellBackend::name(self)
    }
    fn launch_command(&self) -> String {
        ShellBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        ShellBackend::init_message(self, task_file)
    }
    fn idle_pattern(&self) -> &str {
        ShellBackend::idle_pattern(self)
    }
    fn processing_pattern(&self) -> &str {
        ShellBackend::processing_pattern(self)
    }
}

impl Provider for ShellBackend {
    fn name(&self) -> &str {
        ShellBackend::name(self)
    }
    fn launch_command(&self) -> String {
        ShellBackend::launch_command(self)
    }
    fn init_message(&self, task_file: &str) -> String {
        ShellBackend::init_message(self, task_file)
    }
    fn exit_command(&self) -> String {
        ShellBackend::exit_command(self)
    }
}
