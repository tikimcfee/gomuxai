// Package tmux_test provides integration tests for tmux operations.
// These tests require tmux to be installed and available.
package tmux_test

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

// testSessionPrefix ensures unique session names for tests
const testSessionPrefix = "gomuxai-test-"

// uniqueSessionName generates a unique session name for tests
func uniqueSessionName(t *testing.T) string {
	return fmt.Sprintf("%s%s-%d", testSessionPrefix, t.Name(), time.Now().UnixNano())
}

// cleanupSession kills a session if it exists (best-effort cleanup)
func cleanupSession(name string) {
	exec.Command("tmux", "kill-session", "-t", name).Run()
}

// requireTmux skips the test if tmux is not available
func requireTmux(t *testing.T) {
	t.Helper()
	if _, err := exec.LookPath("tmux"); err != nil {
		t.Skip("tmux not available, skipping integration test")
	}
}

// TestCreateSession tests session creation
func TestCreateSession(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	// Verify session exists
	exists, err := client.SessionExists(sessionName)
	if err != nil {
		t.Fatalf("checking session exists: %v", err)
	}
	if !exists {
		t.Error("session should exist after creation")
	}
}

// TestCreateSession_Duplicate tests that creating a duplicate session fails
func TestCreateSession_Duplicate(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create session first time
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	// Try to create again - should fail
	err = client.CreateSession(sessionName, nil)
	if err == nil {
		t.Error("expected error when creating duplicate session")
	}
	if !strings.Contains(err.Error(), "already exists") {
		t.Errorf("expected 'already exists' error, got: %v", err)
	}
}

// TestCreateSession_WithEnv tests session creation with environment variables
func TestCreateSession_WithEnv(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	env := map[string]string{
		"TEST_VAR": "test_value",
	}

	err = client.CreateSession(sessionName, env)
	if err != nil {
		t.Fatalf("creating session with env: %v", err)
	}

	// Session should exist
	exists, err := client.SessionExists(sessionName)
	if err != nil {
		t.Fatalf("checking session exists: %v", err)
	}
	if !exists {
		t.Error("session should exist after creation")
	}

	// The session's first shell must see the variable, not just later panes
	if err := client.SendKeys(sessionName, `echo "TEST_VAR=[$TEST_VAR]"`); err != nil {
		t.Fatalf("sending keys: %v", err)
	}
	if err := client.SendSpecialKey(sessionName, "Enter"); err != nil {
		t.Fatalf("sending enter: %v", err)
	}
	waitLine(t, client, sessionName, "TEST_VAR=[test_value]")
}

// TestKillSession tests session termination
func TestKillSession(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName) // cleanup in case test fails

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	// Kill session
	err = client.KillSession(sessionName)
	if err != nil {
		t.Fatalf("killing session: %v", err)
	}

	// Verify session no longer exists
	exists, err := client.SessionExists(sessionName)
	if err != nil {
		t.Fatalf("checking session exists: %v", err)
	}
	if exists {
		t.Error("session should not exist after kill")
	}
}

// TestKillSession_NonExistent tests killing a non-existent session
func TestKillSession_NonExistent(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	err = client.KillSession("nonexistent-session-12345")
	if err == nil {
		t.Error("expected error when killing non-existent session")
	}
}

// TestSessionExists tests existence checking
func TestSessionExists(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Should not exist initially
	exists, err := client.SessionExists(sessionName)
	if err != nil {
		t.Fatalf("checking session exists: %v", err)
	}
	if exists {
		t.Error("session should not exist before creation")
	}

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	// Should exist now
	exists, err = client.SessionExists(sessionName)
	if err != nil {
		t.Fatalf("checking session exists: %v", err)
	}
	if !exists {
		t.Error("session should exist after creation")
	}
}

// TestListSessions tests session enumeration
func TestListSessions(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	// List sessions
	sessions, err := client.ListSessions()
	if err != nil {
		t.Fatalf("listing sessions: %v", err)
	}

	// Find our session
	found := false
	for _, s := range sessions {
		if s.Name == sessionName {
			found = true
			if s.Windows < 1 {
				t.Error("session should have at least 1 window")
			}
			break
		}
	}
	if !found {
		t.Errorf("session %q not found in list", sessionName)
	}
}

