package natsbus_test

import (
	"context"
	"testing"
	"time"

	"github.com/nats-io/nats.go/jetstream"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// setupTestCoordinator creates a full test environment: bus, streams, KV, coordinator.
func setupTestCoordinator(t *testing.T) (*natsbus.Coordinator, *natsbus.Bus, func()) {
	t.Helper()
	bus, _, busCleanup := setupTestBus(t)
	ctx := context.Background()
	if err := bus.EnsureStreams(ctx); err != nil {
		busCleanup()
		t.Fatalf("EnsureStreams: %v", err)
	}
	coord, err := natsbus.NewCoordinator(ctx, bus)
	if err != nil {
		busCleanup()
		t.Fatalf("creating coordinator: %v", err)
	}
	return coord, bus, busCleanup
}

// =============================================================================
// State Operations
// =============================================================================

func TestCoordinator_SetState_GetState(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	if err := coord.SetState(ctx, "helper", "status", "idle"); err != nil {
		t.Fatalf("SetState: %v", err)
	}

	val, err := coord.GetState(ctx, "helper", "status")
	if err != nil {
		t.Fatalf("GetState: %v", err)
	}
	if val != "idle" {
		t.Errorf("got %q, want %q", val, "idle")
	}
}

func TestCoordinator_GetState_NotFound(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	_, err := coord.GetState(ctx, "nonexistent", "key")
	if err == nil {
		t.Error("expected error for non-existent key, got nil")
	}
}

func TestCoordinator_SetState_Overwrites(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	coord.SetState(ctx, "helper", "status", "idle")
	coord.SetState(ctx, "helper", "status", "processing")

	val, err := coord.GetState(ctx, "helper", "status")
	if err != nil {
		t.Fatalf("GetState: %v", err)
	}
	if val != "processing" {
		t.Errorf("got %q, want %q", val, "processing")
	}
}

func TestCoordinator_DeleteState(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	coord.SetState(ctx, "helper", "status", "idle")

	if err := coord.DeleteState(ctx, "helper", "status"); err != nil {
		t.Fatalf("DeleteState: %v", err)
	}

	_, err := coord.GetState(ctx, "helper", "status")
	if err == nil {
		t.Error("expected error after delete, got nil")
	}
}

func TestCoordinator_ListState(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	coord.SetState(ctx, "helper", "status", "idle")
	coord.SetState(ctx, "helper", "progress", "3/5")
	coord.SetState(ctx, "helper", "result", "found bugs")

	entries, err := coord.ListState(ctx, "helper")
	if err != nil {
		t.Fatalf("ListState: %v", err)
	}

	if len(entries) != 3 {
		t.Fatalf("expected 3 entries, got %d", len(entries))
	}

	// Check that all keys are present
	found := map[string]string{}
	for _, e := range entries {
		found[e.Key] = e.Value
	}

	expected := map[string]string{
		"status":   "idle",
		"progress": "3/5",
		"result":   "found bugs",
	}
	for k, v := range expected {
		if found[k] != v {
			t.Errorf("key %q: got %q, want %q", k, found[k], v)
		}
	}
}

// =============================================================================
// Publish + Consume Events
// =============================================================================

func TestCoordinator_Publish_EventPersisted(t *testing.T) {
	coord, bus, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	event := natsbus.AgentEvent{
		Agent:     "researcher",
		Type:      natsbus.EventStatus,
		Status:    "processing",
		Timestamp: time.Now(),
	}
	if err := coord.Publish(ctx, event); err != nil {
		t.Fatalf("Publish: %v", err)
	}

	// Consume from the stream
	cons, err := bus.JS().CreateOrUpdateConsumer(ctx, natsbus.StreamAgentEvents, jetstream.ConsumerConfig{
		FilterSubject: "agent.researcher.event.status",
		DeliverPolicy: jetstream.DeliverAllPolicy,
		AckPolicy:     jetstream.AckExplicitPolicy,
	})
	if err != nil {
		t.Fatalf("creating consumer: %v", err)
	}

	msgs, err := cons.Fetch(1, jetstream.FetchMaxWait(2*time.Second))
	if err != nil {
		t.Fatalf("fetching: %v", err)
	}

	count := 0
	for msg := range msgs.Messages() {
		count++
		msg.Ack()
	}
	if count != 1 {
		t.Errorf("expected 1 event, got %d", count)
	}
}

func TestCoordinator_Publish_JSONRoundTrip(t *testing.T) {
	coord, bus, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	now := time.Now().Truncate(time.Millisecond) // JSON loses sub-ms precision
	event := natsbus.AgentEvent{
		Agent:     "helper",
		Type:      natsbus.EventDone,
		Message:   "task complete",
		From:      "coordinator",
		Timestamp: now,
	}
	if err := coord.Publish(ctx, event); err != nil {
		t.Fatalf("Publish: %v", err)
	}

	// Consume and unmarshal
	cons, err := bus.JS().CreateOrUpdateConsumer(ctx, natsbus.StreamAgentEvents, jetstream.ConsumerConfig{
		FilterSubject: "agent.helper.event.done",
		DeliverPolicy: jetstream.DeliverAllPolicy,
		AckPolicy:     jetstream.AckExplicitPolicy,
	})
	if err != nil {
		t.Fatalf("creating consumer: %v", err)
	}

	msgs, err := cons.Fetch(1, jetstream.FetchMaxWait(2*time.Second))
	if err != nil {
		t.Fatalf("fetching: %v", err)
	}

	for msg := range msgs.Messages() {
		decoded, err := natsbus.DecodeAgentEvent(msg.Data())
		if err != nil {
			t.Fatalf("decoding event: %v", err)
		}
		if decoded.Agent != "helper" {
			t.Errorf("Agent = %q, want %q", decoded.Agent, "helper")
		}
		if decoded.Type != natsbus.EventDone {
			t.Errorf("Type = %q, want %q", decoded.Type, natsbus.EventDone)
		}
		if decoded.Message != "task complete" {
			t.Errorf("Message = %q, want %q", decoded.Message, "task complete")
		}
		if decoded.From != "coordinator" {
			t.Errorf("From = %q, want %q", decoded.From, "coordinator")
		}
		if !decoded.Timestamp.Equal(now) {
			t.Errorf("Timestamp = %v, want %v", decoded.Timestamp, now)
		}
		msg.Ack()
	}
}

// =============================================================================
// WaitForEvent
// =============================================================================

func TestCoordinator_WaitForEvent_ReceivesPublished(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	// Publish after a short delay
	go func() {
		time.Sleep(100 * time.Millisecond)
		coord.Publish(ctx, natsbus.AgentEvent{
			Agent:     "helper",
			Type:      natsbus.EventDone,
			Message:   "finished",
			Timestamp: time.Now(),
		})
	}()

	// WaitForEvent should block until the event arrives
	event, err := coord.WaitForEvent(ctx, "helper", natsbus.EventDone)
	if err != nil {
		t.Fatalf("WaitForEvent: %v", err)
	}
	if event.Agent != "helper" {
		t.Errorf("Agent = %q, want %q", event.Agent, "helper")
	}
	if event.Message != "finished" {
		t.Errorf("Message = %q, want %q", event.Message, "finished")
	}
}

func TestCoordinator_WaitForEvent_ContextCancelled(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()

	// WaitForEvent with no publish -- should return context error
	_, err := coord.WaitForEvent(ctx, "helper", natsbus.EventDone)
	if err == nil {
		t.Fatal("expected context error, got nil")
	}
}

func TestCoordinator_WaitForEvent_OnlyMatchesFilter(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx, cancel := context.WithTimeout(context.Background(), 500*time.Millisecond)
	defer cancel()

	// Publish a status event (not done)
	coord.Publish(ctx, natsbus.AgentEvent{
		Agent:     "helper",
		Type:      natsbus.EventStatus,
		Status:    "processing",
		Timestamp: time.Now(),
	})

	// WaitForEvent for "done" should NOT receive the status event
	_, err := coord.WaitForEvent(ctx, "helper", natsbus.EventDone)
	if err == nil {
		t.Error("expected timeout error, got nil -- wrong event type was matched")
	}
}

// =============================================================================
// Inbox
// =============================================================================

func TestCoordinator_SendToInbox_Persisted(t *testing.T) {
	coord, bus, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	msg := natsbus.InboxMessage{
		From:      "coordinator",
		Content:   "analyze auth.go",
		Timestamp: time.Now(),
	}
	if err := coord.SendToInbox(ctx, msg, "helper"); err != nil {
		t.Fatalf("SendToInbox: %v", err)
	}

	// Consume from the inbox stream
	cons, err := bus.JS().CreateOrUpdateConsumer(ctx, natsbus.StreamAgentInbox, jetstream.ConsumerConfig{
		FilterSubject: "agent.helper.inbox",
		DeliverPolicy: jetstream.DeliverAllPolicy,
		AckPolicy:     jetstream.AckExplicitPolicy,
	})
	if err != nil {
		t.Fatalf("creating consumer: %v", err)
	}

	msgs, err := cons.Fetch(1, jetstream.FetchMaxWait(2*time.Second))
	if err != nil {
		t.Fatalf("fetching: %v", err)
	}

	count := 0
	for m := range msgs.Messages() {
		count++
		decoded, err := natsbus.DecodeInboxMessage(m.Data())
		if err != nil {
			t.Fatalf("decoding inbox message: %v", err)
		}
		if decoded.From != "coordinator" {
			t.Errorf("From = %q, want %q", decoded.From, "coordinator")
		}
		if decoded.Content != "analyze auth.go" {
			t.Errorf("Content = %q, want %q", decoded.Content, "analyze auth.go")
		}
		m.Ack()
	}
	if count != 1 {
		t.Errorf("expected 1 inbox message, got %d", count)
	}
}

func TestCoordinator_SendToInbox_MultipleAgentsIsolated(t *testing.T) {
	coord, bus, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	// Send to two different agents
	coord.SendToInbox(ctx, natsbus.InboxMessage{
		From: "coordinator", Content: "for agent1", Timestamp: time.Now(),
	}, "agent1")
	coord.SendToInbox(ctx, natsbus.InboxMessage{
		From: "coordinator", Content: "for agent2", Timestamp: time.Now(),
	}, "agent2")

	// Consume only agent1's inbox
	cons, err := bus.JS().CreateOrUpdateConsumer(ctx, natsbus.StreamAgentInbox, jetstream.ConsumerConfig{
		FilterSubject: "agent.agent1.inbox",
		DeliverPolicy: jetstream.DeliverAllPolicy,
		AckPolicy:     jetstream.AckExplicitPolicy,
	})
	if err != nil {
		t.Fatalf("creating consumer: %v", err)
	}

	msgs, err := cons.Fetch(10, jetstream.FetchMaxWait(1*time.Second))
	if err != nil {
		t.Fatalf("fetching: %v", err)
	}

	count := 0
	for m := range msgs.Messages() {
		count++
		decoded, _ := natsbus.DecodeInboxMessage(m.Data())
		if decoded.Content != "for agent1" {
			t.Errorf("got message for wrong agent: %q", decoded.Content)
		}
		m.Ack()
	}
	if count != 1 {
		t.Errorf("expected 1 message for agent1, got %d", count)
	}
}

// =============================================================================
// Status Report Pattern (simulates agent report flow)
// =============================================================================

func TestCoordinator_StatusReport_SetsKVAndPublishesEvent(t *testing.T) {
	coord, bus, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	// Simulate what `agent report idle` does
	coord.SetState(ctx, "helper", "status", "idle")
	coord.Publish(ctx, natsbus.AgentEvent{
		Agent:     "helper",
		Type:      natsbus.EventStatus,
		Status:    "idle",
		Timestamp: time.Now(),
	})

	// Verify KV
	val, err := coord.GetState(ctx, "helper", "status")
	if err != nil {
		t.Fatalf("GetState: %v", err)
	}
	if val != "idle" {
		t.Errorf("KV status = %q, want %q", val, "idle")
	}

	// Verify event stream
	cons, err := bus.JS().CreateOrUpdateConsumer(ctx, natsbus.StreamAgentEvents, jetstream.ConsumerConfig{
		FilterSubject: "agent.helper.event.status",
		DeliverPolicy: jetstream.DeliverAllPolicy,
		AckPolicy:     jetstream.AckExplicitPolicy,
	})
	if err != nil {
		t.Fatalf("creating consumer: %v", err)
	}
	msgs, err := cons.Fetch(1, jetstream.FetchMaxWait(2*time.Second))
	if err != nil {
		t.Fatalf("fetching: %v", err)
	}
	count := 0
	for m := range msgs.Messages() {
		count++
		m.Ack()
	}
	if count != 1 {
		t.Errorf("expected 1 status event, got %d", count)
	}
}

func TestCoordinator_StatusReport_DoneAlsoPublishesDoneEvent(t *testing.T) {
	coord, _, cleanup := setupTestCoordinator(t)
	defer cleanup()

	ctx := context.Background()

	// Start waiting before publishing (WaitForEvent uses DeliverNewPolicy)
	go func() {
		time.Sleep(100 * time.Millisecond)
		// Simulate `agent report done "finished"`
		coord.SetState(ctx, "helper", "status", "done")
		coord.Publish(ctx, natsbus.AgentEvent{
			Agent:     "helper",
			Type:      natsbus.EventStatus,
			Status:    "done",
			Timestamp: time.Now(),
		})
		coord.Publish(ctx, natsbus.AgentEvent{
			Agent:     "helper",
			Type:      natsbus.EventDone,
			Message:   "finished",
			Timestamp: time.Now(),
		})
	}()

	waitCtx, cancel := context.WithTimeout(ctx, 2*time.Second)
	defer cancel()

	event, err := coord.WaitForEvent(waitCtx, "helper", natsbus.EventDone)
	if err != nil {
		t.Fatalf("WaitForEvent: %v", err)
	}
	if event.Message != "finished" {
		t.Errorf("Message = %q, want %q", event.Message, "finished")
	}

	// Also verify KV was set
	val, err := coord.GetState(ctx, "helper", "status")
	if err != nil {
		t.Fatalf("GetState: %v", err)
	}
	if val != "done" {
		t.Errorf("KV status = %q, want %q", val, "done")
	}
}
