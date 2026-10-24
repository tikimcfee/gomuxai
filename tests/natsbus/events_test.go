package natsbus_test

import (
	"context"
	"errors"
	"net"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
)

const testHost = "hosta"

// subscribeStatus subscribes to every status event on testHost and waits
// until the server has the subscription.
func subscribeStatus(t *testing.T, nc *nats.Conn) *nats.Subscription {
	t.Helper()
	sub, err := nc.SubscribeSync(natsbus.SubjectAllStatus(testHost))
	if err != nil {
		t.Fatal(err)
	}
	if err := nc.Flush(); err != nil {
		t.Fatal(err)
	}
	return sub
}

func nextEvent(t *testing.T, sub *nats.Subscription) natsbus.StatusEvent {
	t.Helper()
	msg, err := sub.NextMsg(2 * time.Second)
	if err != nil {
		t.Fatalf("no status event: %v", err)
	}
	ev, err := natsbus.DecodeStatusEvent(msg)
	if err != nil {
		t.Fatal(err)
	}
	return ev
}

// publish publishes and flushes a status event. Safe to call from any
// goroutine: it reports failures with Errorf.
func publish(t *testing.T, nc *nats.Conn, agent, status string) {
	t.Helper()
	if err := natsbus.PublishStatus(nc, natsbus.StatusEvent{Host: testHost, Agent: agent, Status: status, At: time.Now()}); err != nil {
		t.Errorf("publish: %v", err)
	}
	if err := nc.Flush(); err != nil {
		t.Errorf("flush: %v", err)
	}
}

func TestPublishStatus_RejectsUnsafeNames(t *testing.T) {
	nc := natstest.Start(t)
	for _, ev := range []natsbus.StatusEvent{
		{Host: "", Agent: "a", Status: "idle"},
		{Host: "air.local", Agent: "a", Status: "idle"},
		{Host: testHost, Agent: "a.b", Status: "idle"},
	} {
		if err := natsbus.PublishStatus(nc, ev); err == nil {
			t.Errorf("PublishStatus(%+v) succeeded, want an error", ev)
		}
	}
}

func TestPublisher_ConnectsOnlyOnFirstPublish(t *testing.T) {
	nc := natstest.Start(t)
	sub := subscribeStatus(t, nc)

	var connects atomic.Int32
	p := natsbus.NewPublisher(func() (*nats.Conn, error) {
		connects.Add(1)
		return nats.Connect(nc.ConnectedUrl())
	})
	if err := p.Flush(time.Second); err != nil {
		t.Fatalf("Flush before any publish: %v", err)
	}
	if n := connects.Load(); n != 0 {
		t.Fatalf("connected %d times before publishing", n)
	}

	for _, status := range []string{"processing", "idle"} {
		if err := p.PublishStatus(natsbus.StatusEvent{Host: testHost, Agent: "w", Status: status}); err != nil {
			t.Fatalf("publish %s: %v", status, err)
		}
	}
	if err := p.Close(); err != nil {
		t.Fatalf("Close: %v", err)
	}
	if n := connects.Load(); n != 1 {
		t.Errorf("connected %d times, want 1", n)
	}
	if ev := nextEvent(t, sub); ev.Status != "processing" {
		t.Errorf("first event = %+v", ev)
	}
	if ev := nextEvent(t, sub); ev.Status != "idle" {
		t.Errorf("second event = %+v", ev)
	}
}

func TestPublisher_NoBusIsTriedOnce(t *testing.T) {
	var connects atomic.Int32
	p := natsbus.NewPublisher(func() (*nats.Conn, error) {
		connects.Add(1)
		return nil, natsbus.ErrNoBus
	})
	for range 3 {
		if err := p.PublishStatus(natsbus.StatusEvent{Host: testHost, Agent: "w", Status: "idle"}); !errors.Is(err, natsbus.ErrNoBus) {
			t.Fatalf("err = %v, want ErrNoBus", err)
		}
	}
	if n := connects.Load(); n != 1 {
		t.Errorf("connected %d times, want 1", n)
	}
	if err := p.Close(); err != nil {
		t.Errorf("Close with no connection: %v", err)
	}
}

