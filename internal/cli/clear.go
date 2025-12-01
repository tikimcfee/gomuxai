package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var clearCmd = &cobra.Command{
	Use:   "clear <session>",
	Short: "Clear a tmux session's screen and scrollback",
	Long: `Clear clears both the visible screen and the scrollback buffer.

This is useful for getting a clean slate before capturing output.

Examples:
  agent clear my-session`,
	Args: cobra.ExactArgs(1),
	RunE: runClear,
}

func runClear(cmd *cobra.Command, args []string) error {
	session := args[0]

	if err := tmuxClient.Clear(session); err != nil {
		return err
	}

	fmt.Printf("Cleared %q\n", session)
	return nil
}
