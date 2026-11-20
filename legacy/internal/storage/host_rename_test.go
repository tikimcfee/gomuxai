package storage

import (
	"context"
	"path/filepath"
	"testing"
	"time"
)

// A peer renamed old -> new: agents it spawned here and links to its
// friction entries follow it; other hosts' references stay.
func TestRenameHost(t *testing.T) {
	s, err := NewSQLiteStorage(filepath.Join(t.TempDir(), "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	ctx := context.Background()
	now := time.Date(2026, 11, 8, 10, 0, 0, 0, time.UTC)

	for _, a := range []*Agent{
		{Name: "h1", SessionName: "agent-h1", Backend: "claude", SpawnedBy: "lead@Old-Mac", CreatedAt: now},
		{Name: "h2", SessionName: "agent-h2", Backend: "claude", SpawnedBy: "lead@old-macro", CreatedAt: now}, // another host
		{Name: "h3", SessionName: "agent-h3", Backend: "claude", SpawnedBy: "h1", CreatedAt: now},             // local parent
	} {
		if err := s.CreateAgent(ctx, a); err != nil {
			t.Fatal(err)
		}
	}
	f := &Friction{UID: "u1", Host: "h", FiledBy: "x", CreatedAt: now, UpdatedAt: now, Kind: "bug", Summary: "s", Status: FrictionOpen}
	g := &Friction{UID: "u2", Host: "h", FiledBy: "x", CreatedAt: now, UpdatedAt: now, Kind: "bug", Summary: "s", Status: FrictionOpen}
	for _, e := range []*Friction{f, g} {
		if err := s.AddFriction(ctx, e); err != nil {
			t.Fatal(err)
		}
	}
	for _, l := range []*FrictionLink{
		{FrictionID: f.ID, PeerHost: "old-mac", PeerID: 34, LinkedBy: "x", LinkedAt: now},
		{FrictionID: f.ID, PeerHost: "hub", PeerID: 2, LinkedBy: "x", LinkedAt: now},
		// g links to #7 under both names: the old one goes.
		{FrictionID: g.ID, PeerHost: "old-mac", PeerID: 7, LinkedBy: "x", LinkedAt: now},
		{FrictionID: g.ID, PeerHost: "new-mac", PeerID: 7, LinkedBy: "x", LinkedAt: now},
	} {
		if err := s.AddFrictionLink(ctx, l); err != nil {
			t.Fatal(err)
		}
	}

	n, err := s.RenameHost(ctx, "OLD-MAC", "new-mac")
	if err != nil {
		t.Fatal(err)
	}
	if n.Agents != 1 || n.FrictionLinks != 2 {
		t.Errorf("renamed = %+v, want 1 agent and 2 links", n)
	}
	want := map[string]string{"h1": "lead@new-mac", "h2": "lead@old-macro", "h3": "h1"}
	for name, parent := range want {
		a, err := s.GetAgent(ctx, name)
		if err != nil || a.SpawnedBy != parent {
			t.Errorf("%s spawned_by = %q (%v), want %q", name, a.SpawnedBy, err, parent)
		}
	}
	tree, err := s.ReadTree(ctx, false)
	if err != nil {
		t.Fatal(err)
	}
	if r := tree.Runs["h1"]; r == nil || r.Parent != "lead@new-mac" {
		t.Errorf("h1's run = %+v, want parent lead@new-mac", r)
	}
	hosts := func(id int64) map[string]bool {
		ls, err := s.ListFrictionLinks(ctx, id)
		if err != nil {
			t.Fatal(err)
		}
		m := map[string]bool{}
		for _, l := range ls {
			m[l.PeerHost] = true
		}
		return m
	}
	if h := hosts(f.ID); len(h) != 2 || !h["new-mac"] || !h["hub"] {
		t.Errorf("#%d links to %v, want new-mac and hub", f.ID, h)
	}
	if h := hosts(g.ID); len(h) != 1 || !h["new-mac"] {
		t.Errorf("#%d links to %v, want new-mac only", g.ID, h)
	}
}
