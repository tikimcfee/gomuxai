# NATS Integration Plan for gomuxai

### Current Architecture Analysis

The current coordination model has three components that are the source of the pain points we ran into:

**1. SQLite as the message bus (the "polling" problem)**

The `messages` table in SQLite serves as a queue. The `watcher` package (`internal/watcher/watcher.go`) runs a polling loop on a `time.Ticker` (default 2 seconds), iterating over all agents, checking their pending messages, then checking their status via tmux capture + regex match, and only then delivering. This creates cascading latency: 2s poll interval, times N agents, times the time to tmux-capture and regex-match each one.

**2. State-as-coordination (the "when is it done?" problem)**

The `state` table is a key-value store. Agents are told in their task instructions to `agent state set <name> status done` when finished. The coordinator then polls with `agent state get <name> status` in a shell `while true; sleep 5` loop. There is no notification mechanism -- the producer does not know if anyone read the update, and the consumer does not know when the update arrived. This is the "kinda worked, kinda didn't" we described.

**3. Status detection via terminal scraping (the "unknown" problem)**

`AgentService.Status()` captures 50 lines of tmux output and runs regex patterns. This is inherently unreliable because the terminal state is transient (a spinner character may or may not be on screen at capture time).

### What NATS Replaces and What It Preserves

**NATS replaces:**
- The `messages` table + `watcher` polling loop (replaced by NATS subjects + subscribers)
- The `state` table as the coordination mechanism (replaced by NATS KV store backed by JetStream)
- The polling `while true; sleep 5` pattern for "is the agent done?" (replaced by subscriptions with acknowledgment)
- **Terminal scraping for status detection** (replaced by agents self-reporting status via NATS events)

The terminal scraping approach (`AgentService.Status()` capturing 50 lines of tmux output and regex-matching spinner characters) is fundamentally unreliable and should be removed entirely rather than patched. The `GetStatus()`, `IdlePattern()`, and `ProcessingPattern()` methods on the `Provider` interface exist solely to support this scraping and can be stripped. Agents will instead **report their own status** through NATS -- via CLI hooks (Claude Code), MCP tools (any backend), or direct `agent` CLI calls.

**NATS does NOT replace:**
- The `agents` table (still need to track agent metadata, session names, backends)
- The `tasks` table (still a standalone repository, no coordination semantics)
- The `logs` table (historical data, not coordination -- though log events could also flow through NATS)
- tmux pane management, backend configs, CLI commands for `spawn`, `send`, `capture`, `keys`, `key`, `clear`, `session`
- The backend registry (still needed for launch commands, init messages, exit commands -- just not status patterns)

**SQLite stays for persistence** -- NATS JetStream provides at-least-once delivery and KV store, but SQLite remains the canonical record for agent metadata, task definitions, and historical logs. The `messages` table and `state` table become NATS-backed and can be removed from SQLite once NATS is fully integrated.

### Implementation Plan

#### Phase 0: Add NATS dependencies and the embedded server

**New file: `internal/natsbus/server.go`**

This embeds the NATS server directly in the Go binary. No external NATS process needed.

```go
package natsbus

import (
    "fmt"
    "time"

    natsserver "github.com/nats-io/nats-server/v2/server"
    "github.com/nats-io/nats.go"
    "github.com/nats-io/nats.go/jetstream"
)

// Bus is the embedded NATS server + client.
type Bus struct {
    server *natsserver.Server
    conn   *nats.Conn
    js     jetstream.JetStream
}

// BusConfig configures the embedded NATS server.
type BusConfig struct {
    StoreDir string // e.g., ".gomuxai/nats" -- JetStream file store
    Port     int    // 0 = random available port (for testing)
}

func NewBus(cfg BusConfig) (*Bus, error) { ... }
func (b *Bus) Close() { ... }
func (b *Bus) Conn() *nats.Conn { return b.conn }
func (b *Bus) JS() jetstream.JetStream { return b.js }
func (b *Bus) ClientURL() string { ... } // "nats://127.0.0.1:<port>"
```

The key architectural decision: the NATS server starts when the first `agent` process needs it and writes a pidfile + URL to `.gomuxai/nats.url`. Subsequent `agent` CLI invocations connect as clients. This is the "singleton server" pattern. When the server-owning process exits, the next CLI invocation becomes the new server.

Actually, a simpler approach that avoids the pidfile dance: **every `agent` CLI invocation tries to connect to the URL in `.gomuxai/nats.url`; if it fails, it starts an embedded server itself.** This makes any `agent` process the server if none exists. The embedded server is lightweight (single process, in-memory with file-backed JetStream for persistence).

**New dependencies in `go.mod`:**
```
github.com/nats-io/nats-server/v2 v2.10.x
github.com/nats-io/nats.go v1.37.x
```

#### Phase 1: Define the NATS subject hierarchy and message types

**New file: `internal/natsbus/subjects.go`**

```go
package natsbus

import "fmt"

// Subject hierarchy:
//
// agent.{name}.event.{type}     -- agent lifecycle events (JetStream, persisted)
//   agent.researcher.event.spawned
//   agent.researcher.event.status     -- status changed (idle/processing/error)
//   agent.researcher.event.done       -- agent signaled completion
//   agent.researcher.event.request    -- agent is requesting help
//   agent.researcher.event.message    -- message delivered to agent
//
// agent.{name}.inbox              -- direct messages to an agent (JetStream, persisted)
//
// agent.{name}.state.{key}       -- NATS KV bucket per agent (JetStream KV)
//
// system.agents                   -- global agent registry events (spawn/cleanup)

func SubjectEvent(agentName, eventType string) string {
    return fmt.Sprintf("agent.%s.event.%s", agentName, eventType)
}

func SubjectInbox(agentName string) string {
    return fmt.Sprintf("agent.%s.inbox", agentName)
}

func SubjectAllEvents() string {
    return "agent.*.event.*"
}

func SubjectAgentEvents(agentName string) string {
    return fmt.Sprintf("agent.%s.event.*", agentName)
}

const SubjectSystemAgents = "system.agents"
```

**New file: `internal/natsbus/messages.go`**

```go
package natsbus

import "time"

// EventType enumerates agent event types.
type EventType string

const (
    EventSpawned    EventType = "spawned"
    EventDone       EventType = "done"
    EventStatus     EventType = "status"
    EventRequest    EventType = "request"
    EventMessage    EventType = "message"
    EventError      EventType = "error"
    EventCleanedUp  EventType = "cleaned_up"
)

// AgentEvent is published on agent.{name}.event.{type}.
type AgentEvent struct {
    Agent     string    `json:"agent"`
    Type      EventType `json:"type"`
    Status    string    `json:"status,omitempty"`    // for EventStatus
    Message   string    `json:"message,omitempty"`   // for EventMessage, EventRequest, EventDone
    From      string    `json:"from,omitempty"`      // sender agent name
    Timestamp time.Time `json:"timestamp"`
}

// InboxMessage is published on agent.{name}.inbox.
type InboxMessage struct {
    From      string    `json:"from"`
    Content   string    `json:"content"`
    ReplyTo   string    `json:"reply_to,omitempty"` // NATS reply subject for ack
    Timestamp time.Time `json:"timestamp"`
}
```

