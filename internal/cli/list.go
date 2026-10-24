package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var listCmd = &cobra.Command{
	Use:   "list",
	Short: "List the agents on this host (or another)",
	Long: `List shows the agents recorded on this host, with their status, parent,
tmux session, backend, and creation time. --host lists another host's agents
through the bus.

STATUS is the same live status "agent status" shows (idle, processing,
waiting, error, exited, unknown); "agent status <name> -v" says more. A host
running an older agent sends none, shown as "-".

PARENT is the agent that spawned it with "agent spawn", or "-" when a person
did. Each agent's helpers are listed right after it, so a team reads as one
block. An agent can clean up only its own helpers and theirs.`,
	Example: `  agent list
  agent list --host hub
  agent list --json`,
	Args: cobra.NoArgs,
	RunE: runList,
}

var (
	listHost string
	listJSON bool
)

func init() {
	listCmd.Flags().StringVar(&listHost, "host", "", "Host whose agents to list (default: this host)")
	listCmd.Flags().BoolVar(&listJSON, "json", false, "Print the agents as JSON")
}

// agentJSON is an agent as list --json prints it.
type agentJSON struct {
	Name      string    `json:"name"`
	Host      string    `json:"host"`
	Status    string    `json:"status,omitempty"`
	Parent    string    `json:"parent,omitempty"`
	Session   string    `json:"session"`
	Backend   string    `json:"backend"`
	CreatedAt time.Time `json:"created_at"`
}

func runList(cmd *cobra.Command, args []string) error {
	agents, err := listAgents(listHost)
	if err != nil {
		return err
	}

	if listJSON {
		host := cfg.Host
		if isRemoteHost(listHost) {
			host = listHost
		}
		out := make([]agentJSON, 0, len(agents))
		for _, a := range teamOrder(agents) {
			out = append(out, agentJSON{Name: a.Name, Host: host, Status: a.Status, Parent: a.SpawnedBy, Session: a.Session, Backend: a.Backend, CreatedAt: a.CreatedAt})
		}
		return printJSON(out)
	}

	if len(agents) == 0 {
		if isRemoteHost(listHost) {
			fmt.Printf("No agents on host %s.\n", listHost)
		} else {
			fmt.Printf("No agents on host %s (agent spawn <name> --backend claude starts one).\n", cfg.Host)
		}
		return nil
	}

	fmt.Printf("%-20s %-11s %-20s %-25s %-10s %s\n", "NAME", "STATUS", "PARENT", "SESSION", "BACKEND", "CREATED")
	fmt.Println("---------------------------------------------------------------------------------------------")
	waiting := 0
	for _, agent := range teamOrder(agents) {
		parent := agent.SpawnedBy
		if parent == "" {
			parent = "-"
		}
		status := agent.Status
		if status == "" {
			status = "-" // a host that doesn't send status
		}
		if status == string(service.AgentStatusWaiting) {
			waiting++
		}
		fmt.Printf("%-20s %-11s %-20s %-25s %-10s %s\n",
			agent.Name,
			status,
			parent,
			agent.Session,
			agent.Backend,
			agent.CreatedAt.Format("2006-01-02 15:04:05"),
		)
	}
	if waiting > 0 {
		hint("%s waiting on a person (a permission prompt or question): agent status <name> -v shows what it's asking", plural(waiting, "agent"))
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
