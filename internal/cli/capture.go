package cli

import (
	"fmt"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

var captureCmd = &cobra.Command{
	Use:   "capture <name>[@host]",
	Short: "Capture output from an agent's terminal",
	Long: `Capture retrieves the current content from an agent's tmux pane.

By default, ANSI escape codes are stripped for clean output.
Use --raw to preserve escape codes.
An agent on another host is addressed as name@host (needs a running bus).

Examples:
  agent capture researcher
  agent capture builder --lines 100
  agent capture builder --raw
  agent capture builder@hub`,
	Args: cobra.ExactArgs(1),
	RunE: runCapture,
}

var (
	captureLines int
	captureRaw   bool
)

func init() {
	captureCmd.Flags().IntVar(&captureLines, "lines", 50, "Number of lines to capture")
	captureCmd.Flags().BoolVar(&captureRaw, "raw", false, "Preserve ANSI escape codes")
}

func runCapture(cmd *cobra.Command, args []string) error {
	ref, remote, err := parseTarget(args[0])
	if err != nil {
		return err
	}

	var output string
	if remote {
		err = callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
			output, err = c.Capture(getContext(), ref, captureLines, captureRaw)
			return err
		})
	} else {
		output, err = agentService.Capture(getContext(), ref.Name, captureLines, captureRaw)
	}
	if err != nil {
		return err
	}

	fmt.Print(output)
	return nil
}
