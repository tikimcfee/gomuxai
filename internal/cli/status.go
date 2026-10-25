package cli

import (
	"encoding/json"
	"fmt"
	"sort"
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
permission prompt, the full tool call: what approve would allow), the tool,
and how many messages are held for the agent until its turn ends (see
"agent send"). --json has the same fields.

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
			Held:   info.Held,
		}
		if json.Valid([]byte(info.ToolInput)) {
			out.ToolInput = json.RawMessage(info.ToolInput)
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
		// At a prompt, the input says in full what an answer allows; the
		// detail then only needs to name it.
		input := ""
		if info.Status == service.AgentStatusWaiting {
			input = formatToolInput(info.ToolInput)
		}
		if info.Detail != "" && input != "" {
			fmt.Printf("  detail: %s\n", service.OneLine(info.Detail))
		} else if info.Detail != "" {
			fmt.Printf("  detail: %s\n", wrapIndented(info.Detail, statusWrapWidth, statusIndent))
		}
		if info.Tool != "" {
			fmt.Printf("  tool:   %s\n", info.Tool)
		}
		if input != "" {
			fmt.Printf("  input:  %s\n", input)
		}
		if info.Held > 0 {
			fmt.Println(heldLine(ref.Name, remote, info))
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

// heldLine is status -v's line about the messages waiting for an agent.
func heldLine(name string, remote bool, info *service.StatusInfo) string {
	n := plural(info.Held, "message")
	var line string
	switch {
	case remote:
		line = "  held:   " + n
	case agentService.HoldsMessages(getContext(), name) && (info.Status == service.AgentStatusIdle || info.Status == service.AgentStatusError):
		// The turn ended without its Stop hook: Esc, a deny, a failure.
		line = "  held:   " + n + ", typed in once it has been at its prompt for 10s (agent deliver " + name + " types it now)"
	case agentService.HoldsMessages(getContext(), name):
		line = "  held:   " + n + ", delivered when its turn ends"
	default:
		line = "  queued: " + n + ", delivered by agent watch when it's idle"
	}
	if info.Status == service.AgentStatusExited {
		line += "; it has exited, so they won't be"
	}
	return line
}

// statusJSONOut is a status as --json prints it.
type statusJSONOut struct {
	Agent  string `json:"agent"`
	Host   string `json:"host"`
	Status string `json:"status"`
	Source string `json:"source"`
	Event  string `json:"event,omitempty"`
	Detail string `json:"detail,omitempty"`
	Tool   string `json:"tool,omitempty"`
	// ToolInput is the full input of the last tool call that may need
	// permission: at a prompt, what approving it allows.
	ToolInput json.RawMessage `json:"tool_input,omitempty"`
	UpdatedAt *time.Time      `json:"updated_at,omitempty"`
	// Held is how many messages wait for the agent: held for the end of
	// its turn, or queued for agent watch.
	Held int `json:"held,omitempty"`
}

// statusWrapWidth is the width status -v wraps a detail to, and
// statusIndent lines up its continuation lines under the first.
const (
	statusWrapWidth = 100
	statusIndent    = "          "
)

// formatToolInput renders a tool call's input (a JSON object) for status
// -v: one "key: value" per field, in order, with string values as they are,
// line breaks included, so a command reads as it would run. It returns ""
// for an empty or malformed input.
func formatToolInput(input string) string {
	var fields map[string]json.RawMessage
	if input == "" || json.Unmarshal([]byte(input), &fields) != nil || len(fields) == 0 {
		return ""
	}
	keys := make([]string, 0, len(fields))
	for k := range fields {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	var lines []string
	for _, k := range keys {
		value := string(fields[k])
		var s string
		if json.Unmarshal(fields[k], &s) == nil {
			value = s
		}
		lines = append(lines, k+": "+wrapIndented(value, statusWrapWidth, statusIndent+"  "))
	}
	return strings.Join(lines, "\n"+statusIndent)
}

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
		ToolInput: resp.ToolInput,
		UpdatedAt: resp.UpdatedAt,
		Held:      resp.Held,
	}, nil
}
