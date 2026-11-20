package service

import (
	"context"
	"fmt"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// A CLI can show a dialog that no hook reports and its session status
// doesn't tell from idle (backend.DialogReader): Claude's offers, notices
// and setup wizards, which come up at startup or when a turn ends. Keys go
// to the dialog while it is up, so a message typed then is lost, and its
// Enter picks the dialog's highlighted option. So the screen is read for
// one whenever the agent would otherwise be idle: status reports it as
// waiting, with DialogTool as the tool; send holds messages (or refuses,
// for an agent that doesn't hold them) until it is gone; approve and deny
// answer it.

// DialogTool is the tool status names for such a dialog, and what approve
// and deny's --expect take to name it.
const DialogTool = "dialog"

// dialogDetailPrefix starts the detail of a dialog, before its title.
const dialogDetailPrefix = "dialog: "

// DialogTitle reports whether a status (and its detail, as status and the
// bus carry them) is a dialog, and its title.
func DialogTitle(status AgentStatus, detail string) (string, bool) {
	if status != AgentStatusWaiting || !strings.HasPrefix(detail, dialogDetailPrefix) {
		return "", false
	}
	title, _, _ := strings.Cut(strings.TrimPrefix(detail, dialogDetailPrefix), dialogHighlightedSep)
	return title, true
}

// IsDialog reports whether info is an agent waiting at a dialog its CLI
// shows (not a permission, question or trust prompt).
func (info *StatusInfo) IsDialog() bool {
	_, ok := DialogTitle(info.Status, info.Detail)
	return ok && info.Tool == DialogTool
}

// dialogHighlightedSep separates a dialog's title from its highlighted
// option in its detail.
const dialogHighlightedSep = "; Enter picks: "

// dialogFromDetail reads back the dialog a status detail describes.
func dialogFromDetail(detail string) backend.Dialog {
	title, high, _ := strings.Cut(strings.TrimPrefix(detail, dialogDetailPrefix), dialogHighlightedSep)
	return backend.Dialog{Title: title, Highlighted: high}
}

// dialogDetail is the status detail for d.
func dialogDetail(d backend.Dialog) string {
	detail := dialogDetailPrefix + d.Title
	if d.Highlighted != "" {
		detail += dialogHighlightedSep + d.Highlighted
	}
	return detail
}

// dialogOnScreen reads the agent's screen for a dialog its backend reports
// as one (backend.DialogReader). Permission, question and trust prompts
// have their own handling, so they don't count.
func (s *AgentService) dialogOnScreen(agent *storage.Agent) (backend.Dialog, bool) {
	provider, err := s.provider(agent)
	if err != nil {
		return backend.Dialog{}, false
	}
	dr, ok := provider.(backend.DialogReader)
	if !ok {
		return backend.Dialog{}, false
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0)
	if err != nil {
		return backend.Dialog{}, false
	}
	return dialogIn(provider, dr, screen)
}

// dialogIn reads a dialog off screen with dr, unless provider recognizes
// it as a permission, question or trust prompt.
func dialogIn(provider backend.Provider, dr backend.DialogReader, screen string) (backend.Dialog, bool) {
	if pr, ok := provider.(backend.PermissionResponder); ok && pr.PermissionPromptVisible(screen) {
		return backend.Dialog{}, false
	}
	if qp, ok := provider.(backend.QuestionPrompter); ok && qp.QuestionPromptVisible(screen) {
		return backend.Dialog{}, false
	}
	if tp, ok := provider.(backend.TrustPrompter); ok {
		if _, on := tp.TrustPrompt(screen); on {
			return backend.Dialog{}, false
		}
	}
	return dr.Dialog(screen)
}

// dialogStatus is the status of an agent whose resolved status, info, says
// it takes input (idle, error, unknown) or waits with nothing to say on
// what, while its screen shows a dialog: waiting, at that dialog. ok is
// false if the screen shows none.
func (s *AgentService) dialogStatus(agent *storage.Agent, info *StatusInfo) (*StatusInfo, bool) {
	switch info.Status {
	case AgentStatusIdle, AgentStatusError, AgentStatusUnknown:
	case AgentStatusWaiting:
		if info.Tool != "" || len(info.pending) > 0 {
			return nil, false // a prompt it already knows
		}
	default:
		return nil, false
	}
	d, ok := s.dialogOnScreen(agent)
	if !ok {
		return nil, false
	}
	waiting := *info
	waiting.Status, waiting.EndReason, waiting.Source = AgentStatusWaiting, "", StatusSourceScrape
	waiting.Tool, waiting.ToolInput, waiting.Detail = DialogTool, "", dialogDetail(d)
	waiting.pending, waiting.prompt = nil, nil
	return &waiting, true
}

// dialogSendErr is the error for typing into an agent at a dialog.
func dialogSendErr(agent *storage.Agent, d backend.Dialog) error {
	return fmt.Errorf("agent %s shows a dialog (%s), which takes what is typed, and the message's Enter would pick %s: %s",
		agent.Name, OneLine(d.Title), highlightedOr(d, "its highlighted option"), dialogHint(agent))
}

// highlightedOr names d's highlighted option, quoted, or says other.
func highlightedOr(d backend.Dialog, other string) string {
	if d.Highlighted == "" {
		return other
	}
	return fmt.Sprintf("%q", d.Highlighted)
}

// dialogHint says how a person gets an agent past a dialog.
func dialogHint(agent *storage.Agent) string {
	return fmt.Sprintf("agent deny %s closes it (Esc), agent approve %s picks the highlighted option (Enter), or agent capture %s shows it and agent key %s <key> answers it otherwise; held messages are typed in once it's gone",
		agent.Name, agent.Name, agent.Name, agent.SessionName)
}

// respondDialog answers the dialog info describes: approve presses the
// key that picks its highlighted option, deny the one that closes it. As
// for a permission prompt, the dialog must still be on screen right before
// the keypress. Held messages are typed once the agent is back at its
// prompt.
func (s *AgentService) respondDialog(ctx context.Context, agent *storage.Agent, info *StatusInfo, resp backend.PermissionResponse) (*StatusInfo, error) {
	provider, err := s.provider(agent)
	if err != nil {
		return nil, unloadedBackend(agent)
	}
	dr, ok := provider.(backend.DialogReader)
	if !ok {
		return nil, fmt.Errorf("agent %s runs backend %q, which can't answer a dialog: use agent keys %s@%s to press the keys yourself", agent.Name, agent.Backend, agent.Name, s.cfg.Host)
	}
	key, err := dr.DialogKey(resp)
	if err != nil {
		return nil, fmt.Errorf("agent %s: %w", agent.Name, err)
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0)
	if err != nil {
		return nil, fmt.Errorf("capturing screen: %w", err)
	}
	d, on := dialogIn(provider, dr, screen)
	if !on {
		return nil, fmt.Errorf("agent %s has no dialog on screen; it may have just been answered (agent status %s -v shows what it's doing)", agent.Name, agent.Name)
	}
	if title, _ := DialogTitle(info.Status, info.Detail); d.Title != title {
		return nil, fmt.Errorf("agent %s shows another dialog (%s) than the one its status described (%s): check it with agent status %s -v before answering it", agent.Name, OneLine(d.Title), OneLine(title), agent.Name)
	}
	if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
		return nil, err
	}
	if s.HeldCount(ctx, agent.Name) > 0 {
		_ = s.startDeliverer(agent.Name) // one already waiting keeps its lock
	}
	return info, nil
}
