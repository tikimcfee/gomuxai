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
	// ReplaceAgent closes the named agent's run and opens a new one under
	// agent, keeping its state and messages (a resume).
	ReplaceAgent(ctx context.Context, agent *Agent) error

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
	// ClaimHandover marks receiverID's handover message id delivered and
	// returns it; nil if there is no such message awaiting handover.
	ClaimHandover(ctx context.Context, receiverID string, id int64) (*Message, error)
	// FailHandover marks handover message id failed, reporting whether this
	// call did: false if it was already handed over.
	FailHandover(ctx context.Context, id int64) (bool, error)
	// GetMessageStatus returns message id's status.
	GetMessageStatus(ctx context.Context, id int64) (MessageStatus, error)

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

	// Session reports: every session's last hook report (the global hook).
	RecordSessionReport(ctx context.Context, r *SessionReport) error
	SessionReports(ctx context.Context) (map[string]*SessionReport, error)

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
	AddFrictionLink(ctx context.Context, l *FrictionLink) error                       // or update the existing one
	ListFrictionLinks(ctx context.Context, frictionID int64) ([]*FrictionLink, error) // 0: every link
	UpdateFrictionLink(ctx context.Context, l *FrictionLink) error
	DeleteFrictionLink(ctx context.Context, frictionID int64, peerHost string, peerID int64) (bool, error)

	// RenameHost rewrites references to another host renamed from old to
	// new: agents' spawned_by and friction links.
	RenameHost(ctx context.Context, old, new string) (HostRenamed, error)

	// Lifecycle
	Close() error
}
