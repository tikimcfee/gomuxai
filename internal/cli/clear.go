package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var clearCmd = &cobra.Command{
	Use:   "clear <session>",
	Short: "Clear a tmux session's screen and scrollback",
	Long: `Clear empties a tmux session's pane and drops tmux's scrollback, for a clean
slate before capturing output. It takes a session name (agent "helper"'s
session is "agent-helper").

tmux resets the pane itself; nothing is typed, so the program in the pane
isn't told. The pane is empty when the command returns: a shell's prompt
comes back with its next output (press Enter for one now), and a full-screen
program, such as an agent's CLI, stays blank until it next redraws.`,
	Example: `  agent clear my-session`,
	Args:    cobra.ExactArgs(1),
	RunE:    runClear,
}

func runClear(cmd *cobra.Command, args []string) error {
	session := args[0]
	if err := checkSession(session); err != nil {
		return err
	}

	if err := tmuxClient.Clear(session); err != nil {
		return err
	}

	fmt.Printf("Cleared session %s (screen and scrollback).\n", session)
	return nil
}
