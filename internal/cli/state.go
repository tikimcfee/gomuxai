package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

var stateCmd = &cobra.Command{
	Use:   "state",
	Short: "Manage agent state",
	Long: `State provides operations for managing agent key-value state.

State is stored in the database as key-value pairs per agent.
This replaces filesystem-based state directories.`,
}

var stateSetCmd = &cobra.Command{
	Use:   "set <name> <key> <value>",
	Short: "Set a state entry for an agent",
	Long: `Set creates or updates a state entry for an agent.

Examples:
  agent state set researcher status working
  agent state set researcher progress "50%"`,
	Args: cobra.ExactArgs(3),
	RunE: runStateSet,
}

var stateGetCmd = &cobra.Command{
	Use:   "get <name> <key>",
	Short: "Get a state entry for an agent",
	Long: `Get retrieves a state value for an agent.

Examples:
  agent state get researcher status`,
	Args: cobra.ExactArgs(2),
	RunE: runStateGet,
}

var stateListCmd = &cobra.Command{
	Use:   "list <name>",
	Short: "List all state entries for an agent",
	Long: `List shows all key-value state entries for an agent.

Examples:
  agent state list researcher`,
	Args: cobra.ExactArgs(1),
	RunE: runStateList,
}

var stateClearCmd = &cobra.Command{
	Use:   "clear <name>",
	Short: "Clear all state for an agent",
	Long: `Clear removes all state entries for an agent.

Examples:
  agent state clear researcher`,
	Args: cobra.ExactArgs(1),
	RunE: runStateClear,
}

func init() {
	stateCmd.AddCommand(stateSetCmd)
	stateCmd.AddCommand(stateGetCmd)
	stateCmd.AddCommand(stateListCmd)
	stateCmd.AddCommand(stateClearCmd)
}

// stateAgent reads a state command's agent argument. State lives in each
// host's own database and isn't served over the bus, so "name@<other host>"
// is refused rather than read as a local agent that has no state.
func stateAgent(arg string) (string, error) {
	ref, remote, err := parseTarget(arg)
	if err != nil {
		return "", err
	}
	if remote {
		return "", fmt.Errorf("state is kept per host, and %s's is on %q: read it there, or have the agent send it to you (agent capture %s shows its screen)", ref, ref.Host, ref)
	}
	return ref.Name, nil
}

func runStateSet(cmd *cobra.Command, args []string) error {
	name, err := stateAgent(args[0])
	if err != nil {
		return err
	}
	key := args[1]
	value := args[2]

	entry := &storage.StateEntry{
		AgentName: name,
		Key:       key,
		Value:     value,
		UpdatedAt: time.Now(),
	}

	if err := store.SetState(getContext(), entry); err != nil {
		return err
	}

	fmt.Printf("State set: %s.%s = %s\n", name, key, value)
	return nil
}

func runStateGet(cmd *cobra.Command, args []string) error {
	name, err := stateAgent(args[0])
	if err != nil {
		return err
	}
	key := args[1]

	entry, err := store.GetState(getContext(), name, key)
	if err != nil {
		return err
	}

	if entry == nil {
		fmt.Printf("No state found for %s.%s\n", name, key)
		return nil
	}

	fmt.Println(entry.Value)
	return nil
}

func runStateList(cmd *cobra.Command, args []string) error {
	name, err := stateAgent(args[0])
	if err != nil {
		return err
	}

	entries, err := store.ListState(getContext(), name)
	if err != nil {
		return err
	}

	if len(entries) == 0 {
		fmt.Printf("No state found for agent %q\n", name)
		return nil
	}

	fmt.Printf("%-20s %-30s %s\n", "KEY", "VALUE", "UPDATED")
	fmt.Println("------------------------------------------------------------------------")

	for _, entry := range entries {
		value := entry.Value
		if len(value) > 30 {
			value = value[:27] + "..."
		}
		fmt.Printf("%-20s %-30s %s\n", entry.Key, value, entry.UpdatedAt.Format(time.RFC3339))
	}

	return nil
}

func runStateClear(cmd *cobra.Command, args []string) error {
	name, err := stateAgent(args[0])
	if err != nil {
		return err
	}

	if err := store.DeleteState(getContext(), name); err != nil {
		return err
	}

	fmt.Printf("State cleared for agent %q\n", name)
	return nil
}
