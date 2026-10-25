package service_test

import (
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// TestPreToolUseAllowsSafeAgentCommands: the PreToolUse hook approves one
// plain run of the agent binary with a safe subcommand, however it's
// invoked, and prints nothing for anything else.
func TestPreToolUseAllowsSafeAgentCommands(t *testing.T) {
	env := setupTestEnv(t)
	root := t.TempDir()
	bin := filepath.Join(root, "bin", "agent")
	if err := os.Mkdir(filepath.Dir(bin), 0755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(bin, []byte("#!/bin/sh\n"), 0755); err != nil {
		t.Fatal(err)
	}
	env.cfg.AgentBinary = bin
	fake := spawnFake(t, env, "worker")

	hook := func(tool, command string) []byte {
		t.Helper()
		input, _ := json.Marshal(map[string]string{"command": command})
		payload, _ := json.Marshal(map[string]any{
			"hook_event_name": "PreToolUse", "session_id": fake.sessionID, "cwd": root,
			"tool_name": tool, "tool_input": json.RawMessage(input),
		})
		out, err := env.service.HandleHook(context.Background(), "worker", "", "", payload)
		if err != nil {
			t.Fatalf("HandleHook %s: %v", payload, err)
		}
		return out
	}

	for _, cmd := range []string{"./bin/agent status worker -v", "./bin/agent -q list", bin + " state get worker _result"} {
		var v struct {
			HookSpecificOutput struct {
				HookEventName      string `json:"hookEventName"`
				PermissionDecision string `json:"permissionDecision"`
			} `json:"hookSpecificOutput"`
		}
		out := hook("Bash", cmd)
		if err := json.Unmarshal(out, &v); err != nil || v.HookSpecificOutput.HookEventName != "PreToolUse" ||
			v.HookSpecificOutput.PermissionDecision != "allow" {
			t.Errorf("%s: hook output %s, want a PreToolUse allow (%v)", cmd, out, err)
		}
		// Approving doesn't replace the status report.
		if got, want := stateOf(t, env, "worker", "_detail"), "Bash: "+cmd; got != want {
			t.Errorf("_detail = %q, want %q", got, want)
		}
	}
	for _, cmd := range []string{"./bin/agent spawn x --backend claude", "./bin/agent list; rm -rf ~", "$A list", "ls"} {
		if out := hook("Bash", cmd); out != nil {
			t.Errorf("%s: hook output %s, want none", cmd, out)
		}
	}
	if out := hook("Read", "./bin/agent list"); out != nil {
		t.Errorf("Read: hook output %s, want none", out)
	}
}