// TestSendKeys tests key transmission
func TestSendKeys(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)

	// The command line says hel''lo, so only the command's output is hello.
	typeLine(t, client, sessionName, "echo hel''lo")
	waitLine(t, client, sessionName, "hello")
}

// SendKeys carries `agent send` text, which can come from another host over
// the bus. Text that is also a tmux key name or starts with '-' must be typed
// as is, not sent as that key or parsed as a send-keys flag.
func TestSendKeys_IsLiteral(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)

	// cat -v shows control keys as ^X, so a key sent instead of the text
	// shows up differently (or, for C-c, ends cat).
	startCat(t, client, sessionName)

	texts := []string{"Escape", "C-c", "0x41", "^A", "-R", "-t%0", "--"}
	for _, text := range texts {
		if err := client.SendKeys(sessionName, text); err != nil {
			t.Fatalf("sending %q: %v", text, err)
		}
		client.SendSpecialKey(sessionName, "Enter")
	}
	// A special key starting with '-' isn't a flag either.
	client.SendSpecialKey(sessionName, "-N2")
	client.SendSpecialKey(sessionName, "Enter")

	// Each one typed by the terminal, then echoed by cat.
	waitPane(t, client, sessionName, 50, "each text typed literally and echoed by cat", func(screen string) bool {
		for _, text := range append(texts, "-N2") {
			if countLine(screen, text) < 2 {
				return false
			}
		}
		return true
	})
}

// A pane in copy mode (a mouse-wheel scroll enters it with -e) took typed
// text as copy-mode commands: send-keys failed with "not in a mode" once
// one left the mode, and nothing reached the program (friction #40). Text
// and keys now go to the program, out of whatever mode the pane was in.
func TestSendKeys_PaneInAMode(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)
	// tmux's default; a user's config may pick vi.
	if out, err := exec.Command("tmux", "set-option", "-w", "-t", sessionName+":", "mode-keys", "emacs").CombinedOutput(); err != nil {
		t.Fatalf("setting mode-keys: %v: %s", err, out)
	}
	startCat(t, client, sessionName)

	inMode := func() string {
		out, err := exec.Command("tmux", "display-message", "-p", "-t", sessionName+":", "#{pane_in_mode}").Output()
		if err != nil {
			t.Fatalf("reading pane_in_mode: %v", err)
		}
		return strings.TrimSpace(string(out))
	}
	enter := func(mode ...string) {
		t.Helper()
		if out, err := exec.Command("tmux", append(mode, "-t", sessionName+":")...).CombinedOutput(); err != nil {
			t.Fatalf("%v: %v: %s", mode, err, out)
		}
		if inMode() != "1" {
			t.Fatalf("%v: pane not in a mode", mode)
		}
	}

	msg := "[from glyph3d-js-root@monolith1] [CF] One more fix, urgent: read …/briefs/e2e-shared-fix.md and do it; tell me when gated."
	enter("copy-mode", "-e")
	if err := client.SendKeys(sessionName, msg); err != nil {
		t.Fatalf("typing into a pane in copy mode: %v", err)
	}
	if err := client.SendSpecialKey(sessionName, "Enter"); err != nil {
		t.Fatalf("Enter into a pane in copy mode: %v", err)
	}
	// A paste (how messages are typed) leaves the mode too.
	enter("copy-mode", "-e")
	if err := client.PasteText(sessionName, "pasted out of copy mode"); err != nil {
		t.Fatalf("pasting into a pane in copy mode: %v", err)
	}
	if inMode() != "0" {
		t.Error("pane still in copy mode after a paste")
	}
	client.SendSpecialKey(sessionName, "Enter")
	enter("clock-mode")
	if err := client.SendSpecialKey(sessionName, "x"); err != nil {
		t.Fatalf("a key into a pane in clock mode: %v", err)
	}
	client.SendSpecialKey(sessionName, "Enter")

	if inMode() != "0" {
		t.Error("pane still in a mode after typing")
	}
	// cat -v shows the bytes as UTF-8 escapes (M-...), so look for the
	// ASCII ends of the message: typed by the terminal, then echoed by cat.
	waitPane(t, client, sessionName, 50, "the message, the paste and the key, each typed and echoed by cat", func(screen string) bool {
		return strings.Count(screen, "tell me when gated.") >= 2 &&
			strings.Count(screen, "pasted out of copy mode") >= 2 &&
			countLine(screen, "x") >= 2
	})
}

