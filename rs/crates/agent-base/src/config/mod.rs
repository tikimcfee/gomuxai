//! Go `internal/config`: configuration management for the agent tool.

#[allow(clippy::module_inception)]
mod config;
#[cfg(target_os = "macos")]
mod host_darwin;
#[cfg(not(target_os = "macos"))]
mod host_other;

pub use config::*;
#[cfg(target_os = "macos")]
use host_darwin::local_host_name;
#[cfg(not(target_os = "macos"))]
use host_other::local_host_name;
