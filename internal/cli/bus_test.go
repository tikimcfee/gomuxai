package cli

import (
	"strings"
	"testing"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// The bus session's name is the same for every data dir, so stop and start
// must leave a session alone when this data dir's bus isn't the one in it.
func TestBusStopLeavesAnotherDataDirsBus(t *testing.T) {
	setupCLI(t)
	old := busSession
	busSession = "rpc-cli-gomux-bus"
	t.Cleanup(func() { busSession = old })
	if err := tmuxClient.CreateSession(busSession, nil); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { tmuxClient.KillSession(busSession) })
	busConnect = func() (*nats.Conn, error) { return nil, natsbus.ErrNoBus }

	for name, run := range map[string]func() error{
		"stop":  func() error { return stopBus(busStopCmd, nil) },
		"start": func() error { return startBus(busStartCmd, nil) },
	} {
		_, err := stdout(t, run)
		if err == nil || !strings.Contains(err.Error(), "another agent binary") {
			t.Errorf("bus %s: err = %v, want a refusal naming another binary's bus", name, err)
		}
		if exists, _ := tmuxClient.SessionExists(busSession); !exists {
			t.Fatalf("bus %s killed the session", name)
		}
	}
}