// TestSendSpecialKey tests special key transmission
func TestSendSpecialKey(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)

	// C-c discards the half-typed line.
	if err := client.SendKeys(sessionName, "echo cancel''led"); err != nil {
		t.Fatalf("sending keys: %v", err)
	}
	if err := client.SendSpecialKey(sessionName, "C-c"); err != nil {
		t.Fatalf("sending C-c: %v", err)
	}
	// The shell takes keys in order, so once the next line has run, the
	// C-c has been handled.
	typeLine(t, client, sessionName, "echo after-$((40+2))")
	screen := waitLine(t, client, sessionName, "after-42")
	if countLine(screen, "cancelled") != 0 {
		t.Errorf("the line before C-c ran; pane:\n%s", screen)
	}
}

// TestCapture tests output capture
func TestCapture(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)

	// Whole lines: the command line holds the words, not the lines.
	typeLine(t, client, sessionName, "echo line1; echo line2; echo line3")
	waitPane(t, client, sessionName, 50, "lines line1, line2 and line3", func(screen string) bool {
		return countLine(screen, "line1") > 0 && countLine(screen, "line2") > 0 && countLine(screen, "line3") > 0
	})
}

// TestCapture_StripANSI tests ANSI stripping in capture
func TestCapture_StripANSI(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)

	// Send ANSI-colored output
	typeLine(t, client, sessionName, `printf "\033[31mred-%s\033[0m\n" $((40+2))`)
	output := waitLine(t, client, sessionName, "red-42")
	if strings.Contains(output, "\033[") {
		t.Errorf("expected ANSI codes to be stripped; got %q", output)
	}
}

// TestPipePane tests log piping
func TestPipePane(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)
	logPath := filepath.Join(t.TempDir(), "pipe.log")

	// Start pipe-pane
	err = client.PipePane(sessionName, logPath)
	if err != nil {
		t.Fatalf("piping pane: %v", err)
	}
	typeLine(t, client, sessionName, "echo pipe-$((40+2))-out")
	waitFile(t, logPath, "pipe-42-out")

	// Stop pipe-pane
	err = client.StopPipePane(sessionName)
	if err != nil {
		t.Fatalf("stopping pipe: %v", err)
	}
}

// TestPipePane_ShellInjection tests that shell injection is prevented
// CRITICAL: This test MUST fail before the fix and pass after
func TestPipePane_ShellInjection(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)
	tmpDir := t.TempDir()

	// Test multiple injection vectors
	injectionTests := []struct {
		name        string
		malicious   string
		description string
	}{
		{
			name:        "backtick",
			malicious:   "log`touch %s/INJECTED_BACKTICK`.txt",
			description: "backtick command substitution",
		},
		{
			name:        "subshell",
			malicious:   "log$(touch %s/INJECTED_SUBSHELL).txt",
			description: "$() command substitution",
		},
	}

	for _, tc := range injectionTests {
		t.Run(tc.name, func(t *testing.T) {
			markerFile := filepath.Join(tmpDir, "INJECTED_"+strings.ToUpper(tc.name))
			maliciousPath := filepath.Join(tmpDir, fmt.Sprintf(tc.malicious, tmpDir))
			// The path names directories (the touch's argument has
			// slashes): make them, so a safely quoted path is a file cat
			// can write. That it gets the pane's output is the sign the
			// pipe's shell has run, and with it any injected command.
			if err := os.MkdirAll(filepath.Dir(maliciousPath), 0o755); err != nil {
				t.Fatal(err)
			}

			err := client.PipePane(sessionName, maliciousPath)
			reached := true
			if err == nil {
				typeLine(t, client, sessionName, "echo injection-$((40+2))-"+tc.name)
				_, reached = await.Poll(await.Timeout, func() (string, bool) {
					if _, err := os.Stat(markerFile); err == nil {
						return "injected", true
					}
					data, _ := os.ReadFile(maliciousPath)
					return string(data), strings.Contains(string(data), "injection-42-"+tc.name)
				})
				client.StopPipePane(sessionName)
			}

			// Check if injection occurred
			if _, err := os.Stat(markerFile); err == nil {
				t.Errorf("SHELL INJECTION VULNERABILITY via %s!", tc.description)
				t.Error("The PipePane function is vulnerable to command injection.")
				t.Error("Malicious input was able to execute arbitrary commands.")
			} else if !reached {
				t.Errorf("the pane's output never reached %q, so its pipe never ran", maliciousPath)
			}
		})
	}
}

