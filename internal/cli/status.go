package cli

import (
	"encoding/json"
	"fmt"
	"sort"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/types"
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
again), ended (no CLI runs its session any more, with an end reason: quit,
gone, replaced, ...), or unknown. "agent statuses" says what each means.

-v adds what the status means, where it came from, the last event, its
detail (for a permission prompt, the full tool call: what approve would
allow), the tool,
and how many messages are held for the agent while it's busy (see
"agent send"). --json has the same fields.

An "unsent:" line, with or without -v, means a message typed into a Claude
agent's input box is still there because Enter didn't submit it: "agent key
agent-<name> Enter" does. An "unchecked:" line means a message was typed in
without that check (its input box couldn't be read) and no turn has started
since: "agent capture <name>" shows whether it sits there.

Sources, most authoritative first:
  native  the backend CLI's own session status (Claude Code's session registry)
  report  the last status reported by backend hooks or "agent report"
  scrape  terminal output: for backends without hooks, and for an agy turn
          that Esc or Ctrl+C ended, which fires no hook ("interrupted")

When a Claude agent's session on record isn't running, status asks what its
pane runs: a CLI a person started anew there becomes the agent's session
(event "adopted"). If it runs none, a session last reported running is
ended (gone).

A Claude or agy agent started in a folder its CLI hasn't trusted waits at
the CLI's workspace trust prompt, which runs no hook: status reads it off
the screen as waiting, tool "trust", detail "trust: <folder>". agent approve
trusts the folder; agent deny declines it, and the CLI exits.

agy shows its permission and ask_question dialogs with no hook of their own,
so a dialog on screen after the tool's PreToolUse makes it waiting, with
"permission: ..." or "question: ..." as the detail.

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

	// A message Enter failed to submit sits in its input box: say so even
	// without -v, since nothing reaches it until someone presses Enter.
	var unsubmittedAt, uncheckedAt time.Time
	var session, unchecked string
	unsubmitted := false
	if !remote {
		unsubmittedAt, session, unsubmitted = agentService.Unsubmitted(getContext(), ref.Name)
		// Likewise one typed without checking, until a turn starts.
		unchecked, uncheckedAt, _ = agentService.Unchecked(getContext(), ref.Name)
	}

	if statusJSON {
		host := cfg.Host
		if remote {
			host = ref.Host
		}
		out := statusJSONOut{
			Agent:     ref.Name,
			Host:      host,
			Status:    string(info.Status),
			EndReason: string(info.EndReason),
			NativeErr: info.NativeErr,
			Source:    info.Source,
			Event:     info.Event,
			Detail:    info.Detail,
			Tool:      info.Tool,
			Held:      info.Held,
			Model:     info.Model,
		}
		if json.Valid([]byte(info.ToolInput)) {
			out.ToolInput = json.RawMessage(info.ToolInput)
		}
		if !info.UpdatedAt.IsZero() {
			out.UpdatedAt = &info.UpdatedAt
		}
		if unsubmitted {
			out.UnsubmittedAt = &unsubmittedAt
		}
		if unchecked != "" {
			out.Unchecked, out.UncheckedAt = unchecked, &uncheckedAt
		}
		return printJSON(out)
	}

	fmt.Printf("%s: %s\n", name, statusWords(info))
	if statusVerbose {
		fmt.Printf("  means:  %s\n", wrapIndented(statusMeaning(info), statusWrapWidth, statusIndent))
		fmt.Printf("  source: %s\n", info.Source)
		if info.Model != "" {
			fmt.Printf("  model:  %s\n", info.Model)
		}
		if info.NativeErr != "" {
			fmt.Printf("  native: unread: %s\n", wrapIndented(info.NativeErr, statusWrapWidth, statusIndent))
		}
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
	if unsubmitted {
		fmt.Printf("  unsent: a message typed into its input box at %s wasn't submitted; it sits there: agent key %s Enter submits it\n",
			unsubmittedAt.Format("15:04:05"), session)
	}
	if unchecked != "" {
		fmt.Printf("  unchecked: a message typed in at %s wasn't checked for submission (%s), and no turn has started since: agent capture %s shows whether it sits in its input box\n",
			uncheckedAt.Format("15:04:05"), unchecked, name)
	}
	switch {
	case info.Status == service.AgentStatusEnded:
		hint("agent logs %s shows how it ended; agent cleanup %s removes it; agent statuses says what each status means", ref.Name, ref.Name)
	case info.Status == service.AgentStatusUnknown && info.Source == service.StatusSourceScrape:
		hint("its backend doesn't report status and its screen matched no known prompt: agent capture %s shows the screen", name)
	case info.IsTrustPrompt():
		hint("agent approve %s trusts the folder; agent deny %s declines it, and its CLI exits", name, name)
	case info.Status == service.AgentStatusWaiting && !statusVerbose:
		hint("agent status %s -v shows what it's waiting on", name)
	}
	return nil
}

