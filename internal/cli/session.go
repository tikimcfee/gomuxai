package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

var sessionCmd = &cobra.Command{
	Use:   "session",
	Short: "Create, kill, and list raw tmux sessions",
	Long: `Session manages tmux sessions directly, by session name.

An agent's session is named after it with a prefix ("agent-helper" for agent
"helper"); session list shows which agent each session belongs to. To remove
an agent, use "agent cleanup", which also deletes its record.`,
	Example: `  agent session new scratch
  agent session list`,
	RunE: runGroup,
}

var sessionNewCmd = &cobra.Command{
	Use:   "new <session>",
	Short: "Create a tmux session running a shell",
	Long: `New creates a detached tmux session running a shell, with no agent record:
drive it with "agent keys" and "agent key".`,
	Example: `  agent session new scratch
  agent session new scratch && agent keys scratch "make test"`,
	Args: cobra.ExactArgs(1),
	RunE: runSessionNew,
}

var sessionKillCmd = &cobra.Command{
	Use:   "kill <session>",
	Short: "Kill a tmux session",
	Long: `Kill kills a tmux session. Killing an agent's session leaves its record;
"agent cleanup <name>" removes both.`,
	Example: `  agent session kill scratch`,
	Args:    cobra.ExactArgs(1),
	RunE:    runSessionKill,
}

var sessionListCmd = &cobra.Command{
	Use:     "list",
	Short:   "List all tmux sessions, with the agent each belongs to",
	Example: `  agent session list`,
	Args:    cobra.NoArgs,
	RunE:    runSessionList,
}

var sessionListJSON bool

func init() {
	sessionListCmd.Flags().BoolVar(&sessionListJSON, "json", false, "Print the sessions as JSON")

	sessionCmd.AddCommand(sessionNewCmd)
	sessionCmd.AddCommand(sessionKillCmd)
	sessionCmd.AddCommand(sessionListCmd)
}

func runSessionNew(cmd *cobra.Command, args []string) error {
	name := args[0]
	if exists, _ := tmuxClient.SessionExists(name); exists {
		return fmt.Errorf("tmux session %s already exists (agent session list shows them)", name)
	}

	if err := tmuxClient.CreateSession(name, nil); err != nil {
		return err
	}

	fmt.Printf("Created session %s.\n", name)
	return nil
}

func runSessionKill(cmd *cobra.Command, args []string) error {
	name := args[0]
	if err := checkSession(name); err != nil {
		return err
	}

	if err := tmuxClient.KillSession(name); err != nil {
		return err
	}

	fmt.Printf("Killed session %s.\n", name)
	if a := sessionAgent(name); a != nil {
		hint("it was agent %s's session; its record remains until agent cleanup %s", a.Name, a.Name)
	}
	return nil
}

func runSessionList(cmd *cobra.Command, args []string) error {
	sessions, err := tmuxClient.ListSessions()
	if err != nil {
		return err
	}
	owners := sessionOwners()

	if sessionListJSON {
		type sessionJSON struct {
			Name     string `json:"name"`
			Agent    string `json:"agent,omitempty"`
			Windows  int    `json:"windows"`
			Attached bool   `json:"attached"`
		}
		out := make([]sessionJSON, 0, len(sessions))
		for _, s := range sessions {
			out = append(out, sessionJSON{Name: s.Name, Agent: owners[s.Name], Windows: s.Windows, Attached: s.Attached})
		}
		return printJSON(out)
	}

	if len(sessions) == 0 {
		fmt.Println("No tmux sessions (agent spawn or agent session new creates one).")
		return nil
	}

	fmt.Printf("%-30s %-20s %-10s %s\n", "NAME", "AGENT", "WINDOWS", "ATTACHED")
	fmt.Println("------------------------------------------------------------------------")

	for _, s := range sessions {
		attached := "no"
		if s.Attached {
			attached = "yes"
		}
		owner := owners[s.Name]
		if owner == "" {
			owner = "-"
		}
		fmt.Printf("%-30s %-20s %-10d %s\n", s.Name, owner, s.Windows, attached)
	}

	return nil
}

// sessionOwners maps each agent's session name to the agent's name.
func sessionOwners() map[string]string {
	owners := make(map[string]string)
	agents, _ := agentService.List(getContext())
	for _, a := range agents {
		owners[a.SessionName] = a.Name
	}
	return owners
}

// sessionAgent returns the agent whose session is named session, if any.
func sessionAgent(session string) *storage.Agent {
	agents, _ := agentService.List(getContext())
	for _, a := range agents {
		if a.SessionName == session {
			return a
		}
	}
	return nil
}

// checkSession fails, saying what to use instead, if there is no tmux
// session named name: the session of the agent called name, or close
// session names.
func checkSession(name string) error {
	if exists, err := tmuxClient.SessionExists(name); err != nil || exists {
		return nil
	}
	if a, _ := store.GetAgent(getContext(), name); a != nil {
		return fmt.Errorf("no tmux session %q, but there is an agent %s: this command takes session names, and its session is %s", name, a.Name, a.SessionName)
	}
	var names []string
	if sessions, err := tmuxClient.ListSessions(); err == nil {
		for _, s := range sessions {
			names = append(names, s.Name)
		}
	}
	if similar := util.Similar(name, names); len(similar) > 0 {
		return fmt.Errorf("no tmux session %q; did you mean %s?", name, util.OrList(similar, nil))
	}
	return fmt.Errorf("no tmux session %q (agent session list shows them)", name)
}
