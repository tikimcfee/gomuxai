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
	// ClaimMessage marks a pending message delivered; false if it wasn't
	// pending (someone else delivered it).
	ClaimMessage(ctx context.Context, id int64) (bool, error)
	// ClaimPendingMessages marks receiverID's pending messages delivered,
	// returning them oldest first, and applies update's state change for
	// them in the same transaction (update may be nil).
	ClaimPendingMessages(ctx context.Context, receiverID string, update func(claimed []*Message) StateUpdate) ([]*Message, error)
	DeleteMessagesByAgent(ctx context.Context, agentName string) error

	// Task operations (standalone task repository)
	SetTask(ctx context.Context, task *Task) error
	GetTask(ctx context.Context, name string) (*Task, error)
	ListTasks(ctx context.Context) ([]*Task, error)
	DeleteTask(ctx context.Context, name string) error

	// State operations
	SetState(ctx context.Context, entry *StateEntry) error
	SetStates(ctx context.Context, entries []*StateEntry) error // atomic
	// UpdateStates sets entries and deletes agentName's keys in del, atomically.
	UpdateStates(ctx context.Context, entries []*StateEntry, agentName string, del []string) error
	// RecordReport is UpdateStates plus an agent event, atomically.
	RecordReport(ctx context.Context, entries []*StateEntry, agentName string, del []string, ev *AgentEvent) error
	GetState(ctx context.Context, agentName, key string) (*StateEntry, error)
	ListState(ctx context.Context, agentName string) ([]*StateEntry, error)
	DeleteState(ctx context.Context, agentName string) error
	DeleteStateKey(ctx context.Context, agentName, key string) error

	// Command log
	LogCommand(ctx context.Context, rec *CommandRecord) error
	ListCommands(ctx context.Context, f CommandFilter) ([]*CommandRecord, error) // newest first

	// Friction box
	AddFriction(ctx context.Context, f *Friction) error
	GetFriction(ctx context.Context, id int64) (*Friction, error)            // nil if none
	ListFriction(ctx context.Context, f FrictionFilter) ([]*Friction, error) // most votes, then newest, first
	LastFrictionID(ctx context.Context) (int64, error)                       // 0 if none
	UpdateFriction(ctx context.Context, f *Friction) error
	AddFrictionVote(ctx context.Context, v *FrictionVote) error
	ListFrictionVotes(ctx context.Context, frictionID int64) ([]*FrictionVote, error) // oldest first

	// Lifecycle
	Close() error
}
