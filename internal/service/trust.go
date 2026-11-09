package service

import (
	"context"
	"fmt"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// A CLI started in a folder it hasn't trusted (Claude Code, agy) first asks
// whether to trust it. Until it is answered the CLI runs no hooks and
// Claude's session registry doesn't list the session, so only the screen
// shows the prompt: status reads it there as waiting, with TrustTool as the
// tool, and approve and deny answer it.

// TrustTool is the tool status names for a workspace trust prompt, and what
// approve and deny's --expect take to name it.
const TrustTool = "trust"

// trustDetailPrefix starts the detail of a trust prompt, before its folder.
const trustDetailPrefix = "trust: "

// Events approve and deny record for a trust prompt they answered.
const (
	EventTrusted       = "trusted"
	EventTrustDeclined = "trust-declined"
)

// trustStartGrace is how long after approve trusts a folder the CLI may
// take to show up in its own session list (Claude's registry) before its
// absence means it died.
const trustStartGrace = 30 * time.Second

// TrustPromptFolder reports whether a status (and its detail, as status and
// the bus carry them) is a workspace trust prompt, and the folder it asks
// about.
func TrustPromptFolder(status AgentStatus, detail string) (string, bool) {
	if status != AgentStatusWaiting || !strings.HasPrefix(detail, trustDetailPrefix) {
		return "", false
	}
	return strings.TrimPrefix(detail, trustDetailPrefix), true
}

// IsTrustPrompt reports whether info is an agent waiting at its CLI's
// workspace trust prompt.
func (info *StatusInfo) IsTrustPrompt() bool {
	_, ok := TrustPromptFolder(info.Status, info.Detail)
	return ok && info.Tool == TrustTool
}

// trustPrompter returns agent's backend as a TrustPrompter, if it is one.
func (s *AgentService) trustPrompter(agent *storage.Agent) (backend.TrustPrompter, bool) {
	provider, err := s.provider(agent)
	if err != nil {
		return nil, false
	}
	tp, ok := provider.(backend.TrustPrompter)
	return tp, ok
}

// trustPromptOnScreen reads the agent's screen for its CLI's trust prompt,
// returning the folder it asks about (the agent's own if the screen
// doesn't show it).
func (s *AgentService) trustPromptOnScreen(agent *storage.Agent) (string, bool) {
	tp, ok := s.trustPrompter(agent)
	if !ok {
		return "", false
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0)
	if err != nil {
		return "", false
	}
	folder, ok := tp.TrustPrompt(screen)
	if !ok {
		return "", false
	}
	if folder == "" {
		folder = agent.Cwd
	}
	if folder == "" {
		folder = "its folder"
	}
	return folder, true
}

// trustPromptStatus is the status of an agent whose CLI shows its trust
// prompt. It is looked for only while nothing has reported (the prompt
// comes before any hook) or the last report is approve or deny answering
// it, in case the keys didn't take.
func (s *AgentService) trustPromptStatus(agent *storage.Agent, reported *StatusInfo) (*StatusInfo, bool) {
	if reported != nil && reported.Event != EventTrusted && reported.Event != EventTrustDeclined {
		return nil, false
	}
	folder, ok := s.trustPromptOnScreen(agent)
	if !ok {
		return nil, false
	}
	return &StatusInfo{Status: AgentStatusWaiting, Source: StatusSourceScrape, Tool: TrustTool, Detail: trustDetailPrefix + folder}, true
}

// trustStarting reports whether reported is approve's report of trusting
// the folder, recent enough that the CLI may still be starting.
func trustStarting(reported *StatusInfo) bool {
	return reported != nil && reported.Event == EventTrusted && time.Since(reported.UpdatedAt) < trustStartGrace
}

// TrustBackendName is how messages name the CLI behind a backend: Claude
// for claude, the backend's name otherwise.
func TrustBackendName(backendName string) string {
	if backendName == "claude" {
		return "Claude"
	}
	return backendName
}

// respondTrust answers the workspace trust prompt info describes: approve
// trusts the folder, deny picks "No, exit" (the CLI exits). It checks the
// prompt is still on screen right before the keypress, as RespondPermission
// does for a permission prompt, and reads which option is highlighted from
// that same screen.
func (s *AgentService) respondTrust(ctx context.Context, agent *storage.Agent, info *StatusInfo, resp backend.PermissionResponse) (*StatusInfo, error) {
	folder, _ := TrustPromptFolder(info.Status, info.Detail)
	if resp == backend.PermissionApproveAlways {
		return nil, fmt.Errorf("agent %s is at its workspace trust prompt for %s, which has no \"always\" answer: agent approve %s trusts the folder for good", agent.Name, folder, agent.Name)
	}
	tp, ok := s.trustPrompter(agent)
	if !ok {
		return nil, fmt.Errorf("agent %s runs backend %q, which can't answer a trust prompt: use agent keys %s@%s to press the keys yourself", agent.Name, agent.Backend, agent.Name, s.cfg.Host)
	}
	screen, err := s.tmux.Capture(agent.SessionName, 0)
	if err != nil {
		return nil, fmt.Errorf("capturing screen: %w", err)
	}
	if _, ok := tp.TrustPrompt(screen); !ok {
		return nil, fmt.Errorf("agent %s has no workspace trust prompt on screen; it may have just been answered (agent status %s -v shows what it's doing)", agent.Name, agent.Name)
	}
	keys, report := tp.TrustKeys(screen), StatusReport{
		Status: AgentStatusProcessing, Event: EventTrusted,
		Detail: fmt.Sprintf("trusted %s; its CLI is starting", folder),
	}
	if resp == backend.PermissionDeny {
		keys, report = tp.DeclineTrustKeys(screen), StatusReport{
			Status: AgentStatusEnded, EndReason: backend.EndQuit, Event: EventTrustDeclined,
			Detail: fmt.Sprintf("declined to trust %s, so its CLI exited", folder),
		}
	}
	if keys == nil {
		return nil, fmt.Errorf("agent %s's trust prompt shows neither option highlighted, so the keys to answer it are unknown: agent capture %s shows it; answer it in its session (%s)", agent.Name, agent.Name, agent.SessionName)
	}
	for _, key := range keys {
		if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
			return nil, err
		}
	}
	// The prompt fired no hook, and the CLI won't report until it has
	// started (or, declined, at all): record what was answered.
	if err := s.Report(ctx, agent.Name, report); err != nil {
		return info, err
	}
	if resp != backend.PermissionDeny && s.HeldCount(ctx, agent.Name) > 0 {
		// Spawn held its task while the prompt was up: type it once the
		// CLI is at its prompt.
		_ = s.startDeliverer(agent.Name)
	}
	return info, nil
}

