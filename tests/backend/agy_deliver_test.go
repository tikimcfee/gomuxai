package backend_test

import (
	"bytes"
	"encoding/json"
	"regexp"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// agyOutput decodes PostInvocation's delivery output strictly, as agy's
// protojson does: any field but these two is an error.
func agyOutput(t *testing.T, out []byte) []string {
	t.Helper()
	var v struct {
		InjectSteps []struct {
			UserMessage *string `json:"userMessage"`
		} `json:"injectSteps"`
		TerminationBehavior string `json:"terminationBehavior"`
	}
	dec := json.NewDecoder(bytes.NewReader(out))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&v); err != nil {
		t.Fatalf("delivery output %s: %v", out, err)
	}
	var steps []string
	for _, s := range v.InjectSteps {
		if s.UserMessage == nil {
			t.Fatalf("step without a userMessage in %s", out)
		}
		steps = append(steps, *s.UserMessage)
	}
	if len(steps) == 0 || v.TerminationBehavior != "force_continue" {
		t.Fatalf("delivery output %s: want userMessage steps and force_continue", out)
	}
	return steps
}

func TestAgyPostInvocation_DeliveryOnly(t *testing.T) {
	b := namedagents.NewAgyBackend()
	r, ok, err := b.ParseHookEvent("PostInvocation", []byte(`{"conversationId":"c1","invocationNum":0}`))
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if !r.DeliveryOnly || !b.CanDeliver(r) {
		t.Errorf("PostInvocation: DeliveryOnly=%v CanDeliver=%v, want both", r.DeliveryOnly, b.CanDeliver(r))
	}
	for _, event := range []string{"PreInvocation", "PostToolUse", "Stop"} {
		r, _, _ := b.ParseHookEvent(event, []byte(`{"conversationId":"c1","fullyIdle":true}`))
		if b.CanDeliver(r) || r.DeliveryOnly {
			t.Errorf("%s: CanDeliver=%v DeliveryOnly=%v, want neither (only PostInvocation delivers)", event, b.CanDeliver(r), r.DeliveryOnly)
		}
	}
}

// One userMessage per message, tag kept, text intact; nothing to deliver
// prints nothing (force_continue alone makes the model invent a reply).
func TestAgyDeliveryOutput(t *testing.T) {
	b := namedagents.NewAgyBackend()
	if out := b.DeliveryOutput(nil); out != nil {
		t.Errorf("DeliveryOutput(nil) = %s, want nil", out)
	}
	msgs := []string{
		"[from lead@hub] first",
		"[from a person on hub] two lines\n\"quoted\" `code` tab\tand é ✓",
		"[from lead@hub] bad utf-8: \xff\xfe",
	}
	steps := agyOutput(t, b.DeliveryOutput(msgs))
	if len(steps) != 3 || steps[0] != msgs[0] || steps[1] != msgs[1] {
		t.Errorf("steps = %q, want the messages as they are", steps)
	}
	if !strings.HasPrefix(steps[2], "[from lead@hub] bad utf-8: ") {
		t.Errorf("step 3 = %q", steps[2])
	}
}

// agy cuts a userMessage at about 192 KB, silently, so a big message goes
// in labeled parts that each fit, and together hold all of it.
func TestAgyDeliveryOutput_SplitsBigMessages(t *testing.T) {
	b := namedagents.NewAgyBackend()
	line := strings.Repeat("é", 50) + " line of a big message\n"
	big := "[from lead@hub] " + strings.Repeat(line, 3000) + "END-CODEWORD"
	steps := agyOutput(t, b.DeliveryOutput([]string{"[from lead@hub] small", big}))
	if steps[0] != "[from lead@hub] small" {
		t.Errorf("step 1 = %q", steps[0])
	}
	parts := steps[1:]
	if len(parts) < 4 {
		t.Fatalf("%d bytes went in %d parts, want at least 4", len(big), len(parts))
	}
	label := regexp.MustCompile(`(?m)^\[part \d+/\d+ of the message above\]\n|\n\[part \d+/\d+: the message goes on in the next prompt\]$`)
	var joined strings.Builder
	for i, p := range parts {
		if len(p) > 64<<10 {
			t.Errorf("part %d is %d bytes, over 64 KB", i+1, len(p))
		}
		if i == 0 && !strings.HasPrefix(p, "[from lead@hub] ") {
			t.Errorf("part 1 starts %q, want the sender's tag", p[:30])
		}
		joined.WriteString(label.ReplaceAllString(p, ""))
	}
	if joined.String() != big {
		t.Error("the parts, labels removed, don't add up to the message")
	}
	if want := "[part 2/"; !strings.HasPrefix(parts[1], want) {
		t.Errorf("part 2 starts %q, want %q", parts[1][:20], want)
	}
}

func TestAgyAskQuestionSummary(t *testing.T) {
	b := namedagents.NewAgyBackend()
	payload := `{"conversationId":"c1","toolCall":{"name":"ask_question","args":{"questions":[{"options":["red","green","blue"],"question":"Which color do you prefer?"},{"options":["a","b"],"question":"And?"}]}}}`
	r, _, err := b.ParseHookEvent("PreToolUse", []byte(payload))
	if err != nil {
		t.Fatal(err)
	}
	if want := "ask_question: Which color do you prefer? (red, green, blue); 1 more question"; r.Detail != want {
		t.Errorf("detail = %q, want %q", r.Detail, want)
	}
}

