package cli

import (
	"context"
	"io"
	"os"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/await"
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
	createLiveAgent(t, &storage.Agent{Name: name, SessionName: "rpc-cli-" + name, Backend: "claude", CreatedAt: time.Now()})
	report(t, name, status, detail)
}

func report(t *testing.T, name string, status service.AgentStatus, detail string) {
	t.Helper()
	if err := agentService.Report(context.Background(), name, service.StatusReport{Status: status, Detail: detail}); err != nil {
		t.Fatal(err)
	}
}

// startTurn records what `agent send` leaves behind: a message sent, then
// the turn it started. --next then counts the turn's end whenever it lands,
// even before wait's first check, so a test needn't time it after one.
func startTurn(t *testing.T, name string) {
	t.Helper()
	ctx := context.Background()
	now := time.Now()
	if err := store.SetState(ctx, &storage.StateEntry{AgentName: name, Key: service.StateKeySentAt, Value: now.Format(time.RFC3339Nano), UpdatedAt: now}); err != nil {
		t.Fatal(err)
	}
	if err := agentService.Report(ctx, name, service.StatusReport{Status: service.AgentStatusProcessing, ClearResult: true}); err != nil {
		t.Fatal(err)
	}
}

// liveStdout is stdout, with what fn prints readable from out while it runs.
func liveStdout(t *testing.T, out *syncBuffer, fn func() error) (string, error) {
	t.Helper()
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	copied := make(chan struct{})
	go func() {
		io.Copy(out, r)
		close(copied)
	}()
	old := os.Stdout
	os.Stdout = w
	runErr := fn()
	w.Close()
	os.Stdout = old
	<-copied
	return out.String(), runErr
}

// String returns everything written so far.
func (b *syncBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.String()
}

// whenPrinted runs fn in the background once out holds line: wait has then
// handled everything before it, so what fn changes comes after. It never
// runs fn after the test has ended.
func whenPrinted(t *testing.T, out *syncBuffer, line string, fn func()) {
	stop, done := make(chan struct{}), make(chan struct{})
	t.Cleanup(func() {
		close(stop)
		<-done
	})
	go func() {
		defer close(done)
		stopped := func() bool {
			select {
			case <-stop:
				return true
			default:
				return false
			}
		}
		_, printed := await.Poll(await.Timeout, func() (string, bool) {
			return "", stopped() || slices.Contains(out.lines(), line)
		})
		if printed && !stopped() {
			fn()
		}
	}()
}

func TestWaitSeveral_PrintsEachAsItArrives(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	startTurn(t, "worker")
	startTurn(t, "builder")
	waitFor, waitNext = "idle,waiting", true
	report(t, "builder", service.AgentStatusIdle, "")
	live := &syncBuffer{}
	whenPrinted(t, live, "builder: idle", func() {
		report(t, "worker", service.AgentStatusWaiting, "permission: Bash: rm -rf build")
	})

	out, err := liveStdout(t, live, func() error { return runWait(waitCmd, []string{"worker", "builder", "worker"}) })
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
	setupWait(t, service.AgentStatusEnded)
	waitFor = "idle,waiting"

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"builder", "worker"}) })
	if out != "worker: waiting (permission: Bash: ls)\n" {
		t.Errorf("stdout = %q", out)
	}
	if want := "1 of 2 agents reached idle or waiting; not builder (ended)"; err == nil || err.Error() != want || ExitCode(err) != 1 {
		t.Fatalf("err = %v; want %q, exit 1", err, want)
	}
	if !strings.Contains(hints.String(), "agent wait: agent builder: status is ended") {
		t.Errorf("stderr = %q; want the exit reported", hints.String())
	}
}

func TestWaitSeveral_Timeout(t *testing.T) {
	setupWait(t, service.AgentStatusProcessing)
	// The timeout is what's tested; it only has to outlast wait's first
	// checks on a loaded machine.
	waitFor, waitTimeout = "idle", 500*time.Millisecond
	report(t, "worker", service.AgentStatusIdle, "")

	out, err := stdout(t, func() error { return runWait(waitCmd, []string{"worker", "builder"}) })
	if out != "worker: idle\n" {
		t.Errorf("stdout = %q", out)
	}
	if want := "timed out after 500ms waiting for 1 agent to be idle (last status: builder processing)"; err == nil || err.Error() != want || ExitCode(err) != waitTimeoutExitCode {
		t.Fatalf("err = %v (exit %d); want %q, exit 2", err, ExitCode(err), want)
	}
}

// --follow prints each prompt once, including one that came and went
// between checks, until the timeout.
func TestWaitFollow_PrintsEachArrivalOnce(t *testing.T) {
	setupWait(t, service.AgentStatusIdle)
	// --follow runs until the timeout: long enough for the three prompts
	// on a loaded machine.
	waitFor, waitFollow, waitTimeout = "waiting", true, time.Second
	live := &syncBuffer{}
	whenPrinted(t, live, "worker: waiting (permission: Bash: ls)", func() {
		report(t, "worker", service.AgentStatusProcessing, "approved")
		report(t, "worker", service.AgentStatusWaiting, "permission: Bash: pwd")
	})
	whenPrinted(t, live, "worker: waiting (permission: Bash: pwd)", func() {
		// The next one comes and goes faster than a check.
		report(t, "worker", service.AgentStatusProcessing, "approved")
		report(t, "worker", service.AgentStatusWaiting, "permission: Bash: whoami")
	})

	out, err := liveStdout(t, live, func() error { return runWait(waitCmd, []string{"worker"}) })
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
	// --follow runs until the timeout: long enough for a relist on a
	// loaded machine.
	waitFor, waitFollow, waitTimeout = "waiting", true, time.Second
	live := &syncBuffer{}
	whenPrinted(t, live, "worker: waiting (permission: Bash: ls)", func() {
		addAgent(t, "late", service.AgentStatusWaiting, "permission: Edit: a.go")
	})

	out, _ := liveStdout(t, live, func() error { return runWait(waitCmd, nil) })
	want := "worker: waiting (permission: Bash: ls)\nlate: waiting (permission: Edit: a.go)\n"
	if out != want {
		t.Errorf("stdout = %q, want %q", out, want)
	}
}