#### Phase 2: NATS JetStream streams and KV buckets

**New file: `internal/natsbus/streams.go`**

```go
package natsbus

import (
    "context"

    "github.com/nats-io/nats.go/jetstream"
)

const (
    StreamAgentEvents = "AGENT_EVENTS"
    StreamAgentInbox  = "AGENT_INBOX"
    KVBucketState     = "agent_state" // KV bucket for agent state
)

// EnsureStreams creates or updates the JetStream streams.
func (b *Bus) EnsureStreams(ctx context.Context) error {
    // Events stream -- captures all agent.*.event.* subjects
    _, err := b.js.CreateOrUpdateStream(ctx, jetstream.StreamConfig{
        Name:      StreamAgentEvents,
        Subjects:  []string{"agent.*.event.*"},
        Retention: jetstream.LimitsPolicy,
        MaxAge:    24 * time.Hour, // keep events for 24h
        Storage:   jetstream.FileStorage,
    })
    if err != nil {
        return fmt.Errorf("creating events stream: %w", err)
    }

    // Inbox stream -- captures all agent.*.inbox subjects
    _, err = b.js.CreateOrUpdateStream(ctx, jetstream.StreamConfig{
        Name:      StreamAgentInbox,
        Subjects:  []string{"agent.*.inbox"},
        Retention: jetstream.WorkQueuePolicy, // message consumed once
        Storage:   jetstream.FileStorage,
    })
    if err != nil {
        return fmt.Errorf("creating inbox stream: %w", err)
    }

    return nil
}

// EnsureKV creates the state KV bucket.
func (b *Bus) EnsureKV(ctx context.Context) (jetstream.KeyValue, error) {
    kv, err := b.js.CreateOrUpdateKeyValue(ctx, jetstream.KeyValueConfig{
        Bucket:  KVBucketState,
        History: 5, // keep last 5 values per key
        TTL:     0, // no expiry
        Storage: jetstream.FileStorage,
    })
    return kv, err
}
```

#### Phase 3: New coordination layer -- `internal/natsbus/coordinator.go`

This is the core replacement for the watcher + state polling pattern.

```go
package natsbus

import (
    "context"
    "encoding/json"
    "fmt"
    "time"

    "github.com/nats-io/nats.go/jetstream"
)

// Coordinator provides high-level coordination operations.
type Coordinator struct {
    bus *Bus
    kv  jetstream.KeyValue
}

func NewCoordinator(bus *Bus) (*Coordinator, error) { ... }

// Publish publishes an agent event (fire-and-forget, JetStream persisted).
func (c *Coordinator) Publish(ctx context.Context, event AgentEvent) error {
    data, _ := json.Marshal(event)
    subject := SubjectEvent(event.Agent, string(event.Type))
    _, err := c.bus.js.Publish(ctx, subject, data)
    return err
}

// SendToInbox sends a message to an agent's inbox (JetStream persisted, ack-able).
func (c *Coordinator) SendToInbox(ctx context.Context, msg InboxMessage, targetAgent string) error {
    data, _ := json.Marshal(msg)
    _, err := c.bus.js.Publish(ctx, SubjectInbox(targetAgent), data)
    return err
}

// WaitForEvent blocks until an event matching the filter arrives, or context is cancelled.
// This is the "when is the agent done?" replacement.
func (c *Coordinator) WaitForEvent(ctx context.Context, agentName string, eventType EventType) (*AgentEvent, error) {
    // Create an ephemeral consumer on the events stream filtered to this agent+type
    cons, err := c.bus.js.CreateOrUpdateConsumer(ctx, StreamAgentEvents, jetstream.ConsumerConfig{
        FilterSubject: SubjectEvent(agentName, string(eventType)),
        DeliverPolicy: jetstream.DeliverNewPolicy,
        AckPolicy:     jetstream.AckExplicitPolicy,
    })
    if err != nil {
        return nil, err
    }
    // Fetch one message, block until available or context cancelled
    msgs, err := cons.Fetch(1, jetstream.FetchMaxWait(24*time.Hour))
    // ... unmarshal and return
}

// WatchState watches a specific agent's state key for changes.
func (c *Coordinator) WatchState(ctx context.Context, agentName, key string) (jetstream.KeyWatcher, error) {
    kvKey := fmt.Sprintf("%s.%s", agentName, key)
    return c.kv.Watch(kvKey)
}

// SetState sets a state key for an agent (replaces SQLite state set).
func (c *Coordinator) SetState(ctx context.Context, agentName, key, value string) error {
    kvKey := fmt.Sprintf("%s.%s", agentName, key)
    _, err := c.kv.PutString(kvKey, value)
    return err
}

// GetState gets a state key for an agent.
func (c *Coordinator) GetState(ctx context.Context, agentName, key string) (string, error) {
    kvKey := fmt.Sprintf("%s.%s", agentName, key)
    entry, err := c.kv.Get(kvKey)
    if err != nil {
        return "", err
    }
    return string(entry.Value()), nil
}
```

#### Phase 4: Wire NATS into the existing CLI commands (incremental)

The key insight for incremental adoption: **add NATS alongside SQLite first, then migrate commands one by one.**

**Modify: `internal/config/config.go`**

Add NATS configuration fields:

```go
type Config struct {
    // ... existing fields ...
    
    // NATS
    NATSPort    int    `json:"nats_port"`     // 0 = auto-select
    NATSDataDir string `json:"nats_data_dir"` // defaults to .gomuxai/nats
}
```

**Modify: `internal/cli/root.go`**

Add a `*natsbus.Bus` to the global state, initialized in `setup()`. The bus is only started if needed (lazy init for commands that don't need it, or always-on since it's lightweight).

```go
var (
    // ... existing globals ...
    bus         *natsbus.Bus
    coordinator *natsbus.Coordinator
)

func setup(cmd *cobra.Command, args []string) error {
    // ... existing setup ...
    
    // Initialize NATS bus
    bus, err = natsbus.NewBus(natsbus.BusConfig{
        StoreDir: filepath.Join(cfg.NATSDataDir),
        Port:     cfg.NATSPort,
    })
    if err != nil {
        return fmt.Errorf("initializing NATS: %w", err)
    }
    
    coordinator, err = natsbus.NewCoordinator(bus)
    if err != nil {
        return fmt.Errorf("initializing coordinator: %w", err)
    }
    
    // Pass coordinator to agent service
    agentService = service.NewAgentService(cfg, store, tmuxClient, backends, coordinator)
    
    return nil
}
```

**Modify: `internal/service/agent_service.go`**

Add `coordinator` field. Modify `Spawn()` to publish a `spawned` event. Modify `Cleanup()` to publish a `cleaned_up` event. Modify `Queue()` to publish to NATS inbox instead of/in addition to SQLite messages table.

