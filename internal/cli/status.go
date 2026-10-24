package cli

import (
	"fmt"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var statusVerbose bool

var statusCmd = &cobra.Command{
	Use:   "status <name>[@host]",
	Short: "Check the status of an agent",
	Long: `Status reports whether an agent is idle, processing, waiting (blocked on a
human, e.g. a permission prompt), error, exited, or unknown.

Sources, most authoritative first:
  native  the backend CLI's own session status (claude agents --json)
  report  the last status reported by backend hooks or "agent report"
  scrape  regex over terminal output (backends without hooks)

An agent on another host is addressed as name@host (needs a running bus).

Examples:
  agent status researcher
  agent status builder -v   # also show source, last event, and detail
  agent status builder@hub  # an agent on host "hub"`,
	Args: cobra.ExactArgs(1),
	RunE: runStatus,
}

func init() {
	statusCmd.Flags().BoolVarP(&statusVerbose, "verbose", "v", false, "Show source, last event, and detail")
}

func runStatus(cmd *cobra.Command, args []string) error {
	name := args[0]
	ref, remote, err := parseTarget(name)
	if err != nil {
		return err
	}

	var info *service.StatusInfo
	if remote {
		info, err = remoteStatus(ref)
	} else {
		info, err = agentService.StatusInfo(getContext(), ref.Name)
	}
	if err != nil {
		return err
	}

	fmt.Printf("%s: %s\n", name, info.Status)
	if statusVerbose {
		fmt.Printf("  source: %s\n", info.Source)
		if info.Event != "" {
			fmt.Printf("  event:  %s (%s ago)\n", info.Event, time.Since(info.UpdatedAt).Round(time.Second))
		}
		if info.Detail != "" {
			fmt.Printf("  detail: %s\n", info.Detail)
		}
		if info.Tool != "" {
			fmt.Printf("  tool:   %s\n", info.Tool)
		}
	}
	return nil
}

func remoteStatus(ref natsbus.AgentRef) (*service.StatusInfo, error) {
	client, closeFn, err := dialRemote(ref.Host)
	if err != nil {
		return nil, err
	}
	defer closeFn()
	resp, err := client.Status(getContext(), ref)
	if err != nil {
		return nil, err
	}
	return &service.StatusInfo{
		Status:    service.AgentStatus(resp.Status),
		Source:    resp.Source,
		Event:     resp.Event,
		Detail:    resp.Detail,
		Tool:      resp.Tool,
		UpdatedAt: resp.UpdatedAt,
	}, nil
}
