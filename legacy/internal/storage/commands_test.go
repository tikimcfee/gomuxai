package storage

import (
	"context"
	"path/filepath"
	"slices"
	"testing"
	"time"
)

func TestCommandLog(t *testing.T) {
	s, err := NewSQLiteStorage(filepath.Join(t.TempDir(), "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	ctx := context.Background()
	if err := s.CreateAgent(ctx, &Agent{Name: "lead-review", SessionName: "agent-lead-review", Backend: "claude", SpawnedBy: "lead", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}

	start := time.Date(2026, 10, 24, 14, 0, 0, 0, time.UTC)
	logged := []*CommandRecord{
		{StartedAt: start, Agent: "", Cwd: "/w", Args: []string{"list"}},
		{StartedAt: start.Add(time.Minute), Agent: "lead-review", Cwd: "/w", Args: []string{"wait", "x", "--for", "idel"}, ExitCode: 1, Error: `invalid status "idel"`},
		{StartedAt: start.Add(2 * time.Minute), Agent: "lead", Cwd: "/w", Args: []string{"send", "lead-review", "hi"}},
		{StartedAt: start.Add(3 * time.Minute), Agent: "", Cwd: "/v", Args: nil, ExitCode: 2, Error: "timed out"},
	}
	for _, rec := range logged {
		rec.Host, rec.Build = "h", "c7253f9"
		if err := s.LogCommand(ctx, rec); err != nil {
			t.Fatal(err)
		}
	}
	if got := logged[1].Parent; got != "lead" {
		t.Errorf("a spawned agent's command: Parent = %q, want %q", got, "lead")
	}
	if got := logged[2].Parent; got != "" {
		t.Errorf("an unlisted agent's command: Parent = %q, want none", got)
	}

	ids := func(f CommandFilter) []int64 {
		t.Helper()
		recs, err := s.ListCommands(ctx, f)
		if err != nil {
			t.Fatal(err)
		}
		var out []int64
		for _, r := range recs {
			out = append(out, r.ID)
		}
		return out
	}
	tests := []struct {
		name string
		f    CommandFilter
		want []int64
	}{
		{"all, newest first", CommandFilter{}, []int64{4, 3, 2, 1}},
		{"limit", CommandFilter{Limit: 2}, []int64{4, 3}},
		{"an agent's", CommandFilter{ByAgent: true, Agent: "lead-review"}, []int64{2}},
		{"a person's", CommandFilter{ByAgent: true}, []int64{4, 1}},
		{"a person's in a directory", CommandFilter{ByAgent: true, Cwd: "/w"}, []int64{1}},
		{"failed", CommandFilter{Failed: true}, []int64{4, 2}},
		{"since", CommandFilter{Since: start.Add(90 * time.Second)}, []int64{4, 3}},
	}
	for _, tt := range tests {
		if got := ids(tt.f); !slices.Equal(got, tt.want) {
			t.Errorf("%s: ids = %v, want %v", tt.name, got, tt.want)
		}
	}

	recs, err := s.ListCommands(ctx, CommandFilter{Limit: 3})
	if err != nil {
		t.Fatal(err)
	}
	got, want := recs[2], logged[1]
	if !got.StartedAt.Equal(want.StartedAt) || !slices.Equal(got.Args, want.Args) || got.ExitCode != 1 ||
		got.Error != want.Error || got.Agent != "lead-review" || got.Parent != "lead" || got.Build != "c7253f9" {
		t.Errorf("read back %+v, want %+v", got, want)
	}
	if recs[0].Args == nil || len(recs[0].Args) != 0 {
		t.Errorf("no args read back as %#v, want an empty list", recs[0].Args)
	}
}

func TestCommandLog_Prunes(t *testing.T) {
	oldKeep, oldEvery := commandLogKeep, commandLogPruneEvery
	t.Cleanup(func() { commandLogKeep, commandLogPruneEvery = oldKeep, oldEvery })
	commandLogKeep, commandLogPruneEvery = 10, 4

	s, err := NewSQLiteStorage(filepath.Join(t.TempDir(), "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	ctx := context.Background()
	for range 25 {
		if err := s.LogCommand(ctx, &CommandRecord{StartedAt: time.Now(), Args: []string{"list"}}); err != nil {
			t.Fatal(err)
		}
	}
	recs, err := s.ListCommands(ctx, CommandFilter{})
	if err != nil {
		t.Fatal(err)
	}
	// The last prune ran at id 24 and kept ids 15 to 24; 25 came after.
	if len(recs) != 11 || recs[len(recs)-1].ID != 15 {
		t.Errorf("kept %d records, oldest id %d; want 11, from 15", len(recs), recs[len(recs)-1].ID)
	}
}
