package cli

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

func TestTeamOrder(t *testing.T) {
	agents := []natsbus.AgentInfo{
		{Name: "lead-review", SpawnedBy: "lead"},
		{Name: "solo"},
		{Name: "lead-build-test", SpawnedBy: "lead-build"},
		{Name: "lead"},
		{Name: "lead-build", SpawnedBy: "lead"},
		{Name: "stray", SpawnedBy: "gone"}, // its parent was cleaned up
		{Name: "x", SpawnedBy: "y"},        // a loop, from reused names
		{Name: "y", SpawnedBy: "x"},
	}
	var got []string
	for _, a := range teamOrder(agents) {
		got = append(got, a.Name)
	}
	want := "solo lead lead-review lead-build lead-build-test stray x y"
	if strings.Join(got, " ") != want {
		t.Errorf("teamOrder = %v, want %s", got, want)
	}
}

func TestListShowsParents(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	ctx := context.Background()
	created := time.Date(2026, 10, 1, 9, 31, 0, 0, time.UTC)
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "worker-test", SessionName: "rpc-cli-worker-test", Backend: "claude", SpawnedBy: "worker", CreatedAt: created}); err != nil {
		t.Fatal(err)
	}

	out, err := stdout(t, func() error { return runList(listCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(out), "\n")
	if len(lines) != 4 || !strings.Contains(lines[0], "PARENT") {
		t.Fatalf("list:\n%s", out)
	}
	// The newer helper still follows its parent.
	if fields := strings.Fields(lines[2]); fields[0] != "worker" || fields[1] != "-" {
		t.Errorf("parent row = %q", lines[2])
	}
	if fields := strings.Fields(lines[3]); fields[0] != "worker-test" || fields[1] != "worker" {
		t.Errorf("helper row = %q", lines[3])
	}
}
