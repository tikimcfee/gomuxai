package service

import "testing"

func TestDetectCLIInArgs(t *testing.T) {
	tests := []struct {
		name        string
		args        []string
		wantBackend string
		wantSession string
		wantHooked  bool
		wantCmd     string
	}{
		{
			name:        "plain claude with resume",
			args:        []string{"/usr/local/bin/claude", "--resume", "abc-123"},
			wantBackend: "claude",
			wantSession: "abc-123",
			wantHooked:  false,
			wantCmd:     "claude",
		},
		{
			name:        "unhooked agy via env wrapper",
			args:        []string{"env", "-u", "SSH_CONNECTION", "agy", "--conversation", "conv-99"},
			wantBackend: "agy",
			wantSession: "conv-99",
			wantHooked:  false,
			wantCmd:     "agy",
		},
		{
			name:        "hooked agy with gomuxai add-dir",
			args:        []string{"/Users/lugo/.local/bin/agy", "--add-dir", "/Users/lugo/.local/bin/.gomuxai/agents/helper", "--conversation=conv-42"},
			wantBackend: "agy",
			wantSession: "conv-42",
			wantHooked:  true,
			wantCmd:     "agy",
		},
		{
			name:        "aider",
			args:        []string{"aider", "--model", "sonnet"},
			wantBackend: "aider",
			wantSession: "",
			wantHooked:  false,
			wantCmd:     "aider",
		},
		{
			name:        "custom tui falls back to shell backend",
			args:        []string{"vim", "README.md"},
			wantBackend: "shell",
			wantSession: "",
			wantHooked:  false,
			wantCmd:     "vim",
		},
		{
			name:        "editor opening file in antigravity-cli path is not misclassified as agy",
			args:        []string{"vim", "/Users/lugo/.gemini/antigravity-cli/config.json", "claude"},
			wantBackend: "shell",
			wantSession: "",
			wantHooked:  false,
			wantCmd:     "vim",
		},
		{
			name:        "node wrapper running agy script is classified as agy",
			args:        []string{"/usr/local/bin/node", "--no-warnings", "/opt/antigravity-cli/bin/agy", "--conversation", "conv-node"},
			wantBackend: "agy",
			wantSession: "conv-node",
			wantHooked:  false,
			wantCmd:     "agy",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			be, sess, hooked, cmd := detectCLIInArgs(tt.args)
			if be != tt.wantBackend || sess != tt.wantSession || hooked != tt.wantHooked || cmd != tt.wantCmd {
				t.Errorf("detectCLIInArgs(%v) = (%q, %q, %v, %q), want (%q, %q, %v, %q)",
					tt.args, be, sess, hooked, cmd, tt.wantBackend, tt.wantSession, tt.wantHooked, tt.wantCmd)
			}
		})
	}
}

func TestExtractAgyConversationFromLockPath(t *testing.T) {
	got := extractAgyConversationFromLockPath("/Users/lugo/.gemini/antigravity-cli/presence/a9a93a44-198a-4ab7-acdd-c435f1cb200c.lock")
	if got != "a9a93a44-198a-4ab7-acdd-c435f1cb200c" {
		t.Errorf("got %q, want uuid", got)
	}
	if other := extractAgyConversationFromLockPath("/tmp/other.lock"); other != "" {
		t.Errorf("got %q for non-presence lock, want empty", other)
	}
}
