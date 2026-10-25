package service

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// Held messages: a message sent to a busy agent whose backend can take text
// from a hook (backend.HookDeliverer) is stored as a pending row in the
// messages table instead of being typed into its input box mid-turn. Who
// delivers it:
//
//   - the hook that ends the turn (Claude's Stop): it claims every held
//     message and prints them as its output, and the agent goes on with
//     them (reportDelivering);
//   - Send itself, if that hook ran between Send's status check and the
//     store (hold);
//   - the deliverer, `agent deliver --wait`, which Send starts for turns
//     that end with no hook at all (Esc, a deny, a failed turn): it types
//     the held messages once the agent has sat at its prompt for a while
//     (DeliverHeld);
//   - the next Send that finds the agent at its prompt, which types the
//     held messages ahead of its own;
//   - `agent watch`, if one runs, as for any queued message.
//
// Each claims the rows atomically before delivering, so a message is
// delivered once.

// EventDelivered is the event recorded when a hook hands an agent its held
// messages: it goes on with them, so the turn isn't over.
const EventDelivered = "delivered"

// SendOptions changes how Send delivers.
type SendOptions struct {
	// Now types the message even while the agent is busy (Claude takes it
	// at its next tool boundary), instead of holding it for the turn's end.
	Now bool
	// From is the sender recorded with a held message: an agent's name,
	// "user", or "remote" for a send from another host.
	From string
}

// SendResult says what Send did with a message.
type SendResult struct {
	Status AgentStatus // what Send found the agent doing (unknown if it couldn't tell)
	Detail string      // the status detail, e.g. the prompt a waiting agent is at
	// Held: stored, to be delivered when the agent's turn ends (or, per
	// HeldUntil, its next step). HeldCount is how many messages are held
	// for it now, this one included.
	Held      bool
	HeldCount int
	HeldUntil string // when the hook that delivers it runs, e.g. "its turn ends"
	// InputBlocked, for a message held although the agent was at its
	// prompt, says why: its input line isn't empty (see InputBlockedError).
	InputBlocked string
	// Typed is how many messages were typed in: this one, after any held
	// ones it overtook (oldest first, as one prompt).
	Typed int
}

// holdsMessages reports whether messages to agent can be held for the end
// of its turn: its backend takes text from a hook, and spawn wired its
// hooks to this binary (only spawn sets _spawn_id).
func (s *AgentService) holdsMessages(ctx context.Context, agent *storage.Agent) bool {
	p, err := s.provider(agent)
	if err != nil {
		return false
	}
	if _, ok := p.(backend.HookDeliverer); !ok {
		return false
	}
	return s.stateValue(ctx, agent.Name, StateKeySpawnID) != ""
}

// HoldsMessages reports whether a message to name that finds it busy is
// held for the end of its turn rather than typed (see Send).
func (s *AgentService) HoldsMessages(ctx context.Context, name string) bool {
	agent, err := s.getAgent(ctx, name)
	return err == nil && s.holdsMessages(ctx, agent)
}

// DeliveryPoint says when the hook that hands name its held messages runs,
// e.g. "its turn ends" (see backend.HookDeliverer).
func (s *AgentService) DeliveryPoint(ctx context.Context, name string) string {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return "its turn ends"
	}
	return s.deliveryPoint(agent)
}

func (s *AgentService) deliveryPoint(agent *storage.Agent) string {
	if p, err := s.provider(agent); err == nil {
		if hd, ok := p.(backend.HookDeliverer); ok {
			return hd.DeliveryPoint()
		}
	}
	return "its turn ends"
}

// turnOver reports whether the last report says the agent's turn is over
// and its prompt takes input: idle, error, or a Stop that left background
// work running (native status says busy until that work ends).
func turnOver(reported *StatusInfo) bool {
	if reported == nil {
		return false
	}
	switch reported.Status {
	case AgentStatusIdle, AgentStatusError:
		return true
	case AgentStatusProcessing:
		return reported.Event == "Stop"
	}
	return false
}

