package cli

import (
	"context"
	"strings"
	"testing"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// noBus makes the bus look not running, as on a host without one.
func noBus(t *testing.T) {
	busConnect = func() (*nats.Conn, error) { return nil, natsbus.ErrNoBus }
}

func TestOrient(t *testing.T) {
	setupCLI(t)
	noBus(t)
	addTeam(t, map[string]string{"lead": "", "lead-a": "lead"})
	ctx := context.Background()
	if err := agentService.Report(ctx, "lead-a", service.StatusReport{Status: service.AgentStatusProcessing}); err != nil {
		t.Fatal(err)
	}
	if err := agentService.Queue(ctx, "user", "lead", "hi"); err != nil {
		t.Fatal(err)
	}

	var person strings.Builder
	if err := orient(&person, ""); err != nil {
		t.Fatal(err)
	}
	want := `You are a person at a terminal, on host local.
Agents here (3): lead unknown, lead-a processing, worker waiting
Bus: not set up (only needed to reach other hosts: agent bus --help)
`
	if !strings.HasPrefix(person.String(), want) {
		t.Errorf("person's orientation:\n%s\nwant it to start:\n%s", person.String(), want)
	}

	var agent strings.Builder
	if err := orient(&agent, "lead-a"); err != nil {
		t.Fatal(err)
	}
	for _, line := range []string{
		"You are agent lead-a on host local, a helper of lead.\n",
		"Your helpers: none (3 agents here: agent list).\n",
	} {
		if !strings.Contains(agent.String(), line) {
			t.Errorf("agent's orientation lacks %q:\n%s", line, agent.String())
		}
	}

	var lead strings.Builder
	if err := orient(&lead, "lead"); err != nil {
		t.Fatal(err)
	}
	for _, line := range []string{
		"Your helpers (1 of 3 agents here): lead-a processing\n",
		"Queued for you: 1 message, delivered when you're idle, but no watcher is running to deliver (agent watch).\n",
	} {
		if !strings.Contains(lead.String(), line) {
			t.Errorf("lead's orientation lacks %q:\n%s", line, lead.String())
		}
	}
	if n := strings.Count(person.String(), "\n"); n > 10 {
		t.Errorf("orientation is %d lines, want at most 10", n)
	}
}

// "agent" alone orients; "agent --help" still gives the full help.
func TestRootHelpStillFull(t *testing.T) {
	if rootCmd.RunE == nil {
		t.Fatal("agent alone has no RunE")
	}
	var out strings.Builder
	rootCmd.SetOut(&out)
	t.Cleanup(func() { rootCmd.SetOut(nil) })
	if err := rootCmd.Help(); err != nil {
		t.Fatal(err)
	}
	for _, want := range []string{"Agents:", "Low-level tmux", "spawn", "-q, --quiet"} {
		if !strings.Contains(out.String(), want) {
			t.Errorf("help lacks %q", want)
		}
	}
}