```go
type AgentService struct {
    cfg         *config.Config
    storage     storage.Storage
    tmux        tmux.TmuxOps
    backends    *backend.Registry
    coordinator *natsbus.Coordinator // NEW -- nil-safe for backward compat
}
```

**Modify: `internal/cli/state.go`**

`agent state set` now publishes to NATS KV and also fires an event so subscribers are notified immediately:

```go
func runStateSet(cmd *cobra.Command, args []string) error {
    name := args[0]
    key := args[1]
    value := args[2]
    
    // Write to NATS KV (primary)
    if coordinator != nil {
        if err := coordinator.SetState(getContext(), name, key, value); err != nil {
            return err
        }
        // Also publish an event so watchers get notified
        coordinator.Publish(getContext(), natsbus.AgentEvent{
            Agent:   name,
            Type:    natsbus.EventStatus,
            Status:  key + "=" + value,
            Timestamp: time.Now(),
        })
    }
    
    // Write to SQLite (fallback/persistence -- remove later)
    entry := &storage.StateEntry{...}
    store.SetState(getContext(), entry)
    
    fmt.Printf("State set: %s.%s = %s\
", name, key, value)
    return nil
}
```

**Modify: `internal/cli/queue.go`**

Instead of writing to the SQLite `messages` table and relying on the watcher, publish directly to NATS:

```go
func runQueue(cmd *cobra.Command, args []string) error {
    name := args[0]
    message := strings.Join(args[1:], " ")
    sender := determineSender()
    
    if coordinator != nil {
        msg := natsbus.InboxMessage{
            From:      sender,
            Content:   message,
            Timestamp: time.Now(),
        }
        if err := coordinator.SendToInbox(getContext(), msg, name); err != nil {
            return err
        }
        fmt.Printf("Queued message from %q to %q (via NATS)\
", sender, name)
        return nil
    }
    
    // Fallback to SQLite if NATS not available
    return agentService.Queue(getContext(), sender, name, message)
}
```

#### Phase 5: Replace the watcher with a NATS consumer + remove terminal scraping

**Create: `internal/natsbus/watcher.go`**

The new watcher subscribes to `agent.*.inbox` via a durable JetStream consumer. When a message arrives, it checks the target agent's **NATS KV status** (not tmux capture) and either delivers immediately or re-naks with a delay:

```go
// NATSWatcher replaces the polling watcher entirely.
type NATSWatcher struct {
    bus         *Bus
    coordinator *Coordinator
    tmux        tmux.TmuxOps    // only for Send -- delivering keys to the pane
    storage     storage.Storage // only for agent session name lookup
}

func (w *NATSWatcher) Start(ctx context.Context) error {
    cons, err := w.bus.js.CreateOrUpdateConsumer(ctx, StreamAgentInbox, jetstream.ConsumerConfig{
        Durable:       "inbox-watcher",
        FilterSubject: "agent.*.inbox",
        AckPolicy:     jetstream.AckExplicitPolicy,
        MaxDeliver:    10,
        AckWait:       30 * time.Second,
        BackOff: []time.Duration{
            1 * time.Second,
            2 * time.Second,
            5 * time.Second,
            10 * time.Second,
        },
    })

    // Consume messages
    cc, err := cons.Consume(func(msg jetstream.Msg) {
        agentName := extractAgentName(msg.Subject())

        var inboxMsg InboxMessage
        json.Unmarshal(msg.Data(), &inboxMsg)

        // Check status via NATS KV -- NOT tmux capture
        status, err := w.coordinator.GetState(ctx, agentName, "status")
        if err != nil || status != "idle" {
            msg.NakWithDelay(5 * time.Second) // retry later
            return
        }

        // Deliver via tmux (send keys to the pane)
        agent, _ := w.storage.GetAgent(ctx, agentName)
        if err := w.tmux.SendKeys(agent.SessionName, inboxMsg.Content, true); err != nil {
            msg.NakWithDelay(5 * time.Second)
            return
        }

        msg.Ack()

        // Publish delivery event
        w.coordinator.Publish(ctx, AgentEvent{
            Agent:   agentName,
            Type:    EventMessage,
            Message: inboxMsg.Content,
            From:    inboxMsg.From,
        })
    })
}
```

**Delete: the old `internal/watcher/watcher.go`** -- the entire polling watcher package is replaced.

**Modify: `internal/backend/provider.go`** -- strip status detection from the Provider interface:

```go
// Provider defines the interface for AI CLI tool backends.
// Status detection has been removed -- agents self-report via NATS.
type Provider interface {
    // Identity
    Name() string
    SessionPrefix() string

    // Lifecycle
    Initialize(ctx context.Context) error
    Cleanup() error

    // Output parsing (still useful for capture/diagnostics, not coordination)
    ExtractLastMessage(output string) string

    // Commands
    LaunchCommand() string
    InitMessage(taskFile string) string
    ExitCommand() string
}
```

The following methods are **removed** from Provider and all implementations (ClaudeBackend, AiderBackend, GenericBackend):
- `GetStatus(ctx context.Context, output string) (Status, error)`
- `GetIdlePatternForLog() string`
- `IdlePattern() string`
- `ProcessingPattern() string`

**Modify: `internal/service/agent_service.go`** -- the `Status()` method is replaced:

```go
// Status checks the agent's self-reported status from NATS KV.
// Falls back to "unknown" if no status has been reported yet.
func (s *AgentService) Status(ctx context.Context, name string) (AgentStatus, error) {
    if s.coordinator == nil {
        return AgentStatusUnknown, nil
    }
    status, err := s.coordinator.GetState(ctx, name, "status")
    if err != nil {
        return AgentStatusUnknown, nil // no status reported yet
    }
    switch status {
    case "idle":
        return AgentStatusIdle, nil
    case "processing":
        return AgentStatusProcessing, nil
    case "error":
        return AgentStatusError, nil
    case "done":
        return AgentStatusIdle, nil // done implies idle for delivery purposes
    default:
        return AgentStatusUnknown, nil
    }
}
```

This is fundamentally better than the polling watcher because:
- Messages arrive via push, not poll
- NATS handles the retry/backoff/max-deliver natively
- No scanning all agents every N seconds -- only agents with pending messages get checked
- Status comes from the agent itself (authoritative) not terminal scraping (guesswork)
- JetStream persists messages across restarts
- The entire regex-matching / tmux-capture / "unknown status" category of bugs is eliminated

#### Phase 6: Agent status reporting -- hooks, MCP, and the CLI command

This is the critical phase that closes the loop. NATS gives us the bus; now agents need to actually **publish their status to it**. There are three mechanisms, layered from simplest to most universal:

##### 6a: The CLI command (`agent report`)

The simplest mechanism. Any agent -- or any shell script -- can report status:

```bash
agent report idle                    # I'm ready for work
agent report processing              # I'm working
agent report done "finished task X"  # I'm done (message optional)
agent report error "hit rate limit"  # Something went wrong
```

**New file: `internal/cli/report.go`**

