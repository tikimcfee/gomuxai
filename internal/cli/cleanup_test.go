package cli

import (
	"context"
	"fmt"
	"os"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/service"
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
cleaned up "worker": killed session rpc-cli-worker; removed its record, state, queued messages, and files; its log stays (agent logs worker)
  note: its helpers worker-a remain (agent cleanup worker-a)
`
	if out != want {
		t.Errorf("printed:\n%s\nwant:\n%s", out, want)
	}
	if got := listed(t, "worker", "worker-a", "worker-b"); len(got) != 1 || got[0] != "worker-a" {
		t.Errorf("still listed: %v", got)
	}
}

// addRecord adds an agent record, with a running tmux session (a shell) if
// live. An agent with no session resolves to ended (gone); a live aider
// agent, read off its screen, is running.
func addRecord(t *testing.T, a storage.Agent, live bool) {
	t.Helper()
	if a.SessionName == "" {
		a.SessionName = "rpc-cli-" + a.Name
	}
	if a.Backend == "" {
		a.Backend = "claude"
	}
	a.CreatedAt = time.Now()
	if live {
		if err := tmuxClient.CreateSession(a.SessionName, nil); err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { tmuxClient.KillSession(a.SessionName) })
	}
	if err := store.CreateAgent(context.Background(), &a); err != nil {
		t.Fatal(err)
	}
}

// holdMessages holds n messages for name, as send does for a busy agent.
func holdMessages(t *testing.T, name string, n int) {
	t.Helper()
	for i := range n {
		if err := store.CreateMessage(context.Background(), &storage.Message{
			SenderID: "someone", ReceiverID: name, Content: fmt.Sprintf("message %d", i), Status: storage.MessageStatusPending, CreatedAt: time.Now(),
		}); err != nil {
			t.Fatal(err)
		}
	}
}

// heldFor counts the messages held for name.
func heldFor(t *testing.T, name string) int {
	t.Helper()
	held, err := store.GetPendingMessages(context.Background(), name, 100)
	if err != nil {
		t.Fatal(err)
	}
	return len(held)
}

// checkLines fails unless out's lines start, in order, with want's.
func checkLines(t *testing.T, what, out string, want []string) {
	t.Helper()
	lines := strings.Split(strings.TrimRight(out, "\n"), "\n")
	ok := len(lines) == len(want)
	for i := 0; ok && i < len(lines); i++ {
		ok = strings.HasPrefix(lines[i], want[i])
	}
	if !ok {
		t.Errorf("%s printed:\n%s\nwant lines starting:\n%s", what, out, strings.Join(want, "\n"))
	}
}

// A person's cleanup --ended: ended agents go (helpers first; an adopted
// one's session stays running), running ones and an ended agent's running
// helper stay, and --dry-run before it lists the same and changes nothing.
func TestCleanupEnded_Person(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("TMUX_PANE", "")
	ctx := context.Background()

	addRecord(t, storage.Agent{Name: "gone"}, false)
	addRecord(t, storage.Agent{Name: "gone-h", SpawnedBy: "gone"}, false)
	addRecord(t, storage.Agent{Name: "lead"}, false)
	addRecord(t, storage.Agent{Name: "lead-run", SpawnedBy: "lead", Backend: "aider"}, true)
	addRecord(t, storage.Agent{Name: "runner", Backend: "aider"}, true)
	userSess := fmt.Sprintf("rpc-cli-user-ended-%d", os.Getpid())
	addRecord(t, storage.Agent{Name: "adopted", SessionName: userSess, Adopted: true}, true)
	if err := agentService.Report(ctx, "adopted", service.StatusReport{Status: service.AgentStatusEnded, EndReason: "quit", Detail: "prompt_input_exit"}); err != nil {
		t.Fatal(err)
	}
	holdMessages(t, "gone", 2)

	// Helpers first, then by name.
	want := []string{
		`would clean up "gone-h": ended (gone): its tmux session rpc-cli-gone-h is gone (a reboot, or the tmux server was killed); agent resume gone-h continues its session`,
		`  its session rpc-cli-gone-h is already gone; no held messages`,
		`would clean up "adopted": ended (`,
		`  adopted: its session ` + userSess + ` stays running, and only the record goes; no held messages`,
		`would clean up "gone": ended (gone): its tmux session rpc-cli-gone is gone`,
		`  its session rpc-cli-gone is already gone; deletes 2 messages held for it`,
		`would clean up "lead": ended (gone)`,
		`  its session rpc-cli-lead is already gone; no held messages`,
		`  note: its helpers lead-run haven't ended, so they stay (agent cleanup lead-run)`,
		`Would clean up 4 ended agents; nothing changed (dry run).`,
	}
	out, err := stdout(t, func() error { return runCleanupEnded(true) })
	if err != nil {
		t.Fatalf("dry run: %v\n%s", err, out)
	}
	checkLines(t, "cleanup --ended --dry-run", out, want)
	all := []string{"gone", "gone-h", "lead", "lead-run", "runner", "adopted"}
	if got := listed(t, all...); len(got) != len(all) {
		t.Errorf("dry run removed agents: still listed %v", got)
	}
	if n := heldFor(t, "gone"); n != 2 {
		t.Errorf("dry run left %d held messages, want 2", n)
	}

	out, err = stdout(t, func() error { return runCleanupEnded(false) })
	if err != nil {
		t.Fatalf("cleanup --ended: %v\n%s", err, out)
	}
	checkLines(t, "cleanup --ended", out, []string{
		`cleaned up "gone-h": removed its record, state, queued messages, and files; its log stays (agent logs gone-h)`,
		`cleaned up "adopted": left session ` + userSess + ` running; removed`,
		`cleaned up "gone": removed`,
		`cleaned up "lead": removed`,
		`  note: its helpers lead-run haven't ended, so they stay (agent cleanup lead-run)`,
		`Cleaned up 4 ended agents.`,
	})
	if got := listed(t, all...); !slices.Equal(got, []string{"lead-run", "runner"}) {
		t.Errorf("still listed: %v, want lead-run and runner", got)
	}
	if n := heldFor(t, "gone"); n != 0 {
		t.Errorf("%d held messages left for gone, want them deleted with it", n)
	}
	for _, s := range []string{userSess, "rpc-cli-lead-run", "rpc-cli-runner"} {
		if exists, _ := tmuxClient.SessionExists(s); !exists {
			t.Errorf("session %s was killed", s)
		}
	}

	// Nothing ended is left.
	out, err = stdout(t, func() error { return runCleanupEnded(false) })
	if err != nil || out != "No ended agents to clean up here.\n" {
		t.Errorf("second cleanup --ended = %q, %v", out, err)
	}
}

