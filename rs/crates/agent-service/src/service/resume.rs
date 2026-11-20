//! Go `internal/service/resume.go`.

use anyhow::{Context, Result, anyhow, bail};

use agent_base::types::resumable_backend;
use agent_store::Agent;

use super::agent_service::{
    AGENT_STATUS_ENDED, AgentService, AgentStatus, SpawnInternal, SpawnOptions,
};
use super::ctx::Ctx;
use super::status::{
    STATE_KEY_END_REASON, STATE_KEY_MODEL, STATE_KEY_RESULT, STATE_KEY_STATUS, STATE_KEY_TRANSCRIPT,
};

/// ResumeOptions holds options for resuming an ended agent.
#[derive(Debug, Clone, Default)]
pub struct ResumeOptions {
    pub name: String,
    /// SessionID is the session the agent continues: its last one (Claude's
    /// session id, agy's conversation id).
    pub session_id: String,
    /// Cwd is the directory the session ran in, where the agent starts;
    /// "" for this process's working directory.
    pub cwd: String,
    /// Pass --dangerously-skip-permissions to Claude / Agy
    pub skip_permissions: bool,
    /// Confirm the CLI's workspace trust prompt on launch
    pub trust_workspace: bool,
}

/// keptOnResume are the reserved state keys a resume keeps: the last turn's
/// reply, the transcript of the session it continues, and the model it was
/// spawned on. The rest describe
/// the run that ended (its status, prompts, spawn id) and would only mislead.
pub(crate) const KEPT_ON_RESUME: &[&str] =
    &[STATE_KEY_RESULT, STATE_KEY_TRANSCRIPT, STATE_KEY_MODEL];

impl AgentService {
    /// Resume relaunches an ended agent on its session, under its own record:
    /// the same name, backend and parent, so its helpers stay its helpers, its
    /// own state keys stay, and messages held for it are delivered. It starts
    /// in opts.cwd, where the session ran. Only an
    /// ended claude or agy agent that gomuxai started can be resumed: an adopted
    /// one runs in the person's own tmux session (adopted_resume_error). A failed
    /// resume leaves it ended.
    pub fn resume(&self, ctx: &Ctx, opts: ResumeOptions) -> Result<Agent> {
        let agent = self.get_agent(&opts.name)?;
        if agent.adopted {
            return Err(self.adopted_resume_error(&agent));
        }
        let (info, _) = self.resolve_status(&agent.name)?;
        self.resume_refusal(&agent, &info.status, &opts.session_id)?;

        // A spawned CLI that quit leaves its terminal behind, running a bare shell.
        if self
            .tmux
            .session_exists(&agent.session_name)
            .unwrap_or(false)
        {
            self.tmux
                .kill_session(&agent.session_name)
                .with_context(|| format!("closing its old session {}", agent.session_name))?;
        }
        // Its run closes with the status it ended in, which its last report may
        // not say (no hook fires when the machine goes down).
        for (key, value) in [
            (STATE_KEY_STATUS, AGENT_STATUS_ENDED.to_string()),
            (STATE_KEY_END_REASON, info.end_reason.as_str().to_string()),
        ] {
            self.set_state(&agent.name, key, &value)
                .context("recording how its run ended")?;
        }

        let resumed = self.spawn_internal(
            ctx,
            SpawnOptions {
                name: agent.name.clone(),
                backend: agent.backend.clone(),
                spawned_by: agent.spawned_by.clone(),
                model: self.state_value(&agent.name, STATE_KEY_MODEL),
                skip_permissions: opts.skip_permissions,
                trust_workspace: opts.trust_workspace,
                resume: opts.session_id.clone(),
                cwd: opts.cwd.clone(),
                ..Default::default()
            },
            SpawnInternal {
                replacing: Some(agent.clone()),
                ..Default::default()
            },
        )?;
        // Messages held for it when it ended never went out: the deliverer types
        // them in once it sits at its prompt.
        if self.held_count(&agent.name) > 0 {
            let _ = self.start_deliverer(&agent.name);
        }
        Ok(resumed)
    }

    /// ResumeRefusal says why resume refuses agent, given its resolved status
    /// and the session it would continue; Ok when it can be resumed. It holds
    /// types.Resumable's rules, with a reason for each.
    pub fn resume_refusal(
        &self,
        agent: &Agent,
        status: &AgentStatus,
        session_id: &str,
    ) -> Result<()> {
        if agent.adopted {
            return Err(self.adopted_resume_error(agent));
        }
        if !resumable_backend(&agent.backend) {
            bail!(
                "agent {} runs {}: resume continues claude and agy sessions",
                agent.name,
                agent.backend
            );
        }
        if session_id.is_empty() {
            bail!(
                "no session to resume agent {} on: its CLI never named one (agent logs {} --terminal shows what it did)",
                agent.name,
                agent.name
            );
        }
        if *status != AGENT_STATUS_ENDED {
            bail!(
                "agent {} is {status}, not ended: only an ended agent can be resumed (agent status {} -v)",
                agent.name,
                agent.name
            );
        }
        Ok(())
    }

    /// LastSessionID is the session resume would continue the agent on, as far
    /// as its records say: the one its newest transcript names, else its
    /// _session_id. Empty when it has none.
    pub fn last_session_id(&self, name: &str) -> String {
        if let Some(last) = self.transcripts(name).last() {
            let id = agent_transcript::session_id_from_path(&last.path);
            if !id.is_empty() {
                return id;
            }
        }
        self.state_value(name, super::status::STATE_KEY_SESSION_ID)
    }

    /// AdoptedResumeError is why an adopted agent can't be resumed: its session is
    /// the person's own, so gomuxai neither types a launch into their shell nor
    /// makes a session to replace it. It says what to do instead.
    pub fn adopted_resume_error(&self, agent: &Agent) -> anyhow::Error {
        let cli = if agent.backend == "agy" {
            "agy"
        } else {
            "claude"
        };
        if !self
            .tmux
            .session_exists(&agent.session_name)
            .unwrap_or(false)
        {
            return anyhow!(
                "agent {} is an adopted session (session {}, which is gone): resume doesn't relaunch CLIs in sessions gomuxai didn't start; agent cleanup {}, then agent spawn {} --backend {cli} --continue (or --resume <session>) starts it in a session of gomuxai's own",
                agent.name,
                agent.session_name,
                agent.name,
                agent.name
            );
        }
        anyhow!(
            "agent {} is an adopted session (session {}): resume doesn't relaunch CLIs in sessions gomuxai didn't start; start the CLI there yourself ({cli} -c continues its last conversation), and adopt keeps tracking it",
            agent.name,
            agent.session_name
        )
    }

    /// clearRunState deletes the reserved state keys that describe the agent's
    /// ended run, except KEPT_ON_RESUME.
    pub(crate) fn clear_run_state(&self, name: &str) -> Result<()> {
        let entries = self.storage.list_state(name).context("reading its state")?;
        let del: Vec<String> = entries
            .into_iter()
            .filter(|e| e.key.starts_with('_') && !KEPT_ON_RESUME.contains(&e.key.as_str()))
            .map(|e| e.key)
            .collect();
        if del.is_empty() {
            return Ok(());
        }
        self.storage
            .update_states(&[], name, &del)
            .context("clearing its ended run's status")?;
        Ok(())
    }
}
