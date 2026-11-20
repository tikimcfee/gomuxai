//! Go `internal/backend/registry.go`.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use anyhow::Context;

use crate::backend::Backend;
use crate::generic::{GenericConfig, new_generic_backend};
use crate::namedagents;
use crate::provider::Provider;

/// Holds the per-agent options a [`Provider`] is created with.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderConfig {
    /// Makes Claude and Agy pass --dangerously-skip-permissions.
    pub skip_permissions: bool,

    /// The per-agent bundle directory under .gomuxai/agents/<name>.
    pub agent_dir: String,

    /// The absolute directory the agent runs in.
    pub cwd: String,

    /// The session Claude or Agy continues instead of starting a new one:
    /// Claude's session id, agy's conversation id.
    pub resume: String,
}

/// Creates a new [`Provider`] instance with the given config.
pub type ProviderFactory = Arc<dyn Fn(ProviderConfig) -> Box<dyn Provider> + Send + Sync>;

/// Wrapped by [`Registry::get`]'s error for a name with no backend (Go's
/// `ErrNotFound`, checked with `errors.Is`): check with
/// `err.downcast_ref::<ErrNotFound>()`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "backend {} not found (known: {known}; or define one with --backend-config <file>)",
    agent_base::gostd::strconv::quote(name)
)]
pub struct ErrNotFound {
    /// The name asked for.
    pub name: String,
    /// The known backends, sorted and joined by ", ".
    pub known: String,
}

/// Holds the available backends. Built-in ones have a Provider factory, so
/// each agent gets an instance configured for it. Every backend, custom JSON
/// ones included, is also registered as a [`Backend`]: custom ones spawn
/// from it, and status scraping reads its patterns.
pub struct Registry {
    backends: RwLock<HashMap<String, Arc<dyn Backend>>>,
    factories: RwLock<HashMap<String, ProviderFactory>>,
}

/// Creates a new backend registry with built-in backends.
pub fn new_registry() -> Registry {
    let r = Registry {
        backends: RwLock::new(HashMap::new()),
        factories: RwLock::new(HashMap::new()),
    };

    r.register(Arc::new(namedagents::new_claude_backend()));
    r.register(Arc::new(namedagents::new_aider_backend()));
    r.register(Arc::new(namedagents::new_agy_backend()));
    r.register(Arc::new(namedagents::new_shell_backend()));

    r.register_factory("claude", |cfg: ProviderConfig| {
        Box::new(namedagents::new_claude_backend_with_config(
            namedagents::ClaudeConfig {
                skip_permissions: cfg.skip_permissions,
                resume: cfg.resume,
            },
        ))
    });
    r.register_factory("aider", |_cfg: ProviderConfig| {
        Box::new(namedagents::new_aider_backend())
    });
    r.register_factory("agy", |cfg: ProviderConfig| {
        Box::new(namedagents::new_agy_backend_with_config(
            namedagents::AgyConfig {
                skip_permissions: cfg.skip_permissions,
                agent_dir: cfg.agent_dir,
                cwd: cfg.cwd,
                resume: cfg.resume,
            },
        ))
    });
    r.register_factory("shell", |_cfg: ProviderConfig| {
        Box::new(namedagents::new_shell_backend())
    });

    r
}

impl Default for Registry {
    fn default() -> Self {
        new_registry()
    }
}

impl Registry {
    /// Creates a new backend registry with built-in backends.
    pub fn new() -> Self {
        new_registry()
    }

    /// Adds a backend to the registry.
    pub fn register(&self, b: Arc<dyn Backend>) {
        let mut backends = self.backends.write().unwrap_or_else(|e| e.into_inner());
        backends.insert(b.name().to_string(), b);
    }

    /// Retrieves a backend by name.
    pub fn get(&self, name: &str) -> anyhow::Result<Arc<dyn Backend>> {
        let backends = self.backends.read().unwrap_or_else(|e| e.into_inner());
        match backends.get(name) {
            Some(b) => Ok(Arc::clone(b)),
            None => {
                let mut names: Vec<&str> = backends.keys().map(String::as_str).collect();
                names.sort_unstable();
                Err(ErrNotFound {
                    name: name.to_string(),
                    known: names.join(", "),
                }
                .into())
            }
        }
    }

