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
	deadline := time.Now().Add(3 * time.Second)
	for {
		output, err := client.Capture(sessionName, 20)
		if err != nil {
			t.Fatalf("capturing: %v", err)
		}
		if strings.Contains(output, "TEST_VAR=[test_value]") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("shell did not see TEST_VAR; output:\n%s", output)
		}
		time.Sleep(100 * time.Millisecond)
	}
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

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	// Give the session time to initialize
	time.Sleep(100 * time.Millisecond)

	// Send some keys
	err = client.SendKeys(sessionName, "echo hello")
	if err != nil {
		t.Fatalf("sending keys: %v", err)
	}

	// Send Enter to execute
	err = client.SendSpecialKey(sessionName, "Enter")
	if err != nil {
		t.Fatalf("sending Enter: %v", err)
	}

	// Wait for command to execute
	time.Sleep(200 * time.Millisecond)

	// Capture output
	output, err := client.Capture(sessionName, 50)
	if err != nil {
		t.Fatalf("capturing output: %v", err)
	}

	if !strings.Contains(output, "hello") {
		t.Errorf("expected output to contain 'hello', got: %s", output)
	}
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
	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)
	if err := client.CreateSession(sessionName, nil); err != nil {
		t.Fatalf("creating session: %v", err)
	}
	time.Sleep(100 * time.Millisecond)

	// cat -v shows control keys as ^X, so a key sent instead of the text
	// shows up differently (or, for C-c, ends cat).
	client.SendKeys(sessionName, "cat -v")
	client.SendSpecialKey(sessionName, "Enter")
	time.Sleep(200 * time.Millisecond)

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
	time.Sleep(300 * time.Millisecond)

	output, err := client.Capture(sessionName, 50)
	if err != nil {
		t.Fatalf("capturing output: %v", err)
	}
	lines := map[string]int{}
	for _, line := range strings.Split(output, "\n") {
		lines[strings.TrimSpace(line)]++
	}
	for _, text := range append(texts, "-N2") {
		if lines[text] < 2 { // typed by the terminal, then echoed by cat
			t.Errorf("%q not typed literally; pane:\n%s", text, output)
		}
	}
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
	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)
	if err := client.CreateSession(sessionName, nil); err != nil {
		t.Fatalf("creating session: %v", err)
	}
	// tmux's default; a user's config may pick vi.
	if out, err := exec.Command("tmux", "set-option", "-w", "-t", sessionName+":", "mode-keys", "emacs").CombinedOutput(); err != nil {
		t.Fatalf("setting mode-keys: %v: %s", err, out)
	}
	time.Sleep(100 * time.Millisecond)
	client.SendKeys(sessionName, "cat -v")
	client.SendSpecialKey(sessionName, "Enter")
	time.Sleep(200 * time.Millisecond)

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
	time.Sleep(300 * time.Millisecond)

	if inMode() != "0" {
		t.Error("pane still in a mode after typing")
	}
	output, err := client.Capture(sessionName, 50)
	if err != nil {
		t.Fatalf("capturing output: %v", err)
	}
	// cat -v shows the bytes as UTF-8 escapes (M-...), so look for the
	// ASCII ends of the message: typed by the terminal, then echoed by cat.
	if n := strings.Count(output, "tell me when gated."); n < 2 {
		t.Errorf("message not typed into the program (%d copies); pane:\n%s", n, output)
	}
	if n := strings.Count(output, "pasted out of copy mode"); n < 2 {
		t.Errorf("paste not typed into the program (%d copies); pane:\n%s", n, output)
	}
	lines := map[string]int{}
	for _, line := range strings.Split(output, "\n") {
		lines[strings.TrimSpace(line)]++
	}
	if lines["x"] < 2 {
		t.Errorf("key not typed into the program; pane:\n%s", output)
	}
}

// TestSendSpecialKey tests special key transmission
func TestSendSpecialKey(t *testing.T) {
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

	// Give the session time to initialize
	time.Sleep(100 * time.Millisecond)

	// Test C-c (interrupt)
	err = client.SendSpecialKey(sessionName, "C-c")
	if err != nil {
		t.Fatalf("sending C-c: %v", err)
	}

	// Test Enter
	err = client.SendSpecialKey(sessionName, "Enter")
	if err != nil {
		t.Fatalf("sending Enter: %v", err)
	}
}

