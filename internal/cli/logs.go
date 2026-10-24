package cli

import (
	"fmt"
	"slices"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var logsCmd = &cobra.Command{
	Use:   "logs <name>",
	Short: "View agent logs from database",
	Long: `Logs retrieves captured log entries for an agent from the database.

Log entries are stored ANSI-stripped and can be queried by count or time.

Examples:
  agent logs researcher
  agent logs researcher --lines 100
  agent logs researcher --since 5m`,
	Args: cobra.ExactArgs(1),
	RunE: runLogs,
}

var (
	logsLines int
	logsSince string
)

func init() {
	logsCmd.Flags().IntVar(&logsLines, "lines", 50, "Number of log entries to retrieve")
	logsCmd.Flags().StringVar(&logsSince, "since", "", "Show logs since duration ago (e.g., 5m, 1h)")
}

func runLogs(cmd *cobra.Command, args []string) error {
	name := args[0]

	var entries []*storage.LogEntry
	if logsSince != "" {
		duration, err := time.ParseDuration(logsSince)
		if err != nil {
			return fmt.Errorf("invalid duration %q: %w", logsSince, err)
		}
		entries, err = store.GetLogsSince(getContext(), name, time.Now().Add(-duration))
		if err != nil {
			return err
		}
		if len(entries) == 0 {
			fmt.Printf("No logs found for agent %q since %s\n", name, logsSince)
			return nil
		}
	} else {
		var err error
		entries, err = store.GetLogs(getContext(), name, logsLines)
		if err != nil {
			return err
		}
		if len(entries) == 0 {
			fmt.Printf("No logs found for agent %q\n", name)
			return nil
		}
		// GetLogs returns the most recent first
		slices.Reverse(entries)
	}

	for _, entry := range entries {
		fmt.Printf("[%s] %s\n", entry.CapturedAt.Format(time.RFC3339), entry.Content)
	}
	return nil
}
