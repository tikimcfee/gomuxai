package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var listCmd = &cobra.Command{
	Use:   "list",
	Short: "List all managed agents",
	Long: `List shows all agents currently being managed.

This displays agents tracked in the database along with their parent, tmux
session, backend, and creation time. --host lists another host's agents
through the bus.

PARENT is the agent that spawned it with "agent spawn", or "-" when a person
did. Each agent's helpers are listed right after it, so a team reads as one
block. An agent can clean up only its own helpers and theirs.

Examples:
  agent list
  agent list --host hub`,
	Args: cobra.NoArgs,
	RunE: runList,
}

var listHost string

func init() {
	listCmd.Flags().StringVar(&listHost, "host", "", "Host whose agents to list (default: this host)")
}

func runList(cmd *cobra.Command, args []string) error {
	agents, err := listAgents(listHost)
	if err != nil {
		return err
	}

	if len(agents) == 0 {
		fmt.Println("No agents found")
		return nil
	}

	fmt.Printf("%-20s %-20s %-25s %-10s %s\n", "NAME", "PARENT", "SESSION", "BACKEND", "CREATED")
	fmt.Println("--------------------------------------------------------------------------------")
	for _, agent := range teamOrder(agents) {
		parent := agent.SpawnedBy
		if parent == "" {
			parent = "-"
		}
		fmt.Printf("%-20s %-20s %-25s %-10s %s\n",
			agent.Name,
			parent,
			agent.Session,
			agent.Backend,
			agent.CreatedAt.Format("2006-01-02 15:04:05"),
		)
	}
	return nil
}

// teamOrder lists each agent's helpers right after it, transitively, and
// otherwise keeps the agents' order.
func teamOrder(agents []natsbus.AgentInfo) []natsbus.AgentInfo {
	listed := make(map[string]bool, len(agents))
	for _, a := range agents {
		listed[a.Name] = true
	}
	helpers := make(map[string][]natsbus.AgentInfo)
	for _, a := range agents {
		if listed[a.SpawnedBy] {
			helpers[a.SpawnedBy] = append(helpers[a.SpawnedBy], a)
		}
	}

	out := make([]natsbus.AgentInfo, 0, len(agents))
	placed := make(map[string]bool, len(agents))
	var place func(natsbus.AgentInfo)
	place = func(a natsbus.AgentInfo) {
		if placed[a.Name] {
			return
		}
		placed[a.Name] = true
		out = append(out, a)
		for _, h := range helpers[a.Name] {
			place(h)
		}
	}
	for _, a := range agents {
		if !listed[a.SpawnedBy] {
			place(a)
		}
	}
	// A parent's name can be reused after cleanup, so recorded parents can
	// form a loop with no agent at its top. Place those agents too.
	for _, a := range agents {
		place(a)
	}
	return out
}

// listAgents returns host's agents, in the form the bus serves them for
// both local and remote hosts, so both print the same.
func listAgents(host string) ([]natsbus.AgentInfo, error) {
	if !isRemoteHost(host) {
		resp, err := service.NewRPCHandler(agentService).List(getContext())
		if err != nil {
			return nil, err
		}
		return resp.Agents, nil
	}
	if err := natsbus.ValidateHost(host); err != nil {
		return nil, err
	}
	var agents []natsbus.AgentInfo
	err := callRemote(host, func(c *natsbus.RPCClient) (err error) {
		agents, err = c.List(getContext(), host)
		return err
	})
	return agents, err
}
