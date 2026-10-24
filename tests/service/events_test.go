package service_test

import (
	"context"
	"errors"
	"fmt"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/service"
)

const eventsHost = "evthost"

// setupEventsEnv is setupTestEnv with a bus host name and evt- tmux sessions.
func setupEventsEnv(t *testing.T) *testEnv {
	t.Helper()
	env := setupTestEnv(t)
	t.Cleanup(env.cleanup)
	env.cfg.Host = eventsHost
	env.cfg.SessionPrefix = fmt.Sprintf("evt-%d-", time.Now().UnixNano())
	return env
}

// busPublisher returns a publisher connected to nc's server, as the CLI's is
// to the local bus.
func busPublisher(t *testing.T, nc *nats.Conn) *natsbus.Publisher {
	t.Helper()
	p := natsbus.NewPublisher(func() (*nats.Conn, error) { return nats.Connect(nc.ConnectedUrl()) })
	t.Cleanup(func() { p.Close() })
	return p
}

// failingPublisher stands in for a bus that is down or misbehaving.
type failingPublisher struct{ calls atomic.Int32 }

func (f *failingPublisher) PublishStatus(natsbus.StatusEvent) error {
	f.calls.Add(1)
	return errors.New("bus exploded")
}

func TestReport_PublishesStatusEvent(t *testing.T) {
	env := setupEventsEnv(t)
	ctx := context.Background()
	spawnFake(t, env, "worker")

	nc := natstest.Start(t)
	sub, err := nc.SubscribeSync(natsbus.SubjectStatus(eventsHost, "worker"))
	if err != nil {
		t.Fatal(err)
	}
	nc.Flush()
	pub := busPublisher(t, nc)
	env.service.SetEventPublisher(pub)

	before := time.Now()
	err = env.service.Report(ctx, "worker", service.StatusReport{
		Status: service.AgentStatusWaiting, Event: "PermissionRequest", Detail: "permission: Bash", Tool: "Bash",
	})
	if err != nil {
		t.Fatalf("Report: %v", err)
	}
	if err := pub.Flush(time.Second); err != nil {
		t.Fatalf("Flush: %v", err)
	}

	msg, err := sub.NextMsg(2 * time.Second)
	if err != nil {
		t.Fatalf("no event: %v", err)
	}
	ev, err := natsbus.DecodeStatusEvent(msg)
	if err != nil {
		t.Fatal(err)
	}
	want := natsbus.StatusEvent{Host: eventsHost, Agent: "worker", Status: "waiting", Event: "PermissionRequest", Detail: "permission: Bash", Tool: "Bash"}
	got := ev
	got.At = time.Time{}
	if got != want {
		t.Errorf("event = %+v, want %+v", got, want)
	}
	if ev.At.Before(before.Add(-time.Second)) || ev.At.After(time.Now()) {
		t.Errorf("event time %v not around the report", ev.At)
	}
}

func TestReport_PublishFailureDoesNotFailReport(t *testing.T) {
	env := setupEventsEnv(t)
	ctx := context.Background()
	spawnFake(t, env, "worker")

	// A publisher that errors, and one whose connection has died.
	failing := &failingPublisher{}
	nc := natstest.Start(t)
	closed := busPublisher(t, nc)
	closed.Close()

	for name, pub := range map[string]service.EventPublisher{"failing": failing, "closed": closed} {
		env.service.SetEventPublisher(pub)
		if err := env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: name}); err != nil {
			t.Fatalf("%s publisher: Report failed: %v", name, err)
		}
		info, err := env.service.StatusInfo(ctx, "worker")
		if err != nil {
			t.Fatal(err)
		}
		if info.Status != service.AgentStatusIdle || info.Event != name {
			t.Errorf("%s publisher: status = %s/%s, want the report recorded", name, info.Status, info.Event)
		}
	}
	if failing.calls.Load() != 1 {
		t.Errorf("failing publisher called %d times, want 1", failing.calls.Load())
	}
}

// Hook events that carry no status change record nothing, so they must not
// open a bus connection either: hooks run on every agent event.
func TestReportHook_IgnoredEventDoesNotTouchBus(t *testing.T) {
	env := setupEventsEnv(t)
	spawnFake(t, env, "worker")

	var connects atomic.Int32
	env.service.SetEventPublisher(natsbus.NewPublisher(func() (*nats.Conn, error) {
		connects.Add(1)
		return nil, natsbus.ErrNoBus
	}))
	if err := env.service.ReportHook(context.Background(), "worker", "", "", []byte(`{"hook_event_name":"SubagentStart","session_id":"s"}`)); err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
	if n := connects.Load(); n != 0 {
		t.Errorf("connected %d times for an ignored hook event", n)
	}
}

// End to end: wait on a local agent while its hooks report.
func TestWait_LocalAgentReportsIdle(t *testing.T) {
	env := setupEventsEnv(t)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	spawnFake(t, env, "worker")
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing, Event: "UserPromptSubmit"})

	nc := natstest.Start(t)
	env.service.SetEventPublisher(busPublisher(t, nc))

	current := func(ctx context.Context) (string, error) {
		info, err := env.service.StatusInfo(ctx, "worker")
		if err != nil {
			return "", err
		}
		return string(info.Status), nil
	}
	reported := make(chan error, 1)
	go func() {
		time.Sleep(100 * time.Millisecond)
		reported <- env.service.ReportHook(ctx, "worker", "", "", []byte(`{"hook_event_name":"Stop","session_id":"s","last_assistant_message":"done"}`))
	}()

	status, err := natsbus.Wait(ctx, nc, natsbus.WaitOptions{
		Host: eventsHost, Agent: "worker", For: []string{"idle"}, Current: current, Interval: time.Hour,
	})
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
	if err := <-reported; err != nil {
		t.Fatalf("ReportHook: %v", err)
	}
}

func TestWait_LocalAgentWithoutBus(t *testing.T) {
	env := setupEventsEnv(t)
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	spawnFake(t, env, "worker")
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusProcessing})

	reported := make(chan error, 1)
	go func() {
		time.Sleep(100 * time.Millisecond)
		reported <- env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle})
	}()

	status, err := natsbus.Wait(ctx, nil, natsbus.WaitOptions{
		Host: eventsHost, Agent: "worker", For: []string{"idle"}, Interval: 20 * time.Millisecond,
		Current: func(ctx context.Context) (string, error) {
			st, err := env.service.Status(ctx, "worker")
			return string(st), err
		},
	})
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
	if err := <-reported; err != nil {
		t.Fatalf("Report: %v", err)
	}
}
