package cli

import (
	"context"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/tmux"
)

func TestAdoptAndCleanupPreservesSession(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("TMUX_PANE", "")

	userSess := "rpc-cli-user-session"
	if err := tmuxClient.CreateSession(userSess, nil); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { tmuxClient.KillSession(userSess) })

	adoptSession = userSess
	adoptResume = ""
	adoptContinue = false
	backendName = ""
	t.Cleanup(func() {
		adoptSession, adoptResume, adoptContinue, backendName = "", "", false, ""
	})

	out, err := stdout(t, func() error { return runAdopt(adoptCmd, []string{"my-adopted"}) })
	if err != nil {
		t.Fatalf("runAdopt: %v", err)
	}
	if !strings.Contains(out, "Adopted agent my-adopted") || !strings.Contains(out, userSess) {
		t.Errorf("adopt output = %q, want Adopted agent my-adopted and session %s", out, userSess)
	}

	agent, err := store.GetAgent(context.Background(), "my-adopted")
	if err != nil || agent == nil || !agent.Adopted || agent.SessionName != userSess {
		t.Fatalf("stored agent = %+v, %v", agent, err)
	}

	// When TMUX_PANE points at the adopted session's pane, callerAgent() resolves
	// the adopted agent even without AGENT_NAME set.
	pane, err := tmuxClient.(*tmux.Client).FirstPane(userSess)
	if err != nil {
		t.Fatalf("FirstPane: %v", err)
	}
	t.Setenv("TMUX_PANE", pane.ID)
	if got := callerAgent(); got != "my-adopted" {
		t.Errorf("callerAgent() inside adopted pane = %q, want my-adopted", got)
	}
	// A secondary pane in the same session does not impersonate the first pane's agent.
	if splitOut, sErr := tmuxClient.Capture(userSess, 1); sErr == nil {
		_ = splitOut
	}

	// Cleaning up the adopted agent (from outside its pane) leaves its tmux session running.
	t.Setenv("TMUX_PANE", "")
	cleanOut, err := stdout(t, func() error { return runCleanup(cleanupCmd, []string{"my-adopted"}) })
	if err != nil {
		t.Fatalf("runCleanup: %v", err)
	}
	if !strings.Contains(cleanOut, "left session "+userSess+" running") {
		t.Errorf("cleanup output = %q, want 'left session %s running'", cleanOut, userSess)
	}
	if exists, _ := tmuxClient.SessionExists(userSess); !exists {
		t.Errorf("adopted tmux session %s was killed by cleanup", userSess)
	}
}
