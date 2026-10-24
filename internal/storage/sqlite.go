package storage

import (
	"context"
	"database/sql"
	"fmt"
	"time"

	_ "modernc.org/sqlite"
)

// SQLiteStorage implements Storage using SQLite.
type SQLiteStorage struct {
	db *sql.DB
}

// NewSQLiteStorage creates a new SQLite storage instance.
func NewSQLiteStorage(dbPath string) (*SQLiteStorage, error) {
	// Per-connection pragmas go in the DSN so every pooled connection gets
	// them; a one-off db.Exec only reaches whichever connection runs it.
	// busy_timeout: backend hooks write status from several short-lived
	// processes at once. foreign_keys: cleanup relies on the agents → state
	// and agents → logs cascades.
	db, err := sql.Open("sqlite", dbPath+"?_pragma=busy_timeout(5000)&_pragma=foreign_keys(1)&_pragma=synchronous(NORMAL)")
	if err != nil {
		return nil, fmt.Errorf("opening database: %w", err)
	}

	// journal_mode is stored in the database file, so once is enough.
	if _, err := db.Exec("PRAGMA journal_mode = WAL"); err != nil {
		db.Close()
		return nil, fmt.Errorf("executing PRAGMA journal_mode = WAL: %w", err)
	}

	s := &SQLiteStorage{db: db}
	if err := s.migrate(); err != nil {
		db.Close()
		return nil, fmt.Errorf("running migrations: %w", err)
	}

	return s, nil
}

// migrate creates the database schema.
func (s *SQLiteStorage) migrate() error {
	schema := `
	CREATE TABLE IF NOT EXISTS agents (
		name TEXT PRIMARY KEY,
		session_name TEXT NOT NULL,
		backend TEXT NOT NULL,
		task_file TEXT,
		log_file TEXT,
		created_at TEXT NOT NULL
	);

	CREATE TABLE IF NOT EXISTS messages (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		sender_id TEXT NOT NULL,
		receiver_id TEXT NOT NULL,
		content TEXT NOT NULL,
		status TEXT NOT NULL DEFAULT 'pending',
		created_at TEXT NOT NULL
	);

	CREATE INDEX IF NOT EXISTS idx_messages_receiver_status
		ON messages(receiver_id, status);

	-- Tasks as standalone repository (not tied to agents)
	CREATE TABLE IF NOT EXISTS tasks (
		name TEXT PRIMARY KEY,
		content TEXT NOT NULL,
		source_file TEXT,
		created_at TEXT NOT NULL
	);

	-- Log entries (ANSI-stripped, queryable)
	CREATE TABLE IF NOT EXISTS logs (
		id INTEGER PRIMARY KEY AUTOINCREMENT,
		agent_name TEXT NOT NULL,
		content TEXT NOT NULL,
		captured_at TEXT NOT NULL,
		FOREIGN KEY (agent_name) REFERENCES agents(name) ON DELETE CASCADE
	);

	-- Index for log queries
	CREATE INDEX IF NOT EXISTS idx_logs_agent_time ON logs(agent_name, captured_at DESC);

	-- State as key-value store
	CREATE TABLE IF NOT EXISTS state (
		agent_name TEXT NOT NULL,
		key TEXT NOT NULL,
		value TEXT NOT NULL,
		updated_at TEXT NOT NULL,
		PRIMARY KEY (agent_name, key),
		FOREIGN KEY (agent_name) REFERENCES agents(name) ON DELETE CASCADE
	);
	`

	_, err := s.db.Exec(schema)
	return err
}

// CreateAgent creates a new agent record.
func (s *SQLiteStorage) CreateAgent(ctx context.Context, agent *Agent) error {
	query := `
		INSERT INTO agents (name, session_name, backend, task_file, log_file, created_at)
		VALUES (?, ?, ?, ?, ?, ?)
	`

	_, err := s.db.ExecContext(ctx, query,
		agent.Name,
		agent.SessionName,
		agent.Backend,
		agent.TaskFile,
		agent.LogFile,
		agent.CreatedAt.Format(time.RFC3339),
	)

	if err != nil {
		return fmt.Errorf("inserting agent: %w", err)
	}

	return nil
}

