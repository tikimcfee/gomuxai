//! Go `internal/backend/namedagents`: the built-in backends (claude, agy,
//! aider, shell), file for file.

mod agy;
mod aider;
mod claude;
mod claude_dialog;
mod claude_hooks;
mod claude_settings;
mod hooks;
mod shell;
mod trust;

pub use agy::{
    AgyBackend, AgyConfig, agy_footer_model, new_agy_backend, new_agy_backend_with_config,
};
pub use aider::{AiderBackend, new_aider_backend};
pub use claude::{
    CLAUDE_RESTART_BLOCKING_FLAGS, CLAUDE_SPAWN_SETTINGS, ClaudeBackend, ClaudeConfig,
    new_claude_backend, new_claude_backend_with_config,
};
pub use claude_hooks::{claude_sessions, registry_status};
pub use claude_settings::{
    CLAUDE_HOOK_EVENTS, HookState, HooksReport, claude_hook_command, claude_hooks_status,
    claude_settings_path, hook_state_word, install_claude_hooks, remove_claude_hooks,
};
pub use shell::{ShellBackend, new_shell_backend};
