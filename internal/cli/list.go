package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

var listCmd = &cobra.Command{
	Use:   "list",
	Short: "List all managed agents",
	Long: `List shows all agents currently being managed.

This displays agents tracked in the database along with their backend
and creation time. --host lists another host's agents through the bus.

Examples:
  agent list
  agent list --host hub`,
	Args: cobra.NoArgs,
	RunE: runList,
}

var listHost string

func init() {
	listCmd.Flags().StringVar(&listHost, "host", "", "List the agents on this host (default: this machine)")
}

func runList(cmd *cobra.Command, args []string) error {
	if isRemoteHost(listHost) {
		return runRemoteList(listHost)
	}

	agents, err := agentService.List(getContext())
	if err != nil {
		return err
	}

	if len(agents) == 0 {
		fmt.Println("No agents found")
		return nil
	}

	printAgentHeader()
	for _, agent := range agents {
		printAgentRow(agent.Name, agent.SessionName, agent.Backend, agent.CreatedAt)
	}

	return nil
}

func runRemoteList(host string) error {
	if err := natsbus.ValidateHost(host); err != nil {
		return err
	}
	client, closeFn, err := dialRemote(host)
	if err != nil {
		return err
	}
	defer closeFn()
	agents, err := client.List(getContext(), host)
	if err != nil {
		return err
	}

	if len(agents) == 0 {
		fmt.Println("No agents found")
		return nil
	}

	printAgentHeader()
	for _, agent := range agents {
		printAgentRow(agent.Name, agent.Session, agent.Backend, agent.CreatedAt)
	}
	return nil
}

func printAgentHeader() {
	fmt.Printf("%-20s %-25s %-10s %s\n", "NAME", "SESSION", "BACKEND", "CREATED")
	fmt.Println("------------------------------------------------------------")
}

func printAgentRow(name, session, backend string, created time.Time) {
	fmt.Printf("%-20s %-25s %-10s %s\n",
		name,
		session,
		backend,
		created.Format("2006-01-02 15:04:05"),
	)
}