// sendHeld is Send for an agent that holds messages: it holds the message
// while the agent is mid-turn or at a prompt, and otherwise types it, after
// any messages still held.
func (s *AgentService) sendHeld(ctx context.Context, agent *storage.Agent, message, from string) (SendResult, error) {
	res := SendResult{Status: AgentStatusUnknown}
	if info, err := s.StatusInfo(ctx, agent.Name); err == nil {
		res.Status, res.Detail = info.Status, info.Detail
	}
	switch res.Status {
	case AgentStatusExited:
		return res, exitedErr(agent)
	case AgentStatusProcessing, AgentStatusWaiting:
		// Waiting too: a held message never goes near the dialog, and once
		// the prompt is answered, the turn's end delivers it.
		return s.hold(ctx, agent, res, message, from)
	}
	n, err := s.typeHeld(ctx, agent, message)
	var blocked *InputBlockedError
	if errors.As(err, &blocked) {
		// A person may be typing: hold this one (behind any held), for
		// the deliverer to type once the input line is empty.
		res, err = s.hold(ctx, agent, res, message, from)
		if res.Held {
			res.InputBlocked = blocked.Why
		}
		return res, err
	}
	res.Typed = n
	return res, err
}

// hold stores message for delivery at the end of the agent's turn, unless
// the last report says the turn is already over.
func (s *AgentService) hold(ctx context.Context, agent *storage.Agent, res SendResult, message, from string) (SendResult, error) {
	if from == "" {
		from = "user"
	}
	msg := &storage.Message{
		SenderID:   from,
		ReceiverID: agent.Name,
		Content:    message,
		Status:     storage.MessageStatusPending,
		CreatedAt:  time.Now(),
	}
	if err := s.storage.CreateMessage(ctx, msg); err != nil {
		return res, fmt.Errorf("holding message: %w", err)
	}
	// For wait --next: the turn that takes this message starts later.
	s.markSent(ctx, agent.Name)

	// The turn may be over although status said busy: its Stop may have
	// run since the status check (or native status lags it), finding
	// nothing held, or left background work running. That hook claims and
	// reports in one transaction, so if it missed this message, its report
	// is visible now: type the message.
	if reported, err := s.reportedStatus(ctx, agent.Name); err == nil && turnOver(reported) {
		if n, err := s.typeHeld(ctx, agent, ""); err == nil && n > 0 {
			res.Typed = n
			return res, nil
		}
	}

	res.Held = true
	res.HeldUntil = s.deliveryPoint(agent)
	res.HeldCount = 1
	if held, err := s.storage.GetPendingMessages(ctx, agent.Name, maxHeldCount); err == nil && len(held) > 0 {
		res.HeldCount = len(held)
	}
	_ = s.startDeliverer(agent.Name) // best effort: the turn's hook delivers either way
	return res, nil
}

// maxHeldCount bounds how many held messages are counted.
const maxHeldCount = 1000

// HeldCount returns how many messages are held for name.
func (s *AgentService) HeldCount(ctx context.Context, name string) int {
	held, err := s.storage.GetPendingMessages(ctx, name, maxHeldCount)
	if err != nil {
		return 0
	}
	return len(held)
}

// InputBlockedError is typeHeld's error when the agent's input line isn't
// known to be empty: typed messages would mix into what a person is typing.
type InputBlockedError struct {
	Agent string
	Why   string // e.g. "it has text in it"
}

func (e *InputBlockedError) Error() string {
	return fmt.Sprintf("agent %s's input line isn't empty (%s), so messages typed now would mix into what a person is typing there: held messages are typed once it's empty (agent send --now types anyway)", e.Agent, e.Why)
}

// inputBlocked returns why typing into the agent would land in a person's
// input: "" if its input line is empty, or the backend can't read it.
func (s *AgentService) inputBlocked(agent *storage.Agent) string {
	p, err := s.provider(agent)
	if err != nil {
		return ""
	}
	ir, ok := p.(backend.InputReader)
	if !ok {
		return ""
	}
	screen, err := s.tmux.CaptureStyled(agent.SessionName, 0)
	if err != nil {
		return "" // SendUnchecked reports a missing session
	}
	switch text, found := ir.InputText(screen); {
	case !found:
		return "it isn't on screen"
	case text != "":
		return "it has text in it"
	}
	return ""
}

