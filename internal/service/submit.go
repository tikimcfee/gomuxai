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
// If the box never shows the text, something else took the keys: a dialog
// (see dialog.go) that came up since the sender checked the screen. Then no
// Enter is pressed, which would pick the dialog's highlighted option, and
// it returns a DialogTookError. If the screen shows no dialog it presses
// Enter as above, but says the message wasn't seen (StateKeyUnchecked)
// rather than that it was sent.
//
// It presses no more Enters once the box is off screen (a dialog may have
// taken its place) or shows anything else (a person typing). Each wait is
// bounded by the config's SubmitWait; 0 turns all this off.
//
// A CLI that hasn't started a turn since it was launched may still be
// starting: right after spawn (or resume) returns, Claude may not have drawn
// its input box yet, and what is typed then sits in the box once it does,
// its Enter lost (friction #38). So when the box isn't on screen and no turn
// has started, it first waits, up to startupWaits SubmitWaits, for the box
// to come up, then goes on as above. A dialog that comes up instead (the
// trust prompt) ends the wait with checkScreen's error, unless the sender
// meant to type into whatever is there (SendUnchecked).
//
// If it can't read the box before typing (it isn't on screen, or the screen
// can't be captured), it types and presses Enter without checking, and says
// so: the sender's result names why, and StateKeyUnchecked records it, so
// status shows it until a turn starts.

// submitEnters is how many times Enter is pressed for one message, at most.
const submitEnters = 3

// submitPoll is how often the input box is read while typing a message.
const submitPoll = 50 * time.Millisecond

// startupWaits is how many SubmitWaits typing waits for the input box of
// a CLI that may still be starting (see above): 30s by default.
const startupWaits = 10

// startupPoll is how often the screen is read while waiting for that box.
const startupPoll = 200 * time.Millisecond

// submitQuiet is how long the input box must stay unchanged after typing
// before the text counts as taken in: a paste's placeholder or an image
// path's [Image #N] can take a moment to replace the text.
const submitQuiet = 150 * time.Millisecond

// StateKeyUnsubmitted is what a message's input box showed when Enter
// failed to submit it: status compares it with what the box shows now.
const StateKeyUnsubmitted = "_unsubmitted"

// StateKeyUnchecked is why a message was typed without checking it was
// submitted (see typeMessage); its time is when.
const StateKeyUnchecked = "_unchecked"

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

// DialogTookError is the error for a message typed into an agent whose
// input box never showed it because a dialog came up and took the keys:
// Enter wasn't pressed, so the dialog is unanswered.
type DialogTookError struct {
	Agent  *storage.Agent
	Dialog backend.Dialog
}