// TestPipePane_SpecialCharacters tests paths with special characters
func TestPipePane_SpecialCharacters(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)
	tmpDir := t.TempDir()

	testCases := []struct {
		name     string
		filename string
	}{
		{"spaces", "file with spaces.log"},
		{"quotes", "file'with'quotes.log"},
		{"dollar", "file$var.log"},
		{"backtick", "file`cmd`.log"},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			logPath := filepath.Join(tmpDir, tc.filename)

			err := client.PipePane(sessionName, logPath)
			if err != nil {
				// It's OK if it fails with an error - that's safe
				t.Logf("PipePane with %q failed (safe): %v", tc.filename, err)
				return
			}
			defer client.StopPipePane(sessionName)

			// If it succeeded, the file by that exact name gets the output.
			typeLine(t, client, sessionName, "echo special-$((40+2))-"+tc.name)
			waitFile(t, logPath, "special-42-"+tc.name)
		})
	}
}

// TestStopPipePane tests stopping log piping
func TestStopPipePane(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)
	logPath := filepath.Join(t.TempDir(), "stop-pipe.log")

	// Start pipe
	err = client.PipePane(sessionName, logPath)
	if err != nil {
		t.Fatalf("piping pane: %v", err)
	}

	// Output before the stop reaches the file.
	typeLine(t, client, sessionName, "echo before-$((40+2))-stop")
	waitFile(t, logPath, "before-42-stop")

	// Stop pipe
	err = client.StopPipePane(sessionName)
	if err != nil {
		t.Fatalf("stopping pipe: %v", err)
	}
	if p := panePipe(t, sessionName); p != "0" {
		t.Fatalf("pane_pipe = %q after the stop; want 0", p)
	}

	// Output after it doesn't. Once tmux shows it on the pane it has
	// handled it, with no pipe to send it to: then the file can't get it.
	typeLine(t, client, sessionName, "echo after-$((40+2))-stop")
	waitLine(t, client, sessionName, "after-42-stop")
	content, err := os.ReadFile(logPath)
	if err != nil {
		t.Fatalf("reading log: %v", err)
	}
	if strings.Contains(string(content), "after-42-stop") {
		t.Errorf("pipe-pane did not stop - output after stop was captured:\n%s", content)
	}
}

// TestClear tests that Clear empties both the visible pane and the
// scrollback, at once and without typing into the pane (friction #5: typing
// "clear" raced the shell, and scroll-on-clear kept the old screen). The
// marker is printed past the pane's height, so some of it is in the
// scrollback before Clear runs.
func TestClear(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	sessionName := newShell(t, client)

	// cat keeps the shell from printing its prompt after the marker, which
	// could land after Clear, so the pane is quiet once the last line shows.
	typeLine(t, client, sessionName, "for i in $(seq 1 60); do echo clear-$((40+2))-marker-$i; done; cat")
	waitLine(t, client, sessionName, "clear-42-marker-60")
	before, err := client.Capture(sessionName, 200)
	if err != nil {
		t.Fatalf("capturing before clear: %v", err)
	}
	if countLine(before, "clear-42-marker-1") != 1 {
		t.Fatalf("expected the first marker line in the scrollback before clearing:\n%s", before)
	}

	if err := client.Clear(sessionName); err != nil {
		t.Fatalf("clearing: %v", err)
	}

	// No waiting: Clear is done when it returns.
	output, err := client.Capture(sessionName, 200)
	if err != nil {
		t.Fatalf("capturing after clear: %v", err)
	}
	if strings.TrimSpace(output) != "" {
		t.Fatalf("expected an empty pane and scrollback after clear, got:\n%s", output)
	}

	// Nothing was typed into the pane, and its program still works: cat's
	// next line lands on the clean pane (the terminal's echo, then cat's
	// copy), with nothing from before.
	typeLine(t, client, sessionName, "after-clear")
	output = waitPane(t, client, sessionName, 200, "cat echoing the line", func(screen string) bool {
		return countLine(screen, "after-clear") == 2
	})
	if strings.TrimSpace(output) != "after-clear\nafter-clear" {
		t.Errorf("expected only the new line after clear, got:\n%s", output)
	}
}