```go
var reportCmd = &cobra.Command{
    Use:   "report <status> [message]",
    Short: "Report this agent's status (for coordination)",
    Args:  cobra.MinimumNArgs(1),
    RunE: func(cmd *cobra.Command, args []string) error {
        agentName := os.Getenv("AGENT_NAME")
        if agentName == "" {
            return fmt.Errorf("AGENT_NAME not set -- are you running inside an agent session?")
        }

        status := args[0]
        message := ""
        if len(args) > 1 {
            message = strings.Join(args[1:], " ")
        }

        // Write to NATS KV (authoritative status)
        coordinator.SetState(getContext(), agentName, "status", status)

        // Publish event (so watchers/waiters get notified immediately)
        coordinator.Publish(getContext(), natsbus.AgentEvent{
            Agent:     agentName,
            Type:      natsbus.EventStatus,
            Status:    status,
            Message:   message,
            Timestamp: time.Now(),
        })

        // If "done", also publish a done event for `agent wait`
        if status == "done" {
            coordinator.Publish(getContext(), natsbus.AgentEvent{
                Agent:     agentName,
                Type:      natsbus.EventDone,
                Message:   message,
                Timestamp: time.Now(),
            })
        }

        return nil
    },
}
```

This is what hooks and MCP tools call under the hood.

##### 6b: Claude Code hooks integration

Claude Code supports hooks that fire shell commands on lifecycle events. When `agent spawn` creates a Claude session, it writes a hooks configuration that automatically reports status to NATS.

**Modify: `internal/service/agent_service.go` -- `Spawn()`**

After creating the tmux session and before launching the CLI, write a hooks config:

```go
// Write Claude Code hooks config for status reporting
if backend.Name() == "claude" {
    hooksConfig := map[string]interface{}{
        "hooks": map[string]interface{}{
            // When Claude starts processing (tool use begins)
            "PreToolUse": []map[string]interface{}{
                {
                    "matcher": "",
                    "hooks": []map[string]string{
                        {"type": "command", "command": "agent report processing"},
                    },
                },
            },
            // When Claude finishes a response (stops to wait for input)
            "PostResponse": []map[string]interface{}{
                {
                    "matcher": "",
                    "hooks": []map[string]string{
                        {"type": "command", "command": "agent report idle"},
                    },
                },
            },
            // When the conversation/task ends
            "Stop": []map[string]interface{}{
                {
                    "matcher": "",
                    "hooks": []map[string]string{
                        {"type": "command", "command": "agent report done"},
                    },
                },
            },
        },
    }
    // Write to the session's working directory as .claude/settings.local.json
    // or pass via CLI flags if Claude Code supports that
}
```

The exact hook event names depend on Claude Code's hook API. The key insight: **the hook runs `agent report <status>`, which hits the NATS bus**. The agent binary is already on `$PATH` in the tmux session, and `$AGENT_NAME` is set, so the hook is self-contained.

For **Aider** and other backends, equivalent hooks can be configured through their respective hook/callback systems. The common denominator is always `agent report <status>`.

##### 6c: MCP server for universal agent coordination

This is the "works with everything" approach. Instead of requiring each backend to have its own hook/SDK integration, expose the coordination API as an **MCP (Model Context Protocol) server** that any MCP-capable agent can discover and call.

**New file: `internal/mcp/server.go`**

The `agent` binary starts an MCP server (stdio transport) that is passed to agent backends as an MCP tool source. Any agent that speaks MCP can call these tools:

```go
package mcp

// Tools exposed via MCP:
//
// report_status(status, message?)     -- report this agent's status
// send_message(target, content)       -- send a message to another agent
// get_state(agent, key)               -- read another agent's state
// set_state(key, value)               -- set own state
// wait_for(agent, event, timeout?)    -- block until agent emits event
// list_agents()                       -- list all active agents

type CoordinationServer struct {
    coordinator *natsbus.Coordinator
    agentName   string // from $AGENT_NAME
}

// Tool: report_status
// Called by the agent to report its own status.
// This is the MCP equivalent of `agent report <status>`.
func (s *CoordinationServer) ReportStatus(status string, message string) error {
    s.coordinator.SetState(ctx, s.agentName, "status", status)
    s.coordinator.Publish(ctx, natsbus.AgentEvent{
        Agent:  s.agentName,
        Type:   natsbus.EventStatus,
        Status: status,
        Message: message,
    })
    return nil
}

// Tool: send_message
// Send a message to another agent's inbox.
func (s *CoordinationServer) SendMessage(target, content string) error {
    return s.coordinator.SendToInbox(ctx, natsbus.InboxMessage{
        From:    s.agentName,
        Content: content,
    }, target)
}

// Tool: wait_for
// Block until a specific agent emits an event.
func (s *CoordinationServer) WaitFor(agent string, event string, timeoutSec int) (*natsbus.AgentEvent, error) {
    ctx, cancel := context.WithTimeout(ctx, time.Duration(timeoutSec)*time.Second)
    defer cancel()
    return s.coordinator.WaitForEvent(ctx, agent, natsbus.EventType(event))
}
```

**How it's wired into spawn:**

When `agent spawn` launches a backend, it starts the MCP server and passes its connection to the backend CLI:

```go
// For Claude Code:
// claude --mcp-config '{"gomuxai": {"command": "agent", "args": ["mcp", "serve"]}}'
//
// For any MCP-capable backend:
// The MCP server runs as a subprocess that the backend discovers.
```

**New CLI command: `agent mcp serve`**

Starts the MCP coordination server on stdio (for subprocess use by agent backends):

```go
var mcpServeCmd = &cobra.Command{
    Use:   "serve",
    Short: "Start MCP coordination server (stdio transport)",
    RunE: func(cmd *cobra.Command, args []string) error {
        server := mcp.NewCoordinationServer(coordinator, os.Getenv("AGENT_NAME"))
        return server.ServeStdio() // blocks until stdin closes
    },
}
```

**Why MCP instead of (or alongside) hooks:**

| Approach | Pros | Cons |
|----------|------|------|
| CLI hooks (6b) | Simple, works today, no protocol overhead | Backend-specific config, fire-and-forget (no return values) |
| MCP server (6c) | Universal (any MCP client), bidirectional, typed tools | Requires MCP support in the backend CLI |
| Both | Maximum compatibility | Two code paths to maintain |

The recommendation is **both**: hooks for fire-and-forget status reporting (simple, reliable), MCP for richer coordination (send messages, wait for events, query state). Claude Code supports both hooks and MCP. Other backends may only support one. The `agent report` CLI command is the common substrate -- hooks call it, MCP tools call the same coordinator methods.

##### 6d: Request-reply pattern (`agent ask`)

With status reporting working, request-reply becomes straightforward:

**New CLI command: `agent ask <target-agent> <question>`**

