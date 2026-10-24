// Package storage provides SQLite-based persistence for the agent tool.
package storage

import "time"

// Agent represents a managed agent in the database.
type Agent struct {
	Name        string    `json:"name"`
	SessionName string    `json:"session_name"`
	Backend     string    `json:"backend"`
	TaskFile    string    `json:"task_file,omitempty"`
	LogFile     string    `json:"log_file,omitempty"`
	SpawnedBy   string    `json:"spawned_by,omitempty"` // agent that spawned it; "" for a person
	CreatedAt   time.Time `json:"created_at"`
}

// MessageStatus represents the delivery status of a message.
type MessageStatus string

const (
	MessageStatusPending   MessageStatus = "pending"
	MessageStatusDelivered MessageStatus = "delivered"
	MessageStatusFailed    MessageStatus = "failed"
)

// Message represents a queued message between agents.
type Message struct {
	ID         int64         `json:"id"`
	SenderID   string        `json:"sender_id"`
	ReceiverID string        `json:"receiver_id"`
	Content    string        `json:"content"`
	Status     MessageStatus `json:"status"`
	CreatedAt  time.Time     `json:"created_at"`
}

// Task represents a reusable task definition.
// Tasks are standalone entities that can be referenced by multiple agents.
type Task struct {
	Name       string    `json:"name"`
	Content    string    `json:"content"`
	SourceFile string    `json:"source_file,omitempty"`
	CreatedAt  time.Time `json:"created_at"`
}

// CommandRecord is one run of the agent command, for agent history.
type CommandRecord struct {
	ID         int64     `json:"id"`
	StartedAt  time.Time `json:"started_at"`
	DurationMS int64     `json:"duration_ms"`
	Host       string    `json:"host"`
	Agent      string    `json:"agent,omitempty"`  // AGENT_NAME; "" for a person
	Parent     string    `json:"parent,omitempty"` // the agent's spawned_by, filled in by LogCommand
	Cwd        string    `json:"cwd"`
	Args       []string  `json:"args"` // without the program name
	ExitCode   int       `json:"exit_code"`
	Error      string    `json:"error,omitempty"` // first line of the error
	Build      string    `json:"build"`
}

// CommandFilter selects command records. The zero value selects all.
type CommandFilter struct {
	ByAgent bool   // only Agent's commands
	Agent   string // with ByAgent; "" for a person's
	Cwd     string // only commands run in this directory, if set
	Failed  bool   // only commands that exited non-zero
	Since   time.Time
	Limit   int // the newest Limit records, if set
}

// StateEntry represents a key-value state entry.
type StateEntry struct {
	AgentName string    `json:"agent_name"`
	Key       string    `json:"key"`
	Value     string    `json:"value"`
	UpdatedAt time.Time `json:"updated_at"`
}
