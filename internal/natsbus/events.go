package natsbus

// Status events (v1): publishing a StatusEvent per recorded report, and
// waiting on them. See contract.go for the subjects and the event type.

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"sync"
	"time"

	"github.com/nats-io/nats.go"
)

// DefaultFlushTimeout bounds how long a short-lived process waits for its
// buffered events to reach the bus before it exits. Hook processes live for
// milliseconds, and a core NATS publish only fills a client-side buffer, so
// without a flush their events can vanish with the process.
const DefaultFlushTimeout = 200 * time.Millisecond

// PublishStatus publishes ev on SubjectStatus(ev.Host, ev.Agent). Like any
// core NATS publish it only buffers: flush before the process exits.
func PublishStatus(nc *nats.Conn, ev StatusEvent) error {
	if err := ValidateHost(ev.Host); err != nil {
		return err
	}
	if !namePattern.MatchString(ev.Agent) {
		return fmt.Errorf("invalid agent name %q", ev.Agent)
	}
	data, err := json.Marshal(ev)
	if err != nil {
		return fmt.Errorf("encoding status event: %w", err)
	}
	return nc.Publish(SubjectStatus(ev.Host, ev.Agent), data)
}

// Publisher publishes status events over a connection it opens on first use,
// so a process that never publishes never touches the bus. The connect is
// attempted once per Publisher; a failure (usually ErrNoBus) is remembered
// and returned by every later publish.
type Publisher struct {
	connect LocalConnector

	mu    sync.Mutex
	tried bool
	nc    *nats.Conn
	err   error
}

// NewPublisher returns a Publisher that connects with connect when it first
// publishes.
func NewPublisher(connect LocalConnector) *Publisher {
	return &Publisher{connect: connect}
}

// PublishStatus publishes ev, connecting first if this is the first publish.
func (p *Publisher) PublishStatus(ev StatusEvent) error {
	nc, err := p.conn()
	if err != nil {
		return err
	}
	return PublishStatus(nc, ev)
}

func (p *Publisher) conn() (*nats.Conn, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	if !p.tried {
		p.tried = true
		p.nc, p.err = p.connect()
		if p.nc == nil && p.err == nil {
			p.err = ErrNoBus
		}
	}
	return p.nc, p.err
}

// Flush waits up to timeout until the bus has received everything published
// so far. It is a no-op if nothing was published.
func (p *Publisher) Flush(timeout time.Duration) error {
	p.mu.Lock()
	nc := p.nc
	p.mu.Unlock()
	if nc == nil {
		return nil
	}
	if err := nc.FlushTimeout(timeout); err != nil {
		return fmt.Errorf("flushing status events: %w", err)
	}
	return nil
}

// Close flushes (bounded by DefaultFlushTimeout) and closes the connection.
// Later publishes fail.
func (p *Publisher) Close() error {
	err := p.Flush(DefaultFlushTimeout)
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.nc != nil {
		p.nc.Close()
	}
	p.tried, p.nc, p.err = true, nil, nats.ErrConnectionClosed
	return err
}

// DecodeStatusEvent parses a message published on a status subject.
func DecodeStatusEvent(msg *nats.Msg) (StatusEvent, error) {
	var ev StatusEvent
	if err := json.Unmarshal(msg.Data, &ev); err != nil {
		return ev, fmt.Errorf("decoding status event on %s: %w", msg.Subject, err)
	}
	return ev, nil
}

// WaitOptions configures Wait.
type WaitOptions struct {
	Host  string // host whose events to watch
	Agent string

	// For lists the statuses that end the wait successfully.
	For []string
	// Fail lists statuses that end the wait with an error because For can't
	// follow them, e.g. "exited" when waiting for "idle".
	Fail []string
	// Next ignores For until the agent has been seen in another status, so
	// the wait ends at the next arrival, not at a status it's already in.
	// For waiting right after sending work: until the agent picks the
	// message up, it still looks idle from its previous turn.
	Next bool

	// Current returns the agent's status now. Wait calls it once right after
	// subscribing and then every Interval, as a backstop for status changes
	// that publish no event (e.g. Claude's native status moving back to idle
	// after an interrupted permission prompt). Without a bus it is the only
	// source. A remote agent can be waited on by passing an RPC status call.
	Current func(context.Context) (string, error)
	// Interval between calls to Current.
	Interval time.Duration
}

// subscribeFlushTimeout bounds the round trip that confirms the subscription
// is registered with the server.
const subscribeFlushTimeout = 2 * time.Second

// Wait blocks until the agent's status is one of opts.For and returns it.
// nc may be nil (no bus), in which case Wait only polls opts.Current.
//
// It subscribes to the agent's status events before it checks the current
// status, so a transition between the two can't be missed. The wait ends at
// the first status in For seen after Wait starts, from either an event or a
// check, even if the agent has moved on since. With opts.Next, statuses in
// For count only after one that isn't.
//
// On error, the returned status is the last one seen ("" if none). When ctx
// ends, the error wraps ctx.Err(), so a timeout satisfies
// errors.Is(err, context.DeadlineExceeded).
func Wait(ctx context.Context, nc *nats.Conn, opts WaitOptions) (string, error) {
	if len(opts.For) == 0 {
		return "", fmt.Errorf("no statuses to wait for")
	}
	if opts.Current == nil {
		return "", fmt.Errorf("no current-status check")
	}
	if opts.Interval <= 0 {
		return "", fmt.Errorf("check interval must be positive, got %v", opts.Interval)
	}

	var events chan *nats.Msg // stays nil without a bus: never ready
	if nc != nil {
		events = make(chan *nats.Msg, 64)
		sub, err := nc.ChanSubscribe(SubjectStatus(opts.Host, opts.Agent), events)
		if err != nil {
			return "", fmt.Errorf("subscribing to status events: %w", err)
		}
		defer sub.Unsubscribe()
		// The server only routes events to the subscription once it has
		// processed the SUB. Confirm that before the first check, or a
		// transition right after the check could still be missed.
		if err := nc.FlushTimeout(subscribeFlushTimeout); err != nil {
			return "", fmt.Errorf("subscribing to status events: %w", err)
		}
	}

	last := ""
	armed := !opts.Next
	// settle reports whether status ends the wait, and with what error.
	settle := func(status string) (bool, error) {
		last = status
		switch {
		case slices.Contains(opts.For, status):
			return armed, nil
		case slices.Contains(opts.Fail, status):
			return true, fmt.Errorf("agent %s: status is %s, so it won't become %s", opts.Agent, status, strings.Join(opts.For, " or "))
		}
		armed = true
		return false, nil
	}
	check := func() (bool, error) {
		status, err := opts.Current(ctx)
		if err != nil {
			if ctx.Err() != nil {
				return true, fmt.Errorf("waiting for %s: %w", opts.Agent, ctx.Err())
			}
			return true, err
		}
		return settle(status)
	}

	if done, err := check(); done {
		return last, err
	}
	ticker := time.NewTicker(opts.Interval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return last, fmt.Errorf("waiting for %s: %w", opts.Agent, ctx.Err())
		case msg := <-events:
			ev, err := DecodeStatusEvent(msg)
			if err != nil {
				continue // not ours to fail on; the backstop still runs
			}
			if done, err := settle(ev.Status); done {
				return last, err
			}
		case <-ticker.C:
			if done, err := check(); done {
				return last, err
			}
		}
	}
}
