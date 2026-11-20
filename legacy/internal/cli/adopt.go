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
	Short: "Create a bus entry for an existing g3d-termd session",
	Long: `Adopt gives an existing g3d-termd session a seat on the agent bus under <name>,
so other agents and hosts can reach it with agent send, status, capture, and
wait without restarting or interrupting what is running in the session.

Run "agent adopt <name>" inside a g3d terminal in the target namespace
(default "agents", or --termd-ns <ns>; "default" and "wall" are refused), or
pass --session <session> to adopt another terminal in that namespace.
If Claude Code is running in the session (or started there later), its session
and status are tracked automatically; otherwise the session uses the "shell"
backend (status is "unknown", and messages are typed immediately).

Cleaning up an adopted session ("agent cleanup <name>") unregisters it from the
bus while leaving the pre-existing session running.

gomuxai never launches anything in an adopted session: agent resume refuses an
adopted agent. If its CLI exits, start it again there yourself (claude -c
continues its last conversation); adopt keeps tracking it.`,
	Example: `  agent adopt helper
  agent adopt helper --session dev
  agent adopt helper --session dev --backend agy`,
	Args: cobra.ExactArgs(1),
	RunE: runAdopt,
}

func init() {
	adoptCmd.Flags().StringVar(&adoptSession, "session", "",
		"Existing g3d-termd session id to adopt (defaults to $G3D_TERMD_ID)")
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
		CurrentPane: os.Getenv("G3D_TERMD_ID"),
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
	fmt.Fprintf(&b, "Adopted agent %s (session %s)\n", a.Name, a.SessionName)
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
