package natsbus_test

import (
	"context"
	"encoding/json"
	"errors"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
)

var allOps = []string{natsbus.OpList, natsbus.OpStatus, natsbus.OpCapture, natsbus.OpSend, natsbus.OpRespond, natsbus.OpKeys}

// fakeRPC records requests and answers with canned data. An agent named
// "ghost" doesn't exist; "slow" blocks until the request is canceled;
// "boom" panics.
type fakeRPC struct {
	mu   sync.Mutex
	got  []any
	slow chan struct{} // closed when a "slow" request starts
}

func (f *fakeRPC) record(req any) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.got = append(f.got, req)
}

func (f *fakeRPC) last() any {
	f.mu.Lock()
	defer f.mu.Unlock()
	if len(f.got) == 0 {
		return nil
	}
	return f.got[len(f.got)-1]
}

func (f *fakeRPC) check(ctx context.Context, agent string) error {
	switch agent {
	case "ghost":
		return errors.New(`agent "ghost" not found`)
	case "slow":
		close(f.slow)
		<-ctx.Done()
		return ctx.Err()
	case "boom":
		panic("handler bug")
	}
	return nil
}

func (f *fakeRPC) List(ctx context.Context) (*natsbus.ListResponse, error) {
	f.record(natsbus.ListRequest{})
	return &natsbus.ListResponse{Agents: []natsbus.AgentInfo{{Name: "worker", Session: "agent-worker", Backend: "claude"}}}, nil
}

func (f *fakeRPC) Status(ctx context.Context, req natsbus.StatusRequest) (*natsbus.StatusResponse, error) {
	f.record(req)
	if err := f.check(ctx, req.Agent); err != nil {
		return nil, err
	}
	return &natsbus.StatusResponse{Status: "waiting", Source: "report", Tool: "Bash"}, nil
}

func (f *fakeRPC) Capture(ctx context.Context, req natsbus.CaptureRequest) (*natsbus.CaptureResponse, error) {
	f.record(req)
	if err := f.check(ctx, req.Agent); err != nil {
		return nil, err
	}
	return &natsbus.CaptureResponse{Output: "screen of " + req.Agent}, nil
}

func (f *fakeRPC) Send(ctx context.Context, req natsbus.SendRequest) (*natsbus.SendResponse, error) {
	f.record(req)
	if err := f.check(ctx, req.Agent); err != nil {
		return nil, err
	}
	return &natsbus.SendResponse{Status: "idle", Typed: 1}, nil
}

func (f *fakeRPC) Respond(ctx context.Context, req natsbus.RespondRequest) error {
	f.record(req)
	return f.check(ctx, req.Agent)
}

func (f *fakeRPC) Keys(ctx context.Context, req natsbus.KeysRequest) error {
	f.record(req)
	return f.check(ctx, req.Agent)
}

// serveFake serves a fakeRPC as host "h1" and returns a client on a second
// connection to the same server.
func serveFake(t *testing.T, ops []string, timeout time.Duration) (*fakeRPC, *natsbus.RPCClient, func()) {
	t.Helper()
	nc := natstest.Start(t)
	fake := &fakeRPC{slow: make(chan struct{})}
	stop, err := natsbus.ServeRPC(nc, "h1", fake, ops)
	if err != nil {
		t.Fatalf("ServeRPC: %v", err)
	}
	t.Cleanup(stop)
	client := natsbus.NewRPCClient(natstest.Connect(t, nc.ConnectedUrl()), timeout)
	return fake, client, stop
}

var worker = natsbus.AgentRef{Name: "worker", Host: "h1"}

func TestRPC_AllOpsRoundTrip(t *testing.T) {
	fake, client, _ := serveFake(t, allOps, 0)
	ctx := context.Background()

	agents, err := client.List(ctx, "h1")
	if err != nil || len(agents) != 1 || agents[0].Name != "worker" || agents[0].Session != "agent-worker" {
		t.Errorf("List = %+v, %v", agents, err)
	}

	st, err := client.Status(ctx, worker)
	if err != nil || st.Status != "waiting" || st.Tool != "Bash" {
		t.Errorf("Status = %+v, %v", st, err)
	}
	if got := fake.last(); got != (natsbus.StatusRequest{Agent: "worker"}) {
		t.Errorf("status request = %+v", got)
	}

	out, err := client.Capture(ctx, worker, 20, true)
	if err != nil || out != "screen of worker" {
		t.Errorf("Capture = %q, %v", out, err)
	}
	if got := fake.last(); got != (natsbus.CaptureRequest{Agent: "worker", Lines: 20, Raw: true}) {
		t.Errorf("capture request = %+v", got)
	}

	if err := client.Send(ctx, worker, "hello there"); err != nil {
		t.Errorf("Send: %v", err)
	}
	if got := fake.last(); got != (natsbus.SendRequest{Agent: "worker", Message: "hello there"}) {
		t.Errorf("send request = %+v", got)
	}

	err = client.Respond(ctx, worker, natsbus.RespondRequest{Agent: "ignored", Response: natsbus.ResponseDeny, ExpectTool: "Bash"})
	if err != nil {
		t.Errorf("Respond: %v", err)
	}
	if got := fake.last(); got != (natsbus.RespondRequest{Agent: "worker", Response: "deny", ExpectTool: "Bash"}) {
		t.Errorf("respond request = %+v", got)
	}

	if err := client.Keys(ctx, worker, "ls", "Enter"); err != nil {
		t.Errorf("Keys: %v", err)
	}
	if got := fake.last(); got != (natsbus.KeysRequest{Agent: "worker", Text: "ls", Key: "Enter"}) {
		t.Errorf("keys request = %+v", got)
	}
}

