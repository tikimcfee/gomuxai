package cli

import (
	"context"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// setupWait is setupCLI (worker waiting on "permission: Bash: ls") plus a
// second agent, builder, in status builder, with wait's flags reset after.
func setupWait(t *testing.T, builder service.AgentStatus) {
	t.Helper()
	setupCLI(t)
	noBus(t)
	t.Cleanup(func() {
		waitFor, waitTimeout, waitInterval = "", 0, 0
		waitNext, waitAny, waitFollow = false, false, false
	})
	waitInterval = 20 * time.Millisecond
	addAgent(t, "builder", builder, "")
}

func addAgent(t *testing.T, name string, status service.AgentStatus, detail string) {
	t.Helper()
	ctx := context.Background()
	if err := store.CreateAgent(ctx, &storage.Agent{Name: name, SessionName: "rpc-cli-" + name, Backend: "claude", CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	report(t, name, status, detail)
}

func report(t *testing.T, name string, status service.AgentStatus, detail string) {
	t.Helper()
	if err := agentService.Report(context.Background(), name, service.StatusReport{Status: status, Detail: detail}); err != nil {
		t.Fatal(err)
	}
}

// later runs fn after d, in the background.
func later(d time.Duration, fn func()) {
	go func() {
		time.Sleep(d)
		fn()
	}()
}

func TestWaitSeveral_PrintsEachAsItArrives(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	report(t, "worker", service.AgentStatusProcessing, "")
	waitFor, waitNext = "idle,waiting", true
	later(50*time.Millisecond, func() { report(t, "builder", service.AgentStatusIdle, "") })
	later(150*time.Millisecond, func() { report(t, "worker", service.AgentStatusWaiting, "permission: Bash: rm -rf build") })

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"worker", "builder", "worker"}) })
	if want := "builder: idle\nworker: waiting (permission: Bash: rm -rf build)\n"; err != nil || out != want {
		t.Fatalf("wait = %q, %v; want %q", out, err, want)
	}
}

func TestWaitAny_ReturnsAtTheFirst(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	waitFor, waitAny = "idle,waiting", true

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"builder", "worker"}) })
	if want := "worker: waiting (permission: Bash: ls)\n"; err != nil || out != want {
		t.Fatalf("wait = %q, %v; want %q", out, err, want)
	}
}

// With no names, --any watches every agent on the host except the caller.
func TestWaitAny_NoNamesWatchesEveryOtherAgent(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	waitFor = "waiting"
	if err := runWait(waitCmd, nil); err == nil || !strings.Contains(err.Error(), "--any or --follow") {
		t.Fatalf("no names without --any: err = %v", err)
	}

	waitAny = true
	out, err := stdout(t, func() error { return runWait(waitCmd, nil) })
	if want := "worker: waiting (permission: Bash: ls)\n"; err != nil || out != want {
		t.Fatalf("wait = %q, %v; want %q", out, err, want)
	}

	t.Setenv("AGENT_NAME", "worker")
	waitTimeout = 100 * time.Millisecond
	out, err = stdout(t, func() error { return runWait(waitCmd, nil) })
	if out != "" || ExitCode(err) != waitTimeoutExitCode {
		t.Fatalf("wait as worker = %q, %v; want a timeout (builder never waits)", out, err)
	}
}

func TestWaitSeveral_UnknownNameFailsAtOnce(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	waitFor = "idle"
	start := time.Now()
	err := runWait(waitCmd, []string{"builder", "biulder"})
	if err == nil || !strings.Contains(err.Error(), "biulder") || ExitCode(err) != 1 {
		t.Fatalf("err = %v (exit %d); want a not-found error naming biulder", err, ExitCode(err))
	}
	if time.Since(start) > time.Second {
		t.Errorf("took %v; a missing name should fail before waiting", time.Since(start))
	}
}

// An agent that exits is reported, the others are still waited on, and the
// wait exits 1.
func TestWaitSeveral_OneExits(t *testing.T) {
	setupWait(t, service.AgentStatusExited)
	waitFor = "idle,waiting"

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"builder", "worker"}) })
	if out != "worker: waiting (permission: Bash: ls)\n" {
		t.Errorf("stdout = %q", out)
	}
	if want := "1 of 2 agents reached idle or waiting; not builder (exited)"; err == nil || err.Error() != want || ExitCode(err) != 1 {
		t.Fatalf("err = %v; want %q, exit 1", err, want)
	}
	if !strings.Contains(hints.String(), "agent wait: agent builder: status is exited") {
		t.Errorf("stderr = %q; want the exit reported", hints.String())
	}
}

func TestWaitSeveral_Timeout(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	waitFor, waitTimeout = "idle", 100*time.Millisecond
	later(20*time.Millisecond, func() { report(t, "worker", service.AgentStatusIdle, "") })

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"worker", "builder"}) })
	if out != "worker: idle\n" {
		t.Errorf("stdout = %q", out)
	}
	if want := "timed out after 100ms waiting for 1 agent to be idle (last status: builder processing)"; err == nil || err.Error() != want || ExitCode(err) != waitTimeoutExitCode {
		t.Fatalf("err = %v (exit %d); want %q, exit 2", err, ExitCode(err), want)
	}
}

// --follow prints each prompt once, including one that came and went
// between checks, until the timeout.
func TestWaitFollow_PrintsEachArrivalOnce(t *testing.T) {
	setupWait(t, service.AgentStatusIdle)
	waitFor, waitFollow, waitTimeout = "waiting", true, 400*time.Millisecond
	later(100*time.Millisecond, func() {
		report(t, "worker", service.AgentStatusProcessing, "approved")
		time.Sleep(50 * time.Millisecond)
		report(t, "worker", service.AgentStatusWaiting, "permission: Bash: pwd")
		time.Sleep(80 * time.Millisecond)
		// The next one comes and goes faster than a check.
		report(t, "worker", service.AgentStatusProcessing, "approved")
		report(t, "worker", service.AgentStatusWaiting, "permission: Bash: whoami")
	})

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"worker"}) })
	want := "worker: waiting (permission: Bash: ls)\nworker: waiting (permission: Bash: pwd)\n"
	if !strings.HasPrefix(out, want) || !strings.Contains(out, "whoami") || strings.Count(out, "\n") != 3 {
		t.Errorf("stdout = %q; want the three prompts once each", out)
	}
	if ExitCode(err) != waitTimeoutExitCode {
		t.Errorf("err = %v; want the timeout's exit 2", err)
	}
}

// With no names, --follow picks up agents spawned while it runs.
func TestWaitFollow_NoNamesPicksUpNewAgents(t *testing.T) {
	setupWait(t, service.AgentStatusIdle)
	old := waitRelistInterval
	t.Cleanup(func() { waitRelistInterval = old })
	waitRelistInterval = 30 * time.Millisecond
	waitFor, waitFollow, waitTimeout = "waiting", true, 300*time.Millisecond
	later(100*time.Millisecond, func() { addAgent(t, "late", service.AgentStatusWaiting, "permission: Edit: a.go") })

	out, _ := stdout(t, func() error { return runWait(waitCmd, nil) })
	want := "worker: waiting (permission: Bash: ls)\nlate: waiting (permission: Edit: a.go)\n"
	if out != want {
		t.Errorf("stdout = %q, want %q", out, want)
	}
}