```go
func runAsk(cmd *cobra.Command, args []string) error {
    target := args[0]
    question := strings.Join(args[1:], " ")

    // Publish request with a NATS reply subject
    replySubject := nats.NewInbox()
    sub, _ := bus.Conn().SubscribeSync(replySubject)
    defer sub.Unsubscribe()

    msg := natsbus.InboxMessage{
        From:    os.Getenv("AGENT_NAME"),
        Content: question,
        ReplyTo: replySubject,
    }
    coordinator.SendToInbox(getContext(), msg, target)

    // Block waiting for reply (the target agent or its MCP server publishes one)
    reply, err := sub.NextMsgWithContext(getContext())
    if err != nil {
        return fmt.Errorf("waiting for reply: %w", err)
    }

    fmt.Print(string(reply.Data))
    return nil
}
```

**New CLI commands: `agent nats publish/subscribe`**

Low-level NATS access for scripting and debugging:

```go
var natsPublishCmd = &cobra.Command{
    Use:   "publish <subject> <json-data>",
    Short: "Publish a NATS message",
    Args:  cobra.ExactArgs(2),
    RunE: func(cmd *cobra.Command, args []string) error {
        return bus.Conn().Publish(args[0], []byte(args[1]))
    },
}

var natsSubscribeCmd = &cobra.Command{
    Use:   "subscribe <subject>",
    Short: "Subscribe and print messages until Ctrl+C",
    Args:  cobra.ExactArgs(1),
    RunE: func(cmd *cobra.Command, args []string) error {
        // Subscribe and print messages until signal
    },
}
```

#### Phase 7: tmux wait-for integration alongside NATS

For simple pane synchronization, `tmux wait-for` can work alongside NATS. When an agent signals completion via NATS, the coordinator can also signal a tmux wait-for channel:

```go
// After receiving a "done" event via NATS subscription:
tmuxClient.SendSpecialKey("", "") // not directly, but:
// exec.Command("tmux", "wait-for", "-S", "agent-"+agentName+"-done").Run()
```

This lets shell scripts do `tmux wait-for agent-researcher-done` as a simple blocking primitive alongside the full NATS event system.

### Testing Strategy

Tests follow the existing codebase conventions: standard Go `testing` package only (no testify), table-driven tests, real dependencies (not mocks), `tests/` directory organized by package, and `setupTestX(t)` helper functions that return a cleanup closure.

Every implementation step writes tests **before or alongside** the production code. The test proves the assumption, the code makes it pass, then we move on. No step ships without its tests green.

#### Test file layout

```
tests/
├── natsbus/
│   ├── server_test.go          # Step 1: Bus lifecycle
│   ├── streams_test.go         # Step 2: JetStream streams + KV
│   ├── coordinator_test.go     # Step 3: Coordination operations
│   └── watcher_test.go         # Step 8: NATS-based delivery
├── mcp/
│   └── server_test.go          # Step 10: MCP tool calls
├── service/
│   └── agent_service_test.go   # Existing (modified for NATS)
├── backend/
│   └── provider_test.go        # Existing (modified: remove status tests)
└── cli/
    └── integration_test.go     # Existing (modified: add report, wait, etc.)
```

#### Step 1 tests: Bus lifecycle (`tests/natsbus/server_test.go`)

The foundation. If these fail, nothing else matters.

```go
// setupTestBus creates an embedded NATS server on a random port with
// a temp directory for JetStream. Returns the bus and a cleanup function.
func setupTestBus(t *testing.T) (*natsbus.Bus, func()) {
    t.Helper()
    tmpDir, err := os.MkdirTemp("", "natsbus-test-*")
    if err != nil {
        t.Fatalf("creating temp dir: %v", err)
    }
    bus, err := natsbus.NewBus(natsbus.BusConfig{
        StoreDir: tmpDir,
        Port:     0, // random port
    })
    if err != nil {
        os.RemoveAll(tmpDir)
        t.Fatalf("creating bus: %v", err)
    }
    cleanup := func() {
        bus.Close()
        os.RemoveAll(tmpDir)
    }
    return bus, cleanup
}

func TestBus_StartAndConnect(t *testing.T)
    // Bus starts, client connects, ClientURL() returns valid nats:// URL.

func TestBus_PublishSubscribe_Core(t *testing.T)
    // Publish a raw message on a subject, subscribe receives it.
    // This is the smoke test -- if NATS pub/sub doesn't work, stop here.

func TestBus_JetStreamAvailable(t *testing.T)
    // bus.JS() returns non-nil, can create a stream.

func TestBus_Close_Graceful(t *testing.T)
    // Close shuts down server and connection without error.
    // Subsequent Publish returns error (not panic).

func TestBus_ConnectToExisting(t *testing.T)
    // Start bus1, write nats.url. Start bus2 with same StoreDir.
    // bus2 should connect as client (not start a second server).
    // Publish on bus1, receive on bus2.

func TestBus_StaleURLFile_StartsNewServer(t *testing.T)
    // Write a nats.url pointing to a dead port.
    // NewBus should detect the stale URL, remove it, start its own server.

func TestBus_ConcurrentStartRace(t *testing.T)
    // Launch two NewBus() calls concurrently with the same StoreDir.
    // Both should succeed (one starts server, one connects).
    // Neither should panic or deadlock.
```

#### Step 2 tests: Streams + KV (`tests/natsbus/streams_test.go`)

Verify JetStream infrastructure exists before writing coordination logic.

```go
func TestEnsureStreams_CreatesEventStream(t *testing.T)
    // After EnsureStreams, can publish to "agent.test.event.status"
    // and consume from the AGENT_EVENTS stream.

func TestEnsureStreams_CreatesInboxStream(t *testing.T)
    // After EnsureStreams, can publish to "agent.test.inbox"
    // and consume from AGENT_INBOX stream (WorkQueue retention).

func TestEnsureStreams_Idempotent(t *testing.T)
    // Call EnsureStreams twice. No error. Streams still work.

func TestEnsureKV_CreatesBucket(t *testing.T)
    // After EnsureKV, can Put and Get a key.

func TestEnsureKV_History(t *testing.T)
    // Put 3 values for the same key. History(key) returns all 3.

func TestSubjectBuilders(t *testing.T)
    // Table-driven: SubjectEvent, SubjectInbox, SubjectAgentEvents
    // verify correct format strings.
    tests := []struct {
        name     string
        fn       func() string
        expected string
    }{
        {"event", func() string { return SubjectEvent("researcher", "done") },
            "agent.researcher.event.done"},
        {"inbox", func() string { return SubjectInbox("helper") },
            "agent.helper.inbox"},
        // ...
    }
```

#### Step 3 tests: Coordinator (`tests/natsbus/coordinator_test.go`)

The core coordination primitives. Each test validates one assumption.

