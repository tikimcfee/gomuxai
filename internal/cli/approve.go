package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var (
	approveAlways    bool
	permissionForce  bool
	permissionExpect string
)

var approveCmd = &cobra.Command{
	Use:   "approve <name>[@host]",
	Short: "Answer yes to an agent's permission prompt",
	Long: `Approve answers "yes" to the permission prompt an agent is waiting on, and
says which prompt it answered.

Check what it's asking first: "agent status <name> -v" shows the pending
tool call in full. Refuses unless the agent's status is "waiting", and refuses
dialogs that are questions for the user rather than permission prompts (e.g.
Claude's AskUserQuestion: answer those with send/keys). --force overrides
both.

Use --expect with the tool you inspected, so a prompt that replaced it in the
meantime isn't approved blind. An agent on another host is addressed as
name@host (needs a running bus); its host runs the same checks.`,
	Example: `  agent status helper -v && agent approve helper --expect Bash
  agent approve helper --always   # Claude's "yes, don't ask again" option
  agent approve helper@hub --expect Bash`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		resp, verb := backend.PermissionApprove, "Approved"
		if approveAlways {
			resp, verb = backend.PermissionApproveAlways, "Always approved"
		}
		return respondPermission(args[0], resp, verb)
	},
}

var denyCmd = &cobra.Command{
	Use:   "deny <name>[@host]",
	Short: "Answer no to an agent's permission prompt",
	Long: `Deny rejects the permission prompt an agent is waiting on. For Claude this
cancels the current turn; follow up with "agent send" to redirect it.

Refuses unless the agent's status is "waiting" and the prompt is still on
screen (override with --force): for Claude the deny key is Escape, which
would cancel the turn if the prompt was answered in the meantime.`,
	Example: `  agent deny helper
  agent deny helper --expect Bash
  agent deny helper@hub`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		return respondPermission(args[0], backend.PermissionDeny, "Denied")
	},
}

func init() {
	approveCmd.Flags().BoolVar(&approveAlways, "always", false, "Approve and stop asking for this kind of action")
	for _, cmd := range []*cobra.Command{approveCmd, denyCmd} {
		cmd.Flags().BoolVar(&permissionForce, "force", false, "Send even if the agent isn't waiting on a permission prompt")
		cmd.Flags().StringVar(&permissionExpect, "expect", "", "Refuse unless the pending prompt is for this tool (see status -v)")
	}
}

func respondPermission(name string, resp backend.PermissionResponse, verb string) error {
	ref, remote, err := parseTarget(name)
	if err != nil {
		return err
	}
	var info *service.StatusInfo
	if remote {
		err = remoteRespond(ref, resp)
	} else {
		info, err = agentService.RespondPermission(getContext(), ref.Name, resp, service.PermissionOptions{
			Force:      permissionForce,
			ExpectTool: permissionExpect,
		})
	}
	if err != nil {
		return err
	}
	prompt := "prompt"
	if info != nil && info.Tool != "" {
		prompt = info.Tool + " prompt"
	}
	line := fmt.Sprintf("%s %s's %s", verb, name, prompt)
	if info != nil && info.Detail != "" {
		line += fmt.Sprintf(" (%s)", service.OneLine(info.Detail))
	}
	fmt.Println(line + ".")
	if resp == backend.PermissionDeny && !remote {
		if a, err := agentService.Agent(getContext(), ref.Name); err == nil && a.Backend == "claude" {
			hint("denying cancels Claude's turn: tell it what to do instead with agent send %s \"...\"", name)
		}
	}
	return nil
}

func remoteRespond(ref natsbus.AgentRef, resp backend.PermissionResponse) error {
	response, err := service.PermissionResponseName(resp)
	if err != nil {
		return err
	}
	req := natsbus.RespondRequest{
		Response:   response,
		Force:      permissionForce,
		ExpectTool: permissionExpect,
	}
	return callRemote(ref.Host, func(c *natsbus.RPCClient) error {
		return c.Respond(getContext(), ref, req)
	})
}
