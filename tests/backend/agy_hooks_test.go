package backend_test

import (
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// Payloads trimmed from real Antigravity CLI (agy) hook invocations
// (see exploration/agy-hooks-findings.md).
func TestAgyParseHookEvent(t *testing.T) {
	provider := namedagents.NewAgyBackend()

	tests := []struct {
		name       string
		event      string
		payload    string
		wantOK     bool
		wantStatus backend.Status
		wantDetail string
		wantTool   string
	}{
		{
			name:       "pre invocation starts processing",
			event:      "PreInvocation",
			payload:    `{"conversationId":"c1","executionNum":0,"invocationNum":0,"lastUserInput":"hi"}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "prompt received",
		},
		{
			name:       "pre tool use summarizes command",
			event:      "PreToolUse",
			payload:    `{"conversationId":"c1","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo probe","toolSummary":"Echo"}}}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "run_command: echo probe",
			wantTool:   "run_command",
		},
		{
			name:       "pre tool use summarizes file path",
			event:      "PreToolUse",
			payload:    `{"conversationId":"c1","stepIdx":2,"toolCall":{"name":"read_file","args":{"Path":"/tmp/x.go"}}}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "read_file: /tmp/x.go",
			wantTool:   "read_file",
		},
		{
			name:       "post tool use success",
			event:      "PostToolUse",
			payload:    `{"conversationId":"c1","error":"","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"echo probe"}}}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "run_command: echo probe",
			wantTool:   "run_command",
		},
		{
			name:       "post tool use failure",
			event:      "PostToolUse",
			payload:    `{"conversationId":"c1","error":"exit status 1","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"false"}}}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "failed: run_command: false",
			wantTool:   "run_command",
		},
		{
			name:       "stop fully idle",
			event:      "Stop",
			payload:    `{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":true,"lastUserInput":"hi","terminationReason":"NO_TOOL_CALL"}`,
			wantOK:     true,
			wantStatus: backend.StatusIdle,
			wantDetail: "turn finished",
		},
		{
			name:       "stop with background tasks still running",
			event:      "Stop",
			payload:    `{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":false,"lastUserInput":"hi","terminationReason":"NO_TOOL_CALL"}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
			wantDetail: "background tasks running",
		},
		{
			name:       "stop with error message",
			event:      "Stop",
			payload:    `{"conversationId":"c1","error":"API rate limit exceeded","executionNum":0,"fullyIdle":true,"terminationReason":"ERROR"}`,
			wantOK:     true,
			wantStatus: backend.StatusError,
			wantDetail: "API rate limit exceeded",
		},
		{
			name:       "stop with max invocations termination reason",
			event:      "Stop",
			payload:    `{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":true,"terminationReason":"MAX_INVOCATIONS"}`,
			wantOK:     true,
			wantStatus: backend.StatusError,
			wantDetail: "MAX_INVOCATIONS",
		},
		{
			name:       "stop with user canceled",
			event:      "Stop",
			payload:    `{"conversationId":"c1","error":"","executionNum":0,"fullyIdle":true,"terminationReason":"USER_CANCELED"}`,
			wantOK:     true,
			wantStatus: backend.StatusIdle,
			wantDetail: "USER_CANCELED",
		},
		{
			name:    "session start is ignored",
			event:   "SessionStart",
			payload: `{"conversationId":"c1","executionNum":0,"invocationNum":0,"lastUserInput":"hi"}`,
			wantOK:  false,
		},
		{
			name:       "post invocation is reported only to deliver",
			event:      "PostInvocation",
			payload:    `{"conversationId":"c1","initialNumSteps":3,"invocationNum":1}`,
			wantOK:     true,
			wantStatus: backend.StatusProcessing,
		},
		{
			name:    "failed post invocation is left to stop",
			event:   "PostInvocation",
			payload: `{"conversationId":"c1","error":"quota exceeded","invocationNum":0}`,
			wantOK:  false,
		},
		{
			name:    "cancelled post invocation is left to stop",
			event:   "PostInvocation",
			payload: `{"conversationId":"c1","invocationNum":0,"terminationReason":"USER_CANCELED"}`,
			wantOK:  false,
		},
		{
			name:    "unknown event is ignored",
			event:   "SomethingElse",
			payload: `{"conversationId":"c1"}`,
			wantOK:  false,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			r, ok, err := provider.ParseHookEvent(tt.event, []byte(tt.payload))
			if err != nil {
				t.Fatalf("ParseHookEvent: %v", err)
			}
			if ok != tt.wantOK {
				t.Fatalf("ok = %v, want %v", ok, tt.wantOK)
			}
			if !ok {
				return
			}
			if r.Event != tt.event {
				t.Errorf("event = %q, want %q", r.Event, tt.event)
			}
			if r.Status != tt.wantStatus {
				t.Errorf("status = %v, want %v", r.Status, tt.wantStatus)
			}
			if r.Detail != tt.wantDetail {
				t.Errorf("detail = %q, want %q", r.Detail, tt.wantDetail)
			}
			if r.Tool != tt.wantTool {
				t.Errorf("tool = %q, want %q", r.Tool, tt.wantTool)
			}
			if r.SessionID != "c1" {
				t.Errorf("session id = %q, want c1", r.SessionID)
			}
		})
	}
}

// Every payload names the conversation's transcript; agent logs reads it.
func TestAgyParseHookEvent_Transcript(t *testing.T) {
	const path = "/home/user/.gemini/antigravity-cli/brain/c1/.system_generated/logs/transcript_full.jsonl"
	payload := `{"conversationId":"c1","transcriptPath":"` + path + `","stepIdx":1,"toolCall":{"name":"run_command","args":{"CommandLine":"ls"}}}`
	r, ok, err := namedagents.NewAgyBackend().ParseHookEvent("PostToolUse", []byte(payload))
	if err != nil || !ok {
		t.Fatalf("ok=%v err=%v", ok, err)
	}
	if r.Transcript != path {
		t.Errorf("transcript = %q, want %q", r.Transcript, path)
	}
}

func TestAgyParseHookEvent_InvalidJSON(t *testing.T) {
	_, _, err := namedagents.NewAgyBackend().ParseHookEvent("PreInvocation", []byte("not json"))
	if err == nil {
		t.Fatal("expected error for invalid payload")
	}
}

// agy's permission dialog follows PreToolUse, so that event keeps the call
// whole.
func TestAgyParseHookEvent_KeepsToolCallWhole(t *testing.T) {
	payload := `{"conversationId":"c1","toolCall":{"name":"run_command","args":{"CommandLine":"` + strings.Repeat("é", 300) + `"}}}`
	r, _, err := namedagents.NewAgyBackend().ParseHookEvent("PreToolUse", []byte(payload))
	if err != nil {
		t.Fatalf("ParseHookEvent: %v", err)
	}
	if want := "run_command: " + strings.Repeat("é", 300); r.Detail != want {
		t.Errorf("detail = %q, want the whole command", r.Detail)
	}
	if want := `{"CommandLine":"` + strings.Repeat("é", 300) + `"}`; r.ToolInput != want {
		t.Errorf("tool input = %q, want %q", r.ToolInput, want)
	}
}

func TestAgyLaunchCommandWithHooks(t *testing.T) {
	agentDir := filepath.Join(t.TempDir(), "my agent's dir")
	provider := namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{
		AgentDir: agentDir,
	})
	reportCmd := `'/opt/my tools/it'\''s/agent' report --hook --as 'helper' --spawn-id '11111111-2222-3333-4444-555555555555'`

	cmd := provider.LaunchCommandWithHooks(reportCmd, "11111111-2222-3333-4444-555555555555")
	// agy must launch without SSH_* vars, or it ignores the saved login.
	if !strings.HasPrefix(cmd, "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy --add-dir ") {
		t.Fatalf("unexpected launch command: %s", cmd)
	}
	wantExit := `; '/opt/my tools/it'\''s/agent' report exited --as 'helper' --spawn-id '11111111-2222-3333-4444-555555555555'`
	if !strings.HasSuffix(cmd, wantExit) {
		t.Fatalf("launch command missing exit wrapper:\ngot:  %s\nwant suffix: %s", cmd, wantExit)
	}

	// Let a real shell split the agy invocation arguments (before ';').
	agyPart := strings.TrimSuffix(cmd, wantExit)
	out, err := exec.Command("sh", "-c", `printf '%s\n' `+strings.TrimPrefix(agyPart, "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy ")).Output()
	if err != nil {
		t.Fatalf("shell parsing launch command: %v", err)
	}
	args := strings.Split(strings.TrimRight(string(out), "\n"), "\n")
	if len(args) != 2 || args[0] != "--add-dir" || args[1] != agentDir {
		t.Fatalf("unexpected args: %q", args)
	}

	// AppendPromptFlag must place -i before the '; ... report exited' suffix.
	withPrompt := provider.AppendPromptFlag(cmd, "Run task")
	if !strings.HasSuffix(withPrompt, " -i 'Run task'"+wantExit) {
		t.Errorf("AppendPromptFlag with exit wrapper = %s", withPrompt)
	}

	// Verify .agents/hooks.json was generated in agentDir.
	hooksPath := filepath.Join(agentDir, ".agents", "hooks.json")
	data, err := os.ReadFile(hooksPath)
	if err != nil {
		t.Fatalf("reading generated hooks.json: %v", err)
	}

	var doc struct {
		Gomuxai struct {
			PreInvocation []struct {
				Type    string `json:"type"`
				Command string `json:"command"`
			} `json:"PreInvocation"`
			PreToolUse []struct {
				Matcher string `json:"matcher"`
				Hooks   []struct {
					Type    string `json:"type"`
					Command string `json:"command"`
				} `json:"hooks"`
			} `json:"PreToolUse"`
			PostToolUse []struct {
				Matcher string `json:"matcher"`
				Hooks   []struct {
					Type    string `json:"type"`
					Command string `json:"command"`
				} `json:"hooks"`
			} `json:"PostToolUse"`
			PostInvocation []struct {
				Type    string `json:"type"`
				Command string `json:"command"`
			} `json:"PostInvocation"`
			Stop []struct {
				Type    string `json:"type"`
				Command string `json:"command"`
			} `json:"Stop"`
		} `json:"gomuxai"`
	}
	if err := json.Unmarshal(data, &doc); err != nil {
		t.Fatalf("unmarshaling hooks.json: %v\n%s", err, string(data))
	}

	if len(doc.Gomuxai.PreInvocation) != 1 || doc.Gomuxai.PreInvocation[0].Command != reportCmd+" --event PreInvocation" {
		t.Errorf("PreInvocation = %+v", doc.Gomuxai.PreInvocation)
	}
	if len(doc.Gomuxai.PreToolUse) != 1 || len(doc.Gomuxai.PreToolUse[0].Hooks) != 1 ||
		doc.Gomuxai.PreToolUse[0].Hooks[0].Command != reportCmd+" --event PreToolUse" {
		t.Errorf("PreToolUse = %+v", doc.Gomuxai.PreToolUse)
	}
	if len(doc.Gomuxai.PostToolUse) != 1 || len(doc.Gomuxai.PostToolUse[0].Hooks) != 1 ||
		doc.Gomuxai.PostToolUse[0].Hooks[0].Command != reportCmd+" --event PostToolUse" {
		t.Errorf("PostToolUse = %+v", doc.Gomuxai.PostToolUse)
	}
	if len(doc.Gomuxai.PostInvocation) != 1 || doc.Gomuxai.PostInvocation[0].Command != reportCmd+" --event PostInvocation" {
		t.Errorf("PostInvocation = %+v", doc.Gomuxai.PostInvocation)
	}
	if len(doc.Gomuxai.Stop) != 1 || doc.Gomuxai.Stop[0].Command != reportCmd+" --event Stop" {
		t.Errorf("Stop = %+v", doc.Gomuxai.Stop)
	}
}

