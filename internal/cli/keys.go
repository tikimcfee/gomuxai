package cli

import (
	"fmt"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

var keysCmd = &cobra.Command{
	Use:   "keys <session | name@host> <text>",
	Short: "Send text to a tmux session",
	Long: `Keys sends text to a tmux session pane.

By default, Enter is sent after the text. Use --no-enter to skip.

This is a low-level command for agents that need to interact with arbitrary
tmux sessions (not just managed agents).

name@host addresses a managed agent instead of a session, on any host
(including this one). Buses don't serve keys to other hosts unless
configured to.

Examples:
  agent keys my-session "ls -la"
  agent keys my-session "echo hello" --no-enter
  agent keys builder@hub "y" --no-enter`,
	Args: cobra.MinimumNArgs(2),
	RunE: runKeys,
}

var keysNoEnter bool

func init() {
	keysCmd.Flags().BoolVar(&keysNoEnter, "no-enter", false, "Don't send Enter after text")
}

func runKeys(cmd *cobra.Command, args []string) error {
	session := args[0]
	text := strings.Join(args[1:], " ")

	if strings.Contains(session, "@") {
		return agentKeys(session, text)
	}

	if err := tmuxClient.SendKeys(session, text); err != nil {
		return err
	}

	if !keysNoEnter {
		if err := tmuxClient.SendSpecialKey(session, "Enter"); err != nil {
			return err
		}
	}

	fmt.Printf("Sent to %q\n", session)
	return nil
}

// agentKeys sends text (and Enter, unless --no-enter) to the agent named by
// target ("name@host"), on this host or through the bus.
func agentKeys(target, text string) error {
	ref, remote, err := parseTarget(target)
	if err != nil {
		return err
	}
	key := "Enter"
	if keysNoEnter {
		key = ""
	}
	if remote {
		err = callRemote(ref.Host, func(c *natsbus.RPCClient) error {
			return c.Keys(getContext(), ref, text, key)
		})
	} else {
		err = agentService.Keys(getContext(), ref.Name, text, key)
	}
	if err != nil {
		return err
	}
	fmt.Printf("Sent to %q\n", target)
	return nil
}