```go
func setupTestCoordinator(t *testing.T) (*natsbus.Coordinator, *natsbus.Bus, func()) {
    t.Helper()
    bus, busCleanup := setupTestBus(t)
    ctx := context.Background()
    bus.EnsureStreams(ctx)
    coord, err := natsbus.NewCoordinator(bus)
    if err != nil {
        busCleanup()
        t.Fatalf("creating coordinator: %v", err)
    }
    return coord, bus, busCleanup
}

// --- State operations ---

func TestCoordinator_SetState_GetState(t *testing.T)
    // Set "helper.status" = "idle", Get returns "idle".

func TestCoordinator_GetState_NotFound(t *testing.T)
    // Get on a key that was never set returns an error (not empty string).

func TestCoordinator_SetState_Overwrites(t *testing.T)
    // Set "helper.status" = "idle", then "processing". Get returns "processing".

// --- Publish + consume events ---

func TestCoordinator_Publish_EventPersisted(t *testing.T)
    // Publish a status event. Create a consumer on the stream.
    // Fetch returns the event with correct fields.

func TestCoordinator_Publish_JSONRoundTrip(t *testing.T)
    // Publish AgentEvent, consume raw bytes, unmarshal, verify all fields.

// --- WaitForEvent ---

func TestCoordinator_WaitForEvent_ReceivesPublished(t *testing.T)
    // In a goroutine: sleep 100ms, then Publish a "done" event.
    // In main: WaitForEvent blocks, receives it, returns correct AgentEvent.

func TestCoordinator_WaitForEvent_ContextCancelled(t *testing.T)
    // Create context with 200ms timeout. WaitForEvent with no publish.
    // Returns context error, not a hang.

func TestCoordinator_WaitForEvent_OnlyMatchesFilter(t *testing.T)
    // Publish "agent.helper.event.status" (not "done").
    // WaitForEvent(helper, "done") with short timeout should NOT receive it.

// --- Inbox ---

func TestCoordinator_SendToInbox_Persisted(t *testing.T)
    // SendToInbox for "helper". Create a consumer on AGENT_INBOX.
    // Fetch returns the message with From, Content, Timestamp.

func TestCoordinator_SendToInbox_MultipleAgentsIsolated(t *testing.T)
    // Send to "agent1" and "agent2". Consume from agent1's filter.
    // Only agent1's message appears.
```

#### Step 5 tests: Status reporting via NATS

Tests for the `agent report` path and the rewritten `AgentService.Status()`.

```go
// tests/natsbus/coordinator_test.go (additions)

func TestCoordinator_StatusReport_SetsKVAndPublishesEvent(t *testing.T)
    // Simulate what `agent report idle` does:
    // SetState(agent, "status", "idle") + Publish(EventStatus)
    // Verify both KV read and event stream contain the update.

func TestCoordinator_StatusReport_DoneAlsoPublishesDoneEvent(t *testing.T)
    // Simulate `agent report done "finished"`.
    // Verify EventStatus AND EventDone are both on the stream.
    // WaitForEvent(agent, "done") receives it.

// tests/service/agent_service_test.go (modifications)

func TestAgentService_Status_ReadsFromNATSKV(t *testing.T)
    // Set "helper.status" = "idle" in NATS KV.
    // service.Status(ctx, "helper") returns AgentStatusIdle.

func TestAgentService_Status_UnknownWhenNoReport(t *testing.T)
    // Agent exists but has never reported status.
    // service.Status returns AgentStatusUnknown (not error).

func TestAgentService_Status_MapsAllValues(t *testing.T)
    // Table-driven: "idle"→Idle, "processing"→Processing,
    // "error"→Error, "done"→Idle, "garbage"→Unknown
```

#### Step 8 tests: NATS watcher (`tests/natsbus/watcher_test.go`)

This replaces `tests/watcher/watcher_test.go`. The old tests documented SQLite + tmux-scraping behavior. The new tests document NATS + KV-status behavior.

```go
func setupTestWatcher(t *testing.T) (*natsbus.NATSWatcher, *natsbus.Coordinator, func())
    // Full test environment: bus, streams, coordinator, watcher.
    // Uses a mock tmux that records SendKeys calls instead of real tmux.

// --- Delivery based on NATS KV status ---

func TestNATSWatcher_DeliversWhenIdle(t *testing.T)
    // Set agent status = "idle" in KV.
    // Publish an InboxMessage to agent's inbox.
    // Start watcher. Wait briefly.
    // Verify: tmux.SendKeys was called with the message content.

func TestNATSWatcher_NaksWhenProcessing(t *testing.T)
    // Set agent status = "processing" in KV.
    // Publish an InboxMessage.
    // Start watcher. Wait briefly.
    // Verify: tmux.SendKeys was NOT called.
    // Message should still be in the stream (nak'd, will retry).

func TestNATSWatcher_NaksWhenNoStatus(t *testing.T)
    // Agent has no status key in KV (never reported).
    // Publish an InboxMessage.
    // Verify: message is nak'd (not delivered to unknown agents).

func TestNATSWatcher_DeliversAfterStatusChange(t *testing.T)
    // Agent starts as "processing". Publish message.
    // Watcher naks it. Then set status to "idle".
    // On retry (after backoff), watcher delivers successfully.

func TestNATSWatcher_PublishesDeliveryEvent(t *testing.T)
    // After successful delivery, verify an EventMessage event
    // was published on the agent's event subject.

func TestNATSWatcher_MaxDeliverExhausted(t *testing.T)
    // Agent stays "processing" forever. Message hits MaxDeliver.
    // Verify: message is eventually dead-lettered (not retried forever).

// --- Ordering + isolation ---

func TestNATSWatcher_FIFO_WithinAgent(t *testing.T)
    // Publish messages A, B, C to agent's inbox.
    // Agent is idle. Verify delivery order: A, then B, then C.

func TestNATSWatcher_MultipleAgents_Isolated(t *testing.T)
    // Agent1 is idle, Agent2 is processing.
    // Publish to both. Agent1 gets delivery, Agent2 doesn't.
```

Note on the tmux dependency: the watcher tests need to verify that `SendKeys` was called without requiring a real tmux session. This is the one place where we introduce a thin interface:

```go
// natsbus/watcher.go
type TMuxSender interface {
    SendKeys(sessionName string, keys string, enter bool) error
}
```

The production code passes the real `tmux.Client`. Tests pass a recording stub. This matches the spirit of the existing codebase (real deps where possible, minimal interface where required).

#### Step 10 tests: MCP server (`tests/mcp/server_test.go`)

Tests the MCP tool implementations directly (no MCP protocol framing needed -- test the Go methods).

```go
func setupTestMCPServer(t *testing.T) (*mcp.CoordinationServer, *natsbus.Coordinator, func())

func TestMCP_ReportStatus_SetsKVAndPublishes(t *testing.T)
    // Call server.ReportStatus("idle", "").
    // Verify NATS KV has status=idle and event was published.

func TestMCP_SendMessage_ArrivesInInbox(t *testing.T)
    // Call server.SendMessage("helper", "do the thing").
    // Consume from helper's inbox stream. Verify content.

func TestMCP_WaitFor_ReceivesEvent(t *testing.T)
    // In goroutine: publish a "done" event for "helper".
    // Call server.WaitFor("helper", "done", 5).
    // Returns the event (not timeout).

func TestMCP_WaitFor_Timeout(t *testing.T)
    // Call server.WaitFor("helper", "done", 1) with no publish.
    // Returns timeout error within ~1 second.

func TestMCP_GetState_CrossAgent(t *testing.T)
    // Agent A sets state via coordinator. Agent B reads via MCP GetState.
    // Verifies MCP tools can see other agents' state.
```

