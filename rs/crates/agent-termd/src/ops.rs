use anyhow::{Result, bail};
use std::collections::HashMap;
use std::sync::Arc;

/// Describes one live terminal in a `g3d-termd` namespace.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Session {
    pub name: String,
    pub created: String,
    pub attached: bool,
    pub session_id: String,
}

/// Describes a terminal's id, root process PID, and working directory.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PaneInfo {
    pub id: String,
    pub pid: i32,
    pub cwd: String,
}

/// Defines the terminal multiplexer operations gomuxai uses.
pub trait Ops: Send + Sync {
    /// Creates a new terminal running the user's shell in the caller's working
    /// directory with the given request environment.
    fn create_session(&self, name: &str, env: &HashMap<String, String>) -> Result<()>;

    /// Creates a new terminal running the user's shell in `dir` (`""` for the
    /// caller's working directory).
    fn create_session_in(&self, name: &str, dir: &str, env: &HashMap<String, String>)
    -> Result<()>;

    /// Terminates a terminal and its process tree.
    fn kill_session(&self, name: &str) -> Result<()>;

    /// Kills whatever runs in `session` and starts a fresh shell under the same
    /// terminal id, preserving its working directory and environment.
    fn respawn_pane(&self, session: &str) -> Result<()>;

    /// Reports whether a live terminal named `name` exists.
    fn session_exists(&self, name: &str) -> Result<bool>;

    /// Returns all live terminals in the namespace.
    fn list_sessions(&self) -> Result<Vec<Session>>;

    /// Sends literal text to a terminal.
    fn send_keys(&self, session: &str, text: &str) -> Result<()>;

    /// Pastes text into a terminal (bracketed when the running program
    /// requested bracketed paste).
    fn paste_text(&self, session: &str, text: &str) -> Result<()>;

    /// Sends a special key or chord (such as `Enter`, `Escape`, `c-c`) to a
    /// terminal.
    fn send_special_key(&self, session: &str, key: &str) -> Result<()>;

    /// Returns `session`'s visible screen and up to `lines` rows of scrollback
    /// as plain text.
    fn capture(&self, session: &str, lines: i32) -> Result<String>;

    /// Returns `session`'s visible screen and up to `lines` rows of scrollback
    /// with SGR styling escapes preserved.
    fn capture_styled(&self, session: &str, lines: i32) -> Result<String>;

    /// Maps the root process PID of each live terminal to its terminal id.
    fn pane_pids(&self) -> Result<HashMap<i32, String>>;

    /// Returns the daemon namespace this multiplexer manages, or `""` for fakes
    /// without a namespace.
    fn namespace(&self) -> &str {
        ""
    }

    /// Lists the ids of live terminals in the namespace.
    fn session_names(&self) -> Result<Vec<String>> {
        Ok(self.list_sessions()?.into_iter().map(|s| s.name).collect())
    }

    /// Returns a variable from `session`'s environment, or `""`.
    fn session_env(&self, _session: &str, _name: &str) -> String {
        String::new()
    }

    /// Returns the id, root PID, and working directory of `session`.
    fn first_pane(&self, session: &str) -> Result<PaneInfo> {
        let pids = self.pane_pids()?;
        let pid = pids
            .into_iter()
            .find_map(|(pid, id)| (id == session).then_some(pid))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "session {} not found",
                    agent_base::gostd::strconv::quote(session)
                )
            })?;
        let cwd = std::env::current_dir()
            .ok()
            .map(|d| d.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(PaneInfo {
            id: session.to_owned(),
            pid,
            cwd,
        })
    }

    /// Maps a `G3D_TERMD_ID` value to its terminal name if live, or `""`.
    fn session_of_pane(&self, _pane: &str) -> Result<String> {
        Ok(String::new())
    }

    /// Returns the `g3d` executable and argv to replace this process with when
    /// attaching a local terminal to `session`.
    fn attach_command(&self, _session: &str) -> Result<(String, Vec<String>)> {
        bail!("multiplexer does not support interactive attach")
    }
}

impl<T: Ops + ?Sized> Ops for &T {
    fn create_session(&self, name: &str, env: &HashMap<String, String>) -> Result<()> {
        (**self).create_session(name, env)
    }

    fn create_session_in(
        &self,
        name: &str,
        dir: &str,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        (**self).create_session_in(name, dir, env)
    }

    fn kill_session(&self, name: &str) -> Result<()> {
        (**self).kill_session(name)
    }

    fn respawn_pane(&self, session: &str) -> Result<()> {
        (**self).respawn_pane(session)
    }

    fn session_exists(&self, name: &str) -> Result<bool> {
        (**self).session_exists(name)
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        (**self).list_sessions()
    }

    fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        (**self).send_keys(session, text)
    }

    fn paste_text(&self, session: &str, text: &str) -> Result<()> {
        (**self).paste_text(session, text)
    }

    fn send_special_key(&self, session: &str, key: &str) -> Result<()> {
        (**self).send_special_key(session, key)
    }

    fn capture(&self, session: &str, lines: i32) -> Result<String> {
        (**self).capture(session, lines)
    }

    fn capture_styled(&self, session: &str, lines: i32) -> Result<String> {
        (**self).capture_styled(session, lines)
    }

    fn pane_pids(&self) -> Result<HashMap<i32, String>> {
        (**self).pane_pids()
    }

    fn namespace(&self) -> &str {
        (**self).namespace()
    }

    fn session_names(&self) -> Result<Vec<String>> {
        (**self).session_names()
    }

    fn session_env(&self, session: &str, name: &str) -> String {
        (**self).session_env(session, name)
    }

    fn first_pane(&self, session: &str) -> Result<PaneInfo> {
        (**self).first_pane(session)
    }

    fn session_of_pane(&self, pane: &str) -> Result<String> {
        (**self).session_of_pane(pane)
    }

    fn attach_command(&self, session: &str) -> Result<(String, Vec<String>)> {
        (**self).attach_command(session)
    }
}

impl<T: Ops + ?Sized> Ops for Box<T> {
    fn create_session(&self, name: &str, env: &HashMap<String, String>) -> Result<()> {
        (**self).create_session(name, env)
    }

    fn create_session_in(
        &self,
        name: &str,
        dir: &str,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        (**self).create_session_in(name, dir, env)
    }

    fn kill_session(&self, name: &str) -> Result<()> {
        (**self).kill_session(name)
    }

    fn respawn_pane(&self, session: &str) -> Result<()> {
        (**self).respawn_pane(session)
    }

    fn session_exists(&self, name: &str) -> Result<bool> {
        (**self).session_exists(name)
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        (**self).list_sessions()
    }

    fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        (**self).send_keys(session, text)
    }

    fn paste_text(&self, session: &str, text: &str) -> Result<()> {
        (**self).paste_text(session, text)
    }

    fn send_special_key(&self, session: &str, key: &str) -> Result<()> {
        (**self).send_special_key(session, key)
    }

    fn capture(&self, session: &str, lines: i32) -> Result<String> {
        (**self).capture(session, lines)
    }

    fn capture_styled(&self, session: &str, lines: i32) -> Result<String> {
        (**self).capture_styled(session, lines)
    }

    fn pane_pids(&self) -> Result<HashMap<i32, String>> {
        (**self).pane_pids()
    }

    fn namespace(&self) -> &str {
        (**self).namespace()
    }

    fn session_names(&self) -> Result<Vec<String>> {
        (**self).session_names()
    }

    fn session_env(&self, session: &str, name: &str) -> String {
        (**self).session_env(session, name)
    }

    fn first_pane(&self, session: &str) -> Result<PaneInfo> {
        (**self).first_pane(session)
    }

    fn session_of_pane(&self, pane: &str) -> Result<String> {
        (**self).session_of_pane(pane)
    }

    fn attach_command(&self, session: &str) -> Result<(String, Vec<String>)> {
        (**self).attach_command(session)
    }
}

impl<T: Ops + ?Sized> Ops for Arc<T> {
    fn create_session(&self, name: &str, env: &HashMap<String, String>) -> Result<()> {
        (**self).create_session(name, env)
    }

    fn create_session_in(
        &self,
        name: &str,
        dir: &str,
        env: &HashMap<String, String>,
    ) -> Result<()> {
        (**self).create_session_in(name, dir, env)
    }

    fn kill_session(&self, name: &str) -> Result<()> {
        (**self).kill_session(name)
    }

    fn respawn_pane(&self, session: &str) -> Result<()> {
        (**self).respawn_pane(session)
    }

    fn session_exists(&self, name: &str) -> Result<bool> {
        (**self).session_exists(name)
    }

    fn list_sessions(&self) -> Result<Vec<Session>> {
        (**self).list_sessions()
    }

    fn send_keys(&self, session: &str, text: &str) -> Result<()> {
        (**self).send_keys(session, text)
    }

    fn paste_text(&self, session: &str, text: &str) -> Result<()> {
        (**self).paste_text(session, text)
    }

    fn send_special_key(&self, session: &str, key: &str) -> Result<()> {
        (**self).send_special_key(session, key)
    }

    fn capture(&self, session: &str, lines: i32) -> Result<String> {
        (**self).capture(session, lines)
    }

    fn capture_styled(&self, session: &str, lines: i32) -> Result<String> {
        (**self).capture_styled(session, lines)
    }

    fn pane_pids(&self) -> Result<HashMap<i32, String>> {
        (**self).pane_pids()
    }

    fn namespace(&self) -> &str {
        (**self).namespace()
    }

    fn session_names(&self) -> Result<Vec<String>> {
        (**self).session_names()
    }

    fn session_env(&self, session: &str, name: &str) -> String {
        (**self).session_env(session, name)
    }

    fn first_pane(&self, session: &str) -> Result<PaneInfo> {
        (**self).first_pane(session)
    }

    fn session_of_pane(&self, pane: &str) -> Result<String> {
        (**self).session_of_pane(pane)
    }

    fn attach_command(&self, session: &str) -> Result<(String, Vec<String>)> {
        (**self).attach_command(session)
    }
}
