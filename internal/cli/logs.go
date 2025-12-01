package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
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

	if logsSince != "" {
		// Parse duration and get logs since that time
		duration, err := time.ParseDuration(logsSince)
		if err != nil {
			return fmt.Errorf("invalid duration %q: %w", logsSince, err)
		}

		since := time.Now().Add(-duration)
		entries, err := store.GetLogsSince(getContext(), name, since)
		if err != nil {
			return err
		}

		if len(entries) == 0 {
			fmt.Printf("No logs found for agent %q since %s\n", name, logsSince)
			return nil
		}

		for _, entry := range entries {
			fmt.Printf("[%s] %s\n", entry.CapturedAt.Format(time.RFC3339), entry.Content)
		}
		return nil
	}

	// Get recent logs by count
	entries, err := store.GetLogs(getContext(), name, logsLines)
	if err != nil {
		return err
	}

	if len(entries) == 0 {
		fmt.Printf("No logs found for agent %q\n", name)
		return nil
	}

	// Print in chronological order (entries are returned most recent first)
	for i := len(entries) - 1; i >= 0; i-- {
		entry := entries[i]
		fmt.Printf("[%s] %s\n", entry.CapturedAt.Format(time.RFC3339), entry.Content)
	}

	return nil
}
