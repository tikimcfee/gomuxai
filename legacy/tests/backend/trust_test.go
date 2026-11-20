package backend_test

import (
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// claudeTrustScreen is Claude Code v2.1.310's folder trust dialog, as
// agent capture showed it for an agent spawned in a folder Claude hadn't
// trusted (2026-11, an 80-column pane): the folder wraps at the edge.
const claudeTrustScreen = `sh '/opt/gomuxai/.gomuxai/agents/t1/launch.sh'
────────────────────────────────────────────────────────────────────────────────
 Accessing workspace:

 /private/tmp/claude-501/-Users-lugo-localdev-gomuxai--claude-worktrees-trust-p
 rompt/scratchpad/trust-cap

 Quick safety check: Is this a project you created or one you trust? (Like your
 own code, a well-known open source project, or work from your team). If not,
 take a moment to review what's in this folder first.

 Claude Code'll be able to read, edit, and execute files here.

 Security guide

 ❯ No, exit
   Yes, I trust this folder

 Enter to confirm · Esc to cancel


`

// agyTrustScreen is agy's workspace trust prompt, captured the same way.
const agyTrustScreen = `Accessing workspace:

/private/tmp/claude-501/-Users-lugo-localdev-gomuxai--claude-worktrees-trust-
prompt/scratchpad/agy-cap

Do you trust the contents of this project?

Antigravity CLI requires permission to read, edit, and execute files here.

> Yes, I trust this folder
  No, exit

  ↑/↓ Navigate · enter Confirm
                                                           Gemini 4 Argon · high




`

func TestTrustPrompt(t *testing.T) {
	claude, agy := namedagents.NewClaudeBackend(), namedagents.NewAgyBackend()
	// The other option highlighted: Claude after an Up, agy after a Down.
	claudeYes := strings.Replace(strings.Replace(claudeTrustScreen, " ❯ No, exit", "   No, exit", 1), "   Yes, I", " ❯ Yes, I", 1)
	agyNo := strings.Replace(strings.Replace(agyTrustScreen, "> Yes, I", "  Yes, I", 1), "  No, exit", "> No, exit", 1)

	tests := []struct {
		name          string
		tp            backend.TrustPrompter
		screen        string
		folder        string // "" with ok false: no prompt
		trust, refuse string // the keys, comma-separated
	}{
		{"claude, No highlighted", claude, claudeTrustScreen,
			"/private/tmp/claude-501/-Users-lugo-localdev-gomuxai--claude-worktrees-trust-prompt/scratchpad/trust-cap", "Down,Enter", "Enter"},
		{"claude, Yes highlighted", claude, claudeYes,
			"/private/tmp/claude-501/-Users-lugo-localdev-gomuxai--claude-worktrees-trust-prompt/scratchpad/trust-cap", "Enter", "Up,Enter"},
		{"agy, Yes highlighted", agy, agyTrustScreen,
			"/private/tmp/claude-501/-Users-lugo-localdev-gomuxai--claude-worktrees-trust-prompt/scratchpad/agy-cap", "Enter", "Down,Enter"},
		{"agy, No highlighted", agy, agyNo,
			"/private/tmp/claude-501/-Users-lugo-localdev-gomuxai--claude-worktrees-trust-prompt/scratchpad/agy-cap", "Up,Enter", "Enter"},
		// Each CLI's dialog is its own: the other's question isn't there.
		{"claude reading agy's", claude, agyTrustScreen, "", "", ""},
		{"agy reading claude's", agy, claudeTrustScreen, "", "", ""},
		{"claude at its prompt", claude, "● ok\n\n❯ Try \"refactor <filepath>\"\n", "", "", ""},
		// The dialog's words quoted higher up, with the idle prompt below
		// (a transcript, a file on screen), aren't the dialog.
		{"claude, quoted above its prompt", claude, claudeTrustScreen + "\n● That was the trust dialog.\n\n" + strings.Repeat("─", 40) + "\n❯ \n" + strings.Repeat("─", 40) + "\n  ? for shortcuts\n", "", "", ""},
		// A CLI that exited at the dialog leaves it on screen, the shell's
		// prompt under it.
		{"claude, exited", claude, claudeTrustScreen + "➜  trust-cap \n", "", "", ""},
		{"agy, exited", agy, agyTrustScreen + "$ \n", "", "", ""},
		{"agy, quoted above its prompt", agy, agyTrustScreen + "\nok\n\n" + strings.Repeat("─", 40) + "\n> \n" + strings.Repeat("─", 40) + "\n? for shortcuts\n", "", "", ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			folder, ok := tt.tp.TrustPrompt(tt.screen)
			if ok != (tt.folder != "") || folder != tt.folder {
				t.Errorf("TrustPrompt = %q, %v; want %q, %v", folder, ok, tt.folder, tt.folder != "")
			}
			if got := strings.Join(tt.tp.TrustKeys(tt.screen), ","); got != tt.trust {
				t.Errorf("TrustKeys = %q, want %q", got, tt.trust)
			}
			if got := strings.Join(tt.tp.DeclineTrustKeys(tt.screen), ","); got != tt.refuse {
				t.Errorf("DeclineTrustKeys = %q, want %q", got, tt.refuse)
			}
		})
	}
}

// With neither option marked, no keys are guessed: Enter could pick
// either.
func TestTrustKeys_NothingHighlighted(t *testing.T) {
	claude := namedagents.NewClaudeBackend()
	screen := strings.Replace(claudeTrustScreen, " ❯ No, exit", "   No, exit", 1)
	if _, ok := claude.TrustPrompt(screen); !ok {
		t.Fatal("the dialog should still be recognized")
	}
	if keys := claude.TrustKeys(screen); keys != nil {
		t.Errorf("TrustKeys = %v, want nil", keys)
	}
	if keys := claude.DeclineTrustKeys(screen); keys != nil {
		t.Errorf("DeclineTrustKeys = %v, want nil", keys)
	}
}

// A dialog whose header scrolled off still counts; its folder is unknown.
func TestTrustPrompt_NoHeader(t *testing.T) {
	screen := claudeTrustScreen[strings.Index(claudeTrustScreen, " Quick safety check"):]
	if folder, ok := namedagents.NewClaudeBackend().TrustPrompt(screen); !ok || folder != "" {
		t.Errorf("TrustPrompt = %q, %v; want \"\", true", folder, ok)
	}
}
