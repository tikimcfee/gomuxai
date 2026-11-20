//! Go `internal/service/errors.go`.

use agent_base::util;

use super::AgentService;

/// Returned for a name that isn't an agent on this host. Its message says
/// what to use instead: the agent whose tmux session the name is, close
/// names, or how many agents there are and how to list them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentNotFoundError {
    pub name: String,
    pub host: String,
    /// agents on the host
    pub count: usize,
    /// close agent names
    pub similar: Vec<String>,
    /// SessionOf is the agent whose session Name is, if it is one.
    pub session_of: String,
    /// Remote is set when the request came over the bus, so listing the
    /// agents takes --host.
    pub remote: bool,
}

impl std::fmt::Display for AgentNotFoundError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut msg = format!(
            "agent {} not found",
            agent_base::gostd::strconv::quote(&self.name)
        );
        if !self.host.is_empty() {
            msg += &format!(" on host {}", self.host);
        }
        if !self.session_of.is_empty() {
            return write!(
                f,
                "{msg}: that is the session of agent {}, so use the agent name {}",
                self.session_of, self.session_of
            );
        }
        if !self.similar.is_empty() {
            return write!(
                f,
                "{msg}; did you mean {}?",
                util::or_list(&self.similar, None)
            );
        }
        if self.count == 0 {
            return write!(f, "{msg} (it has no agents; agent spawn starts one)");
        }
        let mut list = "agent list".to_string();
        if self.remote && !self.host.is_empty() {
            list += &format!(" --host {}", self.host);
        }
        let agents = if self.count == 1 { "agent" } else { "agents" };
        write!(f, "{msg} ({} {agents} there: {list})", self.count)
    }
}

impl std::error::Error for AgentNotFoundError {}

impl AgentService {
    /// notFound builds the error for name, which has no agent record.
    pub(crate) fn not_found(&self, name: &str) -> anyhow::Error {
        let mut e = AgentNotFoundError {
            name: name.to_string(),
            host: self.cfg.host.clone(),
            ..Default::default()
        };
        let Ok(agents) = self.storage.list_agents() else {
            return e.into();
        };
        e.count = agents.len();
        let mut names = Vec::with_capacity(agents.len());
        for a in &agents {
            names.push(a.name.clone());
            if a.session_name == name {
                e.session_of = a.name.clone();
            }
        }
        e.similar = util::similar(name, &names);
        e.into()
    }
}

/// remoteErr marks an AgentNotFoundError as answering a request from another
/// host, whose agent list takes --host. Like the Go's type assertion, only an
/// error that is itself one (not one wrapped with context) is marked.
pub(crate) fn remote_err(err: anyhow::Error) -> anyhow::Error {
    if let Some(nf) = err.downcast_ref::<AgentNotFoundError>()
        && err.chain().count() == 1
    {
        let mut c = nf.clone();
        c.remote = true;
        return c.into();
    }
    err
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_messages() {
        let mut e = AgentNotFoundError {
            name: "x".into(),
            host: "hub".into(),
            ..Default::default()
        };
        assert_eq!(
            e.to_string(),
            r#"agent "x" not found on host hub (it has no agents; agent spawn starts one)"#
        );
        e.count = 1;
        assert_eq!(
            e.to_string(),
            r#"agent "x" not found on host hub (1 agent there: agent list)"#
        );
        e.count = 3;
        e.remote = true;
        assert_eq!(
            e.to_string(),
            r#"agent "x" not found on host hub (3 agents there: agent list --host hub)"#
        );
        e.similar = vec!["xa".into()];
        assert_eq!(
            e.to_string(),
            r#"agent "x" not found on host hub; did you mean xa?"#
        );
        e.session_of = "lead".into();
        assert_eq!(
            e.to_string(),
            r#"agent "x" not found on host hub: that is the session of agent lead, so use the agent name lead"#
        );
        let wrapped = anyhow::Error::from(AgentNotFoundError {
            remote: false,
            ..e.clone()
        })
        .context("getting");
        assert!(
            !remote_err(wrapped)
                .downcast_ref::<AgentNotFoundError>()
                .unwrap()
                .remote
        );
        let plain = remote_err(anyhow::Error::from(AgentNotFoundError {
            remote: false,
            ..e
        }));
        assert!(plain.downcast_ref::<AgentNotFoundError>().unwrap().remote);
    }
}
