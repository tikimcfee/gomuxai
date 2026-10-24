package cli

import (
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/transcript"
	"github.com/tikimcfee/gomuxai/internal/util"
)

var logsCmd = &cobra.Command{
	Use:   "logs <name>",
	Short: "Print what an agent did (prompts, replies, tool calls), even after cleanup",
	Long: `Logs prints an agent's last events, oldest first, one per line:

  12:04:31 user  run the tests
  12:04:40 tool  Bash: go test ./... → ok (34 lines)
  12:05:02 reply All tests pass.

Kinds: user (a prompt it got), think (its reasoning, where the CLI keeps
it), reply (text it wrote), tool (a call and its result: ok, error,
running in the background, or no result yet). Times are local. Text longer
than a line is cut and marked "…"; --full prints it whole, with each
tool's output. --json prints the events with their session's metadata.

The events come from the transcript the agent's CLI keeps of its session,
whose path the CLI's hooks report (claude and agy agents):
  claude: ~/.claude/projects/<project>/<session id>.jsonl
  agy:    ~/.gemini/antigravity-cli/brain/<id>/.system_generated/logs/transcript_full.jsonl
Each path an agent used is listed in <name>.transcripts beside its terminal
log, so logs finds them after cleanup, and across /clear and respawns under
the same name (a line "--- session <id>" marks where one begins).

--terminal prints the terminal log instead, as does logs when there is no
transcript to read (another backend, no hook has fired yet, the file is
gone or unparseable; a hint says which). That log is everything the
agent's terminal showed, cleaned into text: escapes dropped, blank lines
and screen redraws left out; it has no timestamps. Log files are in the
data dir next to the agent binary, not in the working directory; here:
  ` + filepath.Join(config.DefaultConfig().LogDir, "<name>.log") + `

For the live screen, or an agent on another host (logs are kept per host),
use agent capture.`,
	Example: `  agent logs researcher
  agent logs researcher --lines 10 --full
  agent logs researcher --terminal --lines 100`,
	Args: cobra.ExactArgs(1),
	RunE: runLogs,
}

var (
	logsLines    int
	logsFull     bool
	logsJSON     bool
	logsTerminal bool
)

func init() {
	logsCmd.Flags().IntVar(&logsLines, "lines", 50, "Number of events (or terminal lines) to print")
	logsCmd.Flags().BoolVar(&logsFull, "full", false, "Print each event's text and tool output whole")
	logsCmd.Flags().BoolVar(&logsJSON, "json", false, "Print the events and their sessions as JSON")
	logsCmd.Flags().BoolVar(&logsTerminal, "terminal", false, "Print the terminal log instead of the transcript")
}

func runLogs(cmd *cobra.Command, args []string) error {
	ref, remote, err := parseTarget(args[0])
	if err != nil {
		return err
	}
	if remote {
		return fmt.Errorf("logs are kept per host, and %s's are on %q: agent capture %s shows its screen", ref, ref.Host, ref)
	}
	name := ref.Name
	if logsLines < 1 {
		return fmt.Errorf("--lines must be at least 1")
	}
	if logsJSON && (logsTerminal || logsFull) {
		return fmt.Errorf("--json prints transcript events whole; it doesn't go with --terminal or --full")
	}
	// The name becomes a path component.
	if err := service.ValidateAgentName(name); err != nil {
		return err
	}
	if logsTerminal {
		return printTerminalLog(name)
	}

	sessions, why := readTranscripts(name, logsLines)
	if why != "" {
		if logsJSON {
			return fmt.Errorf("no transcript to print for %s: %s; agent logs %s --terminal prints its terminal log", name, why, name)
		}
		if !logExists(name) {
			if _, err := agentService.Agent(getContext(), name); err != nil {
				return noLog(name)
			}
		}
		hint("printing %s's terminal log: %s", name, why)
		return printTerminalLog(name)
	}
	if logsJSON {
		return printJSON(sessions)
	}
	if len(sessions) == 1 && len(sessions[0].Events) == 0 {
		fmt.Printf("Agent %s's transcript has no events yet (%s).\n", name, sessions[0].Path)
		return nil
	}
	for i, s := range sessions {
		if i > 0 {
			fmt.Printf("--- session %s (%s)\n", s.SessionID, s.Harness)
		}
		for _, e := range s.Events {
			fmt.Println(formatEvent(e, s.CWD, logsFull))
		}
	}
	return nil
}

// logsSession is a transcript's session with the events logs prints.
type logsSession struct {
	*transcript.Session
	Events []transcript.Event `json:"events"`
}

// readTranscripts reads the last n events of the named agent's
// transcripts, newest transcript first, going back to older ones while it
// has fewer than n. It returns the sessions that hold them, oldest first,
// or why there is no transcript to read: the newest must be readable,
// while an older one that isn't just ends the search.
func readTranscripts(name string, n int) ([]logsSession, string) {
	refs := agentService.Transcripts(getContext(), name)
	if len(refs) == 0 {
		return nil, "no transcript recorded for it"
	}
	var sessions []logsSession
	total := 0
	for i := len(refs) - 1; i >= 0 && total < n; i-- {
		ref := refs[i]
		s, err := transcript.ReadFile(ref.Path, agentService.TranscriptReader(ref.Backend), n-total)
		if err != nil {
			if len(sessions) > 0 {
				break
			}
			if errors.Is(err, fs.ErrNotExist) {
				return nil, fmt.Sprintf("its transcript %s is gone", ref.Path)
			}
			return nil, fmt.Sprintf("its transcript %s can't be read: %v", ref.Path, err)
		}
		events := s.Events()
		if keep := n - total; len(events) > keep {
			events = events[len(events)-keep:]
		}
		total += len(events)
		sessions = append(sessions, logsSession{Session: s, Events: events})
		if len(events) == 0 && len(sessions) > 1 {
			sessions = sessions[:len(sessions)-1]
		}
	}
	slices.Reverse(sessions)
	return sessions, ""
}

