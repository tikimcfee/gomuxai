package transcript

import (
	"encoding/json"
	"slices"
	"sort"
)

// Claude Code keeps a conversation as a tree: each record names the one
// before it, its parentUuid. Editing a prompt and sending it again (Esc to
// edit, or a rewind to an earlier prompt) writes the new prompt as a child
// of the record the old one answered, beside the old one, which stays in
// the file with everything that answered it. That branch is left: the
// model never sees it again, and no later record continues it (none of
// the 57 such pairs on the survey host, 2.1.221-2.1.316: three of them
// first prompts, which have no parent, some of them turns that had run
// tools). Records share a parent for other reasons (parallel tool calls,
// hooks, harness notes), but on the survey host two user records holding
// prompts never did, except for an edit or a rewind.

// parent returns the uuid a record's parentUuid names, or "" for null.
func (r *claudeRecord) parent() string {
	var uuid string
	json.Unmarshal(r.Parent, &uuid)
	return uuid
}

// claudeNode is one record of the tree: its line, the records that name
// it as their parent, and the ids of the events it added.
type claudeNode struct {
	line   int
	kids   []string
	events []string
}

// tree places the line's record in the conversation's tree and returns
// the line's events: its adds, then its updates, then, when it is a prompt
// that replaces an earlier one, an update of every event of the branch it
// abandons, abandoned. An abandoned event stays so: its later updates (a
// background call that finishes after all) are marked too.
func (l *claudeLine) tree() []Event {
	c, r := l.c, l.r
	out := append(l.adds, l.updates...)
	for i := range out {
		if c.abandoned[out[i].ID] {
			out[i].Abandoned = true
		}
	}
	// A compaction starts the tree again (its boundary has no parentUuid):
	// what came before it is a summary to the model, and no prompt there
	// can be edited. Forgetting it keeps the parse's memory to the
	// conversation since the last compaction.
	if r.Type == "system" && r.Subtype == "compact_boundary" {
		clear(c.nodes)
		clear(c.asked)
		clear(c.latest)
	}
	// What a rewind may later send again: the events of records in the
	// tree, as they last were.
	for _, e := range out {
		if _, ok := c.latest[e.ID]; ok || e.Op == OpAdd && r.UUID != "" {
			c.latest[e.ID] = e
		}
	}
	if r.UUID == "" {
		return out
	}
	node := c.nodes[r.UUID]
	if node == nil {
		node = &claudeNode{line: l.n}
		c.nodes[r.UUID] = node
		if parent := c.nodes[r.parent()]; parent != nil {
			parent.kids = append(parent.kids, r.UUID)
		}
	}
	for _, e := range l.adds {
		node.events = append(node.events, e.ID)
	}
	// A session's first prompt has a null parent: editing it makes two.
	// A record without the field isn't placed.
	if r.Type != "user" || len(r.Parent) == 0 || !slices.ContainsFunc(l.adds, func(e Event) bool { return e.Kind == KindPrompt }) {
		return out
	}
	old := c.asked[r.parent()]
	c.asked[r.parent()] = r.UUID
	if old == "" || old == r.UUID {
		return out
	}
	out = l.renumber(old, out)
	return append(out, l.abandon(old)...)
}

// renumber gives the prompt that replaces the one record old holds that
// one's turn index, in a file that numbers its turns by counting human
// prompts: it is the same prompt, edited, as Claude Code's promptIndex,
// where a file has it, says. The prompts of the abandoned branch don't
// count.
func (l *claudeLine) renumber(old string, out []Event) []Event {
	c := l.c
	if !c.counted(l.r) || c.turn == nil {
		return out
	}
	var was *TurnRef
	for _, id := range c.nodes[old].events {
		if e := c.latest[id]; e.Kind == KindPrompt {
			was = e.Turn
			break
		}
	}
	if was == nil || was.Index == c.turn.Index {
		return out
	}
	turn := c.turn
	c.prompts = was.Index
	c.setTurn(turn.ID, was.Index, turn.Derived)
	for i := range out {
		if out[i].Turn == turn {
			out[i].Turn = c.turn
			c.latest[out[i].ID] = out[i]
		}
	}
	return out
}

// abandon marks every event of the branch from the record old down
// abandoned: an update of each, in transcript order, at this line. A
// call of the branch still waiting for its result won't get one; a
// background call's task may still report back.
func (l *claudeLine) abandon(old string) []Event {
	c := l.c
	var branch []*claudeNode
	for stack := []string{old}; len(stack) > 0; {
		node := c.nodes[stack[len(stack)-1]]
		stack = stack[:len(stack)-1]
		if node != nil {
			branch = append(branch, node)
			stack = append(stack, node.kids...)
		}
	}
	sort.Slice(branch, func(i, j int) bool { return branch[i].line < branch[j].line })
	var out []Event
	for _, node := range branch {
		for _, id := range node.events {
			e, ok := c.latest[id]
			if !ok || c.abandoned[id] {
				continue
			}
			c.abandoned[id] = true
			e.Op, e.At, e.Line, e.TS, e.Abandoned = OpUpdate, l.at, l.n, l.r.Timestamp, true
			c.latest[id] = e
			out = append(out, e)
			if call := c.tools[id]; call != nil {
				if call.Tool.Result != nil && call.Tool.Result.Status == StatusPending {
					c.settled(id, StatusInterrupted)
				} else {
					call := e
					c.tools[id] = &call
				}
			}
			if c.closing != nil && c.closing.ID == id {
				c.closing = nil
			}
		}
	}
	return out
}