// TestCapture tests output capture
func TestCapture(t *testing.T) {
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

	time.Sleep(100 * time.Millisecond)

	// Generate some output
	err = client.SendKeys(sessionName, "echo line1; echo line2; echo line3")
	if err != nil {
		t.Fatalf("sending keys: %v", err)
	}
	err = client.SendSpecialKey(sessionName, "Enter")
	if err != nil {
		t.Fatalf("sending Enter: %v", err)
	}

	time.Sleep(200 * time.Millisecond)

	// Capture output
	output, err := client.Capture(sessionName, 50)
	if err != nil {
		t.Fatalf("capturing output: %v", err)
	}

	if !strings.Contains(output, "line1") {
		t.Errorf("expected output to contain 'line1', got: %s", output)
	}
	if !strings.Contains(output, "line2") {
		t.Errorf("expected output to contain 'line2', got: %s", output)
	}
	if !strings.Contains(output, "line3") {
		t.Errorf("expected output to contain 'line3', got: %s", output)
	}
}

// TestCapture_StripANSI tests ANSI stripping in capture
func TestCapture_StripANSI(t *testing.T) {
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

	time.Sleep(100 * time.Millisecond)

	// Send ANSI-colored output
	err = client.SendKeys(sessionName, `printf "\033[31mred\033[0m"`)
	if err != nil {
		t.Fatalf("sending keys: %v", err)
	}
	err = client.SendSpecialKey(sessionName, "Enter")
	if err != nil {
		t.Fatalf("sending Enter: %v", err)
	}

	time.Sleep(200 * time.Millisecond)

	// Capture with ANSI stripping
	output, err := client.Capture(sessionName, 50)
	if err != nil {
		t.Fatalf("capturing output: %v", err)
	}

	// Should contain 'red' but not ANSI codes
	if !strings.Contains(output, "red") {
		t.Errorf("expected output to contain 'red', got: %s", output)
	}
	if strings.Contains(output, "\033[") {
		t.Error("expected ANSI codes to be stripped")
	}
}

