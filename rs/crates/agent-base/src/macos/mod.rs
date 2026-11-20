//! Go `internal/macos`: what the agent bus needs to reach the local network
//! on macOS.

#[allow(clippy::module_inception)]
mod macos;
#[cfg(target_os = "macos")]
mod spawn_darwin;
#[cfg(not(target_os = "macos"))]
mod spawn_other;

pub use macos::*;
#[cfg(target_os = "macos")]
pub use spawn_darwin::*;
#[cfg(not(target_os = "macos"))]
pub use spawn_other::*;
