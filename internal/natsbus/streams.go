package natsbus

import (
	"context"
	"fmt"
	"time"

	"github.com/nats-io/nats.go/jetstream"
)

const (
	StreamAgentEvents = "AGENT_EVENTS"
	StreamAgentInbox  = "AGENT_INBOX"
	KVBucketState     = "agent_state"
)

// EnsureStreams creates or updates the JetStream streams.
func (b *Bus) EnsureStreams(ctx context.Context) error {
	// Events stream -- captures all agent.*.event.* subjects
	_, err := b.js.CreateOrUpdateStream(ctx, jetstream.StreamConfig{
		Name:      StreamAgentEvents,
		Subjects:  []string{"agent.*.event.*"},
		Retention: jetstream.LimitsPolicy,
		MaxAge:    24 * time.Hour,
		Storage:   jetstream.FileStorage,
	})
	if err != nil {
		return fmt.Errorf("creating events stream: %w", err)
	}

	// Inbox stream -- captures all agent.*.inbox subjects
	_, err = b.js.CreateOrUpdateStream(ctx, jetstream.StreamConfig{
		Name:      StreamAgentInbox,
		Subjects:  []string{"agent.*.inbox"},
		Retention: jetstream.WorkQueuePolicy,
		Storage:   jetstream.FileStorage,
	})
	if err != nil {
		return fmt.Errorf("creating inbox stream: %w", err)
	}

	return nil
}

// EnsureKV creates or returns the state KV bucket.
func (b *Bus) EnsureKV(ctx context.Context) (jetstream.KeyValue, error) {
	kv, err := b.js.CreateOrUpdateKeyValue(ctx, jetstream.KeyValueConfig{
		Bucket:  KVBucketState,
		History: 5,
		TTL:     0,
		Storage: jetstream.FileStorage,
	})
	return kv, err
}