#### Existing test modifications

**`tests/backend/provider_test.go`** -- Remove all `TestClaudeBackend_GetStatus_*` and `TestAiderBackend_GetStatus_*` tests. These tested regex matching on tmux output, which no longer exists. Keep tests for `LaunchCommand`, `InitMessage`, `ExitCommand`, `ExtractLastMessage`.

**`tests/service/agent_service_test.go`** -- The `TestStatus_*` tests currently test tmux capture + regex matching. Replace with NATS KV-based status tests (see Step 5 above). The `TestSpawn`, `TestSend`, `TestCleanup` tests remain but gain assertions for NATS event publishing.

**`tests/watcher/watcher_test.go`** -- Archived/deleted. Its TDD tests for retry and backoff are replaced by the JetStream consumer's native `BackOff` and `MaxDeliver` config, tested in Step 8.

#### Test execution

All NATS tests use `Port: 0` (random port) and temp directories, so they're fully parallel and leave no state:

```bash
# Run all tests
make test

# Run only NATS tests (fast iteration during development)
go test -v ./tests/natsbus/...

# Run with race detector (important: NATS has goroutines)
go test -race -v ./tests/natsbus/...
```

The race detector flag is important -- the embedded NATS server runs goroutines, the watcher runs goroutines, and the coordinator's WaitForEvent blocks in one goroutine while Publish fires in another. Race conditions are the most likely class of bug in this migration.

### File Change Summary

**New files to create:**

| File | Purpose |
|------|---------|
| `internal/natsbus/server.go` | Embedded NATS server lifecycle, connect-or-start logic |
| `internal/natsbus/subjects.go` | Subject hierarchy constants and builders |
| `internal/natsbus/messages.go` | Go types for events, inbox messages |
| `internal/natsbus/streams.go` | JetStream stream and KV bucket setup |
| `internal/natsbus/coordinator.go` | High-level publish/subscribe/wait/state operations |
| `internal/natsbus/watcher.go` | NATS-based inbox delivery (replaces polling watcher) |
| `internal/mcp/server.go` | MCP coordination server (tools for status, messaging, state) |
| `internal/cli/report.go` | `agent report <status>` command (status self-reporting) |
| `internal/cli/ask.go` | `agent ask` command (request-reply) |
| `internal/cli/nats.go` | `agent nats publish/subscribe` commands |
| `internal/cli/mcp.go` | `agent mcp serve` command (MCP server on stdio) |
| `internal/cli/wait.go` | `agent wait <name> [--event done]` command |
| `tests/natsbus/server_test.go` | Bus lifecycle, connect-or-start, race conditions |
| `tests/natsbus/streams_test.go` | JetStream stream + KV bucket creation, idempotency |
| `tests/natsbus/coordinator_test.go` | State ops, event pub/sub, WaitForEvent, inbox isolation |
| `tests/natsbus/watcher_test.go` | NATS-based delivery: idle/processing gating, ordering, backoff |
| `tests/mcp/server_test.go` | MCP tool implementations: report, send, wait, get_state |

**Existing files to modify:**

| File | Change |
|------|--------|
| `go.mod` | Add `nats-server/v2`, `nats.go`, MCP server library dependencies |
| `internal/config/config.go` | Add `NATSPort`, `NATSDataDir` fields to `Config` |
| `internal/cli/root.go` | Add `bus`, `coordinator` globals; init in `setup()` |
| `internal/service/agent_service.go` | Add `coordinator` field; emit events on spawn/cleanup; rewrite `Status()` to read NATS KV; write Claude hooks config in `Spawn()` |
| `internal/backend/provider.go` | Remove `GetStatus()`, `GetIdlePatternForLog()`, `IdlePattern()`, `ProcessingPattern()` from Provider interface |
| `internal/backend/namedagents/claude.go` | Remove status regex patterns and detection methods; keep commands and `ExtractLastMessage` |
| `internal/backend/namedagents/aider.go` | Remove status regex patterns and detection methods; keep commands and `ExtractLastMessage` |
| `internal/backend/generic.go` | Remove status pattern fields from generic backend |
| `internal/cli/state.go` | Dual-write to NATS KV + SQLite, then eventually NATS-only |
| `internal/cli/queue.go` | Publish to NATS inbox instead of SQLite |
| `internal/cli/watch.go` | Start NATS watcher instead of (or alongside) polling watcher |
| `internal/cli/spawn.go` | Publish `spawned` event; set `NATS_URL` env var; write hooks config; pass MCP server config |
| `internal/cli/cleanup.go` | Publish `cleaned_up` event; purge agent's KV keys |
| `internal/cli/status.go` | Read from NATS KV (no tmux capture fallback) |
| `CLAUDE.md` | Document NATS-based coordination patterns, `agent report`, `agent wait`, MCP tools |
| `AGENT_INTERFACE.md` | Add `agent report`, `agent wait`, `agent ask`, `agent nats`, `agent mcp` docs |
| `tests/backend/provider_test.go` | Remove status detection tests (GetStatus, IdlePattern, etc.) |
| `tests/service/agent_service_test.go` | Replace tmux-based status tests with NATS KV status tests; add event assertions to spawn/cleanup |

**Files to delete:**

| File | Reason |
|------|--------|
| `internal/watcher/watcher.go` | Entire polling watcher package replaced by `natsbus/watcher.go` |
| `tests/watcher/watcher_test.go` | Replaced by `tests/natsbus/watcher_test.go` |

**Files that do NOT change:**

| File | Reason |
|------|--------|
| `internal/tmux/client.go` | tmux pane management is orthogonal to coordination |
| `internal/cli/capture.go` | Still captures from tmux, diagnostic use only |
| `internal/cli/send.go` | Direct send still goes through tmux (no NATS needed) |
| `internal/cli/session.go` | Low-level tmux ops, unchanged |
| `internal/cli/key.go`, `keys.go`, `clear.go` | Low-level tmux ops, unchanged |
| `internal/cli/logs.go` | Historical logs from SQLite, unchanged |
| `internal/cli/list.go` | Agent listing from SQLite, unchanged |
| `internal/util/ansi.go` | Utility, unchanged |
| `internal/backend/registry.go` | Backend registry unchanged (just dispatches by name) |

### Incremental Migration Order

**Step 1 (foundation):** Create `internal/natsbus/server.go` with the embedded NATS server. Write tests that start/stop the server, connect a client, publish/subscribe. This is zero impact on existing code.

**Step 2 (streams + types):** Create `internal/natsbus/streams.go`, `internal/natsbus/subjects.go`, and `internal/natsbus/messages.go`. Set up the JetStream streams and KV bucket. Test independently.

**Step 3 (coordinator):** Create `internal/natsbus/coordinator.go`. Implement `Publish`, `SetState`, `GetState`, `WaitForEvent`. Test independently.

