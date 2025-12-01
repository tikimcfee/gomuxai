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

// LogEntry represents a captured log entry.
type LogEntry struct {
	ID         int64     `json:"id"`
	AgentName  string    `json:"agent_name"`
	Content    string    `json:"content"`
	CapturedAt time.Time `json:"captured_at"`
}

// StateEntry represents a key-value state entry.
type StateEntry struct {
	AgentName string    `json:"agent_name"`
	Key       string    `json:"key"`
	Value     string    `json:"value"`
	UpdatedAt time.Time `json:"updated_at"`
}
