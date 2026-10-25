// Package watcher provides automatic message delivery when agents become idle.
package watcher

import (
	"context"
	"log/slog"
	"sync"
	"time"

	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

const (
	// maxRetries is how many failed sends a message gets before it fails.
	maxRetries = 3
	// baseBackoff is the wait after the first failed send; it doubles with
	// each further failure.
	baseBackoff = time.Second
	// unknownThreshold is how many checks in a row may find the agent's
	// status unknown before its message fails.
	unknownThreshold = 5
)

// messageState tracks retry and unknown status counts for a message.
type messageState struct {
	retryCount   int
	unknownCount int
	lastAttempt  time.Time
}

// Watcher monitors agents and delivers queued messages when idle.
type Watcher struct {
	service  *service.AgentService
	storage  storage.Storage
	interval time.Duration
	stopCh   chan struct{}
	wg       sync.WaitGroup
	logger   *slog.Logger

	// State tracking (protected by mu)
	mu     sync.Mutex
	states map[int64]*messageState
}

// WatcherOption configures a Watcher.
type WatcherOption func(*Watcher)

// WithLogger sets a custom logger (default: slog.Default()).
func WithLogger(logger *slog.Logger) WatcherOption {
	return func(w *Watcher) {
		w.logger = logger
	}
}

// NewWatcher creates a new watcher.
func NewWatcher(svc *service.AgentService, store storage.Storage, interval time.Duration, opts ...WatcherOption) *Watcher {
	w := &Watcher{
		service:  svc,
		storage:  store,
		interval: interval,
		stopCh:   make(chan struct{}),
		logger:   slog.Default(),
		states:   make(map[int64]*messageState),
	}

	for _, opt := range opts {
		opt(w)
	}

	return w
}

// Start begins watching agents.
func (w *Watcher) Start(ctx context.Context) {
	w.wg.Add(1)
	go w.run(ctx)
}

// Stop stops the watcher and waits for it to finish.
func (w *Watcher) Stop() {
	close(w.stopCh)
	w.wg.Wait()
}

func (w *Watcher) run(ctx context.Context) {
	defer w.wg.Done()

	ticker := time.NewTicker(w.interval)
	defer ticker.Stop()

	w.logger.Info("watcher started", "interval", w.interval)

	for {
		select {
		case <-ctx.Done():
			w.logger.Info("watcher stopping: context cancelled")
			return
		case <-w.stopCh:
			w.logger.Info("watcher stopping: stop signal received")
			return
		case <-ticker.C:
			w.checkAndDeliver(ctx)
		}
	}
}

func (w *Watcher) checkAndDeliver(ctx context.Context) {
	// List all agents
	agents, err := w.service.List(ctx)
	if err != nil {
		w.logger.Error("failed to list agents", "error", err)
		return
	}

	for _, agent := range agents {
		w.processAgent(ctx, agent)
	}
}

func (w *Watcher) processAgent(ctx context.Context, agent *storage.Agent) {
	logger := w.logger.With("agent", agent.Name)

	// Check for pending messages
	messages, err := w.storage.GetPendingMessages(ctx, agent.Name, 1)
	if err != nil {
		logger.Error("failed to get pending messages", "error", err)
		return
	}

	if len(messages) == 0 {
		return
	}

	msg := messages[0]
	msgLogger := logger.With("message_id", msg.ID)

	// Check backoff before attempting delivery
	if !w.canAttemptDelivery(msg.ID) {
		msgLogger.Debug("skipping delivery: backoff active")
		return
	}

	// Check if agent is idle
	status, err := w.service.Status(ctx, agent.Name)
	if err != nil {
		msgLogger.Error("failed to check agent status", "error", err)
		return
	}

	msgLogger = msgLogger.With("status", status)

	// unknownThreshold counts unknowns in a row: any known status ends a run.
	if status != service.AgentStatusUnknown {
		w.resetUnknownCount(msg.ID)
	}

	switch status {
	case service.AgentStatusIdle, service.AgentStatusError:
		// error means the last turn failed, not that the agent is gone: for
		// Claude it comes from StopFailure (API error, rate limit), after
		// which the CLI is back at its prompt and takes input like idle. It is
		// also sticky: no hook fires again until the next prompt arrives, so
		// holding messages for it would hold them forever.
		w.deliverMessage(ctx, agent.Name, msg, msgLogger)
	case service.AgentStatusExited:
		// The CLI is gone; typing into the shell left behind would run the
		// message as a command. Nothing restarts it, so fail now.
		msgLogger.Warn("agent exited, marking message as failed")
		w.failMessage(ctx, msg, msgLogger)
	case service.AgentStatusUnknown:
		w.handleUnknownStatus(ctx, msg, msgLogger)
	default:
		// processing, or waiting on a human: deliver once it's idle again
		msgLogger.Debug("agent busy, skipping delivery")
	}
}

func (w *Watcher) failMessage(ctx context.Context, msg *storage.Message, logger *slog.Logger) {
	if err := w.storage.UpdateMessageStatus(ctx, msg.ID, storage.MessageStatusFailed); err != nil {
		logger.Error("failed to update message status", "error", err)
		return
	}
	w.clearState(msg.ID)
}

func (w *Watcher) deliverMessage(ctx context.Context, agentName string, msg *storage.Message, logger *slog.Logger) {
	// Claim it first: an agent's Stop hook, a send, or another watcher may
	// be delivering the same message (see service.Send).
	claimed, err := w.storage.ClaimMessage(ctx, msg.ID)
	if err != nil {
		logger.Error("failed to claim message", "error", err)
		return
	}
	if !claimed {
		w.clearState(msg.ID)
		logger.Debug("message already delivered")
		return
	}
	if _, err := w.service.Send(ctx, agentName, msg.Content, service.SendOptions{Now: true}); err != nil {
		logger.Error("failed to send message", "error", err)
		if err := w.storage.UpdateMessageStatus(ctx, msg.ID, storage.MessageStatusPending); err != nil {
			logger.Error("failed to update message status", "error", err)
		}
		w.handleDeliveryFailure(ctx, msg, logger)
		return
	}

	// Clean up state
	w.clearState(msg.ID)

	logger.Info("message delivered successfully")
}

func (w *Watcher) handleDeliveryFailure(ctx context.Context, msg *storage.Message, logger *slog.Logger) {
	state := w.getOrCreateState(msg.ID)
	state.retryCount++
	state.lastAttempt = time.Now()

	if state.retryCount >= maxRetries {
		logger.Warn("max retries exceeded, marking message as failed",
			"retry_count", state.retryCount,
			"max_retries", maxRetries)
		w.failMessage(ctx, msg, logger)
		return
	}

	backoff := w.calculateBackoff(state.retryCount)
	logger.Info("delivery failed, will retry",
		"retry_count", state.retryCount,
		"next_backoff", backoff)
}

func (w *Watcher) handleUnknownStatus(ctx context.Context, msg *storage.Message, logger *slog.Logger) {
	state := w.getOrCreateState(msg.ID)
	state.unknownCount++

	if state.unknownCount >= unknownThreshold {
		logger.Warn("unknown status threshold exceeded, marking message as failed",
			"unknown_count", state.unknownCount,
			"threshold", unknownThreshold)
		w.failMessage(ctx, msg, logger)
		return
	}

	logger.Debug("agent status unknown, will retry",
		"unknown_count", state.unknownCount,
		"threshold", unknownThreshold)
}

func (w *Watcher) canAttemptDelivery(msgID int64) bool {
	w.mu.Lock()
	defer w.mu.Unlock()

	state, exists := w.states[msgID]
	if !exists {
		return true
	}

	if state.retryCount == 0 {
		return true
	}

	backoff := w.calculateBackoff(state.retryCount)
	return time.Since(state.lastAttempt) >= backoff
}

func (w *Watcher) calculateBackoff(retryCount int) time.Duration {
	// Exponential backoff: baseBackoff * 2^(retryCount-1)
	multiplier := 1 << (retryCount - 1) // 1, 2, 4, 8...
	return baseBackoff * time.Duration(multiplier)
}

func (w *Watcher) getOrCreateState(msgID int64) *messageState {
	w.mu.Lock()
	defer w.mu.Unlock()

	state, exists := w.states[msgID]
	if !exists {
		state = &messageState{}
		w.states[msgID] = state
	}
	return state
}

func (w *Watcher) resetUnknownCount(msgID int64) {
	w.mu.Lock()
	defer w.mu.Unlock()
	if state, exists := w.states[msgID]; exists {
		state.unknownCount = 0
	}
}

func (w *Watcher) clearState(msgID int64) {
	w.mu.Lock()
	defer w.mu.Unlock()
	delete(w.states, msgID)
}
