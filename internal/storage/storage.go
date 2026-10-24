package storage

import (
	"context"
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

	// State operations
	SetState(ctx context.Context, entry *StateEntry) error
	SetStates(ctx context.Context, entries []*StateEntry) error // atomic
	GetState(ctx context.Context, agentName, key string) (*StateEntry, error)
	ListState(ctx context.Context, agentName string) ([]*StateEntry, error)
	DeleteState(ctx context.Context, agentName string) error

	// Command log
	LogCommand(ctx context.Context, rec *CommandRecord) error
	ListCommands(ctx context.Context, f CommandFilter) ([]*CommandRecord, error) // newest first

	// Lifecycle
	Close() error
}