// InputBlocked returns why held messages can't be typed into name now
// because of its input line, or "" if they can (see InputBlockedError).
func (s *AgentService) InputBlocked(ctx context.Context, name string) string {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return ""
	}
	return s.inputBlocked(agent)
}

// typeHeld types the agent's held messages, oldest first, then extra if
// set, as one prompt. It claims them first, so the agent's hook (or
// another sender) can't deliver them too, and holds them again if typing
// fails. It refuses (InputBlockedError) unless the agent's input line is
// empty or unreadable. It returns how many messages it typed.
func (s *AgentService) typeHeld(ctx context.Context, agent *storage.Agent, extra string) (int, error) {
	if err := s.checkScreen(agent); err != nil {
		return 0, err
	}
	if held, err := s.storage.GetPendingMessages(ctx, agent.Name, 1); err == nil && len(held) == 0 && extra == "" {
		return 0, nil
	}
	if why := s.inputBlocked(agent); why != "" {
		return 0, &InputBlockedError{Agent: agent.Name, Why: why}
	}
	claimed, err := s.storage.ClaimPendingMessages(ctx, agent.Name, nil)
	if err != nil {
		return 0, err
	}
	texts := contents(claimed)
	if extra != "" {
		texts = append(texts, extra)
	}
	if len(texts) == 0 {
		return 0, nil
	}
	if err := s.SendUnchecked(ctx, agent.Name, strings.Join(texts, "\n\n")); err != nil {
		for _, m := range claimed {
			_ = s.storage.UpdateMessageStatus(ctx, m.ID, storage.MessageStatusPending)
		}
		return 0, err
	}
	return len(texts), nil
}

// contents returns the messages' texts, each with its "[from ...]" tag.
func contents(msgs []*storage.Message) []string {
	texts := make([]string, len(msgs))
	for i, m := range msgs {
		texts[i] = m.Content
	}
	return texts
}

// reportDelivering records r, the report of a hook that can hand the agent
// its held messages, and claims them in the same transaction (see hold for
// why). With messages claimed, the agent goes on with them: the report says
// processing (event "delivered") and starts a turn, for wait --next, and
// r's result, the reply to a step that wasn't the last, isn't stored. With
// none claimed, r is recorded unless deliveryOnly (the hook says nothing
// else; see backend.HookReport). It returns the hook output that hands the
// messages over, nil if none.
func (s *AgentService) reportDelivering(ctx context.Context, name string, r StatusReport, hd backend.HookDeliverer, deliveryOnly bool) ([]byte, error) {
	now := time.Now()
	ended, err := s.reportUpdate(ctx, name, r, now)
	if err != nil {
		return nil, err
	}
	delivering := r
	delivering.Status, delivering.Event, delivering.Result, delivering.ClearResult = AgentStatusProcessing, EventDelivered, "", true
	going, err := s.reportUpdate(ctx, name, delivering, now)
	if err != nil {
		return nil, err
	}

	claimed, err := s.storage.ClaimPendingMessages(ctx, name, func(claimed []*storage.Message) storage.StateUpdate {
		if len(claimed) == 0 {
			if deliveryOnly {
				return storage.StateUpdate{}
			}
			return ended
		}
		delivering.Detail = plural(len(claimed), "held message") + " delivered"
		for _, e := range going.Entries {
			if e.Key == StateKeyDetail {
				e.Value = delivering.Detail
			}
		}
		if going.Event != nil {
			going.Event.Detail = delivering.Detail
		}
		return going
	})
	if err != nil {
		return nil, err
	}
	if len(claimed) == 0 {
		if !deliveryOnly {
			s.publishStatus(name, r, now)
		}
		return nil, nil
	}
	s.publishStatus(name, delivering, now)
	return hd.DeliveryOutput(contents(claimed)), nil
}

// plural formats a count with a noun, adding "s" unless n is 1.
func plural(n int, noun string) string {
	if n == 1 {
		return "1 " + noun
	}
	return fmt.Sprintf("%d %ss", n, noun)
}

// DeliverOptions tunes DeliverHeld.
type DeliverOptions struct {
	// Wait keeps checking, every Interval, while messages are held;
	// otherwise DeliverHeld checks once.
	Wait     bool
	Interval time.Duration
	// Grace is how long the agent must be seen at its prompt before held
	// messages are typed: a person who just pressed Esc gets that long to
	// type something themselves.
	Grace time.Duration
}

