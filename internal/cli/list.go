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

This displays agents tracked in the database along with their tmux session,
backend, and creation time. --host lists another host's agents through the bus.

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

	fmt.Printf("%-20s %-25s %-10s %s\n", "NAME", "SESSION", "BACKEND", "CREATED")
	fmt.Println("------------------------------------------------------------")
	for _, agent := range agents {
		fmt.Printf("%-20s %-25s %-10s %s\n",
			agent.Name,
			agent.Session,
			agent.Backend,
			agent.CreatedAt.Format("2006-01-02 15:04:05"),
		)
	}
	return nil
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
