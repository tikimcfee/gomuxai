package service

import (
	"slices"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/config"
)

func TestBriefingExplainsSenderTags(t *testing.T) {
	s := &AgentService{cfg: &config.Config{Host: "h"}}
	brief := s.agentInstructions("lead", "")
	for _, want := range []string{`"[from NAME@HOST]"`, `"[from a person on HOST]"`} {
		if !strings.Contains(brief, want) {
			t.Errorf("briefing lacks %s", want)
		}
	}
}

func TestBriefingCoversFriction(t *testing.T) {
	s := &AgentService{cfg: &config.Config{Host: "h", AgentBinary: "/opt/x/agent"}}
	brief := s.agentInstructions("lead", "")
	for _, want := range []string{"## When the agent tool gets in your way", "`agent friction list`",
		"`agent friction +1 ID`", "    agent friction add \"summary\" --kind help|error|missing|bug|docs|other\n"} {
		if !strings.Contains(brief, want) {
			t.Errorf("briefing lacks %q", want)
		}
	}

	prefixes := s.allowedCommandPrefixes()
	for _, want := range []string{"agent friction add", "/opt/x/agent friction +1", "agent friction list", "agent friction show", "agent history", "agent capture", "/opt/x/agent capture"} {
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