// holdFirstMessage holds a just-spawned agent's first message if its CLI
// shows its trust prompt or another dialog (see dialog.go), where typing it
// would be lost (or its Enter answer the dialog). Approving the trust
// prompt delivers it; past another dialog, the deliverer types it once the
// agent is at its prompt. It reports whether the message was held.
func (s *AgentService) holdFirstMessage(ctx context.Context, opts SpawnOptions, agent *storage.Agent, message string) (bool, error) {
	provider, err := s.provider(agent)
	if err != nil {
		return false, nil
	}
	_, trusts := provider.(backend.TrustPrompter)
	_, dialogs := provider.(backend.DialogReader)
	if !trusts && !dialogs {
		return false, nil
	}
	if sc, ok := s.submitChecker(agent); ok {
		// The CLI may still be drawing: wait for its input box, or a
		// dialog in its place, as typing a message does.
		if _, why := s.inputBefore(agent, sc); why != "" {
			// A dialog ends the wait with an error, which is the point:
			// whether it is the trust prompt is read below.
			_, _, _ = s.awaitInputBox(ctx, agent, sc, true)
			if err := ctx.Err(); err != nil {
				return false, err
			}
		}
	}
	_, trust := s.trustPromptOnScreen(agent)
	_, dialog := s.dialogOnScreen(agent)
	if !trust && !dialog {
		return false, nil
	}
	from := opts.SpawnedBy
	if from == "" {
		from = "user"
	}
	msg := &storage.Message{SenderID: from, ReceiverID: agent.Name, Content: message, Status: storage.MessageStatusPending, CreatedAt: time.Now()}
	if err := s.storage.CreateMessage(ctx, msg); err != nil {
		return false, fmt.Errorf("holding the first message: %w", err)
	}
	if dialog {
		_ = s.startDeliverer(agent.Name) // best effort: a send or agent deliver types it too
	}
	return true, nil
}
