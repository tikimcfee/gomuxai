package service

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// Friction entries linked across hosts: each host keeps its half of a link
// (storage.FrictionLink), and the bus's friction op lets the other host
// read the entry, record or remove its half, and resolve it. These helpers
// serve that op and are shared with the CLI, which drives the calling side.

// FrictionIsOpen reports whether an entry with status is still to be fixed.
func FrictionIsOpen(status string) bool {
	return status == storage.FrictionOpen || status == storage.FrictionAccepted
}

// OriginalFriction returns entry id, and the entry that stands for it: its
// original if it was closed as a duplicate (its links moved there), else
// the entry itself. Both are nil if there is no entry id.
func OriginalFriction(ctx context.Context, store storage.Storage, id int64) (asked, original *storage.Friction, err error) {
	asked, err = store.GetFriction(ctx, id)
	if err != nil || asked == nil {
		return nil, nil, err
	}
	original = asked
	if asked.Status == storage.FrictionDuplicate && asked.DuplicateOf != 0 {
		if o, err := store.GetFriction(ctx, asked.DuplicateOf); err != nil {
			return nil, nil, err
		} else if o != nil {
			original = o
		}
	}
	return asked, original, nil
}

// FrictionEntryOf describes f, on host, for another host, with its links.
func FrictionEntryOf(ctx context.Context, store storage.Storage, host string, f *storage.Friction) (natsbus.FrictionEntry, error) {
	e := natsbus.FrictionEntry{
		Host: host, ID: f.ID, UID: f.UID, Status: f.Status, Kind: f.Kind, Summary: f.Summary, Votes: f.Votes,
		ResolvedBy: f.ResolvedBy, ResolvedAt: f.ResolvedAt, Resolution: f.Resolution,
	}
	links, err := store.ListFrictionLinks(ctx, f.ID)
	if err != nil {
		return e, err
	}
	for _, l := range links {
		e.Links = append(e.Links, natsbus.FrictionRef{Host: l.PeerHost, ID: l.PeerID, UID: l.PeerUID})
	}
	return e, nil
}

// SeeFrictionPeer records in l what was seen of its other end, e, at at:
// its id and uid (when e stands for the entry l named, closed as its
// duplicate, l follows it to e), and its state.
func SeeFrictionPeer(l *storage.FrictionLink, e natsbus.FrictionEntry, at time.Time) {
	l.PeerID, l.PeerUID = e.ID, e.UID
	l.PeerStatus, l.PeerKind, l.PeerSummary, l.PeerVotes = e.Status, e.Kind, e.Summary, e.Votes
	l.PeerResolvedBy, l.PeerResolution, l.PeerCheckedAt = e.ResolvedBy, e.Resolution, at
}

// CloseFriction closes f as status (resolved or wontfix) with note, by who.
func CloseFriction(ctx context.Context, store storage.Storage, f *storage.Friction, status, note, who string) error {
	now := time.Now()
	f.Status, f.UpdatedAt, f.ResolvedAt, f.ResolvedBy, f.Resolution = status, now, now, who, note
	return store.UpdateFriction(ctx, f)
}

