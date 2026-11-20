package storage

import (
	"context"
	"path/filepath"
	"slices"
	"testing"
	"time"
)

func TestFriction(t *testing.T) {
	s, err := NewSQLiteStorage(filepath.Join(t.TempDir(), "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	ctx := context.Background()

	if last, err := s.LastFrictionID(ctx); err != nil || last != 0 {
		t.Fatalf("LastFrictionID on an empty box = %d, %v; want 0", last, err)
	}
	if f, err := s.GetFriction(ctx, 1); err != nil || f != nil {
		t.Fatalf("GetFriction(1) on an empty box = %v, %v; want nil", f, err)
	}

	now := time.Date(2026, 10, 24, 14, 0, 0, 0, time.UTC)
	file := func(uid, by, kind, summary string) *Friction {
		t.Helper()
		f := &Friction{UID: uid, Host: "h", FiledBy: by, Build: "c7253f9", CreatedAt: now, UpdatedAt: now,
			Kind: kind, Summary: summary, Status: FrictionOpen}
		if err := s.AddFriction(ctx, f); err != nil {
			t.Fatal(err)
		}
		now = now.Add(time.Minute)
		return f
	}
	vote := func(id int64, uid string) {
		t.Helper()
		if err := s.AddFrictionVote(ctx, &FrictionVote{FrictionID: id, UID: uid, Host: "h", Voter: "x", CreatedAt: now}); err != nil {
			t.Fatal(err)
		}
	}
	a := file("h-a", "lead", "help", "a")
	b := file("h-b", "user", "error", "b")
	c := file("h-c", "lead", "error", "c")
	d := file("h-d", "lead", "bug", "d")
	vote(b.ID, "v1")
	vote(c.ID, "v2")
	vote(c.ID, "v3")
	vote(d.ID, "v4")

	if err := s.AddFriction(ctx, &Friction{UID: "h-a", CreatedAt: now, UpdatedAt: now, Status: FrictionOpen}); err == nil {
		t.Error("filed a second entry with the same uid")
	}

	ids := func(f FrictionFilter) []int64 {
		t.Helper()
		entries, err := s.ListFriction(ctx, f)
		if err != nil {
			t.Fatal(err)
		}
		var out []int64
		for _, e := range entries {
			out = append(out, e.ID)
		}
		return out
	}
	votes := func(id int64) int {
		t.Helper()
		f, err := s.GetFriction(ctx, id)
		if err != nil {
			t.Fatal(err)
		}
		return f.Votes
	}

	if got, want := ids(FrictionFilter{}), []int64{c.ID, d.ID, b.ID, a.ID}; !slices.Equal(got, want) {
		t.Errorf("most votes, then newest: %v, want %v", got, want)
	}
	if got, want := ids(FrictionFilter{Kind: "error", FiledBy: "lead"}), []int64{c.ID}; !slices.Equal(got, want) {
		t.Errorf("by kind and filer: %v, want %v", got, want)
	}

	// d is a duplicate of c, then c of b: b's count takes in both, and d
	// moves onto b.
	resolveDup := func(f *Friction, of int64) {
		t.Helper()
		f.Status, f.DuplicateOf, f.ResolvedBy, f.ResolvedAt, f.UpdatedAt = FrictionDuplicate, of, "user", now, now
		if err := s.UpdateFriction(ctx, f); err != nil {
			t.Fatal(err)
		}
	}
	resolveDup(d, c.ID)
	if got := votes(c.ID); got != 4 { // its 2, d's 1, and d itself
		t.Errorf("after d dup of c: c has %d votes, want 4", got)
	}
	resolveDup(c, b.ID)
	if got := votes(b.ID); got != 6 { // its 1, c's 2 + c, d's 1 + d
		t.Errorf("after c dup of b: b has %d votes, want 6", got)
	}
	if got, want := ids(FrictionFilter{DuplicateOf: b.ID}), []int64{c.ID, d.ID}; !slices.Equal(got, want) {
		t.Errorf("duplicates of b: %v, want %v", got, want)
	}
	if got, want := ids(FrictionFilter{Statuses: []string{FrictionOpen, FrictionAccepted}}), []int64{b.ID, a.ID}; !slices.Equal(got, want) {
		t.Errorf("open: %v, want %v", got, want)
	}

	a.Status, a.AcceptedBy, a.AcceptedAt, a.AcceptNote = FrictionAccepted, "lead", now, "soon"
	// Updates leave the filing alone.
	a.Command = &AttachedCommand{Args: []string{"wait", "x"}, ExitCode: 2, Error: "timed out", At: now}
	if err := s.UpdateFriction(ctx, a); err != nil {
		t.Fatal(err)
	}
	got, err := s.GetFriction(ctx, a.ID)
	if err != nil {
		t.Fatal(err)
	}
	if got.Status != FrictionAccepted || got.AcceptedBy != "lead" || !got.AcceptedAt.Equal(now) || got.AcceptNote != "soon" ||
		!got.ResolvedAt.IsZero() || got.DuplicateOf != 0 || got.Command != nil {
		t.Errorf("accepted entry read back as %+v", got)
	}

	vs, err := s.ListFrictionVotes(ctx, c.ID)
	if err != nil || len(vs) != 2 || vs[0].UID != "v2" {
		t.Errorf("c's votes = %v, %v; want v2, v3", vs, err)
	}
	if last, _ := s.LastFrictionID(ctx); last != d.ID {
		t.Errorf("LastFrictionID = %d, want %d", last, d.ID)
	}
}

func TestFriction_AttachedCommandRoundTrips(t *testing.T) {
	s, err := NewSQLiteStorage(filepath.Join(t.TempDir(), "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	ctx := context.Background()
	at := time.Date(2026, 10, 24, 14, 0, 0, 0, time.UTC)
	cmd := &AttachedCommand{Args: []string{"send", "x", "hi there"}, ExitCode: 1, Error: `agent "x" not found`, At: at}
	f := &Friction{UID: "u", CreatedAt: at, UpdatedAt: at, Summary: "s", Status: FrictionOpen, Command: cmd}
	if err := s.AddFriction(ctx, f); err != nil {
		t.Fatal(err)
	}
	if err := s.AddFrictionVote(ctx, &FrictionVote{FrictionID: f.ID, UID: "v", CreatedAt: at, Command: cmd}); err != nil {
		t.Fatal(err)
	}
	got, _ := s.GetFriction(ctx, f.ID)
	vs, _ := s.ListFrictionVotes(ctx, f.ID)
	for _, c := range []*AttachedCommand{got.Command, vs[0].Command} {
		if c == nil || !slices.Equal(c.Args, cmd.Args) || c.ExitCode != 1 || c.Error != cmd.Error || !c.At.Equal(at) {
			t.Errorf("attached command read back as %+v, want %+v", c, cmd)
		}
	}
}
