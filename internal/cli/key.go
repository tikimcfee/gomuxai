package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var keyCmd = &cobra.Command{
	Use:   "key <session> <key>",
	Short: "Send a special key to a tmux session",
	Long: `Key sends a special key or key combination to a tmux session.

Supported keys include:
  Enter, Escape, Tab, Space
  c-c (Ctrl+C), c-d (Ctrl+D), c-z (Ctrl+Z), etc.
  Up, Down, Left, Right
  Home, End, PgUp, PgDn
  F1-F12

Examples:
  agent key my-session Enter
  agent key my-session c-c
  agent key my-session Escape`,
	Args: cobra.ExactArgs(2),
	RunE: runKey,
}

func runKey(cmd *cobra.Command, args []string) error {
	session := args[0]
	key := args[1]

	if err := tmuxClient.SendSpecialKey(session, key); err != nil {
		return err
	}

	fmt.Printf("Sent %q to %q\n", key, session)
	return nil
}