// Widths of an event line: the head ("15:04:05 reply "), and how much
// text a cut line keeps.
const (
	eventHeadWidth = len("15:04:05 reply ")
	eventTextWidth = 120
	toolTextWidth  = 80
)

// formatEvent renders an event as one line, or with full, as the whole
// text and tool output, continuation lines indented under the text. Paths
// under cwd, the session's working directory, print relative to it.
func formatEvent(e transcript.Event, cwd string, full bool) string {
	ts := "--:--:--"
	if !e.Time.IsZero() {
		ts = e.Time.Local().Format("15:04:05")
	}
	head := fmt.Sprintf("%s %-5s ", ts, e.Kind)
	if e.Tool == nil {
		if full {
			return head + indent(e.Text)
		}
		return head + cutLine(e.Text, eventTextWidth)
	}

	c := e.Tool
	call := c.Name
	if d := c.Detail(cwd); d != "" {
		if full {
			call += ": " + indent(d)
		} else {
			call += ": " + cutLine(d, toolTextWidth)
		}
	}
	output := strings.TrimRight(c.Output, "\n")
	lines := 0
	if strings.TrimSpace(output) != "" {
		lines = strings.Count(output, "\n") + 1
	}
	var result string
	switch c.Status {
	case transcript.StatusNone:
		result = "no result yet"
	case transcript.StatusRunning:
		result = "running in the background"
	case transcript.StatusError:
		result = "error"
	default:
		result = "ok"
	}
	switch {
	case full && lines > 0:
		return fmt.Sprintf("%s%s → %s (%s)\n%s", head, call, result, plural(lines, "line"), indentAll(output))
	case c.Status == transcript.StatusNone || c.Status == transcript.StatusRunning || lines == 0:
		return fmt.Sprintf("%s%s → %s", head, call, result)
	case lines == 1 || c.Status == transcript.StatusError:
		// An error's first line usually says what failed.
		s := fmt.Sprintf("%s%s → %s: %s", head, call, result, cutLine(output, toolTextWidth))
		if lines > 1 {
			s += fmt.Sprintf(" (%s)", plural(lines, "line"))
		}
		return s
	default:
		return fmt.Sprintf("%s%s → %s (%s)", head, call, result, plural(lines, "line"))
	}
}

// cutLine returns the first non-blank line of s, cut to width runes; a cut
// (or dropped lines) is marked "…".
func cutLine(s string, width int) string {
	s = strings.TrimSpace(s)
	line, rest, _ := strings.Cut(s, "\n")
	line = strings.TrimSpace(line)
	cut := strings.TrimSpace(rest) != ""
	if r := []rune(line); len(r) > width {
		line, cut = strings.TrimSpace(string(r[:width-1])), true
	}
	if cut {
		line += "…"
	}
	return line
}

// indent indents the second and later lines of s under an event's text.
func indent(s string) string {
	return strings.ReplaceAll(strings.TrimSpace(s), "\n", "\n"+strings.Repeat(" ", eventHeadWidth))
}

// indentAll indents every line of s under an event's text.
func indentAll(s string) string {
	return strings.Repeat(" ", eventHeadWidth) + indent(s)
}

// logExists reports whether the named agent has a terminal log.
func logExists(name string) bool {
	_, err := os.Stat(cfg.LogPath(name))
	return err == nil
}

// printTerminalLog prints the last logsLines lines of the agent's terminal log.
func printTerminalLog(name string) error {
	lines, err := tailLogLines(cfg.LogPath(name), logsLines)
	if errors.Is(err, fs.ErrNotExist) || (err == nil && len(lines) == 0) {
		if _, agentErr := agentService.Agent(getContext(), name); agentErr != nil {
			return noLog(name)
		}
		fmt.Printf("Agent %s has logged no output yet (%s).\n", name, absPath(cfg.LogPath(name)))
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

// noLog is the error for a name with neither a log nor an agent: it
// suggests close names among both.
func noLog(name string) error {
	var names []string
	if files, err := os.ReadDir(cfg.LogDir); err == nil {
		for _, f := range files {
			if n, ok := strings.CutSuffix(f.Name(), ".log"); ok {
				names = append(names, n)
			}
		}
	}
	if agents, err := agentService.List(getContext()); err == nil {
		for _, a := range agents {
			if !slices.Contains(names, a.Name) {
				names = append(names, a.Name)
			}
		}
	}
	msg := fmt.Sprintf("no log for %q: no agent by that name has run on host %s", name, cfg.Host)
	if similar := util.Similar(name, names); len(similar) > 0 {
		return fmt.Errorf("%s; did you mean %s?", msg, util.OrList(similar, nil))
	}
	return fmt.Errorf("%s (logs are in %s)", msg, absPath(cfg.LogDir))
}

// absPath returns path made absolute, or as it is if that fails.
func absPath(path string) string {
	if abs, err := filepath.Abs(path); err == nil {
		return abs
	}
	return path
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