**Step 4 (wire into root):** Modify `internal/cli/root.go` to initialize the bus in `setup()`. Modify `internal/config/config.go` for NATS config. At this point NATS is running but no commands use it yet.

**Step 5 (status reporting):** Add `agent report` command (`internal/cli/report.go`). Strip status detection methods from the Provider interface and all backend implementations. Rewrite `AgentService.Status()` to read from NATS KV. At this point agents can self-report status but nobody is consuming it yet.

**Step 6 (events):** Modify `internal/cli/spawn.go` and `internal/cli/cleanup.go` to publish events. On spawn, also set initial NATS KV status to "idle" and write Claude Code hooks config for auto-reporting. Existing functionality unchanged; events are additive.

**Step 7 (state dual-write):** Modify `internal/cli/state.go` to write to both NATS KV and SQLite. `agent state get` reads from NATS KV first, falls back to SQLite.

**Step 8 (queue + watcher migration):** Modify `internal/cli/queue.go` to publish to NATS instead of SQLite. Create the NATS-based watcher (`internal/natsbus/watcher.go`). Delete the old polling watcher package.

**Step 9 (new commands):** Add `agent wait`, `agent ask`, `agent nats publish/subscribe`.

**Step 10 (MCP server):** Create `internal/mcp/server.go` and `internal/cli/mcp.go`. Wire MCP server config into spawn for Claude Code (via `--mcp-config`) and any other MCP-capable backends. This gives agents bidirectional coordination tools without backend-specific hook config.

**Step 11 (cleanup):** Remove the `messages` and `state` tables from SQLite. Remove the old `internal/watcher` package entirely. Remove any dual-write fallbacks. SQLite remains for agent metadata, tasks, and logs only.

### NATS Server Lifecycle Detail

The most subtle engineering problem is: who runs the embedded NATS server?

**Approach: first-writer-wins with .gomuxai/nats.url file.**

```go
func NewBus(cfg BusConfig) (*Bus, error) {
    urlFile := filepath.Join(cfg.StoreDir, "nats.url")
    
    // Try to connect to existing server
    if data, err := os.ReadFile(urlFile); err == nil {
        url := string(bytes.TrimSpace(data))
        conn, err := nats.Connect(url, nats.Timeout(500*time.Millisecond))
        if err == nil {
            // Server exists and is reachable
            js, _ := jetstream.New(conn)
            return &Bus{conn: conn, js: js}, nil
        }
        // Server URL is stale, remove it
        os.Remove(urlFile)
    }
    
    // Start embedded server
    opts := &natsserver.Options{
        Port:      cfg.Port,
        StoreDir:  filepath.Join(cfg.StoreDir, "jetstream"),
        JetStream: true,
        NoLog:     true,
    }
    ns, err := natsserver.NewServer(opts)
    if err != nil {
        return nil, err
    }
    ns.Start()
    if !ns.ReadyForConnections(5 * time.Second) {
        return nil, fmt.Errorf("NATS server failed to start")
    }
    
    // Write URL file
    clientURL := ns.ClientURL()
    os.MkdirAll(cfg.StoreDir, 0755)
    os.WriteFile(urlFile, []byte(clientURL), 0644)
    
    // Connect
    conn, err := nats.Connect(clientURL)
    js, _ := jetstream.New(conn)
    
    return &Bus{server: ns, conn: conn, js: js}, nil
}
```

This means:
- `agent spawn researcher --backend claude` starts the NATS server (if first command)
- `agent queue researcher "do X"` connects to the existing server
- `agent state set researcher status done` (run inside agent pane) connects to the same server
- If the server-owning process exits, the next command starts a new one (JetStream data persists on disk)

The `NATS_URL` environment variable is also set in spawned tmux sessions so that agents running `agent` commands inside their panes connect to the correct server.

### What This Solves

**"When is the agent done?"** -- Instead of `while true; sleep 5; agent state get helper status; done`, the coordinator does:
```bash
agent wait helper --event done
```
This blocks on a NATS subscription. Zero polling. Instant notification.

**"Status is always unknown"** -- Instead of capturing 50 lines of tmux output and regex-matching spinner characters (which fails when the spinner isn't on screen, or the output format changes, or the terminal is mid-redraw), agents report their own status:
```bash
# Automatically via Claude Code hooks (configured on spawn):
# PostResponse → agent report idle
# PreToolUse → agent report processing
# Stop → agent report done

# Or via MCP tools (any MCP-capable backend):
# The agent calls report_status("idle") through the coordination MCP server

# Or manually in task instructions:
agent report done "finished analysis"
```
Status is now **authoritative** (the agent says what it is) instead of **inferred** (regex on transient terminal output).

**"No direct programmatic hook"** -- Three layers of hooks, pick the right one:
1. **Claude Code hooks** fire `agent report` automatically on lifecycle events
2. **MCP tools** let any agent call `report_status`, `send_message`, `wait_for` bidirectionally
3. **`agent nats publish`** for raw NATS access from shell scripts

**"Agents didn't know whether to send messages"** -- NATS KV watch tells them. An agent can subscribe to state changes:
```bash
agent nats subscribe "agent.coordinator.event.*"
```
Or use the `agent wait` command for specific events. Or use the `wait_for` MCP tool from within an agent session.

**"Kinda worked, kinda didn't"** -- The polling watcher was unreliable because it combined terminal scraping (unreliable) with SQLite polling (slow). NATS provides at-least-once delivery with explicit acknowledgment. Status comes from the agent itself. The entire `unknown` status category is eliminated -- an agent is either reporting or it hasn't started yet.

### Critical Files for Implementation

- `internal/cli/root.go` - Must be modified to wire in the NATS bus alongside the existing SQLite/tmux setup. This is the integration point where the new dependency meets the existing architecture.
- `internal/service/agent_service.go` - The central business logic layer that must gain a `coordinator` field, emit NATS events from `Spawn()`, `Send()`, `Queue()`, and `Cleanup()`, rewrite `Status()` to read NATS KV, and write hook/MCP configs during `Spawn()`.
- `internal/backend/provider.go` - The `Provider` interface must be simplified: remove `GetStatus()`, `GetIdlePatternForLog()`, `IdlePattern()`, `ProcessingPattern()`. Status detection is no longer a backend concern.
- `internal/backend/namedagents/claude.go` - Remove all status regex patterns and detection methods. The Claude backend becomes purely about commands and output parsing, not status inference.
- `internal/watcher/watcher.go` - The polling loop is the primary code being **deleted** and replaced by `internal/natsbus/watcher.go`. Understanding its retry/backoff logic informs the JetStream consumer configuration.
- `internal/storage/storage.go` - The `Storage` interface defines what SQLite provides today. The new `natsbus.Coordinator` will take over the `Message` and `State` operations, so this interface defines the boundary of the migration.
- `internal/config/config.go` - Must be extended with NATS configuration (port, data directory) and the `getStorageDir()` function (line 66) establishes the pattern for where NATS data should live.
