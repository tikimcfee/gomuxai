package cli

import (
	"fmt"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var listCmd = &cobra.Command{
	Use:   "list",
	Short: "List the agents on this host (or another)",
	Long: `List shows the agents recorded on this host as a nested list, with each
agent's status, backend, parent, tmux session, and creation time. --host lists
another host's agents through the bus.

Status is the same live status "agent status" shows (idle, processing,
waiting, error, ended, unknown); "agent status <name> -v" says more, and
"agent statuses" what each means. A host running an older agent sends none,
shown as "-".

Each agent's helpers (spawned with "agent spawn") are nested right under it,
so a team reads as one tree. An agent can clean up only its own helpers and
theirs.`,
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
	EndReason string    `json:"end_reason,omitempty"`
	Parent    string    `json:"parent,omitempty"`
	Session   string    `json:"session"`
	Backend   string    `json:"backend"`
	Adopted   bool      `json:"adopted,omitempty"`
	CreatedAt time.Time `json:"created_at"`
}

func runList(cmd *cobra.Command, args []string) error {
	listHost = canonicalHost(listHost)
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
			out = append(out, agentJSON{Name: a.Name, Host: host, Status: a.Status, EndReason: a.EndReason, Parent: a.SpawnedBy, Session: a.Session, Backend: a.Backend, Adopted: a.Adopted, CreatedAt: a.CreatedAt})
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

	waiting := 0
	for _, item := range teamTree(agents) {
		agent := item.agent
		status := agent.Status
		if status == "" {
			status = "-" // a host that doesn't send status
		}
		if agent.EndReason != "" {
			status += " (" + agent.EndReason + ")"
		}
		if status == string(service.AgentStatusWaiting) {
			waiting++
		}

		headIndent := ""
		metaIndent := "  "
		if item.depth > 0 {
			headIndent = strings.Repeat("  ", item.depth) + "└ "
			metaIndent = strings.Repeat("  ", item.depth+2)
		}

		backendLabel := agent.Backend
		if agent.Adopted {
			backendLabel += ", adopted"
		}
		fmt.Printf("%s%s: %s (%s)\n", headIndent, agent.Name, status, backendLabel)
		if agent.SpawnedBy != "" {
			fmt.Printf("%sparent:  %s\n", metaIndent, agent.SpawnedBy)
		}
		fmt.Printf("%ssession: %s\n", metaIndent, agent.Session)
		fmt.Printf("%screated: %s\n", metaIndent, agent.CreatedAt.Format("2006-01-02 15:04:05"))
	}
	if waiting > 0 {
		hint("%s waiting on a person (a permission prompt or question): agent status <name> -v shows what it's asking", plural(waiting, "agent"))
	}
	return nil
}

type teamNode struct {
	agent natsbus.AgentInfo
	depth int
}

// teamTree orders each agent's helpers right after it, transitively, with
// their nesting depth under their parent.
func teamTree(agents []natsbus.AgentInfo) []teamNode {
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

	out := make([]teamNode, 0, len(agents))
	placed := make(map[string]bool, len(agents))
	var place func(natsbus.AgentInfo, int)
	place = func(a natsbus.AgentInfo, depth int) {
		if placed[a.Name] {
			return
		}
		placed[a.Name] = true
		out = append(out, teamNode{agent: a, depth: depth})
		for _, h := range helpers[a.Name] {
			place(h, depth+1)
		}
	}
	for _, a := range agents {
		if !listed[a.SpawnedBy] {
			place(a, 0)
		}
	}
	// A parent's name can be reused after cleanup, so recorded parents can
	// form a loop with no agent at its top. Place those agents too.
	for _, a := range agents {
		place(a, 0)
	}
	return out
}

// teamOrder lists each agent's helpers right after it, transitively, and
// otherwise keeps the agents' order.
func teamOrder(agents []natsbus.AgentInfo) []natsbus.AgentInfo {
	nodes := teamTree(agents)
	out := make([]natsbus.AgentInfo, len(nodes))
	for i, n := range nodes {
		out[i] = n.agent
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
