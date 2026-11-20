package cli

import (
	"context"
	"fmt"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/termd"
)

func TestAdoptAndCleanupPreservesSession(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("G3D_TERMD_ID", "")

	userSess := fmt.Sprintf("rpc-cli-user-%d", os.Getpid())
	if err := tmuxClient.CreateSession(userSess, nil); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { tmuxClient.KillSession(userSess) })

	t.Setenv("G3D_TERMD_ID", userSess)
	adoptSession = ""
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

	// A process under the adopted session's pane is the adopted agent's,
	// even without AGENT_NAME set.
	pane, err := tmuxClient.(*termd.Client).FirstPane(userSess)
	if err != nil {
		t.Fatalf("FirstPane: %v", err)
	}
	leave := standUnder(t, pane.PID)
	if got := callerAgent(); got != "my-adopted" {
		t.Errorf("callerAgent() inside adopted pane = %q, want my-adopted", got)
	}
	leave()

	// Cleaning up the adopted agent (from outside its pane) leaves its session running.
	t.Setenv("G3D_TERMD_ID", "")
	cleanOut, err := stdout(t, func() error { return runCleanup(cleanupCmd, []string{"my-adopted"}) })
	if err != nil {
		t.Fatalf("runCleanup: %v", err)
	}
	if !strings.Contains(cleanOut, "left session "+userSess+" running") {
		t.Errorf("cleanup output = %q, want 'left session %s running'", cleanOut, userSess)
	}
	if exists, _ := tmuxClient.SessionExists(userSess); !exists {
		t.Errorf("adopted session %s was killed by cleanup", userSess)
	}
}

func TestAdoptRequiresTermdSession(t *testing.T) {
	setupCLI(t)
	t.Setenv("G3D_TERMD_ID", "")
	adoptSession = ""

	err := runAdopt(adoptCmd, []string{"no-termd"})
	if err == nil || !strings.Contains(err.Error(), "not in a termd session") {
		t.Errorf("err = %v, want not in a termd session", err)
	}
}

// agent resume refuses an adopted agent before it looks for a session or
// enters a directory: its session is the person's own.
func TestResumeRefusesAdoptedAgent(t *testing.T) {
	setupCLI(t)
	t.Setenv("AGENT_NAME", "")
	t.Setenv("G3D_TERMD_ID", "")
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
	want := "agent mine is an adopted session (session " + userSess + "): resume doesn't relaunch CLIs in sessions gomuxai didn't start; start the CLI there yourself (claude -c continues its last conversation), and adopt keeps tracking it"
	if err == nil || err.Error() != want {
		t.Errorf("resume err = %v\nwant %s", err, want)
	}
	if now, _ := os.Getwd(); now != wd {
		t.Errorf("resume moved to %s", now)
	}
	if exists, _ := tmuxClient.SessionExists(userSess); !exists {
		t.Error("the person's session is gone")
	}
}

// standUnder makes this test's commands run, as far as callerAgent can
// tell, in a process under pid (a pane's process, say), until leave is
// called or the test ends.
func standUnder(t *testing.T, pid int) (leave func()) {
	t.Helper()
	old := callerChain
	callerChain = func() ([]int, error) { return []int{os.Getpid(), pid}, nil }
	cachedCallerValid = false
	leave = func() { callerChain, cachedCallerValid = old, false }
	t.Cleanup(leave)
	return leave
}

// AGENT_NAME is inherited by whatever an agent starts, so a process under
// one agent's pane can carry another's (friction #48: a background session
// Claude Code ran under another agent's pane signed its messages with that
// agent's name). The sender is the agent the processes show; AGENT_NAME
// counts only where they show none.
func TestSenderIsTheAgentTheProcessesShow(t *testing.T) {
	setupCLI(t)
	ctx := context.Background()
	session := cfg.SessionName("sender-probe")
	if err := tmuxClient.CreateSession(session, nil); err != nil {
		t.Fatal(err)
	}
	if err := store.CreateAgent(ctx, &storage.Agent{Name: "sender-probe", SessionName: session, Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	pane, err := tmuxClient.(*termd.Client).FirstPane(session)
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("AGENT_NAME", "lead")
	leave := standUnder(t, pane.PID)
	if got := fromAgent("hi"); got != "[from sender-probe@local] hi" {
		t.Errorf("under worker's pane with AGENT_NAME=lead: %q", got)
	}
	if got := senderID(); got != "sender-probe" {
		t.Errorf("sender id %q, want sender-probe", got)
	}
	leave()
	// Outside every agent's pane, AGENT_NAME still names the sender.
	standUnder(t, 1)
	if got := fromAgent("hi"); got != "[from lead@local] hi" {
		t.Errorf("outside the panes: %q", got)
	}
}
