package service

import (
	"slices"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/config"
)

func TestBriefingExplainsSenderTags(t *testing.T) {
	s := &AgentService{cfg: &config.Config{Host: "h"}}
	brief := s.agentInstructions("lead", "", "claude")
	for _, want := range []string{`"[from NAME@HOST]"`, `"[from a person on HOST]"`, `"[from a Claude session on HOST]"`, "no agent record",
		"Stop hook feedback", "--now", "not through a shell variable"} {
		if !strings.Contains(brief, want) {
			t.Errorf("briefing lacks %s", want)
		}
	}
}

// agy gets held messages as prompts after a step, not as Stop hook feedback.
func TestBriefingForAgy(t *testing.T) {
	s := &AgentService{cfg: &config.Config{Host: "h"}}
	brief := s.agentInstructions("lead", "", "agy")
	if strings.Contains(brief, "Stop hook") || !strings.Contains(brief, "current step") || !strings.Contains(brief, "--now") {
		t.Errorf("agy briefing on held messages:\n%s", brief)
	}
	if !strings.Contains(brief, "rather than tmux to reach other agents.\n") {
		t.Error("agy briefing lost the paragraph after held messages")
	}
}

func TestBriefingCoversFriction(t *testing.T) {
	s := &AgentService{cfg: &config.Config{Host: "h", AgentBinary: "/opt/x/agent"}}
	brief := s.agentInstructions("lead", "", "claude")
	for _, want := range []string{"## When the agent tool gets in your way", "`agent friction list`",
		"`agent friction +1 ID`", "    agent friction add \"summary\" --kind help|error|missing|bug|docs|other\n"} {
		if !strings.Contains(brief, want) {
			t.Errorf("briefing lacks %q", want)
		}
	}

	prefixes := s.allowedCommandPrefixes()
	for _, want := range []string{"agent friction add", "/opt/x/agent friction +1", "agent friction list", "agent friction show", "agent history", "agent capture", "/opt/x/agent capture", "agent logs", "/opt/x/agent logs"} {
		if !slices.Contains(prefixes, want) {
			t.Errorf("pre-allowed commands lack %q", want)
		}
	}
	for _, p := range prefixes {
		if strings.Contains(p, "accept") || strings.Contains(p, "resolve") || p == "agent friction" {
			t.Errorf("pre-allowed %q: accepting and resolving friction is for people", p)
		}
	}
}
