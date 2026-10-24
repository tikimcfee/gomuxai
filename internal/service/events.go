package service

import (
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// EventPublisher publishes status events to the agent bus.
// natsbus.Publisher implements it.
type EventPublisher interface {
	PublishStatus(natsbus.StatusEvent) error
}

// SetEventPublisher makes Report publish a StatusEvent for every status it
// records. nil (the default) publishes nothing.
func (s *AgentService) SetEventPublisher(p EventPublisher) {
	s.events = p
}

// publishStatus announces a recorded report on the bus. Best-effort: the
// report is already stored, which is what status readers rely on, so a
// missing or failing bus must not fail it. Publishers that want failures
// seen (other than natsbus.ErrNoBus) report them themselves.
func (s *AgentService) publishStatus(name string, r StatusReport, at time.Time) {
	if s.events == nil {
		return
	}
	_ = s.events.PublishStatus(natsbus.StatusEvent{
		Host:   s.cfg.Host,
		Agent:  name,
		Status: string(r.Status),
		Event:  r.Event,
		Detail: r.Detail,
		Tool:   r.Tool,
		At:     at,
	})
}