// ServeFriction answers another host's friction request.
func (s *AgentService) ServeFriction(ctx context.Context, req natsbus.FrictionRequest) (*natsbus.FrictionResponse, error) {
	resp := &natsbus.FrictionResponse{Entries: []natsbus.FrictionEntry{}}
	if req.From != nil && strings.EqualFold(req.From.Host, s.cfg.Host) {
		return nil, fmt.Errorf("the friction request comes from %s, which is this host, but it came over the bus", req.From.Host)
	}
	if req.Action == natsbus.FrictionGet {
		for _, id := range req.IDs {
			asked, f, err := OriginalFriction(ctx, s.storage, id)
			if err != nil {
				return nil, err
			}
			if f == nil {
				resp.Missing = append(resp.Missing, id)
				continue
			}
			e, err := s.frictionEntry(ctx, asked, f)
			if err != nil {
				return nil, err
			}
			resp.Entries = append(resp.Entries, e)
		}
		return resp, nil
	}

	asked, f, err := OriginalFriction(ctx, s.storage, req.ID)
	if err != nil {
		return nil, err
	}
	if f == nil {
		if req.Action == natsbus.FrictionLink {
			return nil, s.noFriction(ctx, req.ID)
		}
		resp.Missing = []int64{req.ID}
		return resp, nil
	}
	now := time.Now()
	switch req.Action {
	case natsbus.FrictionLink:
		l := &storage.FrictionLink{FrictionID: f.ID, PeerHost: req.From.Host, PeerID: req.From.ID, LinkedBy: req.By,
			LinkedAt: now, Note: req.Note, ConfirmedAt: now}
		SeeFrictionPeer(l, *req.From, now)
		if err := s.storage.AddFrictionLink(ctx, l); err != nil {
			return nil, err
		}
	case natsbus.FrictionUnlink:
		if _, err := s.storage.DeleteFrictionLink(ctx, f.ID, req.From.Host, req.From.ID); err != nil {
			return nil, err
		}
	case natsbus.FrictionResolve:
		if req.UID != "" && asked.UID != req.UID {
			// Another entry has the id now: say what it is, change nothing.
			e, err := FrictionEntryOf(ctx, s.storage, s.cfg.Host, asked)
			if err != nil {
				return nil, err
			}
			resp.Entries = append(resp.Entries, e)
			return resp, nil
		}
		if FrictionIsOpen(f.Status) {
			if err := CloseFriction(ctx, s.storage, f, req.Status, req.Note, req.By); err != nil {
				return nil, err
			}
			resp.Resolved = true
		}
		if req.From != nil {
			s.seeLinkedFrom(ctx, f.ID, *req.From, now)
		}
	}
	// Re-read it: votes and links may have changed.
	if f, err = s.storage.GetFriction(ctx, f.ID); err != nil {
		return nil, err
	}
	e, err := s.frictionEntry(ctx, asked, f)
	if err != nil {
		return nil, err
	}
	resp.Entries = append(resp.Entries, e)
	return resp, nil
}

// frictionEntry is FrictionEntryOf f, saying which entry was asked for
// when f stands for it as its original.
func (s *AgentService) frictionEntry(ctx context.Context, asked, f *storage.Friction) (natsbus.FrictionEntry, error) {
	e, err := FrictionEntryOf(ctx, s.storage, s.cfg.Host, f)
	if err == nil && asked.ID != f.ID {
		e.Requested = &natsbus.FrictionRef{Host: s.cfg.Host, ID: asked.ID, UID: asked.UID}
	}
	return e, err
}

// seeLinkedFrom updates entry id's link to from, if it has one, with
// from's state: a host resolving its entry and this one says how it left
// its own.
func (s *AgentService) seeLinkedFrom(ctx context.Context, id int64, from natsbus.FrictionEntry, at time.Time) {
	links, err := s.storage.ListFrictionLinks(ctx, id)
	if err != nil {
		return
	}
	for _, l := range links {
		if strings.EqualFold(l.PeerHost, from.Host) && l.PeerID == from.ID {
			SeeFrictionPeer(l, from, at)
			_ = s.storage.UpdateFrictionLink(ctx, l)
		}
	}
}

// noFriction is the error for another host's request naming an entry this
// host doesn't have.
func (s *AgentService) noFriction(ctx context.Context, id int64) error {
	last, err := s.storage.LastFrictionID(ctx)
	if err != nil {
		return err
	}
	if last == 0 {
		return fmt.Errorf("no friction #%d on %s: nothing has been filed there yet", id, s.cfg.Host)
	}
	return fmt.Errorf("no friction #%d on %s (latest is #%d)", id, s.cfg.Host, last)
}
