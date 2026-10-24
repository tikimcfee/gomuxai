package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var clearCmd = &cobra.Command{
	Use:   "clear <session>",
	Short: "Clear a tmux session's screen and scrollback",
	Long: `Clear types "clear" into a tmux session's shell and drops tmux's scrollback,
for a clean slate before capturing output. It takes a session name (agent
"helper"'s session is "agent-helper"), and is meant for raw shell sessions:
in an agent's CLI, "clear" would be typed as a message.`,
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
