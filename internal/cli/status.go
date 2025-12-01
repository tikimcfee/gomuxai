package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var statusCmd = &cobra.Command{
	Use:   "status <name>",
	Short: "Check the status of an agent",
	Long: `Status checks whether an agent is idle, processing, or in an unknown state.

The status is determined by matching patterns in the terminal output against
the backend's idle and processing patterns.

Examples:
  agent status researcher
  agent status builder`,
	Args: cobra.ExactArgs(1),
	RunE: runStatus,
}

func runStatus(cmd *cobra.Command, args []string) error {
	name := args[0]

	status, err := agentService.Status(getContext(), name)
	if err != nil {
		return err
	}

	fmt.Printf("%s: %s\n", name, status)
	return nil
}
