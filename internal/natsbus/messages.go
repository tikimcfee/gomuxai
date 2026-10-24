package natsbus

import "time"

// EventType enumerates agent event types.
type EventType string

const (
	EventSpawned   EventType = "spawned"
	EventDone      EventType = "done"
	EventStatus    EventType = "status"
	EventRequest   EventType = "request"
	EventMessage   EventType = "message"
	EventError     EventType = "error"
	EventCleanedUp EventType = "cleaned_up"
)

// AgentEvent is published on agent.{name}.event.{type}.
type AgentEvent struct {
	Agent     string    `json:"agent"`
	Type      EventType `json:"type"`
	Status    string    `json:"status,omitempty"`  // for EventStatus
	Message   string    `json:"message,omitempty"` // for EventMessage, EventRequest, EventDone
	From      string    `json:"from,omitempty"`    // sender agent name
	Timestamp time.Time `json:"timestamp"`
}

// InboxMessage is published on agent.{name}.inbox.
type InboxMessage struct {
	From      string    `json:"from"`
	Content   string    `json:"content"`
	ReplyTo   string    `json:"reply_to,omitempty"` // NATS reply subject for ack
	Timestamp time.Time `json:"timestamp"`
}