// TestClear_NonExistentSession tests clearing a non-existent session
func TestClear_NonExistentSession(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	err = client.Clear("nonexistent-session-12345")
	if err == nil {
		t.Error("expected error when clearing non-existent session")
	}
}

// TestSendKeys_NonExistentSession tests sending keys to non-existent session
func TestSendKeys_NonExistentSession(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	err = client.SendKeys("nonexistent-session-12345", "test")
	if err == nil {
		t.Error("expected error when sending keys to non-existent session")
	}
}

// TestCapture_NonExistentSession tests capturing from non-existent session
func TestCapture_NonExistentSession(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	_, err = client.Capture("nonexistent-session-12345", 50)
	if err == nil {
		t.Error("expected error when capturing from non-existent session")
	}
}

// TestPaneTargeting_IgnoresActiveWindow checks that typed text, special keys,
// and captures all reach the agent's pane when another window is active.
func TestPaneTargeting_IgnoresActiveWindow(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	if err := client.CreateSession(sessionName, nil); err != nil {
		t.Fatalf("creating session: %v", err)
	}
	waitShell(t, client, sessionName)
	// A second window becomes the active one, e.g. a human poking around.
	if out, err := exec.Command("tmux", "new-window", "-t", sessionName).CombinedOutput(); err != nil {
		t.Fatalf("new-window: %v: %s", err, out)
	}

	if err := client.SendKeys(sessionName, "echo marker-$((40+2))"); err != nil {
		t.Fatalf("sending keys: %v", err)
	}
	if err := client.SendSpecialKey(sessionName, "Enter"); err != nil {
		t.Fatalf("sending enter: %v", err)
	}

	// The command only runs if Enter reached the same pane as the text.
	waitLine(t, client, sessionName, "marker-42")
}

func TestPasteText_OnePasteWhenTheAppAsks(t *testing.T) {
	requireTmux(t)
	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}
	// Long enough to be split; it can't end the paste early.
	text := strings.Repeat("0123456789", 150) + "\x1b[201~" + strings.Repeat("abcdefghij", 150) + "\nlast line"
	want := strings.ReplaceAll(text, "\x1b[201~", "")

	for _, tt := range []struct {
		name   string
		asks   bool
		wantIn string
	}{
		{"asks for bracketed paste", true, "\x1b[200~" + want + "\x1b[201~"},
		{"doesn't ask", false, want},
	} {
		t.Run(tt.name, func(t *testing.T) {
			sessionName := uniqueSessionName(t)
			defer cleanupSession(sessionName)
			if err := client.CreateSession(sessionName, nil); err != nil {
				t.Fatalf("creating session: %v", err)
			}
			waitShell(t, client, sessionName)

			// The pane's app takes its input raw and records every byte, as
			// a CLI reads its terminal.
			out := filepath.Join(t.TempDir(), "in")
			mode := ""
			if tt.asks {
				mode = `printf '\033[?2004h'; `
			}
			// READY is printed only once the mode is set: the command line
			// itself says RE''ADY, so its echo can't count as ready (on a
			// slower shell the paste then came before the app asked).
			client.SendKeys(sessionName, fmt.Sprintf(`stty raw -echo; %secho RE''ADY; cat > %s`, mode, out))
			client.SendSpecialKey(sessionName, "Enter")
			waitPane(t, client, sessionName, 0, "the app's READY", func(screen string) bool {
				return strings.Contains(screen, "READY")
			})

			if err := client.PasteText(sessionName, text); err != nil {
				t.Fatalf("PasteText: %v", err)
			}
			got := await.Until(t, fmt.Sprintf("the app to read %d bytes", len(tt.wantIn)), func() (string, bool) {
				data, _ := os.ReadFile(out)
				return string(data), len(data) >= len(tt.wantIn)
			})
			if got != tt.wantIn {
				t.Errorf("the app read %d bytes:\n%q\nwant %d:\n%q", len(got), got, len(tt.wantIn), tt.wantIn)
			}
		})
	}
}