// GetAgent retrieves an agent by name.
func (s *SQLiteStorage) GetAgent(ctx context.Context, name string) (*Agent, error) {
	query := `
		SELECT name, session_name, backend, task_file, log_file, created_at
		FROM agents
		WHERE name = ?
	`

	var agent Agent
	var createdAt string
	var taskFile, logFile sql.NullString

	err := s.db.QueryRowContext(ctx, query, name).Scan(
		&agent.Name,
		&agent.SessionName,
		&agent.Backend,
		&taskFile,
		&logFile,
		&createdAt,
	)

	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("querying agent: %w", err)
	}

	agent.TaskFile = taskFile.String
	agent.LogFile = logFile.String
	agent.CreatedAt, _ = time.Parse(time.RFC3339, createdAt)

	return &agent, nil
}

// ListAgents returns all agents.
func (s *SQLiteStorage) ListAgents(ctx context.Context) ([]*Agent, error) {
	query := `
		SELECT name, session_name, backend, task_file, log_file, created_at
		FROM agents
		ORDER BY created_at DESC
	`

	rows, err := s.db.QueryContext(ctx, query)
	if err != nil {
		return nil, fmt.Errorf("querying agents: %w", err)
	}
	defer rows.Close()

	var agents []*Agent
	for rows.Next() {
		var agent Agent
		var createdAt string
		var taskFile, logFile sql.NullString

		if err := rows.Scan(
			&agent.Name,
			&agent.SessionName,
			&agent.Backend,
			&taskFile,
			&logFile,
			&createdAt,
		); err != nil {
			return nil, fmt.Errorf("scanning agent: %w", err)
		}

		agent.TaskFile = taskFile.String
		agent.LogFile = logFile.String
		agent.CreatedAt, _ = time.Parse(time.RFC3339, createdAt)
		agents = append(agents, &agent)
	}

	return agents, rows.Err()
}

// DeleteAgent removes an agent record.
func (s *SQLiteStorage) DeleteAgent(ctx context.Context, name string) error {
	query := `DELETE FROM agents WHERE name = ?`
	_, err := s.db.ExecContext(ctx, query, name)
	if err != nil {
		return fmt.Errorf("deleting agent: %w", err)
	}
	return nil
}

// CreateMessage creates a new message record.
func (s *SQLiteStorage) CreateMessage(ctx context.Context, msg *Message) error {
	query := `
		INSERT INTO messages (sender_id, receiver_id, content, status, created_at)
		VALUES (?, ?, ?, ?, ?)
	`

	result, err := s.db.ExecContext(ctx, query,
		msg.SenderID,
		msg.ReceiverID,
		msg.Content,
		msg.Status,
		msg.CreatedAt.Format(time.RFC3339),
	)
	if err != nil {
		return fmt.Errorf("inserting message: %w", err)
	}

	msg.ID, _ = result.LastInsertId()
	return nil
}

// GetPendingMessages retrieves pending messages for a receiver.
func (s *SQLiteStorage) GetPendingMessages(ctx context.Context, receiverID string, limit int) ([]*Message, error) {
	query := `
		SELECT id, sender_id, receiver_id, content, status, created_at
		FROM messages
		WHERE receiver_id = ? AND status = 'pending'
		ORDER BY created_at ASC
		LIMIT ?
	`

	rows, err := s.db.QueryContext(ctx, query, receiverID, limit)
	if err != nil {
		return nil, fmt.Errorf("querying messages: %w", err)
	}
	defer rows.Close()

	var messages []*Message
	for rows.Next() {
		var msg Message
		var createdAt string

		if err := rows.Scan(
			&msg.ID,
			&msg.SenderID,
			&msg.ReceiverID,
			&msg.Content,
			&msg.Status,
			&createdAt,
		); err != nil {
			return nil, fmt.Errorf("scanning message: %w", err)
		}

		msg.CreatedAt, _ = time.Parse(time.RFC3339, createdAt)
		messages = append(messages, &msg)
	}

	return messages, rows.Err()
}

// UpdateMessageStatus updates the status of a message.
func (s *SQLiteStorage) UpdateMessageStatus(ctx context.Context, id int64, status MessageStatus) error {
	query := `UPDATE messages SET status = ? WHERE id = ?`
	_, err := s.db.ExecContext(ctx, query, status, id)
	if err != nil {
		return fmt.Errorf("updating message status: %w", err)
	}
	return nil
}

