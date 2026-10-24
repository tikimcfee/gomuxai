package transcript

import (
	"encoding/json"
	"fmt"
	"regexp"
	"strings"
)

// Agy reads the Antigravity CLI's conversation transcripts
// (~/.gemini/antigravity-cli/brain/<conversation id>/.system_generated/logs/transcript_full.jsonl).
// Ported from glyph3d-rs antigravity.rs (parse_antigravity_session).
//
// Each line is one step: USER_INPUT (a prompt), PLANNER_RESPONSE (the
// model's thinking, text, and tool calls), then one GENERIC step per tool
// call, in order, holding its output. SYSTEM_MESSAGE and CHECKPOINT steps
// are left out.
type Agy struct{}

func (Agy) Harness() string { return "agy" }

type agyStep struct {
	StepIndex int    `json:"step_index"`
	Type      string `json:"type"`
	Status    string `json:"status"`
	CreatedAt string `json:"created_at"`
	Content   string `json:"content"`
	Thinking  string `json:"thinking"`
	Error     string `json:"error"`
	CWD       string `json:"cwd"`
	ToolCalls []struct {
		Name string          `json:"name"`
		Args json.RawMessage `json:"args"`
	} `json:"tool_calls"`
}

func (Agy) Parse(data []byte) *Session {
	s := &Session{Harness: "agy"}
	t := turns{s}
	// Tool calls of the current turn still waiting for their GENERIC step.
	var pending []*ToolCall
	eachLine(s, data, func(line []byte) bool {
		var st agyStep
		if json.Unmarshal(line, &st) != nil {
			return false
		}
		at := parseTime(st.CreatedAt)
		s.seen(at)
		if s.CWD == "" {
			s.CWD = st.CWD
		}
		switch st.Type {
		case "USER_INPUT":
			if prompt := agyPrompt(st.Content); prompt != "" {
				t.prompt(prompt, at)
				pending = nil
			}
		case "PLANNER_RESPONSE":
			if text := strings.TrimSpace(st.Thinking); text != "" {
				t.step(Step{Time: at, Thinking: text})
			}
			if text := strings.TrimSpace(st.Content); text != "" {
				t.step(Step{Time: at, Reply: text})
			}
			for i, tc := range st.ToolCalls {
				call := &ToolCall{
					ID:     fmt.Sprintf("step_%d_tool_%d", st.StepIndex, i),
					Name:   tc.Name,
					Input:  tc.Args,
					Status: StatusNone,
				}
				t.step(Step{Time: at, Tool: call})
				pending = append(pending, call)
			}
		case "GENERIC":
			if len(pending) == 0 {
				break
			}
			call := pending[0]
			pending = pending[1:]
			call.Output = agyOutput(st.Content)
			call.Status = agyStatus(st, call.Output)
		}
		return true
	})
	return s
}

// agyPrompt returns what the user typed: USER_INPUT content wraps it in
// <USER_REQUEST> and appends <ADDITIONAL_METADATA> (time, settings changes).
func agyPrompt(content string) string {
	if req, ok := tagText(content, "USER_REQUEST"); ok {
		return req
	}
	return strings.TrimSpace(content)
}

// agyOutput returns a tool's output without the "Created At:" and
// "Completed At:" lines agy puts first.
func agyOutput(content string) string {
	rest := content
	for {
		line, after, _ := strings.Cut(rest, "\n")
		if !strings.HasPrefix(line, "Created At:") && !strings.HasPrefix(line, "Completed At:") {
			break
		}
		rest = after
	}
	return strings.TrimSpace(rest)
}

// agyExitCode matches run_command's "The command exited with code N."
var agyExitCode = regexp.MustCompile(`(?m)^(?:The c|C)ommand exited with code (\d+)\.`)

// agyStatus is a tool step's result: agy marks failed steps ERROR (or
// INVALID, for bad arguments) and backgrounded ones RUNNING. A command that
// exits non-zero counts as failed, as Claude Code's Bash does.
func agyStatus(st agyStep, output string) string {
	switch {
	case st.Status == "ERROR" || st.Status == "INVALID" || st.Error != "":
		return StatusError
	case st.Status == "RUNNING":
		return StatusRunning
	}
	if m := agyExitCode.FindStringSubmatch(output); m != nil && m[1] != "0" {
		return StatusError
	}
	return StatusOK
}
