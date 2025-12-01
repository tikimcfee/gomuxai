package cli

import (
	"fmt"

	"github.com/spf13/cobra"
)

var captureCmd = &cobra.Command{
	Use:   "capture <name>",
	Short: "Capture output from an agent's terminal",
	Long: `Capture retrieves the current content from an agent's tmux pane.

By default, ANSI escape codes are stripped for clean output.
Use --raw to preserve escape codes.

Examples:
  agent capture researcher
  agent capture builder --lines 100
  agent capture builder --raw`,
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
	name := args[0]

	output, err := agentService.Capture(getContext(), name, captureLines, captureRaw)
	if err != nil {
		return err
	}

	fmt.Print(output)
	return nil
}