func TestRPC_RemoteErrorComesBackUnchanged(t *testing.T) {
	_, client, _ := serveFake(t, allOps, 0)

	_, err := client.Status(context.Background(), natsbus.AgentRef{Name: "ghost", Host: "h1"})
	var remote *natsbus.RemoteError
	if !errors.As(err, &remote) || err.Error() != `agent "ghost" not found` || remote.Host != "h1" {
		t.Errorf("err = %#v", err)
	}
}

func TestRPC_PanicBecomesErrorReply(t *testing.T) {
	_, client, _ := serveFake(t, allOps, 0)

	err := client.Send(context.Background(), natsbus.AgentRef{Name: "boom", Host: "h1"}, "x")
	if err == nil || !strings.Contains(err.Error(), "internal error") {
		t.Errorf("err = %v", err)
	}
	// The server survives.
	if _, err := client.Status(context.Background(), worker); err != nil {
		t.Errorf("Status after panic: %v", err)
	}
}

func TestRPC_MalformedRequests(t *testing.T) {
	nc := natstest.Start(t)
	fake := &fakeRPC{slow: make(chan struct{})}
	stop, err := natsbus.ServeRPC(nc, "h1", fake, allOps)
	if err != nil {
		t.Fatal(err)
	}
	defer stop()
	other := natstest.Connect(t, nc.ConnectedUrl())

	tests := []struct {
		op, body, want string
	}{
		{natsbus.OpStatus, `{not json`, "malformed status request"},
		{natsbus.OpCapture, `{"agent": 7}`, "malformed capture request"},
		{natsbus.OpSend, `{}`, "invalid agent name"},
		{natsbus.OpRespond, `{"agent":"../x"}`, "invalid agent name"},
		{natsbus.OpKeys, `{"agent":"worker"}`, "neither text nor key"},
		{natsbus.OpStatus, ``, "invalid agent name"},
	}
	for _, tt := range tests {
		msg, err := other.Request(natsbus.SubjectRPC("h1", tt.op), []byte(tt.body), 2*time.Second)
		if err != nil {
			t.Errorf("%s %q: request failed: %v", tt.op, tt.body, err)
			continue
		}
		var resp natsbus.RPCResponse
		if err := json.Unmarshal(msg.Data, &resp); err != nil {
			t.Errorf("%s %q: reply not an RPCResponse: %v", tt.op, tt.body, err)
			continue
		}
		if !strings.Contains(resp.Error, tt.want) {
			t.Errorf("%s %q: error = %q, want %q", tt.op, tt.body, resp.Error, tt.want)
		}
	}
	if got := fake.last(); got != nil {
		t.Errorf("handler ran for a malformed request: %+v", got)
	}
}

func TestRPC_OnlyAllowedOpsServed(t *testing.T) {
	_, client, _ := serveFake(t, natsbus.DefaultRemoteOps, 0)

	err := client.Keys(context.Background(), worker, "ls", "")
	if err == nil || !strings.Contains(err.Error(), `no bus answering for host "h1"`) {
		t.Errorf("keys on a bus not serving it: err = %v", err)
	}
}

func TestRPC_NoBusForHost(t *testing.T) {
	_, client, _ := serveFake(t, allOps, 0)

	_, err := client.Status(context.Background(), natsbus.AgentRef{Name: "worker", Host: "h2"})
	if err == nil || !strings.Contains(err.Error(), `no bus answering for host "h2"`) {
		t.Errorf("err = %v", err)
	}
}

func TestRPC_Timeout(t *testing.T) {
	fake, client, stop := serveFake(t, allOps, 200*time.Millisecond)

	start := time.Now()
	_, err := client.Status(context.Background(), natsbus.AgentRef{Name: "slow", Host: "h1"})
	if err == nil || !strings.Contains(err.Error(), `host "h1" did not answer status within 200ms`) {
		t.Errorf("err = %v", err)
	}
	if elapsed := time.Since(start); elapsed > 2*time.Second {
		t.Errorf("timeout took %s", elapsed)
	}
	<-fake.slow
	stop() // cancels the stuck handler and waits for it
}

func TestRPC_StopUnsubscribes(t *testing.T) {
	_, client, stop := serveFake(t, allOps, 0)
	stop()
	stop() // idempotent

	_, err := client.Status(context.Background(), worker)
	if err == nil || !strings.Contains(err.Error(), "no bus answering") {
		t.Errorf("err after stop = %v", err)
	}
}

func TestRPC_ClientNeedsHost(t *testing.T) {
	_, client, _ := serveFake(t, allOps, 0)
	if _, err := client.Status(context.Background(), natsbus.AgentRef{Name: "worker"}); err == nil {
		t.Error("expected an error for a ref without a host")
	}
	if _, err := client.List(context.Background(), "a.b"); err == nil {
		t.Error("expected an error for an invalid host")
	}
}

func TestServeRPC_RejectsBadConfig(t *testing.T) {
	nc := natstest.Start(t)
	if _, err := natsbus.ServeRPC(nc, "h.1", &fakeRPC{}, allOps); err == nil {
		t.Error("expected an error for an invalid host")
	}
	if _, err := natsbus.ServeRPC(nc, "h1", &fakeRPC{}, []string{"shell"}); err == nil {
		t.Error("expected an error for an unknown op")
	}
}
