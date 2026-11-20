//! Go `internal/backend` (backend.go, generic.go, provider.go, registry.go)
//! and `internal/backend/namedagents`: the AI CLIs gomuxai runs agents in,
//! what each can do, and the hook contract with Claude Code and agy.
//!
//! Go backends declare what they can do by implementing optional interfaces,
//! which callers find by type assertion. Here each Go interface is a trait of
//! the same name, and [`Provider`] has one accessor per optional trait,
//! `as_<trait in snake_case>`, `None` unless the backend implements it:
//! `if let Some(h) = p.as_hook_provider() { ... }`.

pub mod backend;
pub mod generic;
mod gojson;
pub mod namedagents;
pub mod provider;
pub mod registry;
pub mod session_name;

pub use backend::Backend;
pub use generic::{GenericBackend, GenericConfig, new_generic_backend};
pub use provider::*;
pub use registry::{ErrNotFound, ProviderConfig, ProviderFactory, Registry, new_registry};
pub use session_name::session_name;
