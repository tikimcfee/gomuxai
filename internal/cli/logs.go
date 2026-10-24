package cli

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/util"
)

var logsCmd = &cobra.Command{
	Use:   "logs <name>",
	Short: "View an agent's terminal output log",
	Long: `Logs prints the last lines of an agent's log file, .gomuxai/logs/<name>.log,
which holds everything its terminal has shown.

The file is a raw terminal stream, so logs cleans it into text: escape
sequences are dropped, cursor movements become spaces and line breaks, and
blank lines and consecutive repeats (screen redraws) are left out. Lines a TUI
redrew only in part can show gaps. The file has no timestamps.

The log outlives the agent: cleanup keeps it, and an agent spawned again
under the same name appends to it.

Examples:
  agent logs researcher
  agent logs researcher --lines 100`,
	Args: cobra.ExactArgs(1),
	RunE: runLogs,
}

var logsLines int

func init() {
	logsCmd.Flags().IntVar(&logsLines, "lines", 50, "Number of lines to print")
}

func runLogs(cmd *cobra.Command, args []string) error {
	name := args[0]
	if logsLines < 1 {
		return fmt.Errorf("--lines must be at least 1")
	}
	// The name becomes a path component.
	if err := service.ValidateAgentName(name); err != nil {
		return err
	}

	lines, err := tailLogLines(cfg.LogPath(name), logsLines)
	if errors.Is(err, fs.ErrNotExist) || (err == nil && len(lines) == 0) {
		fmt.Printf("No logs found for agent %q\n", name)
		return nil
	}
	if err != nil {
		return err
	}

	for _, line := range lines {
		fmt.Println(line)
	}
	return nil
}

// logTailChunk is how much of the end of a log file tailLogLines reads
// first. Logs grow to megabytes, mostly escape sequences; the window doubles
// until it holds enough lines of text.
const logTailChunk = 256 << 10

// tailLogLines returns the last n lines of text in the terminal log at path.
func tailLogLines(path string, n int) ([]string, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer f.Close()
	info, err := f.Stat()
	if err != nil {
		return nil, err
	}
	size := info.Size()

	for chunk := int64(logTailChunk); ; chunk *= 2 {
		off := max(size-chunk, 0)
		buf := make([]byte, size-off)
		if _, err := f.ReadAt(buf, off); err != nil && err != io.EOF {
			return nil, fmt.Errorf("reading log: %w", err)
		}
		lines := util.TerminalLines(string(buf))
		if off > 0 && len(lines) > 0 {
			lines = lines[1:] // may start mid-line, or mid-sequence
		}
		if len(lines) >= n || off == 0 {
			return lines[max(len(lines)-n, 0):], nil
		}
	}
}
