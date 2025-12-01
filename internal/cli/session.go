package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var sessionCmd = &cobra.Command{
	Use:   "session",
	Short: "Low-level tmux session operations",
	Long: `Session provides direct tmux session management.

These commands allow agents to create and manage tmux sessions directly
without needing to know raw tmux commands.`,
}

var sessionNewCmd = &cobra.Command{
	Use:   "new <name>",
	Short: "Create a new tmux session",
	Args:  cobra.ExactArgs(1),
	RunE:  runSessionNew,
}

var sessionKillCmd = &cobra.Command{
	Use:   "kill <name>",
	Short: "Kill a tmux session",
	Args:  cobra.ExactArgs(1),
	RunE:  runSessionKill,
}

var sessionListCmd = &cobra.Command{
	Use:   "list",
	Short: "List all tmux sessions",
	Args:  cobra.NoArgs,
	RunE:  runSessionList,
}

func init() {
	sessionCmd.AddCommand(sessionNewCmd)
	sessionCmd.AddCommand(sessionKillCmd)
	sessionCmd.AddCommand(sessionListCmd)
}

func runSessionNew(cmd *cobra.Command, args []string) error {
	name := args[0]

	if err := tmuxClient.CreateSession(name, nil); err != nil {
		return err
	}

	fmt.Printf("Created session %q\n", name)
	return nil
}

func runSessionKill(cmd *cobra.Command, args []string) error {
	name := args[0]

	if err := tmuxClient.KillSession(name); err != nil {
		return err
	}

	fmt.Printf("Killed session %q\n", name)
	return nil
}

func runSessionList(cmd *cobra.Command, args []string) error {
	sessions, err := tmuxClient.ListSessions()
	if err != nil {
		return err
	}

	if len(sessions) == 0 {
		fmt.Println("No sessions found")
		return nil
	}

	fmt.Printf("%-30s %-10s %s\n", "NAME", "WINDOWS", "ATTACHED")
	fmt.Println("------------------------------------------------")

	for _, s := range sessions {
		attached := "no"
		if s.Attached {
			attached = "yes"
		}
		fmt.Printf("%-30s %-10d %s\n", s.Name, s.Windows, attached)
	}

	return nil
}
