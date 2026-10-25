package transcript

import (
	"bytes"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// heldDelivery is a corpus turn with two final replies, one per Stop: the
// second answers a held delivery.
func heldDelivery(t *testing.T) []byte {
	t.Helper()
	data, err := os.ReadFile(filepath.Join(corpusDir, "claude", "held-delivery.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	return data
}

// lineStart returns the offset of the line of data holding s.
func lineStart(t *testing.T, data []byte, s string) int {
	t.Helper()
	i := bytes.Index(data, []byte(s))
	if i < 0 {
		t.Fatalf("no %q", s)
	}
	return bytes.LastIndexByte(data[:i], '\n') + 1
}

const (
	heldTurn   = "389c7d4d-b2a5-5fba-8c0a-ad513ef44a69"
	heldFirst  = "Two TODOs: the port should come from the config (cmd/widget/main.go:22), and cmd/legacy/run.go is marked for deletion after v2."
	heldSecond = "Understood: the only TODO left to act on is reading the port from the config."
)

// Each Stop's reply shows before the transcript holds it; the
// transcript's copy then updates it.
func TestFinals(t *testing.T) {
	data := heldDelivery(t)
	p := Claude{}.NewParser(Options{})
	f := NewFinals()
	var stream []Event
	feed := func(upTo int) {
		for _, e := range p.Feed(data[p.Pos().At:upTo]) {
			stream = append(stream, f.Merge(e))
		}
	}
	stop := func(text, ts string) {
		e, ok := f.Provisional(heldTurn, text, ts, p.Pos())
		if !ok {
			t.Fatalf("no provisional reply for %q", text)
		}
		stream = append(stream, e)
	}

	// The first Stop fires before its reply's line is written.
	feed(lineStart(t, data, heldFirst))
	stop(heldFirst, "2026-10-20T11:00:06.050Z")
	feed(lineStart(t, data, heldSecond))
	stop(heldSecond, "2026-10-20T11:00:07.950Z")
	feed(len(data))

	var says []Event
	for _, e := range stream {
		if e.Say != nil {
			says = append(says, e)
		}
	}
	want := []struct {
		id, op, text string
		provisional  bool
	}{
		{heldTurn + "#final", OpAdd, heldFirst, true},
		{heldTurn + "#final", OpUpdate, heldFirst, false},
		{heldTurn + "#final2", OpAdd, heldSecond, true},
		{heldTurn + "#final2", OpUpdate, heldSecond, false},
	}
	if len(says) != len(want) {
		t.Fatalf("says: %v", summaries(says))
	}
	for i, w := range want {
		e := says[i]
		if e.ID != w.id || e.Op != w.op || e.Say.Text != w.text || e.Say.Provisional != w.provisional || !e.Say.Final {
			t.Errorf("say %d = %s %s %+v, want %s %s %q provisional %v", i, e.ID, e.Op, e.Say, w.id, w.op, w.text, w.provisional)
		}
		if e.Turn == nil || e.Turn.ID != heldTurn || e.Turn.Index != 1 {
			t.Errorf("say %d turn = %+v", i, e.Turn)
		}
	}
	// The update is where the transcript's copy is.
	if u := says[1]; u.Line != 5 || u.At != int64(lineStart(t, data, heldFirst)) || u.TS != "2026-10-20T11:00:06.000Z" {
		t.Errorf("update at line %d, %d, %s; want the transcript line's", u.Line, u.At, u.TS)
	}
	// Folded, each reply shows once: the transcript's 8 events.
	latest := Latest(stream)
	if n := len(latest); n != 8 {
		t.Errorf("latest has %d events, want the transcript's 8: %v", n, summaries(latest))
	}
}

// A Stop whose reply the transcript already holds adds nothing, and a
// transcript nobody sent a provisional reply for passes through as it is.
func TestFinals_TranscriptFirst(t *testing.T) {
	data := heldDelivery(t)
	p := Claude{}.NewParser(Options{})
	f := NewFinals()
	end := lineStart(t, data, heldSecond)
	var stream []Event
	for _, e := range p.Feed(data[:end]) {
		stream = append(stream, f.Merge(e))
	}
	if _, ok := f.Provisional(heldTurn, heldFirst, "2026-10-20T11:00:06.050Z", p.Pos()); ok {
		t.Error("a provisional reply for a reply the transcript holds")
	}
	for _, e := range stream {
		if e.Op == OpUpdate && e.Say != nil {
			t.Errorf("an update with no provisional reply: %+v", e)
		}
	}
	if _, ok := f.Provisional(heldTurn, "", "", p.Pos()); ok {
		t.Error("a provisional reply with no text")
	}
}

// A Stop seen only after its reply landed adds nothing, and the turn's
// next Stop still pairs with the reply after it.
func TestFinals_LateStop(t *testing.T) {
	data := heldDelivery(t)
	p := Claude{}.NewParser(Options{})
	f := NewFinals()
	var stream []Event
	feed := func(upTo int) {
		for _, e := range p.Feed(data[p.Pos().At:upTo]) {
			stream = append(stream, f.Merge(e))
		}
	}
	feed(lineStart(t, data, heldSecond))
	if _, ok := f.Provisional(heldTurn, heldFirst+"\n", "2026-10-20T11:00:06.050Z", p.Pos()); ok {
		t.Fatal("a provisional reply for the reply the transcript holds")
	}
	e, ok := f.Provisional(heldTurn, heldSecond, "2026-10-20T11:00:07.950Z", p.Pos())
	if !ok || e.ID != heldTurn+"#final2" {
		t.Fatalf("second Stop = %+v, %v; want %s#final2", e, ok, heldTurn)
	}
	stream = append(stream, e)
	feed(len(data))
	var got []string
	for _, e := range stream {
		if e.Say != nil && e.Say.Final {
			got = append(got, e.ID+" "+e.Op)
		}
	}
	want := []string{
		"583b7d2b-a4aa-52a2-9829-c8012f0f9942#0 add", heldTurn + "#final2 add", heldTurn + "#final2 update",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Errorf("final replies:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}
}