// Inside an agent, cleanup --ended picks only the caller's own ended helpers
// and theirs, never the caller itself, and skips an ended helper whose own
// helpers are still running, since nothing would tie them to the caller once
// it's gone.
func TestCleanupEnded_InsideAgent(t *testing.T) {
	setupCLI(t)
	t.Setenv("TMUX_PANE", "")
	t.Setenv("AGENT_NAME", "boss")

	addRecord(t, storage.Agent{Name: "boss"}, false) // ended itself, say on a stale record
	addRecord(t, storage.Agent{Name: "boss-a", SpawnedBy: "boss"}, false)
	addRecord(t, storage.Agent{Name: "boss-a-x", SpawnedBy: "boss-a"}, false)
	addRecord(t, storage.Agent{Name: "boss-b", SpawnedBy: "boss"}, false)
	addRecord(t, storage.Agent{Name: "boss-b-r", SpawnedBy: "boss-b", Backend: "aider"}, true)
	addRecord(t, storage.Agent{Name: "boss-c", SpawnedBy: "boss", Backend: "aider"}, true)
	addRecord(t, storage.Agent{Name: "stranger"}, false)
	addRecord(t, storage.Agent{Name: "stranger-h", SpawnedBy: "stranger"}, false)

	out, err := stdout(t, func() error { return runCleanupEnded(true) })
	if err != nil {
		t.Fatalf("dry run: %v\n%s", err, out)
	}
	skips := []string{
		`skipped "boss-b" (ended): its helpers boss-b-r haven't ended, and only a person could clean them up once it's gone`,
		`skipped "boss" (ended): that's you, and an agent can't clean itself up`,
	}
	checkLines(t, "cleanup --ended --dry-run inside boss", out, append(skips,
		`would clean up "boss-a-x": ended (gone)`,
		`  its session rpc-cli-boss-a-x is already gone; no held messages`,
		`would clean up "boss-a": ended (gone)`,
		`  its session rpc-cli-boss-a is already gone; no held messages`,
		`Would clean up 2 ended agents; nothing changed (dry run). Skipped 2. Left 2 more ended agents here that others spawned, not you ("boss") or your helpers.`,
	))

	out, err = stdout(t, func() error { return runCleanupEnded(false) })
	if err != nil {
		t.Fatalf("cleanup --ended: %v\n%s", err, out)
	}
	checkLines(t, "cleanup --ended inside boss", out, append(skips,
		`cleaned up "boss-a-x": removed`,
		`cleaned up "boss-a": removed`,
		`Cleaned up 2 ended agents. Skipped 2. Left 2 more ended agents here that others spawned, not you ("boss") or your helpers.`,
	))
	if got := listed(t, "boss", "boss-a", "boss-a-x", "boss-b", "boss-b-r", "boss-c", "stranger", "stranger-h"); !slices.Equal(got, []string{"boss", "boss-b", "boss-b-r", "boss-c", "stranger", "stranger-h"}) {
		t.Errorf("still listed: %v", got)
	}
}

// --ended takes no names, and --dry-run goes only with --ended.
func TestCleanupEnded_Args(t *testing.T) {
	t.Cleanup(func() { cleanupEnded, cleanupDryRun = false, false })
	for _, tt := range []struct {
		ended, dry bool
		args       []string
		err        string
	}{
		{true, false, []string{"x"}, "--ended picks the agents itself"},
		{false, true, []string{"x"}, "--dry-run goes with --ended"},
		{false, false, nil, "requires at least 1 arg"},
		{true, true, nil, ""},
		{false, false, []string{"x"}, ""},
	} {
		cleanupEnded, cleanupDryRun = tt.ended, tt.dry
		err := cleanupCmd.Args(cleanupCmd, tt.args)
		if (err == nil) != (tt.err == "") || (err != nil && !strings.Contains(err.Error(), tt.err)) {
			t.Errorf("--ended=%v --dry-run=%v %v: err = %v, want %q", tt.ended, tt.dry, tt.args, err, tt.err)
		}
	}
}
