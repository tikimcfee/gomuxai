package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var keyCmd = &cobra.Command{
	Use:   "key <session> <key>",
	Short: "Press a special key (Enter, Escape, c-c, ...) in a tmux session",
	Long: `Key presses a special key or key combination in a tmux session, by session
name (agent "helper"'s session is "agent-helper").

Supported keys include:
  Enter, Escape, Tab, Space
  c-c (Ctrl+C), c-d (Ctrl+D), c-z (Ctrl+Z), etc.
  Up, Down, Left, Right
  Home, End, PgUp, PgDn
  F1-F12

tmux types a name it doesn't know as text.`,
	Example: `  agent key my-session Enter
  agent key agent-helper c-c
  agent key my-session Escape`,
	Args: cobra.ExactArgs(2),
	RunE: runKey,
}

func runKey(cmd *cobra.Command, args []string) error {
	session := args[0]
	key := args[1]
	if err := checkSession(session); err != nil {
		return err
	}

	if err := tmuxClient.SendSpecialKey(session, key); err != nil {
		return err
	}

	fmt.Printf("Pressed %s in session %s.\n", key, session)
	return nil
}