// killableDialer records the sockets it opens so a test can drop them the
// way a process exit does: no NATS-level close, buffered data gone.
type killableDialer struct {
	mu    sync.Mutex
	conns []net.Conn
}

func (d *killableDialer) Dial(network, address string) (net.Conn, error) {
	c, err := net.Dial(network, address)
	if err == nil {
		d.mu.Lock()
		d.conns = append(d.conns, c)
		d.mu.Unlock()
	}
	return c, err
}

func (d *killableDialer) kill() {
	d.mu.Lock()
	defer d.mu.Unlock()
	for _, c := range d.conns {
		c.Close()
	}
}

// A hook process publishes, flushes, and exits. The flush must be enough on
// its own for the event to reach subscribers.
func TestPublisher_FlushDeliversBeforeAbruptExit(t *testing.T) {
	nc := natstest.Start(t)
	sub := subscribeStatus(t, nc)

	dialer := &killableDialer{}
	var pubConn *nats.Conn
	p := natsbus.NewPublisher(func() (*nats.Conn, error) {
		c, err := nats.Connect(nc.ConnectedUrl(), nats.SetCustomDialer(dialer), nats.NoReconnect())
		pubConn = c
		return c, err
	})
	if err := p.PublishStatus(natsbus.StatusEvent{Host: testHost, Agent: "hooked", Status: "idle", Event: "Stop"}); err != nil {
		t.Fatalf("publish: %v", err)
	}
	if err := p.Flush(natsbus.DefaultFlushTimeout); err != nil {
		t.Fatalf("Flush: %v", err)
	}
	dialer.kill() // the process is gone
	defer pubConn.Close()

	if ev := nextEvent(t, sub); ev.Agent != "hooked" || ev.Status != "idle" || ev.Event != "Stop" {
		t.Errorf("event = %+v", ev)
	}
}

// statusSource is a fake agent status for Wait's Current callback.
type statusSource struct {
	mu     sync.Mutex
	status string
	calls  int
	// The last recorded report, as StatusInfo exposes it.
	reported   string
	reportedAt time.Time
	// onFirst runs inside the first call, after the status is read.
	onFirst func()
}

func (s *statusSource) set(status string) {
	s.mu.Lock()
	s.status = status
	s.mu.Unlock()
}

// record simulates a new status report that is also the current status.
func (s *statusSource) record(status string) {
	s.mu.Lock()
	s.status, s.reported, s.reportedAt = status, status, time.Now()
	s.mu.Unlock()
}

func (s *statusSource) current(ctx context.Context) (natsbus.Observation, error) {
	status, err := s.currentStatus(ctx)
	s.mu.Lock()
	defer s.mu.Unlock()
	return natsbus.Observation{Status: status, ReportedStatus: s.reported, ReportedAt: s.reportedAt}, err
}

func (s *statusSource) currentStatus(context.Context) (string, error) {
	s.mu.Lock()
	status := s.status
	s.calls++
	first := s.calls == 1
	s.mu.Unlock()
	if first && s.onFirst != nil {
		s.onFirst()
	}
	return status, nil
}

func (s *statusSource) callCount() int {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.calls
}

func waitOpts(src *statusSource, interval time.Duration, statuses ...string) natsbus.WaitOptions {
	return natsbus.WaitOptions{
		Host:     testHost,
		Agent:    "worker",
		For:      statuses,
		Fail:     []string{"exited"},
		Current:  src.current,
		Interval: interval,
	}
}

// goBackground runs fn in a goroutine that the test waits for before its
// NATS servers shut down (cleanups run in reverse order).
func goBackground(t *testing.T, fn func()) {
	done := make(chan struct{})
	t.Cleanup(func() { <-done })
	go func() {
		defer close(done)
		fn()
	}()
}

