package storage

import (
	"context"
	"time"
)

// Storage defines the interface for persistent storage operations.
type Storage interface {
	// Agent operations
	CreateAgent(ctx context.Context, agent *Agent) error
	GetAgent(ctx context.Context, name string) (*Agent, error)
	ListAgents(ctx context.Context) ([]*Agent, error)
	DeleteAgent(ctx context.Context, name string) error

	// Message operations
	CreateMessage(ctx context.Context, msg *Message) error
	GetPendingMessages(ctx context.Context, receiverID string, limit int) ([]*Message, error)
	UpdateMessageStatus(ctx context.Context, id int64, status MessageStatus) error
	DeleteMessagesByAgent(ctx context.Context, agentName string) error

	// Task operations (standalone task repository)
	SetTask(ctx context.Context, task *Task) error
	GetTask(ctx context.Context, name string) (*Task, error)
	ListTasks(ctx context.Context) ([]*Task, error)
	DeleteTask(ctx context.Context, name string) error

	// Log operations
	AppendLog(ctx context.Context, entry *LogEntry) error
	GetLogs(ctx context.Context, agentName string, limit int) ([]*LogEntry, error)
	GetLogsSince(ctx context.Context, agentName string, since time.Time) ([]*LogEntry, error)
	DeleteLogs(ctx context.Context, agentName string) error

	// State operations
	SetState(ctx context.Context, entry *StateEntry) error
	GetState(ctx context.Context, agentName, key string) (*StateEntry, error)
	ListState(ctx context.Context, agentName string) ([]*StateEntry, error)
	DeleteState(ctx context.Context, agentName string) error

	// Lifecycle
	Close() error
}