// DeleteMessagesByAgent removes all messages involving an agent.
func (s *SQLiteStorage) DeleteMessagesByAgent(ctx context.Context, agentName string) error {
	query := `DELETE FROM messages WHERE sender_id = ? OR receiver_id = ?`
	_, err := s.db.ExecContext(ctx, query, agentName, agentName)
	if err != nil {
		return fmt.Errorf("deleting messages: %w", err)
	}
	return nil
}

// Close closes the database connection.
func (s *SQLiteStorage) Close() error {
	return s.db.Close()
}

// SetTask creates or updates a task in the repository.
func (s *SQLiteStorage) SetTask(ctx context.Context, task *Task) error {
	query := `
		INSERT INTO tasks (name, content, source_file, created_at)
		VALUES (?, ?, ?, ?)
		ON CONFLICT(name) DO UPDATE SET
			content = excluded.content,
			source_file = excluded.source_file,
			created_at = excluded.created_at
	`

	_, err := s.db.ExecContext(ctx, query,
		task.Name,
		task.Content,
		task.SourceFile,
		task.CreatedAt.Format(time.RFC3339),
	)
	if err != nil {
		return fmt.Errorf("setting task: %w", err)
	}

	return nil
}

// GetTask retrieves a task by name.
func (s *SQLiteStorage) GetTask(ctx context.Context, name string) (*Task, error) {
	query := `
		SELECT name, content, source_file, created_at
		FROM tasks
		WHERE name = ?
	`

	var task Task
	var createdAt string
	var sourceFile sql.NullString

	err := s.db.QueryRowContext(ctx, query, name).Scan(
		&task.Name,
		&task.Content,
		&sourceFile,
		&createdAt,
	)

	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("querying task: %w", err)
	}

	task.SourceFile = sourceFile.String
	task.CreatedAt, _ = time.Parse(time.RFC3339, createdAt)

	return &task, nil
}

// ListTasks retrieves all tasks.
func (s *SQLiteStorage) ListTasks(ctx context.Context) ([]*Task, error) {
	query := `
		SELECT name, content, source_file, created_at
		FROM tasks
		ORDER BY name ASC
	`

	rows, err := s.db.QueryContext(ctx, query)
	if err != nil {
		return nil, fmt.Errorf("querying tasks: %w", err)
	}
	defer rows.Close()

	var tasks []*Task
	for rows.Next() {
		var task Task
		var createdAt string
		var sourceFile sql.NullString

		if err := rows.Scan(
			&task.Name,
			&task.Content,
			&sourceFile,
			&createdAt,
		); err != nil {
			return nil, fmt.Errorf("scanning task: %w", err)
		}

		task.SourceFile = sourceFile.String
		task.CreatedAt, _ = time.Parse(time.RFC3339, createdAt)
		tasks = append(tasks, &task)
	}

	return tasks, rows.Err()
}

// DeleteTask removes a task by name.
func (s *SQLiteStorage) DeleteTask(ctx context.Context, name string) error {
	query := `DELETE FROM tasks WHERE name = ?`
	_, err := s.db.ExecContext(ctx, query, name)
	if err != nil {
		return fmt.Errorf("deleting task: %w", err)
	}
	return nil
}

// AppendLog adds a new log entry for an agent.
func (s *SQLiteStorage) AppendLog(ctx context.Context, entry *LogEntry) error {
	query := `
		INSERT INTO logs (agent_name, content, captured_at)
		VALUES (?, ?, ?)
	`

	result, err := s.db.ExecContext(ctx, query,
		entry.AgentName,
		entry.Content,
		entry.CapturedAt.Format(time.RFC3339),
	)
	if err != nil {
		return fmt.Errorf("appending log: %w", err)
	}

	entry.ID, _ = result.LastInsertId()
	return nil
}

// GetLogs retrieves recent log entries for an agent.
func (s *SQLiteStorage) GetLogs(ctx context.Context, agentName string, limit int) ([]*LogEntry, error) {
	query := `
		SELECT id, agent_name, content, captured_at
		FROM logs
		WHERE agent_name = ?
		ORDER BY captured_at DESC
		LIMIT ?
	`

	rows, err := s.db.QueryContext(ctx, query, agentName, limit)
	if err != nil {
		return nil, fmt.Errorf("querying logs: %w", err)
	}
	defer rows.Close()

	var entries []*LogEntry
	for rows.Next() {
		var entry LogEntry
		var capturedAt string

		if err := rows.Scan(
			&entry.ID,
			&entry.AgentName,
			&entry.Content,
			&capturedAt,
		); err != nil {
			return nil, fmt.Errorf("scanning log: %w", err)
		}

		entry.CapturedAt, _ = time.Parse(time.RFC3339, capturedAt)
		entries = append(entries, &entry)
	}

	return entries, rows.Err()
}

