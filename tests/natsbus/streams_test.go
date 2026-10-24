package natsbus_test

import (
	"context"
	"testing"
	"time"

	"github.com/nats-io/nats.go/jetstream"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// =============================================================================
// Subject Builder Tests
// =============================================================================

func TestSubjectBuilders(t *testing.T) {
	tests := []struct {
		name     string
		fn       func() string
		expected string
	}{
		{
			"event status",
			func() string { return natsbus.SubjectEvent("researcher", "status") },
			"agent.researcher.event.status",
		},
		{
			"event done",
			func() string { return natsbus.SubjectEvent("helper", "done") },
			"agent.helper.event.done",
		},
		{
			"inbox",
			func() string { return natsbus.SubjectInbox("helper") },
			"agent.helper.inbox",
		},
		{
			"all events wildcard",
			func() string { return natsbus.SubjectAllEvents() },
			"agent.*.event.*",
		},
		{
			"agent events wildcard",
			func() string { return natsbus.SubjectAgentEvents("analyzer") },
			"agent.analyzer.event.*",
		},
		{
			"system agents",
			func() string { return natsbus.SubjectSystemAgents },
			"system.agents",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got := tt.fn()
			if got != tt.expected {
				t.Errorf("got %q, want %q", got, tt.expected)
			}
		})
	}
}

// =============================================================================
// Stream Creation Tests
// =============================================================================

func TestEnsureStreams_CreatesEventStream(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	ctx := context.Background()

	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("EnsureStreams: %v", err)
	}

	// Publish to an event subject -- should be captured by the stream
	_, err := bus.JS().Publish(ctx, "agent.test.event.status", []byte(`{"test": true}`))
	if err != nil {
		t.Fatalf("publishing to event stream: %v", err)
	}

	// Create a consumer and verify the message is there
	cons, err := bus.JS().CreateOrUpdateConsumer(ctx, natsbus.StreamAgentEvents, jetstream.ConsumerConfig{
		FilterSubject: "agent.test.event.status",
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
		if string(msg.Data()) != `{"test": true}` {
			t.Errorf("got %q, want %q", string(msg.Data()), `{"test": true}`)
		}
		msg.Ack()
	}
	if count != 1 {
		t.Errorf("expected 1 message, got %d", count)
	}
}

func TestEnsureStreams_CreatesInboxStream(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	ctx := context.Background()

	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("EnsureStreams: %v", err)
	}

	// Publish to an inbox subject
	_, err := bus.JS().Publish(ctx, "agent.helper.inbox", []byte(`{"content":"hello"}`))
	if err != nil {
		t.Fatalf("publishing to inbox: %v", err)
	}

	// Create a consumer and verify
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
	for msg := range msgs.Messages() {
		count++
		msg.Ack()
	}
	if count != 1 {
		t.Errorf("expected 1 message, got %d", count)
	}
}

func TestEnsureStreams_Idempotent(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	ctx := context.Background()

	// Call twice -- should not error
	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("first EnsureStreams: %v", err)
	}
	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("second EnsureStreams: %v", err)
	}

	// Streams should still work after double-init
	_, err := bus.JS().Publish(ctx, "agent.test.event.done", []byte("ok"))
	if err != nil {
		t.Errorf("publishing after double EnsureStreams: %v", err)
	}
}

// =============================================================================
// KV Bucket Tests
// =============================================================================

func TestEnsureKV_CreatesBucket(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	ctx := context.Background()
	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("EnsureStreams: %v", err)
	}

	kv, err := bus.EnsureKV(ctx)
	if err != nil {
		t.Fatalf("EnsureKV: %v", err)
	}

	// Put and Get a key
	if _, err := kv.PutString(ctx, "test.key", "test-value"); err != nil {
		t.Fatalf("putting key: %v", err)
	}

	entry, err := kv.Get(ctx, "test.key")
	if err != nil {
		t.Fatalf("getting key: %v", err)
	}
	if string(entry.Value()) != "test-value" {
		t.Errorf("got %q, want %q", string(entry.Value()), "test-value")
	}
}

func TestEnsureKV_Idempotent(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	ctx := context.Background()
	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("EnsureStreams: %v", err)
	}

	// Create twice
	kv1, err := bus.EnsureKV(ctx)
	if err != nil {
		t.Fatalf("first EnsureKV: %v", err)
	}
	kv2, err := bus.EnsureKV(ctx)
	if err != nil {
		t.Fatalf("second EnsureKV: %v", err)
	}

	// Write via kv1, read via kv2
	kv1.PutString(ctx, "shared.key", "shared-value")
	entry, err := kv2.Get(ctx, "shared.key")
	if err != nil {
		t.Fatalf("getting from kv2: %v", err)
	}
	if string(entry.Value()) != "shared-value" {
		t.Errorf("got %q, want %q", string(entry.Value()), "shared-value")
	}
}

func TestEnsureKV_History(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	ctx := context.Background()
	if err := bus.EnsureStreams(ctx); err != nil {
		t.Fatalf("EnsureStreams: %v", err)
	}

	kv, err := bus.EnsureKV(ctx)
	if err != nil {
		t.Fatalf("EnsureKV: %v", err)
	}

	// Put 3 values for the same key
	kv.PutString(ctx, "agent.status", "idle")
	kv.PutString(ctx, "agent.status", "processing")
	kv.PutString(ctx, "agent.status", "done")

	// History should return all 3
	entries, err := kv.History(ctx, "agent.status")
	if err != nil {
		t.Fatalf("getting history: %v", err)
	}

	values := make([]string, 0, 3)
	for _, entry := range entries {
		values = append(values, string(entry.Value()))
	}

	if len(values) != 3 {
		t.Fatalf("expected 3 history entries, got %d", len(values))
	}
	expected := []string{"idle", "processing", "done"}
	for i, v := range values {
		if v != expected[i] {
			t.Errorf("history[%d] = %q, want %q", i, v, expected[i])
		}
	}
}
