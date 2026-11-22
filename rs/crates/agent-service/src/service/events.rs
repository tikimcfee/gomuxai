//! Go `internal/service/events.go`.

use agent_bus::StatusEvent;
use agent_store::Time;

use super::agent_service::AgentService;
use super::status::StatusReport;

/// EventPublisher publishes status events to the agent bus.
/// agent_bus::Publisher implements it.
pub trait EventPublisher: Send + Sync {
    fn publish_status(&self, ev: &StatusEvent) -> anyhow::Result<()>;
}

impl EventPublisher for agent_bus::Publisher {
    fn publish_status(&self, ev: &StatusEvent) -> anyhow::Result<()> {
        agent_bus::Publisher::publish_status(self, ev)
    }
}

/// A store time as the bus carries it.
pub(crate) fn bus_time(t: &Time) -> agent_bus::Time {
    agent_bus::Time(t.0)
}

impl AgentService {
    /// SetEventPublisher makes report publish a StatusEvent for every status it
    /// records. None (the default) publishes nothing.
    pub fn set_event_publisher(&mut self, p: Option<Box<dyn EventPublisher>>) {
        self.events = p;
    }

    /// publishStatus announces a recorded report on the bus. Best-effort: the
    /// report is already stored, which is what status readers rely on, so a
    /// missing or failing bus must not fail it. Publishers that want failures
    /// seen (other than agent_bus::ErrNoBus) report them themselves.
    pub(crate) fn publish_status(&self, name: &str, r: &StatusReport, at: &Time) {
        let Some(events) = &self.events else {
            return;
        };
        let _ = events.publish_status(&StatusEvent {
            host: self.cfg.host.clone(),
            agent: name.to_string(),
            status: r.status.to_string(),
            end_reason: r.end_reason.as_str().to_string(),
            event: r.event.clone(),
            detail: r.detail.clone(),
            tool: r.tool.clone(),
            at: bus_time(at),
            delivered_ids: r.delivered_ids.clone(),
            superseded_ids: r.superseded_ids.clone(),
        });
    }
}