// TestPipePane tests log piping
func TestPipePane(t *testing.T) {
	requireTmux(t)

	client, err := tmux.NewClient()
	if err != nil {
		t.Fatalf("creating client: %v", err)
	}

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create temp file for logging
	tmpFile, err := os.CreateTemp("", "gomuxai-pipe-test-*.log")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	tmpFile.Close()
	defer os.Remove(tmpFile.Name())

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	time.Sleep(100 * time.Millisecond)

	// Start pipe-pane
	err = client.PipePane(sessionName, tmpFile.Name())
	if err != nil {
		t.Fatalf("piping pane: %v", err)
	}

	// Generate some output
	err = client.SendKeys(sessionName, "echo PIPE_TEST_OUTPUT")
	if err != nil {
		t.Fatalf("sending keys: %v", err)
	}
	err = client.SendSpecialKey(sessionName, "Enter")
	if err != nil {
		t.Fatalf("sending Enter: %v", err)
	}

	// Wait for output to be piped
	time.Sleep(500 * time.Millisecond)

	// Stop pipe-pane
	err = client.StopPipePane(sessionName)
	if err != nil {
		t.Fatalf("stopping pipe: %v", err)
	}

	// Check log file content
	content, err := os.ReadFile(tmpFile.Name())
	if err != nil {
		t.Fatalf("reading log file: %v", err)
	}

	if !strings.Contains(string(content), "PIPE_TEST_OUTPUT") {
		t.Errorf("expected log to contain 'PIPE_TEST_OUTPUT', got: %s", string(content))
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

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	time.Sleep(100 * time.Millisecond)

	// Create temp directory for our test
	tmpDir, err := os.MkdirTemp("", "gomuxai-injection-test-")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

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
			os.Remove(markerFile) // Clean up from any previous run

			maliciousPath := filepath.Join(tmpDir, fmt.Sprintf(tc.malicious, tmpDir))

			// Try to use malicious path
			err := client.PipePane(sessionName, maliciousPath)

			// Generate output to trigger the pipe
			if err == nil {
				client.SendKeys(sessionName, "echo injection_test")
				client.SendSpecialKey(sessionName, "Enter")
				time.Sleep(500 * time.Millisecond)
				client.StopPipePane(sessionName)
			}

			// Check if injection occurred
			if _, err := os.Stat(markerFile); err == nil {
				t.Errorf("SHELL INJECTION VULNERABILITY via %s!", tc.description)
				t.Error("The PipePane function is vulnerable to command injection.")
				t.Error("Malicious input was able to execute arbitrary commands.")
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

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create temp directory
	tmpDir, err := os.MkdirTemp("", "gomuxai-special-chars-")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	time.Sleep(100 * time.Millisecond)

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

			// If it succeeded, send some output
			client.SendKeys(sessionName, "echo SPECIAL_CHAR_TEST")
			client.SendSpecialKey(sessionName, "Enter")
			time.Sleep(300 * time.Millisecond)
			client.StopPipePane(sessionName)

			// Check if file was created with correct content
			content, err := os.ReadFile(logPath)
			if err != nil {
				t.Logf("Could not read log file %q: %v", tc.filename, err)
				return
			}

			if !strings.Contains(string(content), "SPECIAL_CHAR_TEST") {
				t.Errorf("Log file %q doesn't contain expected content", tc.filename)
			}
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

	sessionName := uniqueSessionName(t)
	defer cleanupSession(sessionName)

	// Create temp file
	tmpFile, err := os.CreateTemp("", "gomuxai-stop-pipe-test-*.log")
	if err != nil {
		t.Fatalf("creating temp file: %v", err)
	}
	tmpFile.Close()
	defer os.Remove(tmpFile.Name())

	// Create session
	err = client.CreateSession(sessionName, nil)
	if err != nil {
		t.Fatalf("creating session: %v", err)
	}

	time.Sleep(100 * time.Millisecond)

	// Start pipe
	err = client.PipePane(sessionName, tmpFile.Name())
	if err != nil {
		t.Fatalf("piping pane: %v", err)
	}

	// Generate output before stop
	client.SendKeys(sessionName, "echo BEFORE_STOP")
	client.SendSpecialKey(sessionName, "Enter")
	time.Sleep(300 * time.Millisecond)

	// Stop pipe
	err = client.StopPipePane(sessionName)
	if err != nil {
		t.Fatalf("stopping pipe: %v", err)
	}

	// Get file size after stop
	stat1, _ := os.Stat(tmpFile.Name())
	size1 := stat1.Size()

	// Generate more output after stop
	client.SendKeys(sessionName, "echo AFTER_STOP_SHOULD_NOT_APPEAR")
	client.SendSpecialKey(sessionName, "Enter")
	time.Sleep(300 * time.Millisecond)

	// File size should not have grown significantly
	stat2, _ := os.Stat(tmpFile.Name())
	size2 := stat2.Size()

	if size2 > size1+10 {
		content, _ := os.ReadFile(tmpFile.Name())
		if strings.Contains(string(content), "AFTER_STOP") {
			t.Error("pipe-pane did not stop - output after stop was captured")
		}
	}
}

// TestClear tests terminal clearing
func TestClear(t *testing.T) {
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

	time.Sleep(100 * time.Millisecond)

	// Generate some output
	client.SendKeys(sessionName, "echo CLEAR_TEST_MARKER")
	client.SendSpecialKey(sessionName, "Enter")
	time.Sleep(200 * time.Millisecond)

	// Verify output exists
	output, _ := client.Capture(sessionName, 50)
	if !strings.Contains(output, "CLEAR_TEST_MARKER") {
		t.Skip("output not captured, skipping clear test")
	}

	// Clear
	err = client.Clear(sessionName)
	if err != nil {
		t.Fatalf("clearing: %v", err)
	}

	time.Sleep(200 * time.Millisecond)

	// Check that marker is gone from scrollback
	output, err = client.Capture(sessionName, 50)
	if err != nil {
		t.Fatalf("capturing after clear: %v", err)
	}

	if strings.Contains(output, "CLEAR_TEST_MARKER") {
		t.Error("expected marker to be cleared from scrollback")
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
	deadline := time.Now().Add(3 * time.Second)
	for {
		output, err := client.Capture(sessionName, 20)
		if err != nil {
			t.Fatalf("capturing: %v", err)
		}
		if strings.Contains(output, "marker-42") {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("command never ran in the agent pane; capture:\n%s", output)
		}
		time.Sleep(100 * time.Millisecond)
	}
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
			time.Sleep(100 * time.Millisecond)

			// The pane's app takes its input raw and records every byte, as
			// a CLI reads its terminal.
			out := filepath.Join(t.TempDir(), "in")
			mode := ""
			if tt.asks {
				mode = `printf '\033[?2004h'; `
			}
			client.SendKeys(sessionName, fmt.Sprintf(`stty raw -echo; %secho READY; cat > %s`, mode, out))
			client.SendSpecialKey(sessionName, "Enter")
			waitFor(t, func() bool {
				screen, _ := client.Capture(sessionName, 0)
				return strings.Contains(screen, "READY")
			})

			if err := client.PasteText(sessionName, text); err != nil {
				t.Fatalf("PasteText: %v", err)
			}
			var got string
			waitFor(t, func() bool {
				data, _ := os.ReadFile(out)
				got = string(data)
				return len(got) >= len(tt.wantIn)
			})
			if got != tt.wantIn {
				t.Errorf("the app read %d bytes:\n%q\nwant %d:\n%q", len(got), got, len(tt.wantIn), tt.wantIn)
			}
		})
	}
}

func waitFor(t *testing.T, cond func() bool) {
	t.Helper()
	for deadline := time.Now().Add(5 * time.Second); time.Now().Before(deadline); time.Sleep(50 * time.Millisecond) {
		if cond() {
			return
		}
	}
	if !cond() {
		t.Fatal("timed out")
	}
}
