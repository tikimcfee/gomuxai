package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var listCmd = &cobra.Command{
	Use:   "list",
	Short: "List all managed agents",
	Long: `List shows all agents currently being managed.

This displays agents tracked in the database along with their backend
and creation time.`,
	Args: cobra.NoArgs,
	RunE: runList,
}

func runList(cmd *cobra.Command, args []string) error {
	agents, err := agentService.List(getContext())
	if err != nil {
		return err
	}

	if len(agents) == 0 {
		fmt.Println("No agents found")
		return nil
	}

	fmt.Printf("%-20s %-25s %-10s %s\n", "NAME", "SESSION", "BACKEND", "CREATED")
	fmt.Println("------------------------------------------------------------")

	for _, agent := range agents {
		fmt.Printf("%-20s %-25s %-10s %s\n",
			agent.Name,
			agent.SessionName,
			agent.Backend,
			agent.CreatedAt.Format("2006-01-02 15:04:05"),
		)
	}

	return nil
}
