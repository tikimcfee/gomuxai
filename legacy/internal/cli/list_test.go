package cli

import (
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
	created := time.Date(2026, 10, 1, 9, 31, 0, 0, time.UTC)
	createLiveAgent(t, &storage.Agent{Name: "worker-test", SessionName: "rpc-cli-worker-test", Backend: "claude", SpawnedBy: "worker", CreatedAt: created})

	out, err := stdout(t, func() error { return runList(listCmd, nil) })
	if err != nil {
		t.Fatal(err)
	}
	want := strings.Join([]string{
		"worker: waiting (claude)",
		"  session: rpc-cli-worker",
		"  created: 2026-10-01 09:30:00",
		"  └ worker-test: unknown (claude)",
		"      parent:  worker",
		"      session: rpc-cli-worker-test",
		"      created: 2026-10-01 09:31:00",
	}, "\n") + "\n"
	if out != want {
		t.Errorf("list =\n%s\nwant:\n%s", out, want)
	}
}
