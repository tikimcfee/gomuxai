package cli

import (
	"errors"
	"fmt"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

var stateCmd = &cobra.Command{
	Use:   "state",
	Short: "Read and write agents' key-value state",
	Long: `State is a key-value store per agent, for coordination: an agent writes its
progress or results, others read them.

State is stored in this host's database, and belongs to an agent record:
only agents spawned here have state, and cleanup deletes it. Keys starting
with "_" are written by status reports; e.g. "_result" holds a Claude
agent's latest reply (empty while a turn is running).`,
	Example: `  agent state set helper progress "3/5 files done"
  agent state get helper _result`,
	RunE: runGroup,
}

var stateSetCmd = &cobra.Command{
	Use:   "set <name> <key> <value>",
	Short: "Set a state entry for an agent",
	Long: `Set creates or updates a state entry for an agent spawned on this host.

A coordinator that wasn't spawned with "agent spawn" has no agent record, so
it can't have state: have it pass results through a helper's state or
messages instead.`,
	Example: `  agent state set researcher status working
  agent state set researcher progress "50%"`,
	Args: cobra.ExactArgs(3),
	RunE: runStateSet,
}

var stateGetCmd = &cobra.Command{
	Use:   "get <name> <key>",
	Short: "Print a state value",
	Long: `Get prints a state value. It exits 1 if the key isn't set, printing nothing
on stdout; a key that is set but empty (e.g. "_result" during a turn) prints
an empty line and exits 0.`,
	Example: `  agent state get researcher status
  agent state get researcher _result --json`,
	Args: cobra.ExactArgs(2),
	RunE: runStateGet,
}

var stateListCmd = &cobra.Command{
	Use:   "list <name>",
	Short: "List all state entries for an agent",
	Long: `List shows an agent's state entries, values cut to 30 characters (--json
has them whole).`,
	Example: `  agent state list researcher
  agent state list researcher --json`,
	Args: cobra.ExactArgs(1),
	RunE: runStateList,
}

var stateClearCmd = &cobra.Command{
	Use:   "clear <name> [key]",
	Short: "Remove an agent's state entries, or one key",
	Long: `Clear removes one key, or all of an agent's own keys. Without a key it keeps
the "_" keys status reports write, since removing them would stop its hooks'
reports from being recorded; name one of them to remove it.`,
	Example: `  agent state clear researcher
  agent state clear researcher progress`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runStateClear,
}

var stateJSON bool

func init() {
	for _, c := range []*cobra.Command{stateGetCmd, stateListCmd} {
		c.Flags().BoolVar(&stateJSON, "json", false, "Print as JSON")
	}

	stateCmd.AddCommand(stateSetCmd)
	stateCmd.AddCommand(stateGetCmd)
	stateCmd.AddCommand(stateListCmd)
	stateCmd.AddCommand(stateClearCmd)
}

// stateJSONEntry is a state entry as --json prints it.
type stateJSONEntry struct {
	Agent     string    `json:"agent"`
	Key       string    `json:"key"`
	Value     string    `json:"value"`
	UpdatedAt time.Time `json:"updated_at"`
}

func toStateJSON(e *storage.StateEntry) stateJSONEntry {
	return stateJSONEntry{Agent: e.AgentName, Key: e.Key, Value: e.Value, UpdatedAt: e.UpdatedAt}
}

// stateAgent reads a state command's agent argument, which must name an
// agent on this host: state rows belong to an agent record. State lives in
// each host's own database and isn't served over the bus, so
// "name@<other host>" is refused rather than read as a local agent that has
// no state.
func stateAgent(arg string) (string, error) {
	ref, remote, err := parseTarget(arg)
	if err != nil {
		return "", err
	}
	if remote {
		return "", fmt.Errorf("state is kept per host, and %s's is on %q: read it there, or have the agent send it to you (agent capture %s shows its screen)", ref, ref.Host, ref)
	}
	if _, err := agentService.Agent(getContext(), ref.Name); err != nil {
		return "", fmt.Errorf("state belongs to an agent record: %w", err)
	}
	return ref.Name, nil
}

func runStateSet(cmd *cobra.Command, args []string) error {
	name, err := stateAgent(args[0])
	var nf *service.AgentNotFoundError
	if errors.As(err, &nf) && nf.SessionOf == "" && len(nf.Similar) == 0 {
		return fmt.Errorf("%w; an agent not spawned on this host (e.g. a coordinator started by hand) has no record, so it can't have state: pass results with agent send, or through a helper's state", err)
	}
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

	fmt.Printf("Set %s.%s = %q.\n", name, key, value)
	return nil
}

func runStateGet(cmd *cobra.Command, args []string) error {
	name, err := stateAgent(args[0])
	if err != nil {
		return err
	}
	key := args[1]

	ctx := getContext()
	entry, err := store.GetState(ctx, name, key)
	if err != nil {
		return err
	}

	if entry == nil {
		return missingKey(name, key)
	}

	if stateJSON {
		return printJSON(toStateJSON(entry))
	}
	fmt.Println(entry.Value)
	return nil
}

// missingKey is the error for a state key that isn't set: it names close
// keys, or the keys that are set.
func missingKey(name, key string) error {
	entries, _ := store.ListState(getContext(), name)
	var keys []string
	for _, e := range entries {
		keys = append(keys, e.Key)
	}
	msg := fmt.Sprintf("agent %s has no state %q", name, key)
	if similar := util.Similar(key, keys); len(similar) > 0 {
		return fmt.Errorf("%s; did you mean %s?", msg, util.OrList(similar, nil))
	}
	if len(keys) == 0 {
		return fmt.Errorf("%s (it has no state yet)", msg)
	}
	return fmt.Errorf("%s (keys set: %s)", msg, strings.Join(keys, ", "))
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

	if stateJSON {
		out := make([]stateJSONEntry, 0, len(entries))
		for _, e := range entries {
			out = append(out, toStateJSON(e))
		}
		return printJSON(out)
	}

	if len(entries) == 0 {
		fmt.Printf("No state for agent %s yet (agent state set %s <key> <value> adds some).\n", name, name)
		return nil
	}

	fmt.Printf("%-20s %-30s %s\n", "KEY", "VALUE", "UPDATED")
	fmt.Println("------------------------------------------------------------------------")

	for _, entry := range entries {
		value := strings.Join(strings.Fields(entry.Value), " ")
		if len([]rune(value)) > 30 {
			value = string([]rune(value)[:27]) + "..."
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
	ctx := getContext()

	if len(args) == 2 {
		key := args[1]
		entry, err := store.GetState(ctx, name, key)
		if err != nil {
			return err
		}
		if entry == nil {
			return missingKey(name, key)
		}
		if err := store.DeleteStateKey(ctx, name, key); err != nil {
			return err
		}
		fmt.Printf("Cleared %s.%s.\n", name, key)
		return nil
	}

	entries, err := store.ListState(ctx, name)
	if err != nil {
		return err
	}
	cleared, kept := 0, 0
	for _, e := range entries {
		if strings.HasPrefix(e.Key, "_") {
			kept++
			continue
		}
		if err := store.DeleteStateKey(ctx, name, e.Key); err != nil {
			return err
		}
		cleared++
	}

	fmt.Printf("Cleared %s for agent %s", plural(cleared, "key"), name)
	if kept > 0 {
		fmt.Printf(" (kept %s that status reports write)", plural(kept, "\"_\" key"))
	}
	fmt.Println(".")
	return nil
}
