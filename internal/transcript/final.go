package transcript

import (
	"fmt"
	"strings"
)

// Finals merges final replies a harness reports before its transcript
// holds them into a stream. Claude Code writes a turn's closing reply to
// its transcript only after its Stop hook has run, while the hook already
// has the text (last_assistant_message): a reader that waits for the
// transcript shows every turn one reply behind.
//
// Provisional makes the reply a say event of its own, id
// "<promptId>#final", marked provisional. When the transcript's own copy
// lands, Merge sends it as an update of that id, so a reader replaces the
// provisional reply instead of showing it twice; later updates of the copy
// (a reply with no stop reason marked final when its turn ends) keep that
// id. The copy is the turn's next final reply, or its next reply with the
// same text, which is how a reply that isn't final when it lands (no stop
// reason yet, a max_tokens stop) is told. A turn a held delivery continues
// has a Stop, and a final reply, per delivery: the reply of the nth Stop
// is "<promptId>#final<n>".
//
// Finals doesn't number events: the stream that interleaves them sets Seq.
type Finals struct {
	turns map[string]*TurnRef
	// made counts each turn's provisional replies, and seen the final
	// replies its transcript gave: together they number the turn's Stops.
	made map[string]int
	seen map[string]int
	// waiting holds each turn's provisional replies whose copy hasn't
	// landed, oldest first; said, the text of the turn's latest reply in
	// the transcript.
	waiting map[string][]waitingReply
	said    map[string]string
	// sentAs maps each copy to the provisional reply's id it was sent
	// under.
	sentAs map[string]string
}

type waitingReply struct{ id, text string }

// NewFinals returns a Finals with no replies waiting.
func NewFinals() *Finals {
	return &Finals{turns: map[string]*TurnRef{}, made: map[string]int{}, seen: map[string]int{},
		waiting: map[string][]waitingReply{}, said: map[string]string{}, sentAs: map[string]string{}}
}

// finalID is the id of a turn's nth provisional reply, counting from 0.
func finalID(promptID string, n int) string {
	if n == 0 {
		return promptID + "#final"
	}
	return fmt.Sprintf("%s#final%d", promptID, n+1)
}

func sameText(a, b string) bool { return strings.TrimSpace(a) == strings.TrimSpace(b) }

// Provisional returns the event for a final reply the harness reported for
// the turn promptID, at ts, while the transcript ended at pos. It returns
// false when the text is empty, or the transcript has already given that
// reply: no provisional reply is waiting and the turn's latest reply has
// that text. A Stop reported only after its reply landed thus adds
// nothing, and the turn's next Stop still gets the next id.
func (f *Finals) Provisional(promptID, text, ts string, pos Pos) (Event, bool) {
	if text == "" || promptID == "" {
		return Event{}, false
	}
	if len(f.waiting[promptID]) == 0 && sameText(f.said[promptID], text) {
		return Event{}, false
	}
	// Final replies the transcript gave before any Stop was reported for
	// them count among the turn's Stops.
	n := max(f.made[promptID], f.seen[promptID])
	f.made[promptID] = n + 1
	id := finalID(promptID, n)
	f.waiting[promptID] = append(f.waiting[promptID], waitingReply{id, text})
	turn := f.turns[promptID]
	if turn == nil {
		turn = &TurnRef{ID: promptID}
	}
	return Event{
		ID: id, Op: OpAdd, At: pos.At, Line: pos.Line, TS: ts, Turn: turn, Kind: KindSay,
		Say: &SayEvent{Text: text, Final: true, Provisional: true},
	}, true
}

// Merge passes an event of the transcript's stream through: the copy of a
// provisional reply, and every later update of it, comes out as an update
// of that reply, under its id.
func (f *Finals) Merge(e Event) Event {
	if id, ok := f.sentAs[e.ID]; ok {
		e.ID, e.Op = id, OpUpdate
		return e
	}
	if e.Turn == nil {
		return e
	}
	turn := e.Turn.ID
	f.turns[turn] = e.Turn
	if e.Say == nil || e.Op != OpAdd {
		return e
	}
	f.said[turn] = e.Say.Text
	if e.Say.Final {
		f.seen[turn]++
	}
	w := f.waiting[turn]
	if len(w) == 0 || !e.Say.Final && !sameText(e.Say.Text, w[0].text) {
		return e
	}
	f.waiting[turn] = w[1:]
	f.sentAs[e.ID] = w[0].id
	e.ID, e.Op = w[0].id, OpUpdate
	return e
}
