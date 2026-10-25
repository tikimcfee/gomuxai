package tmux

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

// fakeTmux puts a tmux on PATH that logs its arguments, one call per line,
// and acts like a server with a read-only client attached: send-keys is
// refused, the rest succeeds. It returns the log's path.
func fakeTmux(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	log := filepath.Join(dir, "calls.log")
	script := `#!/bin/sh
printf '%s\n' "$*" >> "` + log + `"
case "$1" in
list-panes) echo %7 ;;
send-keys) echo "client is read-only" >&2; exit 1 ;;
list-clients) printf '0\t/dev/pts/3\tmain\n1\t/dev/pts/9\tagent-x\n' ;;
display-message) echo 1 ;;
paste-buffer)
	if [ -n "$FAKE_OLD_TMUX" ] && [ "$4" = -S ]; then echo "command paste-buffer: unknown flag -S" >&2; exit 1; fi ;;
set-buffer) ;;
silent) exit 1 ;;
*) echo "unknown command: $1" >&2; exit 1 ;;
esac
`
	if err := os.WriteFile(filepath.Join(dir, "tmux"), []byte(script), 0o755); err != nil {
		t.Fatal(err)
	}
	t.Setenv("PATH", dir+string(os.PathListSeparator)+os.Getenv("PATH"))
	return log
}

func calls(t *testing.T, log string) string {
	t.Helper()
	b, err := os.ReadFile(log)
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

func TestRun_ErrorCarriesTmuxMessage(t *testing.T) {
	fakeTmux(t)
	_, err := run("bogus", "-t", "x")
	if err == nil || err.Error() != "tmux bogus: unknown command: bogus" {
		t.Errorf("err = %v, want tmux's own message", err)
	}
	var te *Error
	if !errors.As(err, &te) || te.Command != "bogus" {
		t.Errorf("err = %#v, want an *Error for bogus", err)
	}
	// With nothing on stderr, the exit status is all there is to say.
	if _, err := run("silent"); err == nil || err.Error() != "tmux silent: exit status 1" {
		t.Errorf("err = %v, want the exit status", err)
	}
}

func TestIsReadOnly(t *testing.T) {
	ro := commandError([]string{"send-keys"}, "client is read-only\n", errors.New("exit status 1"))
	if !IsReadOnly(ro) || !IsReadOnly(fmt.Errorf("sending keys: %w", ro)) {
		t.Error("IsReadOnly missed tmux's read-only refusal")
	}
	other := commandError([]string{"send-keys"}, "can't find pane: %9", errors.New("exit status 1"))
	if IsReadOnly(other) || IsReadOnly(errors.New("client is read-only")) || IsReadOnly(nil) {
		t.Error("IsReadOnly matched something that isn't tmux refusing a read-only client")
	}
}

func TestSendKeys_PastesPastReadOnlyClient(t *testing.T) {
	log := fakeTmux(t)
	c := &Client{}
	if err := c.SendKeys("agent-x", "-R hi\nthere"); err != nil {
		t.Fatalf("SendKeys = %v, want the text pasted instead", err)
	}
	got := calls(t, log)
	if !strings.Contains(got, "send-keys -l -t %7 -- -R hi\nthere\n") {
		t.Errorf("send-keys not tried first; calls:\n%s", got)
	}
	if !strings.Contains(got, " -- -R hi\nthere\n") || !strings.Contains(got, "set-buffer -b gomuxai-") {
		t.Errorf("text not put in a buffer; calls:\n%s", got)
	}
	if !strings.Contains(got, "paste-buffer -d -r -S -b gomuxai-") || !strings.HasSuffix(got, " -t %7\n") {
		t.Errorf("buffer not pasted into the pane, deleted, unbracketed, byte for byte; calls:\n%s", got)
	}
}

// A tmux older than 3.5 doesn't know -S, and doesn't need it.
func TestPaste_RetriesWithoutSanitizeFlag(t *testing.T) {
	log := fakeTmux(t)
	t.Setenv("FAKE_OLD_TMUX", "1")
	if err := paste("%7", "\x03"); err != nil {
		t.Fatalf("paste = %v, want it retried without -S", err)
	}
	got := calls(t, log)
	if !strings.Contains(got, "paste-buffer -d -r -S -b") || !strings.Contains(got, "paste-buffer -d -r -b") || strings.Contains(got, "delete-buffer") {
		t.Errorf("paste not retried without -S; calls:\n%s", got)
	}
}

func TestSendSpecialKey_PastesArrowInCursorKeyMode(t *testing.T) {
	log := fakeTmux(t)
	c := &Client{}
	if err := c.SendSpecialKey("agent-x", "Down"); err != nil {
		t.Fatalf("SendSpecialKey = %v, want the key pasted instead", err)
	}
	// The fake pane is in cursor key mode, so Down is ESC O B.
	if got := calls(t, log); !strings.Contains(got, "display-message -p -t %7 #{keypad_cursor_flag}") || !strings.Contains(got, " -- \x1bOB\n") {
		t.Errorf("Down not pasted as ESC O B; calls:\n%q", got)
	}
}

func TestSendSpecialKey_ReadOnlyErrorNamesClient(t *testing.T) {
	fakeTmux(t)
	c := &Client{}
	err := c.SendSpecialKey("agent-x", "F5")
	want := `sending key "F5": tmux refuses send-keys to every session while a read-only client is attached (/dev/pts/9 on agent-x), and "F5" isn't a key agent can paste past it: detach that client (tmux detach-client -t /dev/pts/9), or watch agents with agent capture instead of tmux attach -r`
	if err == nil || err.Error() != want {
		t.Errorf("err = %v\nwant %s", err, want)
	}
	var roe *ReadOnlyError
	if !errors.As(err, &roe) || !IsReadOnly(err) {
		t.Errorf("err = %#v, want a ReadOnlyError wrapping tmux's refusal", err)
	}
}

func TestReadOnlyError_NoClientLeft(t *testing.T) {
	err := &ReadOnlyError{Key: "F5"}
	if want := `tmux refused key "F5" because a client is read-only, but none is attached now: try again`; err.Error() != want {
		t.Errorf("err = %v, want %s", err, want)
	}
}

func TestKeyBytes(t *testing.T) {
	for _, tc := range []struct {
		key        string
		cursorKeys bool
		want       string
		ok         bool
	}{
		{"Enter", false, "\r", true},
		{"enter", false, "\r", true},
		{"Escape", false, "\x1b", true},
		{"Tab", false, "\t", true},
		{"BSpace", false, "\x7f", true},
		{"Space", false, " ", true},
		{"Down", false, "\x1b[B", true},
		{"Down", true, "\x1bOB", true},
		{"Up", false, "\x1b[A", true},
		{"Left", true, "\x1bOD", true},
		{"C-c", false, "\x03", true},
		{"c-c", false, "\x03", true},
		{"C-Z", false, "\x1a", true},
		{"1", false, "1", true},
		{"é", false, "é", true},
		{"F5", false, "", false},
		{"M-x", false, "", false},
		{"C-1", false, "", false},
		{"hello", false, "", false},
		{"", false, "", false},
	} {
		got, ok := keyBytes(tc.key, tc.cursorKeys)
		if got != tc.want || ok != tc.ok {
			t.Errorf("keyBytes(%q, %v) = %q, %v; want %q, %v", tc.key, tc.cursorKeys, got, ok, tc.want, tc.ok)
		}
	}
}
