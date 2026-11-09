package namedagents

import "strings"

// trustDialog is a CLI's workspace trust dialog, which Claude Code and agy
// draw alike (as seen with Claude Code v2.1.310 and agy, 2026-11):
//
//	Accessing workspace:
//
//	/path/to/the/folder, wrapped at the screen's width
//
//	<question>
//	...
//	<marker> No, exit
//	  Yes, I trust this folder
//	<footer>
//
// The options sit at the bottom of the screen with the highlighted one
// marked, and Enter picks it; only the footer (and agy's model line) is
// below them. Claude lists "No, exit" first and highlights it; agy lists
// "Yes" first and highlights that.
type trustDialog struct {
	question string // starts the line under the folder, e.g. "Quick safety check"
	marker   string // marks the highlighted option, e.g. "❯"
	yesFirst bool   // "Yes" is listed above "No"
	footer   string // is in the line under the options, e.g. "Enter to confirm"
	below    int    // at most this many non-blank lines are under the options
}

const (
	trustYes    = "Yes, I trust this folder"
	trustNo     = "No, exit"
	trustHeader = "Accessing workspace:"
)

// trustTailLines bounds where the options may be: among the last non-blank
// lines of the screen. Text that merely quotes the options higher up, in a
// transcript or a file shown on screen, isn't the dialog.
const trustTailLines = 6

// read finds the dialog on screen. ok is false unless the question and
// both options are there, the options at the bottom with only the footer
// under them: a CLI that exited leaves its last frame on screen, the
// shell's prompt under it. highlighted is trustYes or trustNo, or "" if
// neither is marked.
func (d trustDialog) read(screen string) (folder, highlighted string, ok bool) {
	lines := strings.Split(screen, "\n")
	yes, no := -1, -1
	seen := 0
	for i := len(lines) - 1; i >= 0 && seen < trustTailLines; i-- {
		line := strings.TrimSpace(lines[i])
		if line == "" {
			continue
		}
		seen++
		marked := strings.HasPrefix(line, d.marker)
		switch strings.TrimSpace(strings.TrimPrefix(line, d.marker)) {
		case trustYes:
			yes = i
			if marked {
				highlighted = trustYes
			}
		case trustNo:
			no = i
			if marked {
				highlighted = trustNo
			}
		}
	}
	if yes < 0 || no < 0 || (yes < no) != d.yesFirst || !d.footerUnder(lines[max(yes, no)+1:]) {
		return "", "", false
	}
	question := -1
	for i := min(yes, no) - 1; i >= 0; i-- {
		if strings.HasPrefix(strings.TrimSpace(lines[i]), d.question) {
			question = i
			break
		}
	}
	if question < 0 {
		return "", "", false
	}
	return trustFolder(lines[:question]), highlighted, true
}

// footerUnder reports whether lines, those under the options, are the
// dialog's footer: no more than d.below non-blank ones, the first with
// d.footer in it.
func (d trustDialog) footerUnder(lines []string) bool {
	var under []string
	for _, line := range lines {
		if strings.TrimSpace(line) != "" {
			under = append(under, line)
		}
	}
	return len(under) > 0 && len(under) <= d.below && strings.Contains(under[0], d.footer)
}

// trustFolder reads the folder under the dialog's header: the lines after
// it up to a blank one, joined (the path wraps at the screen's width). It
// returns "" if the header isn't on screen.
func trustFolder(lines []string) string {
	header := -1
	for i := len(lines) - 1; i >= 0; i-- {
		if strings.TrimSpace(lines[i]) == trustHeader {
			header = i
			break
		}
	}
	if header < 0 {
		return ""
	}
	var b strings.Builder
	for _, line := range lines[header+1:] {
		line = strings.TrimSpace(line)
		if line == "" && b.Len() > 0 {
			break
		}
		b.WriteString(line)
	}
	return b.String()
}

// keys returns the keys that pick "Yes" (trust) or "No" from the
// highlighted option, or nil if the dialog isn't on screen or neither
// option is marked.
func (d trustDialog) keys(screen string, trust bool) []string {
	_, highlighted, ok := d.read(screen)
	if !ok || highlighted == "" {
		return nil
	}
	want := trustNo
	if trust {
		want = trustYes
	}
	if highlighted == want {
		return []string{"Enter"}
	}
	// The other option is highlighted: move to this one.
	if (want == trustYes) == d.yesFirst {
		return []string{"Up", "Enter"}
	}
	return []string{"Down", "Enter"}
}
