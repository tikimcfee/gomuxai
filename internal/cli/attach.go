package cli

import (
	"fmt"
	"os"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

var attachCmd = &cobra.Command{
	Use:   "attach <name>",
	Short: "Open an agent's tmux session in this terminal",
	Long: `Attach opens an agent's tmux session in this terminal, as tmux attach
does: you see its screen and can type into it. Ctrl-b d detaches; the agent
keeps running.

Inside tmux, it switches this terminal's client to the agent's session
instead of starting a tmux inside a tmux.

It never attaches read-only: while a read-only client is attached, tmux
refuses the keys agent send types, so no message could reach the agent.

agent attach name@host opens an agent on another host. The bus can't carry
a terminal, so agent runs ssh: ssh -t <destination> <its agent binary>
attach <name>, where the destination is the host's name unless agent bus
ssh <host> <destination> set another, and the host says through the bus
where its binary is. ssh is a separate trust path from the bus (your ssh
keys and ~/.ssh/config, not the bus's pinned keys), so it needs "ssh
<destination>" to work from here; if ssh fails, its error is what you see.

Either way this process becomes tmux (or ssh): a signal to it reaches them
directly, and nothing is left behind.

agent spawn --attach spawns an agent and attaches to it in one step.`,
	Example: `  agent attach helper
  agent attach helper@hub
  agent spawn helper --backend claude --attach`,
	Args: cobra.ExactArgs(1),
	RunE: runAttach,
}

func runAttach(cmd *cobra.Command, args []string) error {
	ref, remote, err := parseTarget(args[0])
	if err != nil {
		return err
	}
	if remote {
		return sshAttach(ref, "attach")
	}
	agent, err := agentService.Agent(getContext(), ref.Name)
	if err != nil {
		return err
	}
	return attachSession(agent.SessionName)
}

// attachSession hands this terminal to tmux, showing session, until the
// person detaches: this process becomes tmux (execInto).
func attachSession(session string) error {
	bin, err := tmux.Binary()
	if err != nil {
		return fmt.Errorf("attaching to tmux session %s: %w", session, err)
	}
	if err := execInto(bin, append([]string{"tmux"}, attachArgs(session, os.Getenv("TMUX") != "")...)); err != nil {
		return fmt.Errorf("attaching to tmux session %s: %w", session, err)
	}
	return nil
}

// attachArgs are the tmux arguments that show session in this terminal: a
// switch of this client inside tmux, else a new client. Never read-only.
func attachArgs(session string, insideTmux bool) []string {
	if insideTmux {
		return []string{"switch-client", "-t", "=" + session}
	}
	return []string{"attach-session", "-t", "=" + session}
}
