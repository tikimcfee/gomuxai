package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var cleanupCmd = &cobra.Command{
	Use:   "cleanup <name>",
	Short: "Clean up an agent and its resources",
	Long: `Cleanup removes an agent, kills its tmux session, and deletes associated
messages from the database.

Examples:
  agent cleanup researcher
  agent cleanup --all --as coordinator
  agent cleanup --all --except builder,tester`,
	Args: cobra.MaximumNArgs(1),
	RunE: runCleanup,
}

var (
	cleanupAll    bool
	cleanupAs     string
	cleanupExcept string
)

func init() {
	cleanupCmd.Flags().BoolVar(&cleanupAll, "all", false, "Clean up all agents")
	cleanupCmd.Flags().StringVar(&cleanupAs, "as", "", "Your agent name (required with --all to prevent self-termination)")
	cleanupCmd.Flags().StringVar(&cleanupExcept, "except", "", "Comma-separated list of agents to exclude")
}

func runCleanup(cmd *cobra.Command, args []string) error {
	if cleanupAll {
		return cleanupAllAgents()
	}

	if len(args) == 0 {
		return fmt.Errorf("agent name required (or use --all)")
	}

	name := args[0]
	if err := agentService.Cleanup(getContext(), name); err != nil {
		return err
	}

	fmt.Printf("Cleaned up agent %q\n", name)
	return nil
}

func cleanupAllAgents() error {
	agents, err := agentService.List(getContext())
	if err != nil {
		return err
	}

	// Build exclusion set
	exclude := make(map[string]bool)
	if cleanupAs != "" {
		exclude[cleanupAs] = true
	}
	if cleanupExcept != "" {
		for _, name := range splitComma(cleanupExcept) {
			exclude[name] = true
		}
	}

	// Safety check
	if cleanupAs == "" && len(exclude) == 0 {
		return fmt.Errorf("--all requires --as <your-name> to prevent self-termination")
	}

	cleaned := 0
	for _, agent := range agents {
		if exclude[agent.Name] {
			fmt.Printf("Skipping %q\n", agent.Name)
			continue
		}

		if err := agentService.Cleanup(getContext(), agent.Name); err != nil {
			fmt.Printf("Failed to cleanup %q: %v\n", agent.Name, err)
			continue
		}

		fmt.Printf("Cleaned up %q\n", agent.Name)
		cleaned++
	}

	fmt.Printf("Cleaned up %d agents\n", cleaned)
	return nil
}

func splitComma(s string) []string {
	if s == "" {
		return nil
	}
	var result []string
	start := 0
	for i := 0; i < len(s); i++ {
		if s[i] == ',' {
			if i > start {
				result = append(result, s[start:i])
			}
			start = i + 1
		}
	}
	if start < len(s) {
		result = append(result, s[start:])
	}
	return result
}
