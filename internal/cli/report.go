package cli

import (
	"fmt"
	"io"
	"os"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var (
	reportAs      string
	reportHook    bool
	reportSpawnID string
	reportEvent   string
)

var reportCmd = &cobra.Command{
	Use:   "report [status] [detail]",
	Short: "Report an agent's status",
	Long: `Report records an agent's status, so others don't have to infer it from
terminal output. Statuses: idle, processing, waiting, error, exited.

Agents spawned with a hook-capable backend (claude, agy) report automatically:
spawn wires the backend's lifecycle hooks to "agent report --hook".

With --hook, the backend's hook payload is read from stdin. Hook mode never
writes to stdout and always exits 0, so a reporting problem can't disturb
the agent; errors go to stderr.

The agent name defaults to AGENT_NAME.

Examples:
  agent report idle
  agent report processing "running test suite"
  agent report --hook --as helper < payload.json
  agent report --hook --as helper --event PreInvocation < payload.json`,
	Args: cobra.MaximumNArgs(2),
	RunE: runReport,
}

func init() {
	reportCmd.Flags().StringVar(&reportAs, "as", "", "Agent name (defaults to AGENT_NAME)")
	reportCmd.Flags().BoolVar(&reportHook, "hook", false, "Read a backend hook payload from stdin")
	reportCmd.Flags().StringVar(&reportSpawnID, "spawn-id", "", "Drop the report unless the agent is still this spawn (set by spawn)")
	reportCmd.Flags().StringVar(&reportEvent, "event", "", "Hook event name (for backends whose payload omits it)")
}

func runReport(cmd *cobra.Command, args []string) error {
	name := reportAs
	if name == "" {
		name = os.Getenv("AGENT_NAME")
	}
	var err error
	if name == "" {
		err = fmt.Errorf("agent name required: use --as <name> or set AGENT_NAME")
	}

	if reportHook {
		if err == nil {
			err = reportFromHook(name)
		}
		if err != nil {
			fmt.Fprintf(os.Stderr, "agent report: %v\n", err)
		}
		return nil
	}

	if err != nil {
		return err
	}
	if len(args) == 0 {
		return fmt.Errorf("status required")
	}
	status, err := service.ParseAgentStatus(args[0])
	if err != nil {
		return err
	}
	r := service.StatusReport{Status: status, Event: "report", SpawnID: reportSpawnID}
	if len(args) > 1 {
		r.Detail = args[1]
	}
	return agentService.Report(getContext(), name, r)
}

func reportFromHook(name string) error {
	payload, err := io.ReadAll(os.Stdin)
	if err != nil {
		return fmt.Errorf("reading hook payload: %w", err)
	}
	return agentService.ReportHook(getContext(), name, reportSpawnID, reportEvent, payload)
}
