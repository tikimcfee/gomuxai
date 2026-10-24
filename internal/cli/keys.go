package cli

import (
	"fmt"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

var keysCmd = &cobra.Command{
	Use:   "keys <session | name@host> <text>",
	Short: "Type text into a tmux session (then Enter)",
	Long: `Keys types text into a tmux session's pane, then presses Enter (--no-enter
skips it). The text is typed literally: key names like "Escape" are typed as
words; use "agent key" to press special keys.

This is a low-level command for driving arbitrary tmux sessions, and takes a
session name (agent "helper"'s session is "agent-helper"). It does no dialog
checks: to message an agent, use "agent send".

name@host addresses a managed agent instead of a session, on any host
(including this one). Buses don't serve keys to other hosts unless
configured to.`,
	Example: `  agent keys my-session "ls -la"
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
	if err := checkSession(session); err != nil {
		return err
	}

	if err := tmuxClient.SendKeys(session, text); err != nil {
		return err
	}

	if !keysNoEnter {
		if err := tmuxClient.SendSpecialKey(session, "Enter"); err != nil {
			return err
		}
	}

	fmt.Printf("Typed into session %s%s.\n", session, enterNote())
	return nil
}

// enterNote says whether keys pressed Enter after the text.
func enterNote() string {
	if keysNoEnter {
		return " (no Enter)"
	}
	return " and pressed Enter"
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
	fmt.Printf("Typed into agent %s%s.\n", target, enterNote())
	return nil
}
