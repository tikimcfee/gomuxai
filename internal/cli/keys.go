package cli

import (
	"fmt"
	"strings"

	"github.com/spf13/cobra"
)

var keysCmd = &cobra.Command{
	Use:   "keys <session> <text>",
	Short: "Send text to a tmux session",
	Long: `Keys sends text to a tmux session pane.

By default, Enter is sent after the text. Use --no-enter to skip.

This is a low-level command for agents that need to interact with arbitrary
tmux sessions (not just managed agents).

Examples:
  agent keys my-session "ls -la"
  agent keys my-session "echo hello" --no-enter`,
	Args: cobra.MinimumNArgs(2),
	RunE: runKeys,
}

var keysNoEnter bool

func init() {
	keysCmd.Flags().BoolVar(&keysNoEnter, "no-enter", false, "Don't send Enter after text")
}

func runKeys(cmd *cobra.Command, args []string) error {
	session := args[0]
	text := strings.Join(args[1:], " ")

	if err := tmuxClient.SendKeys(session, text); err != nil {
		return err
	}

	if !keysNoEnter {
		if err := tmuxClient.SendSpecialKey(session, "Enter"); err != nil {
			return err
		}
	}

	fmt.Printf("Sent to %q\n", session)
	return nil
}