// statusWords is a status as a line shows it: "ended (quit)" for an end.
func statusWords(info *service.StatusInfo) string {
	if info.Status == service.AgentStatusEnded && info.EndReason != "" {
		return fmt.Sprintf("%s (%s)", info.Status, info.EndReason)
	}
	return string(info.Status)
}

// statusMeaning says what a status (and its end reason) means.
func statusMeaning(info *service.StatusInfo) string {
	means := types.MeaningOf(types.Statuses, string(info.Status))
	if info.Status == service.AgentStatusEnded && info.EndReason != "" {
		if r := types.MeaningOf(types.EndReasons, string(info.EndReason)); r != "" {
			means = string(info.EndReason) + ": " + r
		}
	}
	return means
}

// heldLine is status -v's line about the messages waiting for an agent.
func heldLine(name string, remote bool, info *service.StatusInfo) string {
	n := plural(info.Held, "message")
	var line string
	switch {
	case remote:
		line = "  held:   " + n
	case info.IsTrustPrompt():
		line = "  held:   " + n + ", typed in once the folder is trusted (agent approve " + name + ") and it is at its prompt"
	case agentService.HoldsMessages(getContext(), name) && (info.Status == service.AgentStatusIdle || info.Status == service.AgentStatusError):
		// The turn ended without its Stop hook: Esc, a deny, a failure.
		if why := agentService.InputBlocked(getContext(), name); why != "" {
			line = "  held:   " + n + ", waiting for an empty input line (" + why + ")"
			break
		}
		line = "  held:   " + n + ", typed in once it has been at its prompt for 10s (agent deliver " + name + " types it now)"
	case agentService.HoldsMessages(getContext(), name):
		line = "  held:   " + n + ", delivered when " + agentService.DeliveryPoint(getContext(), name)
	default:
		line = "  queued: " + n + ", delivered by agent watch when it's idle"
	}
	if info.Status == service.AgentStatusEnded {
		line += "; its session has ended, so they won't be"
	}
	return line
}

// statusJSONOut is a status as --json prints it.
type statusJSONOut struct {
	Agent  string `json:"agent"`
	Host   string `json:"host"`
	Status string `json:"status"`
	// EndReason says how the session ended, with a status of ended.
	EndReason string `json:"end_reason,omitempty"`
	Source    string `json:"source"`
	// NativeErr says why the backend CLI's own status couldn't be read.
	NativeErr string `json:"native_error,omitempty"`
	Event     string `json:"event,omitempty"`
	Detail    string `json:"detail,omitempty"`
	Tool      string `json:"tool,omitempty"`
	// ToolInput is the full input of the last tool call that may need
	// permission: at a prompt, what approving it allows.
	ToolInput json.RawMessage `json:"tool_input,omitempty"`
	UpdatedAt *time.Time      `json:"updated_at,omitempty"`
	// Held is how many messages wait for the agent: held for the end of
	// its turn, or queued for agent watch.
	Held int `json:"held,omitempty"`
	// Model is the model spawn --model started it on; absent for the
	// CLI's default.
	Model string `json:"model,omitempty"`
	// UnsubmittedAt is when a message was typed into the agent's input box
	// that Enter failed to submit, if it still sits there.
	UnsubmittedAt *time.Time `json:"unsubmitted_at,omitempty"`
	// Unchecked is why a message typed in at UncheckedAt wasn't checked
	// for submission, while no turn has started since.
	Unchecked   string     `json:"unchecked,omitempty"`
	UncheckedAt *time.Time `json:"unchecked_at,omitempty"`
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
		EndReason: backend.EndReason(resp.EndReason),
		Source:    resp.Source,
		Event:     resp.Event,
		Detail:    resp.Detail,
		Tool:      resp.Tool,
		ToolInput: resp.ToolInput,
		UpdatedAt: resp.UpdatedAt,
		Held:      resp.Held,
		Model:     resp.Model,
	}, nil
}
