package service

import (
	"context"
	"fmt"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// Typing a message into a CLI and pressing Enter are two writes to its
// terminal, and Claude handles whatever has arrived in one read as one
// input. While its event loop is busy (a long render, a paste it is still
// collecting), the text and the Enter arrive together; Claude reads them as
// a paste, where the Enter is a newline, and the message sits in its input
// box until someone presses Enter. The first Enter after that is often
// eaten too. So for a backend that says its CLI does this
// (backend.SubmitChecker), typing a message:
//
//  1. types the text, then waits for the input box to show something new
//     that has stopped changing (the text, or a paste's placeholder), so
//     the Enter arrives on its own;
//  2. presses Enter and waits for the box to empty;
//  3. presses Enter again while the box still shows what was typed, up to
//     submitEnters times in all;
//  4. failing that, records what the box shows (StateKeyUnsubmitted) and
//     returns an UnsubmittedError, so the sender, status, and the next send
//     say so.
//
// It presses no more Enters once the box is off screen (a dialog may have
// taken its place) or shows anything else (a person typing). Each wait is
// bounded by the config's SubmitWait; 0 turns all this off.

// submitEnters is how many times Enter is pressed for one message, at most.
const submitEnters = 3

// submitPoll is how often the input box is read while typing a message.
const submitPoll = 50 * time.Millisecond

// submitQuiet is how long the input box must stay unchanged after typing
// before the text counts as taken in: a paste's placeholder or an image
// path's [Image #N] can take a moment to replace the text.
const submitQuiet = 150 * time.Millisecond

// StateKeyUnsubmitted is what a message's input box showed when Enter
// failed to submit it: status compares it with what the box shows now.
const StateKeyUnsubmitted = "_unsubmitted"

// UnsubmittedError is the error for a message typed into the agent's input
// box that is still there after submitEnters Enters.
type UnsubmittedError struct {
	Agent   string
	Session string
	Enters  int
}

func (e *UnsubmittedError) Error() string {
	return fmt.Sprintf("the message was typed into %s's input box, but it is still there after %d Enters: don't send it again; agent capture %s shows it, and agent key %s Enter submits it",
		e.Agent, e.Enters, e.Agent, e.Session)
}

// submitChecker returns the agent's backend as a SubmitChecker, if it is
// one and checking is on.
func (s *AgentService) submitChecker(agent *storage.Agent) (backend.SubmitChecker, bool) {
	if s.cfg.SubmitWait <= 0 {
		return nil, false
	}
	p, err := s.provider(agent)
	if err != nil {
		return nil, false
	}
	sc, ok := p.(backend.SubmitChecker)
	return sc, ok
}

// readInput reads the agent's input box off its screen.
func (s *AgentService) readInput(agent *storage.Agent, ir backend.InputReader) (string, bool) {
	screen, err := s.tmux.CaptureStyled(agent.SessionName, 0)
	if err != nil {
		return "", false
	}
	return ir.InputText(screen)
}

// typeMessage types text into the agent and presses Enter, making sure
// the Enter submitted it where the backend needs that (see above).
func (s *AgentService) typeMessage(ctx context.Context, agent *storage.Agent, text string) error {
	sc, ok := s.submitChecker(agent)
	if !ok {
		return s.sendLine(agent.SessionName, text)
	}
	before, found := s.readInput(agent, sc)
	if !found {
		return s.sendLine(agent.SessionName, text) // can't watch it: as before
	}
	if err := s.tmux.SendKeys(agent.SessionName, text); err != nil {
		return err
	}
	typed, seen, err := s.settledInput(ctx, agent, sc, before)
	if err != nil {
		return err
	}
	for enters := 1; ; enters++ {
		if err := s.tmux.SendSpecialKey(agent.SessionName, "Enter"); err != nil {
			return err
		}
		var sits bool
		typed, seen, sits, err = s.awaitSubmit(ctx, agent, sc, before, typed, seen)
		if err != nil || !sits {
			if err == nil {
				_ = s.storage.DeleteStateKey(ctx, agent.Name, StateKeyUnsubmitted)
			}
			return err
		}
		if enters == submitEnters {
			_ = s.setState(ctx, agent.Name, StateKeyUnsubmitted, typed)
			return &UnsubmittedError{Agent: agent.Name, Session: agent.SessionName, Enters: enters}
		}
	}
}

// settledInput waits, up to SubmitWait, for the input box to show
// something other than before that then stays the same for submitQuiet.
// It returns that (seen), or seen false if the box didn't change in time.
func (s *AgentService) settledInput(ctx context.Context, agent *storage.Agent, ir backend.InputReader, before string) (typed string, seen bool, err error) {
	deadline := time.Now().Add(s.cfg.SubmitWait)
	var last string
	var since time.Time
	for time.Now().Before(deadline) {
		if err := sleepCtx(ctx, submitPoll); err != nil {
			return "", false, err
		}
		text, found := s.readInput(agent, ir)
		switch {
		case !found || text == before:
			since = time.Time{}
		case since.IsZero() || text != last:
			last, since = text, time.Now()
		case time.Since(since) >= submitQuiet:
			return text, true, nil
		}
	}
	return "", false, nil
}

// awaitSubmit waits, up to SubmitWait, for the Enter just pressed to
// submit what was typed. typed is what the box showed before the Enter, if
// seen; a box that settles on the message only now (its text and the Enter
// arrived together) restarts the wait. It returns sits true if the box
// still shows typed, so another Enter is due; sits false means submitted,
// or that it can't tell (the box is off screen or shows something else),
// so no Enter is due. A box that never showed the message is one it can't
// tell about either.
func (s *AgentService) awaitSubmit(ctx context.Context, agent *storage.Agent, ir backend.InputReader, before, typed string, seen bool) (string, bool, bool, error) {
	deadline := time.Now().Add(s.cfg.SubmitWait)
	var last string
	var since time.Time
	for time.Now().Before(deadline) {
		if err := sleepCtx(ctx, submitPoll); err != nil {
			return typed, seen, false, err
		}
		text, found := s.readInput(agent, ir)
		switch {
		case !found:
			return typed, seen, false, nil
		case seen && text == typed:
			// Still there; the Enter may not have been read yet.
		case seen:
			return typed, seen, false, nil // emptied, or someone typed
		case text == before:
			since = time.Time{} // nothing read yet
		case text == "":
			return typed, seen, false, nil // read and submitted at once
		case since.IsZero() || text != last:
			last, since = text, time.Now()
		case time.Since(since) >= submitQuiet:
			typed, seen = text, true
			deadline = time.Now().Add(s.cfg.SubmitWait)
		}
	}
	return typed, seen, seen, nil
}

// unsubmitted reports whether text, what the agent's input box shows, is
// a message Enter failed to submit (see typeMessage).
func (s *AgentService) unsubmitted(ctx context.Context, agent *storage.Agent, text string) bool {
	return text != "" && s.stateValue(ctx, agent.Name, StateKeyUnsubmitted) == text
}

// Unsubmitted reports whether a message Enter failed to submit still sits
// in name's input box: when it was typed, and the session to press Enter in.
func (s *AgentService) Unsubmitted(ctx context.Context, name string) (at time.Time, session string, ok bool) {
	entry, err := s.storage.GetState(ctx, name, StateKeyUnsubmitted)
	if err != nil || entry == nil || entry.Value == "" {
		return time.Time{}, "", false
	}
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return time.Time{}, "", false
	}
	p, err := s.provider(agent)
	if err != nil {
		return time.Time{}, "", false
	}
	ir, ok := p.(backend.InputReader)
	if !ok {
		return time.Time{}, "", false
	}
	text, found := s.readInput(agent, ir)
	return entry.UpdatedAt, agent.SessionName, found && text == entry.Value
}
