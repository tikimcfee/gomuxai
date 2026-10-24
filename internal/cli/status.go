package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
)

var statusVerbose bool

var statusCmd = &cobra.Command{
	Use:   "status <name>",
	Short: "Check the status of an agent",
	Long: `Status reports whether an agent is idle, processing, waiting (blocked on a
human, e.g. a permission prompt), error, exited, or unknown.

Sources, most authoritative first:
  native  the backend CLI's own session status (claude agents --json)
  report  the last status reported by backend hooks or "agent report"
  scrape  regex over terminal output (backends without hooks)

Examples:
  agent status researcher
  agent status builder -v   # also show source, last event, and detail`,
	Args: cobra.ExactArgs(1),
	RunE: runStatus,
}

func init() {
	statusCmd.Flags().BoolVarP(&statusVerbose, "verbose", "v", false, "Show source, last event, and detail")
}

func runStatus(cmd *cobra.Command, args []string) error {
	name := args[0]

	info, err := agentService.StatusInfo(getContext(), name)
	if err != nil {
		return err
	}

	fmt.Printf("%s: %s\n", name, info.Status)
	if statusVerbose {
		fmt.Printf("  source: %s\n", info.Source)
		if info.Event != "" {
			fmt.Printf("  event:  %s (%s ago)\n", info.Event, time.Since(info.UpdatedAt).Round(time.Second))
		}
		if info.Detail != "" {
			fmt.Printf("  detail: %s\n", info.Detail)
		}
	}
	return nil
}
