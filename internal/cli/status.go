package cli

import (
	"fmt"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var (
	statusVerbose bool
	statusJSON    bool
)

var statusCmd = &cobra.Command{
	Use:   "status <name>[@host]",
	Short: "Show whether an agent is idle, processing, waiting, ...",
	Long: `Status reports whether an agent is idle, processing, waiting (blocked on a
human, e.g. a permission prompt), error (its last turn failed; it takes input
again), exited, or unknown.

-v adds where the status came from, the last event, its detail (for a
permission prompt, the full tool call: what approve would allow), and the
tool. --json has the same fields.

Sources, most authoritative first:
  native  the backend CLI's own session status (claude agents --json)
  report  the last status reported by backend hooks or "agent report"
  scrape  regex over terminal output (backends without hooks)

An agent on another host is addressed as name@host (needs a running bus).`,
	Example: `  agent status researcher
  agent status builder -v   # also show source, last event, and detail
  agent status builder@hub  # an agent on host "hub"
  agent status builder --json`,
	Args: cobra.ExactArgs(1),
	RunE: runStatus,
}

func init() {
	statusCmd.Flags().BoolVarP(&statusVerbose, "verbose", "v", false, "Show source, last event, and detail")
	statusCmd.Flags().BoolVar(&statusJSON, "json", false, "Print the status and its details as JSON")
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

	if statusJSON {
		host := cfg.Host
		if remote {
			host = ref.Host
		}
		out := statusJSONOut{
			Agent:  ref.Name,
			Host:   host,
			Status: string(info.Status),
			Source: info.Source,
			Event:  info.Event,
			Detail: info.Detail,
			Tool:   info.Tool,
		}
		if !info.UpdatedAt.IsZero() {
			out.UpdatedAt = &info.UpdatedAt
		}
		return printJSON(out)
	}

	fmt.Printf("%s: %s\n", name, info.Status)
	if statusVerbose {
		fmt.Printf("  source: %s\n", info.Source)
		if info.Event != "" {
			fmt.Printf("  event:  %s (%s ago)\n", info.Event, time.Since(info.UpdatedAt).Round(time.Second))
		}
		if info.Detail != "" {
			fmt.Printf("  detail: %s\n", wrapIndented(info.Detail, statusWrapWidth, "          "))
		}
		if info.Tool != "" {
			fmt.Printf("  tool:   %s\n", info.Tool)
		}
	}
	switch {
	case info.Status == service.AgentStatusExited:
		hint("agent logs %s shows how it ended; agent cleanup %s removes it", ref.Name, ref.Name)
	case info.Status == service.AgentStatusUnknown && info.Source == service.StatusSourceScrape:
		hint("its backend doesn't report status and its screen matched no known prompt: agent capture %s shows the screen", name)
	case info.Status == service.AgentStatusWaiting && !statusVerbose:
		hint("agent status %s -v shows what it's waiting on", name)
	}
	return nil
}

// statusJSONOut is a status as --json prints it.
type statusJSONOut struct {
	Agent     string     `json:"agent"`
	Host      string     `json:"host"`
	Status    string     `json:"status"`
	Source    string     `json:"source"`
	Event     string     `json:"event,omitempty"`
	Detail    string     `json:"detail,omitempty"`
	Tool      string     `json:"tool,omitempty"`
	UpdatedAt *time.Time `json:"updated_at,omitempty"`
}

// statusWrapWidth is the width status -v wraps a detail to.
const statusWrapWidth = 100

// wrapIndented wraps s to width columns, keeping its own line breaks, and
// indents every line after the first with indent.
func wrapIndented(s string, width int, indent string) string {
	var lines []string
	for _, para := range strings.Split(strings.TrimRight(s, "\n"), "\n") {
		runes := []rune(para)
		for len(runes) > width {
			cut := width
			for i := width; i > width/2; i-- {
				if runes[i] == ' ' {
					cut = i
					break
				}
			}
			lines = append(lines, strings.TrimRight(string(runes[:cut]), " "))
			runes = []rune(strings.TrimLeft(string(runes[cut:]), " "))
		}
		lines = append(lines, string(runes))
	}
	return strings.Join(lines, "\n"+indent)
}

func remoteStatus(ref natsbus.AgentRef) (*service.StatusInfo, error) {
	var resp *natsbus.StatusResponse
	err := callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
		resp, err = c.Status(getContext(), ref)
		return err
	})
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