// ErrNothingHeld is DeliverHeld's error when no messages are held.
var ErrNothingHeld = errors.New("no messages held")

// DeliverHeld types the messages held for name once the agent is at its
// prompt with them still held, which happens when its turn ended without a
// hook that delivers them: Esc, a deny, a failed turn. It returns how many
// it typed, ErrNothingHeld if none were held to begin with, and an error if
// the agent has exited or is gone. Without Wait, it checks once (ignoring
// Grace) and returns with any messages still held while the agent is busy.
func (s *AgentService) DeliverHeld(ctx context.Context, name string, opts DeliverOptions) (int, error) {
	typed := 0
	var atPromptSince time.Time
	for first := true; ; first = false {
		agent, err := s.getAgent(ctx, name)
		if err != nil {
			return typed, err
		}
		held, err := s.storage.GetPendingMessages(ctx, name, maxHeldCount)
		if err != nil {
			return typed, err
		}
		if len(held) == 0 {
			if first {
				return 0, ErrNothingHeld
			}
			return typed, nil
		}
		info, err := s.StatusInfo(ctx, name)
		if err != nil {
			return typed, err
		}
		reported, _ := s.reportedStatus(ctx, name)
		switch {
		case info.Status == AgentStatusExited:
			return typed, fmt.Errorf("agent %s has exited with %s held for it, which won't be delivered: agent logs %s shows how it ended",
				name, plural(len(held), "message"), name)
		case info.Status == AgentStatusIdle || info.Status == AgentStatusError ||
			info.Status == AgentStatusProcessing && turnOver(reported):
			if opts.Wait && s.inputBlocked(agent) != "" {
				// A person is typing: the grace starts once the line is
				// empty, so a pause in their typing isn't typed over.
				atPromptSince = time.Time{}
				break
			}
			if atPromptSince.IsZero() {
				atPromptSince = time.Now()
			}
			if !opts.Wait || time.Since(atPromptSince) >= opts.Grace {
				n, err := s.typeHeld(ctx, agent, "")
				typed += n
				var blocked *InputBlockedError
				switch {
				case err != nil && !opts.Wait:
					return typed, err
				case errors.As(err, &blocked):
					atPromptSince = time.Time{} // typed into since the check above
				case err != nil:
					fmt.Fprintf(os.Stderr, "%s: typing %s's held messages: %v\n", time.Now().Format(time.RFC3339), name, err)
				}
			}
		default:
			atPromptSince = time.Time{}
		}
		if !opts.Wait {
			return typed, nil
		}
		if err := sleepCtx(ctx, opts.Interval); err != nil {
			return typed, err
		}
	}
}

// SetDelivererStarter replaces how a deliverer is started (tests).
func (s *AgentService) SetDelivererStarter(start func(name string) error) {
	s.deliverer = start
}

// startDeliverer starts `agent deliver --wait <name>` detached from this
// process (its own session, so it outlives a Bash tool call), logging to the
// agent's directory. One that finds another holding the agent's lock exits
// at once.
func (s *AgentService) startDeliverer(name string) error {
	if s.deliverer != nil {
		return s.deliverer(name)
	}
	if s.cfg.AgentBinary == "" {
		return errors.New("no agent binary to start a deliverer with")
	}
	logFile, err := os.OpenFile(filepath.Join(s.cfg.AgentDir(name), "deliver.log"), os.O_APPEND|os.O_CREATE|os.O_WRONLY, 0o600)
	if err != nil {
		return fmt.Errorf("opening deliverer log: %w", err)
	}
	defer logFile.Close()
	cmd := exec.Command(s.cfg.AgentBinary, "deliver", "--wait", "-q", name)
	cmd.Stdout, cmd.Stderr = logFile, logFile
	cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true}
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("starting deliverer: %w", err)
	}
	return cmd.Process.Release()
}

// DelivererLockPath is the file a running deliverer for name holds locked.
func (s *AgentService) DelivererLockPath(name string) string {
	return filepath.Join(s.cfg.AgentDir(name), "deliver.lock")
}
