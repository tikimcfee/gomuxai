package service_test

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/natsbus/natstest"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// serveService serves env's service over RPC as host "h1" (every op) and
// returns a client on a separate connection.
func serveService(t *testing.T, env *testEnv) *natsbus.RPCClient {
	t.Helper()
	nc := natstest.Start(t)
	ops := append([]string{natsbus.OpKeys}, natsbus.DefaultRemoteOps...)
	stop, err := natsbus.ServeRPC(nc, "h1", service.NewRPCHandler(env.service), ops)
	if err != nil {
		t.Fatalf("ServeRPC: %v", err)
	}
	t.Cleanup(stop)
	return natsbus.NewRPCClient(natstest.Connect(t, nc.ConnectedUrl()), 5*time.Second)
}

// waitForScreen polls a remote capture until it contains want.
func waitForScreen(t *testing.T, client *natsbus.RPCClient, ref natsbus.AgentRef, want string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		out, err := client.Capture(context.Background(), ref, 50, false)
		if err != nil {
			t.Fatalf("Capture: %v", err)
		}
		if strings.Contains(out, want) {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("capture never showed %q; last:\n%s", want, out)
		}
		time.Sleep(50 * time.Millisecond)
	}
}

func TestRPC_EndToEndThroughService(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.SessionPrefix = "rpc-" + env.cfg.SessionPrefix
	ctx := context.Background()
	fake := spawnFakeWith(t, env, "worker", service.SpawnOptions{SpawnedBy: "lead"})
	client := serveService(t, env)
	ref := natsbus.AgentRef{Name: "worker", Host: "h1"}

	agents, err := client.List(ctx, "h1")
	if err != nil || len(agents) != 1 {
		t.Fatalf("List = %+v, %v", agents, err)
	}
	if a := agents[0]; a.Name != "worker" || a.Backend != "fakehooks" || a.Session != env.cfg.SessionName("worker") || a.SpawnedBy != "lead" {
		t.Errorf("listed %+v", a)
	}

	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusWaiting, Event: "PermissionRequest", Detail: "permission: Bash: ls", Tool: "Bash"})
	st, err := client.Status(ctx, ref)
	if err != nil {
		t.Fatalf("Status: %v", err)
	}
	if st.Status != "waiting" || st.Source != service.StatusSourceReport || st.Tool != "Bash" || st.Detail != "permission: Bash: ls" || st.UpdatedAt.IsZero() {
		t.Errorf("status = %+v", st)
	}

	// send refuses while the agent waits on a prompt: its Enter would answer
	// the prompt, so a remote message can't be used to approve one.
	if err := client.Send(ctx, ref, "echo nope"); err == nil || !strings.Contains(err.Error(), "waiting on a prompt") {
		t.Errorf("send to a waiting agent: err = %v, want a refusal", err)
	}
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle})
	fake.promptGone = true // the dialog is answered

	// send and capture: the shell runs what was sent, and the output shows remotely.
	if err := client.Send(ctx, ref, "echo rpc-send-$((40+2))"); err != nil {
		t.Fatalf("Send: %v", err)
	}
	waitForScreen(t, client, ref, "rpc-send-42")

	if err := client.Keys(ctx, ref, "echo rpc-keys-$((50+5))", "Enter"); err != nil {
		t.Fatalf("Keys: %v", err)
	}
	waitForScreen(t, client, ref, "rpc-keys-55")

	// respond: the service's own checks apply, and their errors come back.
	fake.promptGone = false
	env.service.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusWaiting, Event: "PermissionRequest", Detail: "permission: Bash: ls", Tool: "Bash"})
	err = client.Respond(ctx, ref, natsbus.RespondRequest{Response: natsbus.ResponseApprove, ExpectTool: "Write"})
	if err == nil || !strings.Contains(err.Error(), `waiting on "Bash", not "Write"`) {
		t.Errorf("approve with wrong --expect: err = %v", err)
	}
	if err := client.Respond(ctx, ref, natsbus.RespondRequest{Response: natsbus.ResponseDeny, ExpectTool: "Bash"}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	if st, _ := env.service.Status(ctx, "worker"); st != service.AgentStatusIdle {
		t.Errorf("after remote deny: status = %s, want idle", st)
	}
	err = client.Respond(ctx, ref, natsbus.RespondRequest{Response: natsbus.ResponseApprove})
	if err == nil || !strings.Contains(err.Error(), "not waiting") {
		t.Errorf("approve while idle: err = %v", err)
	}
}

func TestRPC_ServiceErrors(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	env.cfg.SessionPrefix = "rpc-" + env.cfg.SessionPrefix
	ctx := context.Background()
	spawnFake(t, env, "worker")
	client := serveService(t, env)

	_, err := client.Status(ctx, natsbus.AgentRef{Name: "ghost", Host: "h1"})
	if err == nil || !strings.HasPrefix(err.Error(), `agent "ghost" not found (1 agent there`) {
		t.Errorf("status of unknown agent: err = %v", err)
	}
	err = client.Respond(ctx, natsbus.AgentRef{Name: "worker", Host: "h1"}, natsbus.RespondRequest{Response: "yes-please", Force: true})
	if err == nil || !strings.Contains(err.Error(), `invalid permission response "yes-please"`) {
		t.Errorf("unknown response: err = %v", err)
	}
}

func TestPermissionResponseNames(t *testing.T) {
	for _, resp := range []backend.PermissionResponse{backend.PermissionApprove, backend.PermissionApproveAlways, backend.PermissionDeny} {
		name, err := service.PermissionResponseName(resp)
		if err != nil {
			t.Fatalf("PermissionResponseName(%v): %v", resp, err)
		}
		if back, err := service.ParsePermissionResponse(name); err != nil || back != resp {
			t.Errorf("round trip %v -> %q -> %v (%v)", resp, name, back, err)
		}
	}
	if _, err := service.ParsePermissionResponse(""); err == nil {
		t.Error("empty response should be rejected")
	}
}
