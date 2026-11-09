package namedagents

import (
	"fmt"
	"regexp"
	"strings"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Claude Code draws every dialog the same way: a rule across the screen,
// its title on the next line, its body and options (the highlighted one
// marked "❯"), and a footer of key hints such as "Enter to confirm · Esc to
// cancel". Some take the input box's place (a permission prompt, a setup
// wizard); others are drawn beside it, keys going to the dialog: under the
// box in the classic renderer, above it in the fullscreen one. Offers like
// "Teach auto mode about your environment?" come up when a turn ends, and
// fire no hook, and Claude's session registry still says idle.
//
// So a dialog is read off the screen by its footer, wherever one can sit:
// the bottom line of the screen, or the last line above the input box. A
// running turn's footer ("esc to interrupt") and the status line under the
// box aren't key hints alone, so neither counts.

// claudeDialogMaxLines bounds how far above its footer a dialog's top rule
// is looked for.
const claudeDialogMaxLines = 40

// claudeKeyHint matches one key hint of a dialog's footer: a key, "to",
// and what it does ("Esc to cancel", "←/→ to change", "Tab to amend").
var claudeKeyHint = regexp.MustCompile(`^(\S{1,16}) to \S.*$`)

// claudeAnswerKeys are the keys a dialog's footer offers for answering it:
// a footer with neither is something else.
var claudeAnswerKeys = map[string]bool{"enter": true, "esc": true}

// isClaudeDialogFooter reports whether line is a dialog's footer: key hints
// separated by " · ", one of them for Enter or Esc. "esc to interrupt" is a
// running turn's, so it disqualifies the line.
func isClaudeDialogFooter(line string) bool {
	line = strings.TrimSpace(line)
	if line == "" {
		return false
	}
	answers := false
	for _, seg := range strings.Split(line, " · ") {
		seg = strings.TrimSpace(seg)
		m := claudeKeyHint.FindStringSubmatch(seg)
		if m == nil || strings.HasSuffix(seg, "to interrupt") {
			return false
		}
		answers = answers || claudeAnswerKeys[strings.ToLower(m[1])]
	}
	return answers
}

// Dialog reads a Claude dialog off the screen: a footer of key hints at
// the bottom of the screen or right above the input box, under a rule
// whose next line is the dialog's title.
func (b *ClaudeBackend) Dialog(screen string) (types.Dialog, bool) {
	lines := strings.Split(strings.TrimRight(util.StripANSI(screen), "\n"), "\n")
	for _, footer := range claudeFooterCandidates(lines) {
		if !isClaudeDialogFooter(lines[footer]) {
			continue
		}
		if d, ok := claudeDialogAbove(lines, footer); ok {
			return d, true
		}
	}
	return types.Dialog{}, false
}

// claudeFooterCandidates returns where a dialog's footer can be: the last
// non-blank line, and the last non-blank line above the input box.
func claudeFooterCandidates(lines []string) []int {
	var out []int
	if i := lastNonBlank(lines, len(lines)); i >= 0 {
		out = append(out, i)
	}
	if start, _, ok := inputBox(lines, "❯", 0); ok {
		// start-1 is the box's top rule.
		if i := lastNonBlank(lines, start-1); i >= 0 && (len(out) == 0 || i != out[0]) {
			out = append(out, i)
		}
	}
	return out
}

// lastNonBlank returns the index of the last non-blank line before end, or
// -1.
func lastNonBlank(lines []string, end int) int {
	for i := end - 1; i >= 0; i-- {
		if strings.TrimSpace(lines[i]) != "" {
			return i
		}
	}
	return -1
}

// isDialogBorder reports whether line is a dialog's top border: a rule of
// "─" (as the input box's), or a line of "▔" (a picker's, as /model's).
func isDialogBorder(line string) bool {
	if isRule(line) {
		return true
	}
	line = strings.TrimSpace(line)
	return utf8.RuneCountInString(line) >= 10 && strings.Trim(line, "▔") == ""
}

// claudeDialogAbove reads the dialog whose footer is lines[footer]: its
// title is the first line under the nearest border above, and its
// highlighted option the line between them marked "❯".
func claudeDialogAbove(lines []string, footer int) (types.Dialog, bool) {
	for top := footer - 1; top >= 0 && top >= footer-claudeDialogMaxLines; top-- {
		if !isDialogBorder(lines[top]) {
			continue
		}
		var d types.Dialog
		for _, line := range lines[top+1 : footer] {
			text := strings.Join(strings.Fields(line), " ") // no-break spaces too
			switch {
			case text == "":
			case d.Title == "":
				d.Title = text
			case d.Highlighted == "" && strings.HasPrefix(text, "❯"):
				d.Highlighted = strings.TrimSpace(strings.TrimPrefix(text, "❯"))
			}
		}
		return d, d.Title != ""
	}
	return types.Dialog{}, false
}

// DialogKey answers a Claude dialog: Enter picks the highlighted option,
// Escape closes it (its footer's "Esc to cancel"; an offer takes it for
// "not now").
func (b *ClaudeBackend) DialogKey(resp types.PermissionResponse) (string, error) {
	switch resp {
	case types.PermissionApprove:
		return "Enter", nil
	case types.PermissionDeny:
		return "Escape", nil
	}
	return "", fmt.Errorf("a dialog has no \"always\" answer: approve picks its highlighted option, deny closes it")
}