func (e *DialogTookError) Error() string {
	return fmt.Sprintf("the message was typed into %s, but a dialog came up (%s) and took it, so Enter wasn't pressed: %s",
		e.Agent.Name, OneLine(e.Dialog.Title), dialogHint(e.Agent))
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
// the Enter submitted it where the backend needs that (see above). It
// returns why it couldn't check, or "" if it did or the backend needs no
// checking.
// With dialogs, a dialog that comes up while it waits for a starting CLI's
// input box is an error, and nothing is typed.
func (s *AgentService) typeMessage(ctx context.Context, agent *storage.Agent, text string, dialogs bool) (string, error) {
	sc, ok := s.submitChecker(agent)
	if !ok {
		return "", s.pasteLine(agent.SessionName, text)
	}
	before, why := s.inputBefore(agent, sc)
	if why != "" && s.stateValue(ctx, agent.Name, StateKeyTurnAt) == "" {
		var err error
		if before, why, err = s.awaitInputBox(ctx, agent, sc, dialogs); err != nil {
			return "", err
		}
	}
	if why != "" {
		if err := s.pasteLine(agent.SessionName, text); err != nil {
			return "", err
		}
		_ = s.setState(ctx, agent.Name, StateKeyUnchecked, why)
		return why, nil
	}
	if err := s.tmux.PasteText(agent.SessionName, text); err != nil {
		return "", err
	}
	typed, seen, err := s.settledInput(ctx, agent, sc, before)
	if err != nil {
		return "", err
	}
	if !seen {
		// The box didn't take the text: a dialog that came up since the
		// check before typing may have, and Enter would answer it.
		if d, on := s.dialogOnScreen(agent); on {
			return "", &DialogTookError{Agent: agent, Dialog: d}
		}
	}
	for enters := 1; ; enters++ {
		if err := s.tmux.SendSpecialKey(agent.SessionName, "Enter"); err != nil {
			return "", err
		}
		var sits bool
		typed, seen, sits, err = s.awaitSubmit(ctx, agent, sc, before, typed, seen)
		if err != nil || !sits {
			if err != nil {
				return "", err
			}
			_ = s.storage.DeleteStateKey(ctx, agent.Name, StateKeyUnsubmitted)
			if !seen {
				// Something else took the text, or the box is drawn in a
				// way agent doesn't read: say so rather than "Sent".
				why := "its input box never showed the message (a dialog, or a screen agent doesn't recognize, may have taken it)"
				_ = s.setState(ctx, agent.Name, StateKeyUnchecked, why)
				return why, nil
			}
			_ = s.storage.DeleteStateKey(ctx, agent.Name, StateKeyUnchecked)
			return "", nil
		}
		if enters == submitEnters {
			_ = s.setState(ctx, agent.Name, StateKeyUnsubmitted, typed)
			return "", &UnsubmittedError{Agent: agent.Name, Session: agent.SessionName, Enters: enters}
		}
	}
}

// inputBefore reads the agent's input box before a message is typed. It
// returns what the box shows, or why it can't be read.
func (s *AgentService) inputBefore(agent *storage.Agent, ir backend.InputReader) (string, string) {
	screen, err := s.tmux.CaptureStyled(agent.SessionName, 0)
	if err != nil {
		return "", fmt.Sprintf("its screen couldn't be captured (%v)", err)
	}
	text, found := ir.InputText(screen)
	if !found {
		return "", "its input box isn't on screen (a dialog, or a screen agent doesn't recognize)"
	}
	return text, ""
}

// awaitInputBox waits, up to startupWaits SubmitWaits, for the input box
// of an agent whose CLI may still be starting to come on screen. It returns
// what the box shows once it does, or, if it doesn't, why it can't be read
// (as inputBefore). With dialogs, a dialog on screen ends the wait with
// checkScreen's error.
func (s *AgentService) awaitInputBox(ctx context.Context, agent *storage.Agent, ir backend.InputReader, dialogs bool) (string, string, error) {
	bound := startupWaits * s.cfg.SubmitWait
	deadline := time.Now().Add(bound)
	for {
		if dialogs {
			if err := s.checkScreen(agent); err != nil {
				return "", "", err
			}
		}
		if !time.Now().Before(deadline) {
			break
		}
		if err := sleepCtx(ctx, startupPoll); err != nil {
			return "", "", err
		}
		if before, why := s.inputBefore(agent, ir); why == "" {
			return before, "", nil
		}
	}
	_, why := s.inputBefore(agent, ir)
	if why != "" {
		why += fmt.Sprintf(", after waiting %s for it: no turn has started, so the CLI may still have been starting", bound)
	}
	return "", why, nil
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

// Unchecked reports a message typed into name without checking it was
// submitted, if no turn has started since: why, and when it was typed.
func (s *AgentService) Unchecked(ctx context.Context, name string) (why string, at time.Time, ok bool) {
	entry, err := s.storage.GetState(ctx, name, StateKeyUnchecked)
	if err != nil || entry == nil || entry.Value == "" {
		return "", time.Time{}, false
	}
	if reported, err := s.reportedStatus(ctx, name); err == nil && reported != nil && reported.TurnAt.After(entry.UpdatedAt) {
		return "", time.Time{}, false
	}
	return entry.Value, entry.UpdatedAt, true
}
