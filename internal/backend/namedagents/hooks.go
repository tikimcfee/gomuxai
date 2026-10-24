package namedagents

import (
	"encoding/json"
	"fmt"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend/types"
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

// maxDetailLen keeps status details to a single readable line.
const maxDetailLen = 120

// toolSummary renders a tool call as "tool: <most telling input>", the
// input being the first of keys set in it.
func toolSummary(tool string, input json.RawMessage, keys []string) string {
	var fields map[string]any
	_ = json.Unmarshal(input, &fields)
	for _, key := range keys {
		if v, ok := fields[key].(string); ok && v != "" {
			return truncate(tool + ": " + v)
		}
	}
	return tool
}

// permissionKey returns the key that answers a numbered permission dialog:
// "1" approves once, "2" approves always, Escape denies. questionTools raise
// a dialog that asks the user something instead, where "1" would pick an
// answer.
func permissionKey(resp types.PermissionResponse, tool string, questionTools map[string]bool) (string, error) {
	if questionTools[tool] {
		return "", fmt.Errorf("%s is a question for the user, not a permission prompt; answer it with send/keys (or --force)", tool)
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