    /// Registers a factory function for creating providers.
    pub fn register_factory<F>(&self, name: &str, factory: F)
    where
        F: Fn(ProviderConfig) -> Box<dyn Provider> + Send + Sync + 'static,
    {
        let mut factories = self.factories.write().unwrap_or_else(|e| e.into_inner());
        factories.insert(name.to_string(), Arc::new(factory));
    }

    /// Returns a fresh instance of the named built-in backend, configured
    /// with `cfg`.
    pub fn create_provider(
        &self,
        name: &str,
        cfg: ProviderConfig,
    ) -> anyhow::Result<Box<dyn Provider>> {
        let factory = {
            let factories = self.factories.read().unwrap_or_else(|e| e.into_inner());
            factories.get(name).cloned()
        };
        match factory {
            Some(f) => Ok(f(cfg)),
            None => anyhow::bail!(
                "no factory registered for provider {}",
                agent_base::gostd::strconv::quote(name)
            ),
        }
    }

    /// Registers the custom backend a JSON file configures, and returns it.
    pub fn load_from_file(&self, path: &str) -> anyhow::Result<Arc<dyn Backend>> {
        let data = agent_base::gostd::os::read_file(path).context("reading backend config")?;

        let cfg: GenericConfig = crate::gojson::unmarshal_struct(&data, "backend.GenericConfig")
            .context("parsing backend config")?;

        if cfg.name.is_empty() {
            anyhow::bail!("backend config must have a name");
        }

        if cfg.launch_command.is_empty() {
            anyhow::bail!("backend config must have a launch_command");
        }

        let b: Arc<dyn Backend> = Arc::new(new_generic_backend(cfg));
        self.register(Arc::clone(&b));
        Ok(b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_unknown_names_the_known_ones() {
        let r = new_registry();
        let err = r.get("nope").err().expect("an error");
        assert_eq!(
            format!("{err:#}"),
            "backend \"nope\" not found (known: agy, aider, claude, shell; or define one with --backend-config <file>)"
        );
        assert!(err.downcast_ref::<ErrNotFound>().is_some());
        assert_eq!(r.get("claude").unwrap().name(), "claude");
    }

    #[test]
    fn load_from_file() {
        let dir = tempfile::Builder::new().tempdir_in("/var/tmp").unwrap();
        let path = dir.path().join("my-cli.json");
        let path = path.to_str().unwrap();
        std::fs::write(
            path,
            r#"{"name": "my-cli", "launch_command": "my-cli --some-flag", "init_message_format": "Please read %s",
 "idle_pattern": "^> $", "processing_pattern": "(Working|Thinking)"}"#,
        )
        .unwrap();
        let r = new_registry();
        let b = r.load_from_file(path).unwrap();
        assert_eq!(b.launch_command(), "my-cli --some-flag");
        assert_eq!(b.init_message("t.md"), "Please read t.md");
        assert_eq!(r.get("my-cli").unwrap().idle_pattern(), "^> $");

        std::fs::write(path, r#"{"name": "x"}"#).unwrap();
        let err = r.load_from_file(path).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            "backend config must have a launch_command"
        );
        std::fs::write(path, r#"{"launch_command": "x"}"#).unwrap();
        let err = r.load_from_file(path).err().unwrap();
        assert_eq!(format!("{err:#}"), "backend config must have a name");

        let missing = dir.path().join("missing.json");
        let missing = missing.to_str().unwrap();
        let err = r.load_from_file(missing).err().unwrap();
        assert_eq!(
            format!("{err:#}"),
            format!("reading backend config: open {missing}: no such file or directory")
        );

        std::fs::write(path, r#"{"name":"c\ud800","launch_command":"x"}"#).unwrap();
        let b = r.load_from_file(path).unwrap();
        assert_eq!(b.name(), "c\u{fffd}");
    }
}
