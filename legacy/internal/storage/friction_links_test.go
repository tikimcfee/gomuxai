package storage

import (
	"context"
	"path/filepath"
	"testing"
	"time"
)

func TestFrictionLinks(t *testing.T) {
	s, err := NewSQLiteStorage(filepath.Join(t.TempDir(), "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { s.Close() })
	ctx := context.Background()
	now := time.Date(2026, 11, 8, 10, 0, 0, 0, time.UTC)

	file := func(uid string) *Friction {
		t.Helper()
		f := &Friction{UID: uid, Host: "h", FiledBy: "lead", CreatedAt: now, UpdatedAt: now, Kind: "bug", Summary: uid, Status: FrictionOpen}
		if err := s.AddFriction(ctx, f); err != nil {
			t.Fatal(err)
		}
		return f
	}
	links := func(id int64) []*FrictionLink {
		t.Helper()
		ls, err := s.ListFrictionLinks(ctx, id)
		if err != nil {
			t.Fatal(err)
		}
		return ls
	}
	a, b, c := file("h-a"), file("h-b"), file("h-c")

	// Recorded while the other host was unreachable: no uid, unconfirmed.
	l := &FrictionLink{FrictionID: a.ID, PeerHost: "Mono", PeerID: 34, LinkedBy: "lead", LinkedAt: now, Note: "same trust prompt"}
	if err := s.AddFrictionLink(ctx, l); err != nil || l.ID == 0 {
		t.Fatalf("AddFrictionLink = %v, id %d", err, l.ID)
	}
	// Linked again once it answers, spelled in another case: the same link,
	// now confirmed, keeping who first linked it and the note.
	again := &FrictionLink{FrictionID: a.ID, PeerHost: "mono", PeerID: 34, PeerUID: "mono-34", LinkedBy: "other", LinkedAt: now.Add(time.Hour),
		ConfirmedAt: now.Add(time.Hour), PeerStatus: FrictionOpen, PeerSummary: "trust prompt", PeerVotes: 2, PeerCheckedAt: now.Add(time.Hour)}
	if err := s.AddFrictionLink(ctx, again); err != nil || again.ID != l.ID {
		t.Fatalf("relinking = %v, id %d; want the same link, id %d", err, again.ID, l.ID)
	}
	got := links(a.ID)
	if len(got) != 1 {
		t.Fatalf("links of #%d = %d, want 1", a.ID, len(got))
	}
	g := got[0]
	if g.PeerHost != "Mono" || g.PeerUID != "mono-34" || g.LinkedBy != "lead" || !g.LinkedAt.Equal(now) || g.Note != "same trust prompt" ||
		!g.ConfirmedAt.Equal(now.Add(time.Hour)) || g.PeerVotes != 2 || g.PeerSummary != "trust prompt" {
		t.Errorf("relinked link = %+v", g)
	}

	// An update with no check keeps the cache.
	if err := s.AddFrictionLink(ctx, &FrictionLink{FrictionID: a.ID, PeerHost: "mono", PeerID: 34, LinkedBy: "x", LinkedAt: now}); err != nil {
		t.Fatal(err)
	}
	if g := links(a.ID)[0]; g.PeerSummary != "trust prompt" || g.PeerUID != "mono-34" {
		t.Errorf("an unchecked relink lost the cache: %+v", g)
	}

	// UpdateFrictionLink follows the other end to its original.
	g.PeerID, g.PeerStatus, g.PeerCheckedAt = 30, FrictionResolved, now.Add(2*time.Hour)
	if err := s.UpdateFrictionLink(ctx, g); err != nil {
		t.Fatal(err)
	}
	if g := links(a.ID)[0]; g.PeerID != 30 || g.PeerStatus != FrictionResolved {
		t.Errorf("updated link = %+v", g)
	}

	// Closing b as a duplicate of a moves b's links to a; one a already has
	// is dropped.
	for _, peer := range []int64{30, 40} {
		if err := s.AddFrictionLink(ctx, &FrictionLink{FrictionID: b.ID, PeerHost: "mono", PeerID: peer, LinkedBy: "x", LinkedAt: now}); err != nil {
			t.Fatal(err)
		}
	}
	b.Status, b.DuplicateOf = FrictionDuplicate, a.ID
	if err := s.UpdateFriction(ctx, b); err != nil {
		t.Fatal(err)
	}
	if n := len(links(b.ID)); n != 0 {
		t.Errorf("the duplicate kept %d links", n)
	}
	var peers []int64
	for _, l := range links(a.ID) {
		peers = append(peers, l.PeerID)
	}
	if len(peers) != 2 || peers[0] != 30 || peers[1] != 40 {
		t.Errorf("the original's links go to %v, want [30 40]", peers)
	}

	if err := s.AddFrictionLink(ctx, &FrictionLink{FrictionID: c.ID, PeerHost: "other", PeerID: 1, LinkedBy: "x", LinkedAt: now}); err != nil {
		t.Fatal(err)
	}
	if n := len(links(0)); n != 3 {
		t.Errorf("all links = %d, want 3", n)
	}
	if ok, err := s.DeleteFrictionLink(ctx, a.ID, "MONO", 40); err != nil || !ok {
		t.Errorf("DeleteFrictionLink = %v, %v; want true", ok, err)
	}
	if ok, err := s.DeleteFrictionLink(ctx, a.ID, "mono", 40); err != nil || ok {
		t.Errorf("deleting it again = %v, %v; want false", ok, err)
	}
	if n := len(links(a.ID)); n != 1 {
		t.Errorf("after unlinking, #%d has %d links, want 1", a.ID, n)
	}
}

// A store from before links gets the table when it's opened.
func TestFrictionLinks_AddedToOldStores(t *testing.T) {
	path := filepath.Join(t.TempDir(), "gomuxai.db")
	s, err := NewSQLiteStorage(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := s.db.Exec(`DROP TABLE friction_links`); err != nil {
		t.Fatal(err)
	}
	s.Close()
	if s, err = NewSQLiteStorage(path); err != nil {
		t.Fatal(err)
	}
	defer s.Close()
	if _, err := s.ListFrictionLinks(context.Background(), 0); err != nil {
		t.Errorf("listing links in a reopened old store: %v", err)
	}
}