func testCtx(t *testing.T) context.Context {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	t.Cleanup(cancel)
	return ctx
}

func TestWait_ReturnsOnEvent(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "processing"}
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			publish(t, pub, "other", "idle")     // a different agent
			publish(t, pub, "worker", "waiting") // not a target
			src.set("idle")
			publish(t, pub, "worker", "idle")
		})
	}

	start := time.Now()
	status, err := natsbus.Wait(testCtx(t), nc, waitOpts(src, time.Hour, "idle"))
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
	if d := time.Since(start); d > 2*time.Second {
		t.Errorf("took %v", d)
	}
	if n := src.callCount(); n != 2 {
		t.Errorf("Current called %d times; want the initial check plus one confirming the event", n)
	}
}

// A status event is a hint: one that the agent's real status doesn't back up
// (forged by the routing hub, or simply stale) must not end the wait.
func TestWait_UnconfirmedEventDoesNotEndWait(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "processing"}
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			publish(t, pub, "worker", "idle") // forged: the agent is still working
		})
	}

	ctx, cancel := context.WithTimeout(context.Background(), 2500*time.Millisecond)
	defer cancel()
	status, err := natsbus.Wait(ctx, nc, waitOpts(src, time.Hour, "idle"))
	if err == nil {
		t.Fatalf("Wait returned %q on an event the real status never confirmed", status)
	}
	// Initial check, the immediate confirmation, and the short recheck.
	if n := src.callCount(); n < 3 {
		t.Errorf("Current called %d times; want the event confirmed and rechecked", n)
	}
}

// The real status often lags the event (Claude's native status trails its
// hooks), so the confirmation is retried shortly rather than at the next tick.
func TestWait_LaggingStatusIsRecheckedSoon(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "processing"}
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			publish(t, pub, "worker", "idle")
			time.Sleep(300 * time.Millisecond)
			src.set("idle") // the real status catches up
		})
	}

	start := time.Now()
	status, err := natsbus.Wait(testCtx(t), nc, waitOpts(src, time.Hour, "idle"))
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
	if d := time.Since(start); d > 3*time.Second {
		t.Errorf("took %v; the recheck should have caught up well before the next tick", d)
	}
}

func TestWait_AlreadyInTargetStatus(t *testing.T) {
	nc := natstest.Start(t)
	src := &statusSource{status: "waiting"}

	status, err := natsbus.Wait(testCtx(t), nc, waitOpts(src, time.Hour, "idle", "waiting"))
	if err != nil || status != "waiting" {
		t.Fatalf("Wait = %q, %v; want waiting", status, err)
	}
}

// The transition lands right after the status check read the old value:
// only a subscription made before the check sees it.
func TestWait_CatchesChangeBetweenSubscribeAndCheck(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "processing"}
	src.onFirst = func() {
		src.set("idle")
		publish(t, pub, "worker", "idle")
	}

	// No backstop within the test: only the event can end the wait.
	status, err := natsbus.Wait(testCtx(t), nc, waitOpts(src, time.Hour, "idle"))
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
}

// Claude's native status can change with no hook firing (an interrupted
// permission prompt), so no event arrives; the periodic check catches it.
func TestWait_BackstopCatchesChangeWithoutEvent(t *testing.T) {
	nc := natstest.Start(t)
	src := &statusSource{status: "waiting"}
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			src.set("idle")
		})
	}

	status, err := natsbus.Wait(testCtx(t), nc, waitOpts(src, 20*time.Millisecond, "idle"))
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
}

func TestWait_NoBusPolls(t *testing.T) {
	src := &statusSource{status: "processing"}
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			src.set("idle")
		})
	}

	status, err := natsbus.Wait(testCtx(t), nil, waitOpts(src, 20*time.Millisecond, "idle"))
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
}

