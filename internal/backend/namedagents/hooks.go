package namedagents

import (
	"bytes"
	"cmp"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/transcript"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Shared by the backends whose CLI reports status through hooks (claude, agy).

// hookTimeoutSec bounds each hook; `agent report` is a local DB write.
const hookTimeoutSec = 10

// hookCommand and hookMatcher are the hook entries both CLIs' settings take.
type hookCommand struct {
	Type    string `json:"type"`
	Command string `json:"command"`
	Timeout int    `json:"timeout"`
}

type hookMatcher struct {
	Matcher string        `json:"matcher"`
	Hooks   []hookCommand `json:"hooks"`
}

func newHookCommand(command string) hookCommand {
	return hookCommand{Type: "command", Command: command, Timeout: hookTimeoutSec}
}

// maxDetailLen keeps error details to a single readable line.
const maxDetailLen = 120

// maxToolInputLen bounds the tool input a report stores. Larger inputs
// (e.g. a Write of a big file) aren't stored; the detail still names the
// call.
const maxToolInputLen = 64 << 10

// toolSummary renders a tool call as "tool: <what it's about>": the file,
// else the most telling input, from the tool table. It is kept whole, line
// breaks included, and paths stay absolute: for a permission prompt, it is
// what an answer allows.
func toolSummary(tool string, input json.RawMessage) string {
	s := transcript.DescribeTool(tool, input, "")
	if v := cmp.Or(s.Target, s.Detail); v != "" {
		return tool + ": " + v
	}
	return tool
}

// callKey identifies a tool call by its tool and input, for
// HookReport.CallKey. It hashes the input, which may be too large to store.
func callKey(tool string, input json.RawMessage) string {
	var b bytes.Buffer
	if json.Compact(&b, input) != nil {
		b.Reset()
		b.Write(input)
	}
	sum := sha256.Sum256(append([]byte(tool+"\x00"), b.Bytes()...))
	return hex.EncodeToString(sum[:12])
}

// toolInput returns input, compacted, for HookReport.ToolInput, or "" if
// it isn't a JSON object or is too large to store.
func toolInput(input json.RawMessage) string {
	var b bytes.Buffer
	if err := json.Compact(&b, input); err != nil || b.Len() > maxToolInputLen || !bytes.HasPrefix(b.Bytes(), []byte("{")) {
		return ""
	}
	return b.String()
}

// permissionKey returns the key that answers a numbered permission dialog:
// "1" approves once, "2" approves always, Escape denies. questionTools raise
// a dialog that asks the user something instead, where "1" would pick an
// answer.
func permissionKey(resp types.PermissionResponse, tool string, questionTools map[string]bool) (string, error) {
	if questionTools[tool] {
		return "", fmt.Errorf("%s is a question for the user, not a permission prompt; answer it with agent keys (see it with agent capture), or --force", tool)
	}
	switch resp {
	case types.PermissionApprove:
		return "1", nil
	case types.PermissionApproveAlways:
		return "2", nil
	case types.PermissionDeny:
		return "Escape", nil
	default:
		return "", fmt.Errorf("unsupported permission response %d", resp)
	}
}

// dialogTailLines bounds dialog detection to the bottom of the screen, where
// the dialog sits, so numbered options in an older reply can't match.
const dialogTailLines = 12

// screenTail returns the last dialogTailLines non-blank lines of screen,
// bottom line first.
func screenTail(screen string) []string {
	var tail []string
	lines := strings.Split(screen, "\n")
	for i := len(lines) - 1; i >= 0 && len(tail) < dialogTailLines; i-- {
		if strings.TrimSpace(lines[i]) != "" {
			tail = append(tail, lines[i])
		}
	}
	return tail
}

// inputBox finds the input box at the bottom of screen (lines, as split from
// it): lines between two consecutive rules of "─", the first starting with
// prompt. The box is the bottom such region, or, with panels > 0, one of
// up to that many regions above it: agy draws a panel of running
// background tasks between rules of its own below its box. It returns the
// box's indexes, rules excluded, or ok false if there is no such box.
func inputBox(lines []string, prompt string, panels int) (start, end int, ok bool) {
	var rules []int
	for i := len(lines) - 1; i >= 0 && len(rules) < panels+2; i-- {
		if isRule(util.StripANSI(lines[i])) {
			rules = append(rules, i)
		}
	}
	for k := 0; k+1 < len(rules); k++ {
		top, bottom := rules[k+1], rules[k]
		if bottom-top < 2 {
			continue // an empty region is no box (and no panel)
		}
		if strings.HasPrefix(strings.TrimLeft(util.StripANSI(lines[top+1]), " "), prompt) {
			return top + 1, bottom, true
		}
	}
	return 0, 0, false
}

// inputBoxText returns the text typed in the input box of screen (with its
// styling escapes), whose first line starts with prompt, found as inputBox
// finds it. Text drawn dim (placeholders, suggestions) is left out.
func inputBoxText(screen, prompt string, panels int) (string, bool) {
	lines := strings.Split(strings.TrimRight(screen, "\n"), "\n")
	start, end, ok := inputBox(lines, prompt, panels)
	if !ok {
		return "", false
	}
	var text []string
	for i, line := range lines[start:end] {
		line = undimmed(line)
		if i == 0 {
			var ok bool
			if line, ok = strings.CutPrefix(strings.TrimLeft(line, " "), prompt); !ok {
				return "", false
			}
		}
		if t := strings.TrimSpace(strings.ReplaceAll(line, " ", " ")); t != "" {
			text = append(text, t)
		}
	}
	return strings.Join(text, "\n"), true
}

// isRule reports whether line is a horizontal rule of "─", plain or with a
// label set into it: Claude draws a named session's name (--name, /rename)
// into its input box's top rule, as in "──────── worker ─".
func isRule(line string) bool {
	line = strings.TrimSpace(line)
	rest := strings.TrimLeft(line, "─")
	label := strings.TrimRight(rest, "─")
	dashes := utf8.RuneCountInString(line) - utf8.RuneCountInString(label)
	if dashes < 10 {
		return false
	}
	if label == "" {
		return true
	}
	// A label: both ends of the rule, and a space either side of the text.
	return rest != line && label != rest && len(label) > 2 &&
		label[0] == ' ' && label[len(label)-1] == ' ' && !strings.Contains(label, "─")
}

// undimmed returns line's text without escapes, and without the text drawn
// dim (SGR 2, until 22 or a reset).
func undimmed(line string) string {
	var out strings.Builder
	dim := false
	for i := 0; i < len(line); i++ {
		if line[i] != '\x1b' {
			if !dim {
				out.WriteByte(line[i])
			}
			continue
		}
		if i+1 >= len(line) || line[i+1] != '[' {
			continue
		}
		j := i + 2
		for j < len(line) && (line[j] < 0x40 || line[j] > 0x7e) {
			j++
		}
		if j < len(line) && line[j] == 'm' {
			for _, p := range strings.Split(line[i+2:j], ";") {
				switch p {
				case "2":
					dim = true
				case "", "0", "22":
					dim = false
				}
			}
		}
		i = j
	}
	return out.String()
}

func truncate(s string) string {
	runes := []rune(strings.Join(strings.Fields(s), " "))
	if len(runes) > maxDetailLen {
		return string(runes[:maxDetailLen-1]) + "…"
	}
	return string(runes)
}

func firstNonEmpty(values ...string) string {
	for _, v := range values {
		if v != "" {
			return v
		}
	}
	return ""
}