func TestAgyLaunchCommandWithHooks_KeepsSkipPermissions(t *testing.T) {
	provider := namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{
		SkipPermissions: true,
		AgentDir:        t.TempDir(),
	})
	cmd := provider.LaunchCommandWithHooks("agent report --hook", "sid")
	if !strings.HasPrefix(cmd, "env -u SSH_CONNECTION -u SSH_CLIENT -u SSH_TTY agy --dangerously-skip-permissions --add-dir ") {
		t.Errorf("unexpected launch command: %s", cmd)
	}
}

func TestAgyAppendPromptFlag(t *testing.T) {
	provider := namedagents.NewAgyBackend()
	got := provider.AppendPromptFlag("agy --add-dir '/tmp/a'", "Please run viewer's task")
	want := `agy --add-dir '/tmp/a' -i 'Please run viewer'\''s task'`
	if got != want {
		t.Errorf("AppendPromptFlag = %q, want %q", got, want)
	}
}

func TestAgyPermissionKey(t *testing.T) {
	provider := namedagents.NewAgyBackend()
	want := map[backend.PermissionResponse]string{
		backend.PermissionApprove:       "1",
		backend.PermissionApproveAlways: "2",
		backend.PermissionDeny:          "Escape",
	}
	for resp, key := range want {
		got, err := provider.PermissionKey(resp, "run_command")
		if err != nil || got != key {
			t.Errorf("PermissionKey(%v) = %q, %v; want %q", resp, got, err, key)
		}
	}
}

