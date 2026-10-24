package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
)

var (
	approveAlways   bool
	permissionForce bool
)

var approveCmd = &cobra.Command{
	Use:   "approve <name>",
	Short: "Approve an agent's pending permission prompt",
	Long: `Approve answers "yes" to the permission prompt an agent is waiting on.

Check what it's asking first: "agent status <name> -v" shows the pending
tool call. Refuses unless the agent's status is "waiting" (override with --force).

Examples:
  agent approve helper
  agent approve helper --always   # Claude's "yes, don't ask again" option`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		resp := backend.PermissionApprove
		if approveAlways {
			resp = backend.PermissionApproveAlways
		}
		return respondPermission(args[0], resp, "approved")
	},
}

var denyCmd = &cobra.Command{
	Use:   "deny <name>",
	Short: "Deny an agent's pending permission prompt",
	Long: `Deny rejects the permission prompt an agent is waiting on. For Claude this
cancels the current turn; follow up with "agent send" to redirect it.

Refuses unless the agent's status is "waiting" (override with --force).

Examples:
  agent deny helper`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		return respondPermission(args[0], backend.PermissionDeny, "denied")
	},
}

func init() {
	approveCmd.Flags().BoolVar(&approveAlways, "always", false, "Approve and stop asking for this kind of action")
	approveCmd.Flags().BoolVar(&permissionForce, "force", false, "Send even if the agent isn't reported as waiting")
	denyCmd.Flags().BoolVar(&permissionForce, "force", false, "Send even if the agent isn't reported as waiting")
}

func respondPermission(name string, resp backend.PermissionResponse, verb string) error {
	if err := agentService.RespondPermission(getContext(), name, resp, permissionForce); err != nil {
		return err
	}
	fmt.Printf("%s: %s\n", name, verb)
	return nil
}
