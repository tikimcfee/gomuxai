package cli

import (
	"context"
	"fmt"
	"os"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/tmux"
)

func TestAdoptAndCleanupPreservesSession(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("TMUX_PANE", "")

	userSess := fmt.Sprintf("rpc-cli-user-%d", os.Getpid())
	if err := tmuxClient.CreateSession(userSess, nil); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { tmuxClient.KillSession(userSess) })

	adoptSession = userSess
	backendName = ""
	t.Cleanup(func() {
		adoptSession, backendName = "", ""
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

func TestAdoptRequiresTmuxSession(t *testing.T) {
	setupCLI(t)
	t.Setenv("TMUX_PANE", "")
	adoptSession = ""

	err := runAdopt(adoptCmd, []string{"no-tmux"})
	if err == nil || !strings.Contains(err.Error(), "not in a tmux session") {
		t.Errorf("err = %v, want not in a tmux session", err)
	}
}

// agent resume refuses an adopted agent before it looks for a session or
// enters a directory: its tmux session is the person's own.
func TestResumeRefusesAdoptedAgent(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("TMUX_PANE", "")
	userSess := fmt.Sprintf("rpc-cli-user-resume-%d", os.Getpid())
	if err := tmuxClient.CreateSession(userSess, nil); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { tmuxClient.KillSession(userSess) })
	adoptSession, backendName = userSess, "claude"
	t.Cleanup(func() { adoptSession, backendName = "", "" })
	if _, err := stdout(t, func() error { return runAdopt(adoptCmd, []string{"mine"}) }); err != nil {
		t.Fatal(err)
	}
	wd, _ := os.Getwd()

	err := runResume(resumeCmd, []string{"mine"})
	want := "agent mine is an adopted session (tmux session " + userSess + "): resume doesn't relaunch CLIs in sessions gomuxai didn't start; start the CLI there yourself (claude -c continues its last conversation), and adopt keeps tracking it"
	if err == nil || err.Error() != want {
		t.Errorf("resume err = %v\nwant %s", err, want)
	}
	if now, _ := os.Getwd(); now != wd {
		t.Errorf("resume moved to %s", now)
	}
	if exists, _ := tmuxClient.SessionExists(userSess); !exists {
		t.Error("the person's tmux session is gone")
	}
}