// GetLogsSince retrieves log entries since a specific time.
func (s *SQLiteStorage) GetLogsSince(ctx context.Context, agentName string, since time.Time) ([]*LogEntry, error) {
	query := `
		SELECT id, agent_name, content, captured_at
		FROM logs
		WHERE agent_name = ? AND captured_at >= ?
		ORDER BY captured_at ASC
	`

	rows, err := s.db.QueryContext(ctx, query, agentName, since.Format(time.RFC3339))
	if err != nil {
		return nil, fmt.Errorf("querying logs since: %w", err)
	}
	defer rows.Close()

	var entries []*LogEntry
	for rows.Next() {
		var entry LogEntry
		var capturedAt string

		if err := rows.Scan(
			&entry.ID,
			&entry.AgentName,
			&entry.Content,
			&capturedAt,
		); err != nil {
			return nil, fmt.Errorf("scanning log: %w", err)
		}

		entry.CapturedAt, _ = time.Parse(time.RFC3339, capturedAt)
		entries = append(entries, &entry)
	}

	return entries, rows.Err()
}

// DeleteLogs removes all log entries for an agent.
func (s *SQLiteStorage) DeleteLogs(ctx context.Context, agentName string) error {
	query := `DELETE FROM logs WHERE agent_name = ?`
	_, err := s.db.ExecContext(ctx, query, agentName)
	if err != nil {
		return fmt.Errorf("deleting logs: %w", err)
	}
	return nil
}

// SetState creates or updates a state entry for an agent.
func (s *SQLiteStorage) SetState(ctx context.Context, entry *StateEntry) error {
	query := `
		INSERT INTO state (agent_name, key, value, updated_at)
		VALUES (?, ?, ?, ?)
		ON CONFLICT(agent_name, key) DO UPDATE SET
			value = excluded.value,
			updated_at = excluded.updated_at
	`

	_, err := s.db.ExecContext(ctx, query,
		entry.AgentName,
		entry.Key,
		entry.Value,
		entry.UpdatedAt.Format(time.RFC3339),
	)
	if err != nil {
		return fmt.Errorf("setting state: %w", err)
	}

	return nil
}

// GetState retrieves a single state entry.
func (s *SQLiteStorage) GetState(ctx context.Context, agentName, key string) (*StateEntry, error) {
	query := `
		SELECT agent_name, key, value, updated_at
		FROM state
		WHERE agent_name = ? AND key = ?
	`

	var entry StateEntry
	var updatedAt string

	err := s.db.QueryRowContext(ctx, query, agentName, key).Scan(
		&entry.AgentName,
		&entry.Key,
		&entry.Value,
		&updatedAt,
	)

	if err == sql.ErrNoRows {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("querying state: %w", err)
	}

	entry.UpdatedAt, _ = time.Parse(time.RFC3339, updatedAt)
	return &entry, nil
}

// ListState retrieves all state entries for an agent.
func (s *SQLiteStorage) ListState(ctx context.Context, agentName string) ([]*StateEntry, error) {
	query := `
		SELECT agent_name, key, value, updated_at
		FROM state
		WHERE agent_name = ?
		ORDER BY key ASC
	`

	rows, err := s.db.QueryContext(ctx, query, agentName)
	if err != nil {
		return nil, fmt.Errorf("querying state: %w", err)
	}
	defer rows.Close()

	var entries []*StateEntry
	for rows.Next() {
		var entry StateEntry
		var updatedAt string

		if err := rows.Scan(
			&entry.AgentName,
			&entry.Key,
			&entry.Value,
			&updatedAt,
		); err != nil {
			return nil, fmt.Errorf("scanning state: %w", err)
		}

		entry.UpdatedAt, _ = time.Parse(time.RFC3339, updatedAt)
		entries = append(entries, &entry)
	}

	return entries, rows.Err()
}

// DeleteState removes all state entries for an agent.
func (s *SQLiteStorage) DeleteState(ctx context.Context, agentName string) error {
	query := `DELETE FROM state WHERE agent_name = ?`
	_, err := s.db.ExecContext(ctx, query, agentName)
	if err != nil {
		return fmt.Errorf("deleting state: %w", err)
	}
	return nil
}
