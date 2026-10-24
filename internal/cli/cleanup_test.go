package cli

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
)

// addTeam adds agents with no tmux sessions, each name mapped to its parent.
func addTeam(t *testing.T, parents map[string]string) {
	t.Helper()
	for name, parent := range parents {
		if err := store.CreateAgent(context.Background(), &storage.Agent{
			Name: name, SessionName: "rpc-cli-" + name, Backend: "claude", SpawnedBy: parent, CreatedAt: time.Now(),
		}); err != nil {
			t.Fatal(err)
		}
	}
}

// listed reports which of names still have records.
func listed(t *testing.T, names ...string) []string {
	t.Helper()
	var out []string
	for _, name := range names {
		if a, _ := store.GetAgent(context.Background(), name); a != nil {
			out = append(out, name)
		}
	}
	return out
}

func TestCleanup_AgentsCleanUpOnlyTheirHelpers(t *testing.T) {
	setupCLI(t)
	addTeam(t, map[string]string{
		"lead":     "",
		"lead-a":   "lead",
		"lead-a-x": "lead-a",
		"lead-a-y": "lead-a-x",
		"lead-b":   "lead",
		"other":    "",
		"other-h":  "other",
	})
	t.Setenv("AGENT_NAME", "lead")

	tests := []struct {
		args    []string
		want    []string // lines, in order
		removed []string
	}{
		{
			args: []string{"other", "worker"},
			want: []string{
				`refused "other": it was spawned by a person, not by you ("lead") or your helpers`,
				`refused "worker": it was spawned by a person`,
			},
		},
		{
			args: []string{"lead"},
			want: []string{`refused "lead": an agent can't clean itself up`},
		},
		{
			args: []string{"other-h"},
			want: []string{`refused "other-h": it was spawned by "other", not by you ("lead") or your helpers`},
		},
		{
			// It would leave lead-a-x where only a person could reach it.
			args: []string{"lead-a"},
			want: []string{`refused "lead-a": its helpers lead-a-x would be left behind, and only a person could clean them up then; clean them up with it (agent cleanup lead-a lead-a-x)`},
		},
		{
			// A refused helper counts as left behind too.
			args: []string{"lead-a", "lead-a-x"},
			want: []string{
				`refused "lead-a-x": its helpers lead-a-y would be left behind`,
				`refused "lead-a": its helpers lead-a-x would be left behind`,
			},
		},
		{
			// Helpers first, whatever order they're named in.
			args: []string{"lead-a", "lead-a-y", "ghost", "lead-a-x"},
			want: []string{
				`cleaned up "lead-a-y": removed its record, state, queued messages, and files; its log stays (agent logs lead-a-y)`,
				`cleaned up "lead-a-x": removed`,
				`cleaned up "lead-a": removed`,
				`failed "ghost": agent "ghost" not found on host local (5 agents there: agent list)`,
			},
			removed: []string{"lead-a", "lead-a-x", "lead-a-y"},
		},
	}
	for _, tt := range tests {
		out, err := stdout(t, func() error { return runCleanup(cleanupCmd, tt.args) })
		lines := strings.Split(strings.TrimSpace(out), "\n")
		ok := len(lines) == len(tt.want)
		for i := 0; ok && i < len(lines); i++ {
			ok = strings.HasPrefix(lines[i], tt.want[i])
		}
		if !ok {
			t.Errorf("cleanup %v printed:\n%s\nwant lines starting:\n%s", tt.args, out, strings.Join(tt.want, "\n"))
		}
		if err == nil {
			t.Errorf("cleanup %v: no error, want one for the refusals", tt.args)
		}
		if gone := listed(t, tt.removed...); len(gone) > 0 {
			t.Errorf("cleanup %v left %v", tt.args, gone)
		}
	}
	if got := listed(t, "lead", "lead-b", "other", "other-h", "worker"); len(got) != 5 {
		t.Errorf("still listed: %v", got)
	}
}

// A coordinator that wasn't spawned through agent still owns its helpers.
func TestCleanup_UnlistedCallerOwnsItsHelpers(t *testing.T) {
	setupCLI(t)
	addTeam(t, map[string]string{"coord-a": "coord"})
	t.Setenv("AGENT_NAME", "coord")
	out, err := stdout(t, func() error { return runCleanup(cleanupCmd, []string{"coord-a"}) })
	if err != nil || !strings.HasPrefix(out, `cleaned up "coord-a"`) {
		t.Errorf("cleanup = %q, %v", out, err)
	}
}

// A person can clean up anything, and hears which helpers remain.
func TestCleanup_PersonGetsANoteOnRemainingHelpers(t *testing.T) {
	setupCLI(t)
	addTeam(t, map[string]string{"worker-a": "worker", "worker-b": "worker"})
	t.Setenv("AGENT_NAME", "")
	out, err := stdout(t, func() error { return runCleanup(cleanupCmd, []string{"worker", "worker-b"}) })
	if err != nil {
		t.Fatalf("cleanup: %v\n%s", err, out)
	}
	want := `cleaned up "worker-b": removed its record, state, queued messages, and files; its log stays (agent logs worker-b)
cleaned up "worker": removed its record, state, queued messages, and files; its log stays (agent logs worker)
  note: its helpers worker-a remain (agent cleanup worker-a)
`
	if out != want {
		t.Errorf("printed:\n%s\nwant:\n%s", out, want)
	}
	if got := listed(t, "worker", "worker-a", "worker-b"); len(got) != 1 || got[0] != "worker-a" {
		t.Errorf("still listed: %v", got)
	}
}
