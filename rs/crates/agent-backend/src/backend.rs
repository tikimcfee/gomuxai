//! Go `internal/backend/backend.go`.

/// Describes a CLI tool by its launch command and the regex patterns that
/// tell its status from terminal output. Custom JSON backends
/// (--backend-config) implement only this; status scraping uses it for every
/// backend, the built-in ones included.
pub trait Backend: Send + Sync {
    /// Returns the backend identifier.
    fn name(&self) -> &str;

    /// Returns the command to start the CLI tool.
    fn launch_command(&self) -> String;

    /// Returns the initial message to send after launch. If `task_file` is
    /// provided, it should be incorporated into the message.
    fn init_message(&self, task_file: &str) -> String;

    /// Returns a regex pattern that indicates the backend is idle.
    fn idle_pattern(&self) -> &str;

    /// Returns a regex pattern that indicates the backend is processing.
    fn processing_pattern(&self) -> &str;
}
