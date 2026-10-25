package cli

import (
	"errors"
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
	Use:   "report <status> [detail]",
	Short: "Record an agent's status by hand (claude/agy agents report automatically)",
	Long: `Report records an agent's status, so others don't have to infer it from
terminal output. Statuses: idle, processing, waiting, error, exited.

Agents spawned with a hook-capable backend (claude, agy) report automatically:
spawn wires the backend's lifecycle hooks to "agent report --hook".

With --hook, the backend's hook payload is read from stdin. Hook mode exits 0
even if the report fails, so a reporting problem can't disturb the agent;
errors go to stderr. (It still exits 1 if the tool can't start, e.g. its
database can't be opened.) It writes to stdout only to hand the agent the
messages held for it (see "agent send"), from a hook that ends its turn
(Claude's Stop).

The agent name defaults to AGENT_NAME.`,
	Example: `  agent report idle
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
		err = fmt.Errorf("which agent? AGENT_NAME isn't set, so name it with --as <name>")
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
		return usageError(cmd, fmt.Errorf("a status is required (idle, processing, waiting, error, or exited) unless --hook"))
	}
	status, err := service.ParseAgentStatus(args[0])
	if err != nil {
		return err
	}
	ctx := getContext()
	if _, err := agentService.Agent(ctx, name); err != nil {
		return fmt.Errorf("status is recorded on an agent record: %w", err)
	}
	r := service.StatusReport{Status: status, Event: "report", SpawnID: reportSpawnID}
	if len(args) > 1 {
		r.Detail = args[1]
	}
	if err := agentService.Report(ctx, name, r); err != nil {
		return err
	}
	line := fmt.Sprintf("Reported %s %s", name, status)
	if r.Detail != "" {
		line += fmt.Sprintf(" (%s)", r.Detail)
	}
	fmt.Println(line + ".")
	return nil
}

func reportFromHook(name string) error {
	payload, err := io.ReadAll(os.Stdin)
	if err != nil {
		return fmt.Errorf("reading hook payload: %w", err)
	}
	out, err := agentService.HandleHook(getContext(), name, reportSpawnID, reportEvent, payload)
	if out != nil {
		// Held messages, claimed: print them even if recording failed after.
		if _, werr := os.Stdout.Write(out); werr != nil {
			err = errors.Join(err, fmt.Errorf("handing over held messages: %w", werr))
		}
	}
	return err
}
