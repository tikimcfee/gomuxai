package service

import (
	"context"
	"errors"
	"fmt"
	"regexp"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// Handing a long message over by hook. Claude takes a long or multi-line
// message typed into it as a paste, and gives the model its text inside
// <pasted_content> tags, as material to consider rather than as the
// request (backend.PromptHandover says when). Instructions sent to an agent
// would arrive downgraded. So when such a message goes to an agent whose
// hook reaches its CLI (holdsMessages), typeMessage:
//
//  1. stores the message (status handover), and types one short line in
//     its place: the message's first line, cut short, and a handover token
//     naming the stored message ("[gomuxai message #42, 10.4 KB: ...]");
//  2. checks that line was submitted, as for any typed message;
//  3. waits for the hook the submitted prompt runs (Claude's
//     UserPromptSubmit), which finds the token, claims the message, and
//     prints its whole text as the prompt's additional context.
//
// The model then gets the line as the prompt and the whole message beside
// it as hook context, which it takes as coming from the user rather than
// as pasted material. The turn starts and ends as for any typed message,
// so wait --next and _result cover the reply.
//
// If the hook doesn't take the message within handoverWaits SubmitWaits of
// the line's submission, the message is failed (a late hook then hands over nothing)
// and typeMessage says the agent got only the line. If the line wasn't
// submitted (UnsubmittedError) or couldn't be checked, the message waits
// for whenever it is: the hook hands it over then.

// handoverWaits is how many SubmitWaits typing waits, after the short line
// is submitted, for the hook to take the whole message: 9s by default. The
// hook runs before the model is called, so this is the hook's own start-up
// time.
const handoverWaits = 3

// handoverWait is that wait, or 9s with checking off.
func (s *AgentService) handoverWait() time.Duration {
	if s.cfg.SubmitWait <= 0 {
		return handoverWaits * config.DefaultSubmitWait
	}
	return handoverWaits * s.cfg.SubmitWait
}

// handoverPoll is how often it checks.
const handoverPoll = 50 * time.Millisecond

// maxHandoverHead bounds the part of the message's first line the short
// line carries, in runes: far below any paste threshold.
const maxHandoverHead = 160

// handoverToken finds the handover token in a submitted prompt.
var handoverToken = regexp.MustCompile(`\[gomuxai message #([0-9]+), `)

// HandoverError says the hook didn't take a handed-over message's text:
// the agent got only the short line typed in its place.
type HandoverError struct {
	Agent string
	ID    int64
	Wait  time.Duration
}

func (e *HandoverError) Error() string {
	return fmt.Sprintf("typed the first line of the message into %s (as gomuxai message #%d), but its hook didn't take the rest within %s, so it got only that line: agent logs %s shows what it got; send the message again",
		e.Agent, e.ID, e.Wait, e.Agent)
}

// promptHandover returns the agent's backend as a PromptHandover when its
// hook reaches the CLI it runs, so a handed-over message would be taken.
func (s *AgentService) promptHandover(ctx context.Context, agent *storage.Agent) (backend.PromptHandover, bool) {
	p, err := s.provider(agent)
	if err != nil {
		return nil, false
	}
	ph, ok := p.(backend.PromptHandover)
	if !ok || !s.holdsMessages(ctx, agent) {
		return nil, false
	}
	return ph, true
}

// typeHandover types text into the agent as a short line, for its hook to
// hand over in whole (see above). It returns what typeText returns, or a
// HandoverError.
func (s *AgentService) typeHandover(ctx context.Context, agent *storage.Agent, text string, dialogs bool) (string, error) {
	msg := &storage.Message{
		SenderID:   "send",
		ReceiverID: agent.Name,
		Content:    text,
		Status:     storage.MessageStatusHandover,
		CreatedAt:  time.Now(),
	}
	if err := s.storage.CreateMessage(ctx, msg); err != nil {
		return "", fmt.Errorf("storing the message for its hook: %w", err)
	}
	why, err := s.typeText(ctx, agent, handoverLine(msg.ID, text), dialogs)
	var unsubmitted *UnsubmittedError
	switch {
	case errors.As(err, &unsubmitted):
		return "", err // the line sits in its input box: whoever submits it gets the message
	case err != nil:
		_, _ = s.storage.FailHandover(context.WithoutCancel(ctx), msg.ID)
		return "", err
	case why != "":
		return why, nil // maybe not submitted yet: the hook hands it over when it is
	}
	wait := s.handoverWait()
	deadline := time.Now().Add(wait)
	for {
		status, err := s.storage.GetMessageStatus(ctx, msg.ID)
		if err == nil && status != storage.MessageStatusHandover {
			return "", nil
		}
		if time.Now().After(deadline) {
			break
		}
		if err := sleepCtx(ctx, handoverPoll); err != nil {
			break
		}
	}
	if failed, err := s.storage.FailHandover(context.WithoutCancel(ctx), msg.ID); err != nil || !failed {
		return "", err // taken at the last moment
	}
	return "", &HandoverError{Agent: agent.Name, ID: msg.ID, Wait: wait}
}

// handoverLine is the line typed in place of message id: the start of its
// first line, and the token that names it.
func handoverLine(id int64, text string) string {
	head, rest, more := strings.Cut(strings.TrimSpace(text), "\n")
	head = strings.TrimSpace(strings.TrimSuffix(head, "\r"))
	if utf8.RuneCountInString(head) > maxHandoverHead {
		head = string([]rune(head)[:maxHandoverHead])
		more = true
	}
	if more || rest != "" {
		head += " …"
	}
	lines := strings.Count(strings.TrimRight(text, "\n"), "\n") + 1
	size := fmt.Sprintf("%s, %s", byteSize(len(text)), plural(lines, "line"))
	return fmt.Sprintf("%s [gomuxai message #%d, %s: the whole message follows as hook context]", head, id, size)
}

// byteSize formats n bytes for people: "512 B", "10.4 KB".
func byteSize(n int) string {
	if n < 1000 {
		return strconv.Itoa(n) + " B"
	}
	return strconv.FormatFloat(float64(n)/1000, 'f', 1, 64) + " KB"
}

// handoverOutput is the hook output for a prompt that names a message
// awaiting handover to name: its whole text, claimed so it goes out once.
// nil if the prompt names none, or the hook isn't the one that hands over.
func (s *AgentService) handoverOutput(ctx context.Context, name string, provider backend.Provider, hr backend.HookReport) []byte {
	ph, ok := provider.(backend.PromptHandover)
	if !ok {
		return nil
	}
	prompt, ok := ph.SubmittedPrompt(hr)
	if !ok {
		return nil
	}
	m := handoverToken.FindStringSubmatch(prompt)
	if m == nil {
		return nil
	}
	id, err := strconv.ParseInt(m[1], 10, 64)
	if err != nil {
		return nil
	}
	msg, err := s.storage.ClaimHandover(ctx, name, id)
	if err != nil || msg == nil {
		return nil
	}
	// A short header: Claude hands over up to 10,000 characters inline.
	return ph.HandoverOutput(fmt.Sprintf("gomuxai message #%d, whole:\n\n%s", id, msg.Content))
}
