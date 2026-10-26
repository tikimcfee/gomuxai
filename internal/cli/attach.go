package cli

import (
	"fmt"
	"os"
	"os/exec"

	"github.com/spf13/cobra"
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

agent spawn --attach spawns an agent and attaches to it in one step.`,
	Example: `  agent attach helper
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
		return fmt.Errorf("attach opens an agent on this host, and %s is on %q: run agent attach %s there (agent capture %s shows its screen from here)",
			ref.Name, ref.Host, ref.Name, ref)
	}
	agent, err := agentService.Agent(getContext(), ref.Name)
	if err != nil {
		return err
	}
	return attachSession(agent.SessionName)
}

// attachSession hands this terminal to tmux, showing session, until the
// person detaches.
func attachSession(session string) error {
	c := exec.Command("tmux", attachArgs(session, os.Getenv("TMUX") != "")...)
	c.Stdin, c.Stdout, c.Stderr = os.Stdin, os.Stdout, os.Stderr
	if err := c.Run(); err != nil {
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
