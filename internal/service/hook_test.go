package service

import "testing"

// The process shapes the global hook meets, as seen on Linux with
// Claude Code 2.1.316: hook -> CLI -> (wrappers) -> pane shell.
func TestPlaceHook(t *testing.T) {
	clis := map[int]bool{}
	isCLI := func(pid int) (bool, error) { return clis[pid], nil }
	// moved maps a CLI to the CLI whose session it carries on (claudereg.Continues).
	moved := map[int]int{}
	otherCLI := func(cli, pid int) (bool, error) { return clis[pid] && moved[cli] != pid, nil }
	paneOf := map[int]string{100: "agent-worker", 200: "agent-other", 300: "someone-elses"}
	agentOf := map[string]string{"agent-worker": "worker", "agent-other": "other"}
	panes := func() (map[int]string, map[string]string, error) { return paneOf, agentOf, nil }

	for _, tc := range []struct {
		name  string
		chain []int // the hook's ancestors, nearest first
		cli   []int
		moved map[int]int
		want  HookSession
	}{
		{"an agent's own CLI", []int{102, 101, 100, 50}, []int{102}, nil, HookSession{CLIPID: 102, Owner: "worker"}},
		{"through wrappers (sh -c, env)", []int{105, 104, 103, 100}, []int{105}, nil, HookSession{CLIPID: 105, Owner: "worker"}},
		// A `claude -p` a tool ran inside the agent's session: the agent's
		// CLI sits between it and the pane.
		{"a CLI started from inside the session", []int{110, 109, 102, 101, 100}, []int{110, 102}, nil, HookSession{CLIPID: 110}},
		// Claude Code moved the agent's conversation into a background
		// session (Claude Code 2.1.319): bg CLI -> bg-pty-host -> daemon ->
		// the agent's CLI -> pane. The agent goes on in it.
		{"the agent's session moved to a background CLI", []int{160, 159, 158, 102, 101, 100}, []int{160, 102}, map[int]int{160: 102}, HookSession{CLIPID: 160, Owner: "worker"}},
		// A background job the agent's CLI started, not its own session.
		{"a background CLI that isn't the agent's session", []int{170, 159, 158, 102, 101, 100}, []int{170, 102}, nil, HookSession{CLIPID: 170}},
		// cleanup killed the pane: the dying CLI's SessionEnd runs with its
		// parent gone (reparented to init or a subreaper), so it reaches no
		// pane and can't land on a respawn of the same name.
		{"a CLI whose pane is gone", []int{120, 1500}, []int{120}, nil, HookSession{CLIPID: 120}},
		{"a person's terminal", []int{130, 129, 50}, []int{130}, nil, HookSession{CLIPID: 130}},
		{"a pane no agent records", []int{140, 300}, []int{140}, nil, HookSession{CLIPID: 140}},
		{"no CLI above the hook", []int{150, 100}, nil, nil, HookSession{}},
	} {
		clear(clis)
		for _, p := range tc.cli {
			clis[p] = true
		}
		clear(moved)
		for k, v := range tc.moved {
			moved[k] = v
		}
		got, err := placeHook(tc.chain, isCLI, otherCLI, panes)
		if err != nil || got != tc.want {
			t.Errorf("%s: got %+v, %v; want %+v", tc.name, got, err, tc.want)
		}
	}
}

func TestSpawnHooked(t *testing.T) {
	old := []string{"claude", "--session-id", "x", "--settings", `{"hooks":{"Stop":[{"hooks":[{"command":"'/b/agent' report --hook --as 'w' --spawn-id 'x'"}]}]}}`}
	if !spawnHooked(old) {
		t.Error("a CLI spawned with per-spawn hooks")
	}
	if spawnHooked([]string{"claude", "--session-id", "x"}) || spawnHooked([]string{"claude", "--settings", "/tmp/s.json"}) {
		t.Error("a CLI without them")
	}
}

// When the registry can't prove a CLI (no start time recorded), the nesting
// check still counts it: a `claude -p` inside an agent's session stays
// unowned, while the agent's own CLI, holding the payload's session, is
// still found and placed.
func TestPlaceHookWithoutProof(t *testing.T) {
	paneOf := map[int]string{100: "agent-worker"}
	agentOf := map[string]string{"agent-worker": "worker"}
	panes := func() (map[int]string, map[string]string, error) { return paneOf, agentOf, nil }
	registered := map[int]bool{102: true, 110: true} // both unproven
	other := func(_, pid int) (bool, error) { return registered[pid], nil }

	// The nested CLI's hook: its own CLI is 110; 102 (the agent's) is above.
	own := func(pid int) (bool, error) { return pid == 110, nil }
	if got, _ := placeHook([]int{110, 109, 102, 101, 100}, own, other, panes); got != (HookSession{CLIPID: 110}) {
		t.Errorf("nested: %+v", got)
	}
	// The agent's own CLI's hook.
	own = func(pid int) (bool, error) { return pid == 102, nil }
	if got, _ := placeHook([]int{102, 101, 100}, own, other, panes); got != (HookSession{CLIPID: 102, Owner: "worker"}) {
		t.Errorf("own: %+v", got)
	}
}

func TestSpawnHookedPSWords(t *testing.T) {
	// macOS ps -o args= split on spaces.
	words := []string{"claude", "--session-id", "x", "--settings", `{"hooks":{"Stop":[{"hooks":[{"command":"'/b/agent'`, "report", "--hook", "--as", "'w'"}
	if !spawnHooked(words) {
		t.Error("split settings JSON")
	}
}
