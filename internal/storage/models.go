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
	Cwd         string    `json:"cwd,omitempty"`        // where it was spawned; "" if spawned before this was recorded
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

// Friction statuses: open → accepted → resolved, wontfix, or duplicate.
const (
	FrictionOpen      = "open"
	FrictionAccepted  = "accepted"
	FrictionResolved  = "resolved"
	FrictionWontfix   = "wontfix"
	FrictionDuplicate = "duplicate"
)

// Friction is an entry in the friction box: something about the agent tool
// that got in someone's way.
type Friction struct {
	ID          int64            `json:"id"`
	UID         string           `json:"uid"` // unique across hosts, for merging boxes
	Host        string           `json:"host"`
	FiledBy     string           `json:"filed_by"` // agent name, or "user"
	Parent      string           `json:"parent,omitempty"`
	Cwd         string           `json:"cwd,omitempty"`
	Build       string           `json:"build"`
	CreatedAt   time.Time        `json:"created_at"`
	UpdatedAt   time.Time        `json:"updated_at"`
	Kind        string           `json:"kind"`
	Summary     string           `json:"summary"`
	Detail      string           `json:"detail,omitempty"`
	Command     *AttachedCommand `json:"command,omitempty"`
	Status      string           `json:"status"`
	AcceptedBy  string           `json:"accepted_by,omitempty"`
	AcceptedAt  time.Time        `json:"accepted_at,omitzero"`
	AcceptNote  string           `json:"accept_note,omitempty"`
	ResolvedBy  string           `json:"resolved_by,omitempty"` // also for wontfix and duplicate
	ResolvedAt  time.Time        `json:"resolved_at,omitzero"`
	Resolution  string           `json:"resolution,omitempty"` // the note: how it was fixed, or why not
	DuplicateOf int64            `json:"duplicate_of,omitempty"`

	// Votes counts +1s, read only: the entry's own, its duplicates', and
	// one for each duplicate's filing.
	Votes int `json:"votes"`
}

// AttachedCommand is a failed agent command, copied from the command log
// into a friction entry or vote (the log is pruned).
type AttachedCommand struct {
	Args     []string  `json:"args"`
	ExitCode int       `json:"exit_code"`
	Error    string    `json:"error,omitempty"`
	At       time.Time `json:"at"`
}

// FrictionVote is a +1 on a friction entry.
type FrictionVote struct {
	ID         int64            `json:"id"`
	FrictionID int64            `json:"friction_id"`
	UID        string           `json:"uid"`
	Host       string           `json:"host"`
	Voter      string           `json:"voter"` // agent name, or "user"
	Parent     string           `json:"parent,omitempty"`
	Build      string           `json:"build"`
	CreatedAt  time.Time        `json:"created_at"`
	Note       string           `json:"note,omitempty"`
	Command    *AttachedCommand `json:"command,omitempty"`
}

// FrictionFilter selects friction entries. The zero value selects all.
type FrictionFilter struct {
	Statuses    []string // any of these, if set
	Kind        string
	FiledBy     string
	DuplicateOf int64
}

// StateUpdate is a change to an agent's state: Entries to set, and
// AgentName's keys in Delete to remove.
type StateUpdate struct {
	Entries   []*StateEntry
	AgentName string
	Delete    []string
	// Event, if set, is appended to the agent events in the same
	// transaction.
	Event *AgentEvent
}

// StateEntry represents a key-value state entry.
type StateEntry struct {
	AgentName string    `json:"agent_name"`
	Key       string    `json:"key"`
	Value     string    `json:"value"`
	UpdatedAt time.Time `json:"updated_at"`
}
