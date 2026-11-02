package cli

import (
	"errors"
	"fmt"
	"os"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var adoptSession string

var adoptCmd = &cobra.Command{
	Use:   "adopt <name>",
	Short: "Create a bus entry for an existing tmux session",
	Long: `Adopt gives an existing tmux session a seat on the agent bus under <name>,
so other agents and hosts can reach it with agent send, status, capture, and
wait without restarting or interrupting what is running in the session.

Run "agent adopt <name>" inside a tmux pane to adopt that tmux session, or
pass --session <tmux-session> to adopt another tmux session on this host.
If Claude Code is running in the session's first pane (or started there
later), its session and status are tracked automatically; otherwise the
session uses the "shell" backend (status is "unknown", and messages are typed
immediately).

Adopting attaches tmux pipe-pane on the session's first pane for "agent logs"
(replacing any existing pipe-pane on that pane). Cleaning up an adopted tmux
session ("agent cleanup <name>") unregisters it from the bus and stops log
piping while leaving the pre-existing tmux session running.`,
	Example: `  agent adopt helper
  agent adopt helper --session dev
  agent adopt helper --session dev --backend agy`,
	Args: cobra.ExactArgs(1),
	RunE: runAdopt,
}

func init() {
	adoptCmd.Flags().StringVar(&adoptSession, "session", "",
		"Existing tmux session name to adopt (defaults to the current tmux session)")
}

func runAdopt(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	name := args[0]

	// Only pass an explicit --backend (or --backend-config) override; ignore any
	// default backend from an app config unless the user asked for one, so
	// adopt auto-detects Claude or defaults to shell.
	explicitBackend := backendName
	if explicitBackend == "" && backendConfig != "" {
		explicitBackend = cfg.Backend
	}

	res, err := agentService.Adopt(ctx, service.AdoptOptions{
		Name:        name,
		SessionName: adoptSession,
		CurrentPane: os.Getenv("TMUX_PANE"),
		Backend:     explicitBackend,
		SpawnedBy:   nonInteractiveCallerAgent(),
	})
	if errors.Is(err, backend.ErrNotFound) {
		return withBackendConfigErr(err)
	}
	if err != nil {
		return err
	}

	fmt.Print(adoptSummary(res))
	return nil
}

func adoptSummary(res *service.AdoptResult) string {
	a := res.Agent
	var b strings.Builder
	fmt.Fprintf(&b, "Adopted agent %s (tmux session %s)\n", a.Name, a.SessionName)
	fmt.Fprintf(&b, "  Backend: %s\n", a.Backend)
	if a.Cwd != "" {
		fmt.Fprintf(&b, "  Cwd:     %s\n", a.Cwd)
	}
	if res.SessionID != "" {
		fmt.Fprintf(&b, "  Session: %s\n", res.SessionID)
	}
	if !quiet {
		fmt.Fprintf(&b, "\nNext: agent status %s -v  |  agent send %s \"...\"  |  agent cleanup %s (leaves %s running)\n",
			a.Name, a.Name, a.Name, a.SessionName)
	}
	return b.String()
}