func TestWait_Timeout(t *testing.T) {
	nc := natstest.Start(t)
	src := &statusSource{status: "processing"}
	ctx, cancel := context.WithTimeout(context.Background(), 100*time.Millisecond)
	defer cancel()

	status, err := natsbus.Wait(ctx, nc, waitOpts(src, 20*time.Millisecond, "idle"))
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("err = %v, want DeadlineExceeded", err)
	}
	if status != "processing" {
		t.Errorf("last status = %q, want processing", status)
	}
}

func TestWait_FailStatusEndsWait(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "processing"}
	src.onFirst = func() {
		src.set("exited")
		publish(t, pub, "worker", "exited")
	}

	status, err := natsbus.Wait(testCtx(t), nc, waitOpts(src, time.Hour, "idle"))
	if err == nil || status != "exited" {
		t.Fatalf("Wait = %q, %v; want an error at exited", status, err)
	}
}

func TestWait_CurrentErrorEndsWait(t *testing.T) {
	boom := errors.New("agent \"worker\" not found")
	_, err := natsbus.Wait(testCtx(t), nil, natsbus.WaitOptions{
		Agent:    "worker",
		For:      []string{"idle"},
		Current:  func(context.Context) (natsbus.Observation, error) { return natsbus.Observation{}, boom },
		Interval: time.Hour,
	})
	if !errors.Is(err, boom) {
		t.Errorf("err = %v, want %v", err, boom)
	}
}

// Right after a send the agent still reports its previous turn's idle; with
// Next, only an idle after it has left idle ends the wait.
func TestWait_NextWaitsForTheNextArrival(t *testing.T) {
	nc := natstest.Start(t)
	pub := natstest.Connect(t, nc.ConnectedUrl())
	src := &statusSource{status: "idle"}
	var cameBack atomic.Bool
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			publish(t, pub, "worker", "idle") // still the old turn
			// As Report does: record the status, then publish it.
			src.record("processing")
			publish(t, pub, "worker", "processing")
			time.Sleep(50 * time.Millisecond)
			cameBack.Store(true)
			src.record("idle")
			publish(t, pub, "worker", "idle")
		})
	}

	opts := waitOpts(src, time.Hour, "idle")
	opts.Next = true
	status, err := natsbus.Wait(testCtx(t), nc, opts)
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
	if !cameBack.Load() {
		t.Error("Wait returned before the agent left idle and came back")
	}
}

func TestWait_NextArmedByCurrentStatus(t *testing.T) {
	src := &statusSource{status: "processing"} // the turn already started
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			src.set("idle")
		})
	}

	opts := waitOpts(src, 20*time.Millisecond, "idle")
	opts.Next = true
	status, err := natsbus.Wait(testCtx(t), nil, opts)
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle", status, err)
	}
}

// A turn can start and finish between two checks (no events, or events
// lost): --next never sees the agent leave idle, but the newer idle report
// shows a turn happened, which is what --next waits for.
func TestWait_NextArmedByNewerReportBetweenChecks(t *testing.T) {
	src := &statusSource{}
	src.record("idle") // the previous turn's report
	src.onFirst = func() {
		goBackground(t, func() {
			time.Sleep(50 * time.Millisecond)
			src.record("idle") // a whole new turn, already finished
		})
	}
	opts := waitOpts(src, 30*time.Millisecond, "idle")
	opts.Next = true

	status, err := natsbus.Wait(testCtx(t), nil, opts)
	if err != nil || status != "idle" {
		t.Fatalf("Wait = %q, %v; want idle after the newer report", status, err)
	}
}

// The same idle report seen again is not a new turn.
func TestWait_NextNotArmedBySameReport(t *testing.T) {
	src := &statusSource{}
	src.record("idle")
	opts := waitOpts(src, 20*time.Millisecond, "idle")
	opts.Next = true

	ctx, cancel := context.WithTimeout(context.Background(), 300*time.Millisecond)
	defer cancel()
	if status, err := natsbus.Wait(ctx, nil, opts); err == nil {
		t.Fatalf("Wait returned %q without any new turn", status)
	}
}