func TestAgyPermissionKey_RefusesQuestionDialogs(t *testing.T) {
	provider := namedagents.NewAgyBackend()
	if key, err := provider.PermissionKey(backend.PermissionApprove, "ask_question"); err == nil {
		t.Errorf("PermissionKey(approve, ask_question) = %q; want an error", key)
	}
	if _, err := provider.PermissionKey(backend.PermissionApprove, ""); err != nil {
		t.Errorf("empty tool should be treated as a plain prompt: %v", err)
	}
}

func TestAgyPermissionPromptVisible(t *testing.T) {
	provider := namedagents.NewAgyBackend()

	cmdPrompt := strings.Join([]string{
		"Requesting permission for:",
		"   echo interactive-approval-test",
		"Run this command?",
		"> 1. Yes, run command",
		"  2. Yes, and always allow in this conversation for commands that start with 'echo'",
		"  3. Yes, and always allow for commands that start with 'echo' (Persist to settings.json)",
		"  4. No, cancel",
		"  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command",
		"esc to cancel                                                     Gemini 3.8 Flash · low",
	}, "\n")
	if !provider.PermissionPromptVisible(cmdPrompt) {
		t.Error("expected command permission dialog to be visible")
	}

	editPrompt := strings.Join([]string{
		"Accept this file edit?",
		"  1. Yes, accept this change",
		"> 2. Yes, and always allow in this conversation",
		"  3. No, reject this change",
		"esc to cancel",
	}, "\n")
	if !provider.PermissionPromptVisible(editPrompt) {
		t.Error("expected file edit permission dialog to be visible")
	}

	idleScreen := strings.Join([]string{
		">",
		"? for shortcuts                                                   Gemini 3.8 Flash · low",
	}, "\n")
	if provider.PermissionPromptVisible(idleScreen) {
		t.Error("idle screen should not match permission dialog")
	}

	generatingScreen := "Generating... (Enter/Esc to cancel)\nesc to cancel"
	if provider.PermissionPromptVisible(generatingScreen) {
		t.Error("generating spinner without Yes/No options should not match")
	}

	scrolledPast := cmdPrompt + "\n" + strings.Repeat("output line\n", 15) + "? for shortcuts"
	if provider.PermissionPromptVisible(scrolledPast) {
		t.Error("dialog scrolled off the bottom of the screen should not match")
	}
}

