//! Go `internal/service/adopt.go`.

use anyhow::{Result, bail};

use agent_backend::{PaneSession, ProviderConfig};
use agent_store::Agent;
use agent_termd::PaneInfo;

use super::agent_service::{AgentService, SpawnInternal, SpawnOptions, validate_agent_name};
use super::status::{EVENT_ADOPTED, STATE_KEY_SESSION_ID, StatusReport, from_backend_status};

/// AdoptOptions holds options for adopting an existing termd session onto the bus.
#[derive(Debug, Clone, Default)]
pub struct AdoptOptions {
    pub name: String,
    /// explicit target terminal id (--session); "" uses current_pane
    pub session_name: String,
    /// caller's $G3D_TERMD_ID, "" when outside g3d
    pub current_pane: String,
    /// explicit --backend override; "" auto-detects Claude or defaults to "shell"
    pub backend: String,
    pub spawned_by: String,
}

/// AdoptResult describes the outcome of adopt.
#[derive(Debug, Clone, Default)]
pub struct AdoptResult {
    pub agent: Agent,
    /// detected Claude session ID, if any
    pub session_id: String,
}

impl AgentService {
    /// Adopt registers an existing termd session on the agent bus without launching,
    /// stopping, or restarting any processes in the session.
    pub fn adopt(&self, opts: AdoptOptions) -> Result<AdoptResult> {
        validate_agent_name(&opts.name)?;

        let mut target_session = opts.session_name.trim().to_string();
        if target_session.is_empty() && !opts.current_pane.is_empty() {
            target_session = self.session_of_pane(&opts.current_pane)?;
        }
        if target_session.is_empty() {
            bail!("not in a termd session: run agent adopt inside g3d or pass --session <session>");
        }

        let pane = self.first_pane(&target_session)?;

        let mut pane_session: Option<PaneSession> = None;
        if pane.pid > 0
            && (opts.backend.is_empty() || opts.backend == "claude")
            && let Ok(p) = self
                .backends
                .create_provider("claude", ProviderConfig::default())
            && let Some(pp) = p.as_pane_session_provider()
            && let Ok(Some(ps)) = pp.pane_session(pane.pid)
            && !ps.id.is_empty()
        {
            pane_session = Some(ps);
        }

        let mut backend_name = opts.backend.clone();
        if backend_name.is_empty() {
            backend_name = if pane_session.is_some() {
                "claude".into()
            } else {
                "shell".into()
            };
        }

        self.backends.get(&backend_name)?;
        self.prepare_existing_session(&opts.name, &target_session)?;
        let agent = self.store_agent(
            &SpawnOptions {
                name: opts.name.clone(),
                backend: backend_name,
                spawned_by: opts.spawned_by.clone(),
                ..Default::default()
            },
            &SpawnInternal {
                existing_session: target_session.clone(),
                session_cwd: pane.cwd.clone(),
                ..Default::default()
            },
            &target_session,
        )?;

        let mut session_id = String::new();
        if let Some(ps) = pane_session {
            session_id = ps.id.clone();
            let _ = self.set_state(&opts.name, STATE_KEY_SESSION_ID, &ps.id);
            let _ = self.report(
                &opts.name,
                StatusReport {
                    status: from_backend_status(ps.status),
                    event: EVENT_ADOPTED.into(),
                    detail: format!("adopted session {} (pid {}, {})", ps.id, ps.pid, ps.detail),
                    session_id: ps.id.clone(),
                    ..Default::default()
                },
            );
        }

        Ok(AdoptResult { agent, session_id })
    }

    /// The session a pane (`$G3D_TERMD_ID`) belongs to; "" from a multiplexer
    /// that can't say (Go: one that isn't a paneInspector).
    fn session_of_pane(&self, pane: &str) -> Result<String> {
        self.tmux.session_of_pane(pane)
    }

    /// The session's first pane: its id, root pid and working directory.
    fn first_pane(&self, session_name: &str) -> Result<PaneInfo> {
        self.tmux.first_pane(session_name)
    }
}
