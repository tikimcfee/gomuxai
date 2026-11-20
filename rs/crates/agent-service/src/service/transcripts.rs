//! Go `internal/service/transcripts.go`.

use std::io::Write;

use anyhow::{Context, Result, anyhow};

use agent_backend::ProviderConfig;
use agent_base::gostd::os::PathError;
use agent_transcript::Reader;

use super::agent_service::AgentService;
use super::status::STATE_KEY_TRANSCRIPT;

/// TranscriptRef is a CLI transcript an agent's session was recorded in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TranscriptRef {
    /// the agent's backend, which knows the format
    pub backend: String,
    pub path: String,
}

/// ReadTranscriptRefs reads a transcript list (an agent's .transcripts file,
/// in any store), dropping malformed lines and repeats (two hooks can record
/// the same new path at once).
pub fn read_transcript_refs(list_path: &str) -> Vec<TranscriptRef> {
    let Ok(data) = std::fs::read(list_path) else {
        return Vec::new();
    };
    let data = String::from_utf8_lossy(&data);
    let mut refs: Vec<TranscriptRef> = Vec::new();
    for line in data.split('\n') {
        let Some((be, path)) = line.split_once('\t') else {
            continue;
        };
        if path.is_empty() || refs.last().is_some_and(|r| r.path == path) {
            continue;
        }
        refs.push(TranscriptRef {
            backend: be.to_string(),
            path: path.to_string(),
        });
    }
    refs
}

impl AgentService {
    /// recordTranscript adds path to the agent's list of transcripts, unless it
    /// is already the last one. The list lives beside the agent's log, one
    /// "<backend>\t<path>" line per transcript, so like the log it outlives
    /// cleanup and spans respawns under the same name; _transcript only knows
    /// the current one.
    pub(crate) fn record_transcript(
        &self,
        name: &str,
        backend_name: &str,
        path: &str,
    ) -> Result<()> {
        let list_path = self.cfg.transcripts_path(name);
        if read_transcript_refs(&list_path)
            .last()
            .is_some_and(|r| r.path == path)
        {
            return Ok(());
        }
        let mut f = {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&list_path)
                .map_err(|e| anyhow!(PathError::new("open", &list_path, e)))
                .context("recording transcript path")?
        };
        f.write_all(format!("{backend_name}\t{path}\n").as_bytes())
            .map_err(|e| anyhow!(PathError::new("write", &list_path, e)))
            .context("recording transcript path")?;
        Ok(())
    }

    /// Transcripts returns the transcripts the named agent's sessions were
    /// recorded in, oldest first. It reads the list beside the agent's log, so
    /// it works after cleanup; for an agent with no list yet, it falls back to
    /// the _transcript state key.
    pub fn transcripts(&self, name: &str) -> Vec<TranscriptRef> {
        let refs = read_transcript_refs(&self.cfg.transcripts_path(name));
        if !refs.is_empty() {
            return refs;
        }
        let Ok(Some(agent)) = self.storage.get_agent(name) else {
            return Vec::new();
        };
        let path = self.state_value(name, STATE_KEY_TRANSCRIPT);
        if !path.is_empty() {
            return vec![TranscriptRef {
                backend: agent.backend,
                path,
            }];
        }
        Vec::new()
    }

    /// TranscriptReader returns the reader for the transcripts of backend_name's
    /// CLI, or None if the backend has none (or isn't built in).
    pub fn transcript_reader(&self, backend_name: &str) -> Option<Box<dyn Reader>> {
        let provider = self
            .backends
            .create_provider(backend_name, ProviderConfig::default())
            .ok()?;
        provider
            .as_transcript_provider()
            .map(|tp| tp.transcript_reader())
    }
}
