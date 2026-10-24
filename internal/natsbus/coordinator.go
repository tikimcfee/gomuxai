package natsbus

import (
	"context"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/nats-io/nats.go/jetstream"
)

// Coordinator provides high-level coordination operations over NATS.
type Coordinator struct {
	bus *Bus
	kv  jetstream.KeyValue
}

// StateEntry represents a key-value pair from the coordinator state store.
type StateEntry struct {
	Key   string
	Value string
}

// NewCoordinator creates a coordinator backed by the given bus.
// It ensures the KV bucket exists.
func NewCoordinator(ctx context.Context, bus *Bus) (*Coordinator, error) {
	kv, err := bus.EnsureKV(ctx)
	if err != nil {
		return nil, fmt.Errorf("ensuring KV bucket: %w", err)
	}
	return &Coordinator{bus: bus, kv: kv}, nil
}

// Publish publishes an agent event (JetStream persisted).
func (c *Coordinator) Publish(ctx context.Context, event AgentEvent) error {
	data, err := json.Marshal(event)
	if err != nil {
		return fmt.Errorf("marshaling event: %w", err)
	}
	subject := SubjectEvent(event.Agent, string(event.Type))
	_, err = c.bus.js.Publish(ctx, subject, data)
	return err
}

// SendToInbox sends a message to an agent's inbox (JetStream persisted).
func (c *Coordinator) SendToInbox(ctx context.Context, msg InboxMessage, targetAgent string) error {
	data, err := json.Marshal(msg)
	if err != nil {
		return fmt.Errorf("marshaling inbox message: %w", err)
	}
	_, err = c.bus.js.Publish(ctx, SubjectInbox(targetAgent), data)
	return err
}

// SetState sets a state key for an agent in the KV store.
func (c *Coordinator) SetState(ctx context.Context, agentName, key, value string) error {
	kvKey := fmt.Sprintf("%s.%s", agentName, key)
	_, err := c.kv.PutString(ctx, kvKey, value)
	return err
}

// GetState gets a state key for an agent from the KV store.
func (c *Coordinator) GetState(ctx context.Context, agentName, key string) (string, error) {
	kvKey := fmt.Sprintf("%s.%s", agentName, key)
	entry, err := c.kv.Get(ctx, kvKey)
	if err != nil {
		return "", err
	}
	return string(entry.Value()), nil
}

// DeleteState removes a state key for an agent.
func (c *Coordinator) DeleteState(ctx context.Context, agentName, key string) error {
	kvKey := fmt.Sprintf("%s.%s", agentName, key)
	return c.kv.Delete(ctx, kvKey)
}

// ListState returns all state entries for an agent.
func (c *Coordinator) ListState(ctx context.Context, agentName string) ([]StateEntry, error) {
	prefix := agentName + "."
	keys, err := c.kv.Keys(ctx)
	if err != nil {
		return nil, fmt.Errorf("listing keys: %w", err)
	}

	var entries []StateEntry
	for _, k := range keys {
		if strings.HasPrefix(k, prefix) {
			entry, err := c.kv.Get(ctx, k)
			if err != nil {
				continue
			}
			entries = append(entries, StateEntry{
				Key:   strings.TrimPrefix(k, prefix),
				Value: string(entry.Value()),
			})
		}
	}
	return entries, nil
}

// WaitForEvent blocks until an event matching the filter arrives, or context is cancelled.
func (c *Coordinator) WaitForEvent(ctx context.Context, agentName string, eventType EventType) (*AgentEvent, error) {
	filterSubject := SubjectEvent(agentName, string(eventType))

	// Create an ephemeral ordered consumer that only gets new messages
	cons, err := c.bus.js.CreateOrUpdateConsumer(ctx, StreamAgentEvents, jetstream.ConsumerConfig{
		FilterSubject: filterSubject,
		DeliverPolicy: jetstream.DeliverNewPolicy,
		AckPolicy:     jetstream.AckNonePolicy,
		InactiveThreshold: 1 * time.Minute,
	})
	if err != nil {
		return nil, fmt.Errorf("creating consumer: %w", err)
	}

	// Use Messages() for push-based consumption with context cancellation
	iter, err := cons.Messages()
	if err != nil {
		return nil, fmt.Errorf("starting message iterator: %w", err)
	}
	defer iter.Stop()

	// Wait for either a message or context cancellation
	msgCh := make(chan jetstream.Msg, 1)
	errCh := make(chan error, 1)
	go func() {
		msg, err := iter.Next()
		if err != nil {
			errCh <- err
			return
		}
		msgCh <- msg
	}()

	select {
	case msg := <-msgCh:
		event, err := DecodeAgentEvent(msg.Data())
		if err != nil {
			return nil, fmt.Errorf("decoding event: %w", err)
		}
		return event, nil
	case err := <-errCh:
		return nil, err
	case <-ctx.Done():
		return nil, ctx.Err()
	}
}

// DecodeAgentEvent unmarshals an AgentEvent from JSON bytes.
func DecodeAgentEvent(data []byte) (*AgentEvent, error) {
	var event AgentEvent
	if err := json.Unmarshal(data, &event); err != nil {
		return nil, err
	}
	return &event, nil
}

// DecodeInboxMessage unmarshals an InboxMessage from JSON bytes.
func DecodeInboxMessage(data []byte) (*InboxMessage, error) {
	var msg InboxMessage
	if err := json.Unmarshal(data, &msg); err != nil {
		return nil, err
	}
	return &msg, nil
}