const agyRule = "────────────────────────────────────────────────────────────────────────────────"

// agyScreen is the bottom of an agy pane (ANSI-stripped): body lines, then
// the input box holding input, then footer.
func agyScreen(body []string, input, footer string) string {
	lines := append(append([]string{}, body...), agyRule, ">"+input, agyRule, footer, "", "")
	return strings.Join(lines, "\n")
}

// Screens from agy 1.4.7, trimmed.
func TestAgyAtPrompt(t *testing.T) {
	b := namedagents.NewAgyBackend()
	reply := []string{"> [from lead@hub] Reply READY.", "", "  READY", ""}
	interrupted := []string{"● Ran (sleep 40; echo slept) (ctrl+o to expand)", "", "  ⎿  Interrupted · What should Antigravity CLI do instead?"}
	running := []string{"● Running (sleep 40; echo slept) (ctrl+o to expand)", "⣟  Running command...", "└ Tip: Press ctrl+o to expand or collapse tool details."}
	tests := []struct {
		name   string
		screen string
		want   bool
		how    string
	}{
		{"idle", agyScreen(reply, "", "? for shortcuts                                            Gemini 4 Argon · high"), true, ""},
		{"idle, text typed", agyScreen(reply, " half typed text", "                                                           Gemini 4 Argon · high"), true, ""},
		{"interrupted", agyScreen(interrupted, "", "? for shortcuts                                            Gemini 4 Argon · high"), true, "interrupted"},
		{"interrupted, queued text put back", agyScreen(interrupted, " typed while busy", "                                                           Gemini 4 Argon · high"), true, "interrupted"},
		{"running", agyScreen(running, "", "esc to cancel                                              Gemini 4 Argon · high"), false, ""},
		{"running, text typed", agyScreen(running, " typed while busy", "esc to cancel                                              Gemini 4 Argon · high"), false, ""},
		{"running, text queued", agyScreen(append(running, "▸ typed while busy (alt+m to send immediately)"), "", "  Press up to edit queued messages                         Gemini 4 Argon · high"), false, ""},
		{"question dialog", agyQuestionScreen, false, ""},
		{"no box", "user@laptop $ agy\n", false, ""},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			got, how := b.AtPrompt(tt.screen)
			if got != tt.want || how != tt.how {
				t.Errorf("AtPrompt = %v, %q; want %v, %q", got, how, tt.want, tt.how)
			}
		})
	}
}

// agy's input line, styled as tmux capture-pane -e shows it.
func TestAgyInputText(t *testing.T) {
	b := namedagents.NewAgyBackend()
	rule := "\x1b[90m" + agyRule
	screen := func(input string) string {
		return strings.Join([]string{"  READY", "", rule, "\x1b[94m>\x1b[39m" + input, rule,
			"\x1b[39m                                                           \x1b[2mGemini 4 Argon · high"}, "\n")
	}
	if text, found := b.InputText(screen("")); !found || text != "" {
		t.Errorf("empty: %q, %v", text, found)
	}
	if text, found := b.InputText(screen(" half typed text")); !found || text != "half typed text" {
		t.Errorf("typed: %q, %v", text, found)
	}
	if _, found := b.InputText(agyQuestionScreen); found {
		t.Error("question dialog: found an input line")
	}
}

var agyQuestionScreen = strings.Join([]string{
	"? Which color do you prefer?",
	"",
	"Question",
	agyRule,
	"",
	"Question 1/1: Which color do you prefer?",
	"",
	"> 1. red",
	"  2. green",
	"  3. blue",
	"  4. Write-in...",
	"",
	"  ↑/↓ Navigate · enter Select · esc Skip",
	"esc to cancel                                              Gemini 4 Argon · high",
}, "\n")

func TestAgyQuestionPromptVisible(t *testing.T) {
	b := namedagents.NewAgyBackend()
	if !b.QuestionPromptVisible(agyQuestionScreen) {
		t.Error("question dialog not seen")
	}
	if b.PermissionPromptVisible(agyQuestionScreen) {
		t.Error("question dialog taken for a permission prompt")
	}
	yesNo := strings.NewReplacer("1. red", "1. Yes", "2. green", "2. No").Replace(agyQuestionScreen)
	if !b.QuestionPromptVisible(yesNo) || b.PermissionPromptVisible(yesNo) {
		t.Error("a Yes/No question must read as a question, not a permission prompt")
	}
	permission := strings.Join([]string{
		"Run this command?",
		"> 1. Yes, run command",
		"  2. No, cancel",
		"  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command",
		"esc to cancel",
	}, "\n")
	if b.QuestionPromptVisible(permission) || !b.PermissionPromptVisible(permission) {
		t.Error("permission dialog taken for a question")
	}
	idle := agyScreen(nil, "", "? for shortcuts")
	if b.QuestionPromptVisible(idle) {
		t.Error("idle prompt taken for a question")
	}
}