// newShell creates a session for the test and returns once its shell runs
// what's typed. A loaded machine, or a heavy rc file, can take seconds to
// start a shell, and text typed before it reads any is at its mercy.
func newShell(t *testing.T, client *tmux.Client) string {
	t.Helper()
	sessionName := uniqueSessionName(t)
	t.Cleanup(func() { cleanupSession(sessionName) })
	if err := client.CreateSession(sessionName, nil); err != nil {
		t.Fatalf("creating session: %v", err)
	}
	waitShell(t, client, sessionName)
	return sessionName
}

// waitShell returns once the session's shell has run a command. The
// command prints a line its own command line doesn't hold, so the
// terminal's echo of the typed line can't count.
func waitShell(t *testing.T, client *tmux.Client, sessionName string) {
	t.Helper()
	typeLine(t, client, sessionName, "echo shell-$((40+2))-ready")
	waitLine(t, client, sessionName, "shell-42-ready")
}

// startCat runs cat -v in the shell and returns once the shell has handed
// the terminal to it: the echo runs only after the shell has read the
// whole line, so what's typed next goes to cat.
func startCat(t *testing.T, client *tmux.Client, sessionName string) {
	t.Helper()
	typeLine(t, client, sessionName, "echo cat-$((40+2)); cat -v")
	waitLine(t, client, sessionName, "cat-42")
}

// typeLine types line into the session and presses Enter.
func typeLine(t *testing.T, client *tmux.Client, sessionName, line string) {
	t.Helper()
	if err := client.SendKeys(sessionName, line); err != nil {
		t.Fatalf("typing %q: %v", line, err)
	}
	if err := client.SendSpecialKey(sessionName, "Enter"); err != nil {
		t.Fatalf("pressing Enter: %v", err)
	}
}

// waitPane waits until ok holds for the pane (with lines of scrollback)
// and returns that capture.
func waitPane(t *testing.T, client *tmux.Client, sessionName string, lines int, what string, ok func(screen string) bool) string {
	t.Helper()
	return await.Until(t, what, func() (string, bool) {
		screen, err := client.Capture(sessionName, lines)
		if err != nil {
			return "capture failed: " + err.Error(), false
		}
		return screen, ok(screen)
	})
}

// waitLine waits for a line on the pane that is exactly line (trimmed).
func waitLine(t *testing.T, client *tmux.Client, sessionName, line string) string {
	t.Helper()
	return waitPane(t, client, sessionName, 50, fmt.Sprintf("a line %q on the pane", line), func(screen string) bool {
		return countLine(screen, line) > 0
	})
}

// countLine counts the screen's lines that are line, ignoring surrounding
// space.
func countLine(screen, line string) int {
	n := 0
	for _, l := range strings.Split(screen, "\n") {
		if strings.TrimSpace(l) == line {
			n++
		}
	}
	return n
}

// waitFile waits for the file at path to hold want.
func waitFile(t *testing.T, path, want string) {
	t.Helper()
	await.Until(t, fmt.Sprintf("%q in %s", want, path), func() (string, bool) {
		data, err := os.ReadFile(path)
		if err != nil {
			return err.Error(), false
		}
		return string(data), strings.Contains(string(data), want)
	})
}

// panePipe reads whether tmux has a pipe open on the session's pane.
func panePipe(t *testing.T, sessionName string) string {
	t.Helper()
	out, err := exec.Command("tmux", "display-message", "-p", "-t", sessionName+":", "#{pane_pipe}").Output()
	if err != nil {
		t.Fatalf("reading pane_pipe: %v", err)
	}
	return strings.TrimSpace(string(out))
}
