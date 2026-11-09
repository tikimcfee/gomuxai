package backend_test

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// readScreen reads a screen captured with escapes (agent capture --raw)
// from Claude Code v2.1.319, in testdata.
func readScreen(t *testing.T, name string) string {
	t.Helper()
	b, err := os.ReadFile(filepath.Join("testdata", name))
	if err != nil {
		t.Fatal(err)
	}
	return string(b)
}

// A dialog no hook reports must read as one wherever Claude draws it: under
// the input box (classic renderer), above it (fullscreen), or in its place.
// Busy and idle screens, and replies that quote a dialog, must not.
func TestClaudeDialog(t *testing.T) {
	rule := strings.Repeat("─", 80)
	// The auto-mode setup wizard the offer opens, drawn in the input box's
	// place (built from the strings in Claude Code v2.1.319: it needs a
	// signed-in account to open).
	wizard := strings.Join([]string{
		"❯ /auto-mode-setup",
		"",
		rule,
		"  Teach auto mode about your environment?",
		"",
		"  Claude Code reads this project, your recent Claude sessions, and optionally",
		"  your shell history and other repositories.",
		"",
		"  ❯ How you use Claude here   Mixed",
		"    Also scan shell history   [✔]",
		"    Also scan your other repos [ ]",
		"",
		"    Continue",
		"",
		"  ←/→ to change · Enter to continue · Esc to cancel",
	}, "\n")
	idleBox := []string{rule, "❯ ", rule, "  ⏵⏵ auto mode on (shift+tab to cycle)"}
	tests := []struct {
		name        string
		screen      string
		want        bool
		title, high string
	}{
		{"offer under the input box (classic)", readScreen(t, "claude-offer-under-box.txt"), true, "Teach auto mode about your environment?", "1. Yes"},
		{"offer above the input box (fullscreen)", readScreen(t, "claude-offer-above-box.txt"), true, "Teach auto mode about your environment?", "1. Yes"},
		{"setup wizard in the input box's place", wizard, true, "Teach auto mode about your environment?", "How you use Claude here Mixed"},
		// A picker a person opens: its border is "▔", and the registry
		// says waiting with nothing on what.
		{"/model picker", readScreen(t, "claude-model-picker.txt"), true, "Select model", "1. Default (recommended) ✔ Use the default model (currently Opus 5.5) ·"},
		{"permission prompt", claudeBashDialog, true, "Bash command", "1. Yes"},
		{"busy", readScreen(t, "claude-busy.txt"), false, "", ""},
		{"idle", readScreen(t, "claude-idle.txt"), false, "", ""},
		{"running turn", claudeRunning, false, "", ""},
		{"idle, reply mentions options", claudeIdleAfterList, false, "", ""},
		{"starting, no input box yet", " ▐▛███▛█   Claude Code v2.1.319\n▝▜██████▀  Opus 5.5\n", false, "", ""},
		{"reply quoting a footer above the box, no rule", strings.Join(append([]string{"● The dialog said:", "  Enter to confirm · Esc to cancel", ""}, idleBox...), "\n"), false, "", ""},
		{"key hints with no Enter or Esc", strings.Join(append([]string{rule, "  Some panel", "  Tab to switch · Space to toggle"}, idleBox...), "\n"), false, "", ""},
		{"a running turn's footer alone", strings.Join(append(idleBox[:3], "  esc to interrupt"), "\n"), false, "", ""},
		{"empty", "", false, "", ""},
	}
	b := namedagents.NewClaudeBackend()
	for _, tt := range tests {
		d, ok := b.Dialog(tt.screen)
		if ok != tt.want || d.Title != tt.title || d.Highlighted != tt.high {
			t.Errorf("%s: Dialog = %+v, %v; want {Title:%s Highlighted:%s}, %v", tt.name, d, ok, tt.title, tt.high, tt.want)
		}
	}
}

// approve picks the highlighted option; deny closes the dialog (Esc, which
// an offer takes for "not now").
func TestClaudeDialogKey(t *testing.T) {
	b := namedagents.NewClaudeBackend()
	for resp, want := range map[backend.PermissionResponse]string{backend.PermissionApprove: "Enter", backend.PermissionDeny: "Escape"} {
		if key, err := b.DialogKey(resp); err != nil || key != want {
			t.Errorf("DialogKey(%v) = %q, %v; want %q", resp, key, err, want)
		}
	}
	if _, err := b.DialogKey(backend.PermissionApproveAlways); err == nil {
		t.Error("DialogKey(always) should be refused: a dialog has no always")
	}
}
