package transcript

import "fmt"

// Finals merges final replies a harness reports before its transcript
// holds them into a stream. Claude Code writes a turn's closing reply to
// its transcript only after its Stop hook has run, while the hook already
// has the text (last_assistant_message): a reader that waits for the
// transcript shows every turn one reply behind.
//
// Provisional makes the reply a say event of its own, id
// "<promptId>#final", marked provisional. When the transcript's own final
// reply for that turn lands, Merge sends it as an update of that id, so a
// reader replaces the provisional reply instead of showing it twice. A
// turn a held delivery continues has a Stop, and a final reply, per
// delivery: the nth provisional reply of a turn is "<promptId>#final<n>",
// and pairs with the turn's nth final reply.
//
// Finals doesn't number events: the stream that interleaves them sets Seq.
type Finals struct {
	turns map[string]*TurnRef
	// made counts each turn's provisional replies; seen, the final replies
	// its transcript has given.
	made map[string]int
	seen map[string]int
}

// NewFinals returns a Finals with no replies waiting.
func NewFinals() *Finals {
	return &Finals{turns: map[string]*TurnRef{}, made: map[string]int{}, seen: map[string]int{}}
}

// finalID is the id of a turn's nth provisional reply, counting from 0.
func finalID(promptID string, n int) string {
	if n == 0 {
		return promptID + "#final"
	}
	return fmt.Sprintf("%s#final%d", promptID, n+1)
}

// Provisional returns the event for a final reply the harness reported for
// the turn promptID, at ts, while the transcript ended at pos. It returns
// false when the transcript has already given that reply, or the text is
// empty.
func (f *Finals) Provisional(promptID, text, ts string, pos Pos) (Event, bool) {
	n := f.made[promptID]
	if text == "" || promptID == "" || f.seen[promptID] > n {
		return Event{}, false
	}
	f.made[promptID] = n + 1
	turn := f.turns[promptID]
	if turn == nil {
		turn = &TurnRef{ID: promptID}
	}
	return Event{
		ID: finalID(promptID, n), Op: OpAdd, At: pos.At, Line: pos.Line, TS: ts, Turn: turn, Kind: KindSay,
		Say: &SayEvent{Text: text, Final: true, Provisional: true},
	}, true
}

// Merge passes an event of the transcript's stream through: a turn's final
// reply whose provisional reply was sent comes out as the update of that
// reply, under its id.
func (f *Finals) Merge(e Event) Event {
	if e.Turn == nil {
		return e
	}
	id := e.Turn.ID
	f.turns[id] = e.Turn
	if e.Say == nil || !e.Say.Final || e.Op != OpAdd {
		return e
	}
	n := f.seen[id]
	f.seen[id] = n + 1
	if n >= f.made[id] {
		return e
	}
	e.ID, e.Op = finalID(id, n), OpUpdate
	return e
}