// The briefing reaches agy as AGENTS.md at the root of an --add-dir directory,
// which is where agy looks (verified against agy 1.4.x: not .agents/rules).
func TestAgyWithInstructions(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "instructions.md")
	os.WriteFile(path, []byte("# briefing\n"), 0600)

	// Hooks already added the agent's dir: no second --add-dir, wrapper kept.
	provider := namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{AgentDir: dir})
	withHooks := provider.LaunchCommandWithHooks("agent report --hook", "sid")
	cmd := provider.WithInstructions(withHooks, path)
	if strings.Count(cmd, "--add-dir") != 1 || !strings.HasSuffix(cmd, "; agent report exited") {
		t.Errorf("with hooks: %s", cmd)
	}
	if data, err := os.ReadFile(filepath.Join(dir, "AGENTS.md")); err != nil || string(data) != "# briefing\n" {
		t.Errorf("AGENTS.md = %q, %v", data, err)
	}

	// Without hooks, the directory is added, before nothing else.
	plain := namedagents.NewAgyBackend()
	cmd = plain.WithInstructions(plain.LaunchCommand(), path)
	if !strings.HasSuffix(cmd, " --add-dir '"+dir+"'") {
		t.Errorf("without hooks: %s", cmd)
	}
}

func TestAgyTrustKeys(t *testing.T) {
	b := namedagents.NewAgyBackend()
	if got := b.TrustKeys("Do you trust the contents of this project?\n> Yes, I trust this folder"); strings.Join(got, ",") != "Enter" {
		t.Errorf("TrustKeys = %v, want Enter", got)
	}
	if got := b.TrustKeys("? for shortcuts"); got != nil {
		t.Errorf("TrustKeys at the prompt = %v, want nil", got)
	}
}
