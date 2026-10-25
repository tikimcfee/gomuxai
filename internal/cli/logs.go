package cli

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

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

  12:04:31 prompt run the tests
  12:04:40 tool   Bash: go test ./... → ok (34 lines)
  12:05:02 say    All tests pass.
  12:05:02 system turn took 31.2s

Kinds: prompt (a message it got; one that didn't come from a person
typing says where it came from: queued, delivered, task-notification,
peer, command, system), think (its reasoning, where the CLI keeps it), say
(text it wrote), tool (a call and its result: ok, error, interrupted,
running in the background, or no result yet), system (a record of the
CLI's own: turn timing, compaction, API errors, interrupts). Times are
local. Text longer than a line is cut and marked "…"; --full prints it
whole, with each tool's output.

--json prints the events as docs/EVENTS.md describes them, one JSON
object per line: for each transcript, a session line, then its events
(a tool's result is an update of its call, sent again whole).

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
	logsCmd.Flags().BoolVar(&logsJSON, "json", false, "Print the sessions and their events as JSON lines")
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
		return printEventLines(sessions)
	}
	if len(sessions) == 1 && len(sessions[0].events) == 0 {
		fmt.Printf("Agent %s's transcript has no events yet (%s).\n", name, sessions[0].Session.Path)
		return nil
	}
	for i, s := range sessions {
		if i > 0 {
			fmt.Printf("--- session %s (%s)\n", s.Session.SessionID, s.Session.Harness)
		}
		for _, e := range s.events {
			fmt.Println(formatEvent(e, logsFull))
		}
	}
	return nil
}

// logsSession is a transcript read for logs: the stream its lines make,
// and the events logs prints, each in its latest state.
type logsSession struct {
	*transcript.File
	events []transcript.Event
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
		f, err := transcript.ReadFile(ref.Path, agentService.TranscriptReader(ref.Backend), n-total)
		if err != nil {
			if len(sessions) > 0 {
				break
			}
			if errors.Is(err, fs.ErrNotExist) {
				return nil, fmt.Sprintf("its transcript %s is gone", ref.Path)
			}
			return nil, fmt.Sprintf("its transcript %s can't be read: %v", ref.Path, err)
		}
		events := transcript.Latest(f.Events)
		if keep := n - total; len(events) > keep {
			events = events[len(events)-keep:]
		}
		total += len(events)
		sessions = append(sessions, logsSession{File: f, events: events})
		if len(events) == 0 && len(sessions) > 1 {
			sessions = sessions[:len(sessions)-1]
		}
	}
	slices.Reverse(sessions)
	return sessions, ""
}

// printEventLines prints each session as docs/EVENTS.md frames it: a
// session line, then its events, updates included, for the events logs
// would print.
func printEventLines(sessions []logsSession) error {
	enc := json.NewEncoder(os.Stdout)
	enc.SetEscapeHTML(false)
	for _, s := range sessions {
		if err := enc.Encode(sessionLine{"session", s.Session}); err != nil {
			return err
		}
		kept := make(map[string]bool, len(s.events))
		for _, e := range s.events {
			kept[e.ID] = true
		}
		for _, e := range s.Events {
			if !kept[e.ID] {
				continue
			}
			if err := enc.Encode(eventLine{"event", e}); err != nil {
				return err
			}
		}
	}
	return nil
}

// sessionLine and eventLine are the JSON lines of an event stream.
type (
	sessionLine struct {
		Type string `json:"type"`
		transcript.Session
	}
	eventLine struct {
		Type string `json:"type"`
		transcript.Event
	}
)

// Widths of an event line: the head ("15:04:05 prompt "), and how much
// text a cut line keeps.
const (
	eventHeadWidth = len("15:04:05 prompt ")
	eventTextWidth = 120
	toolTextWidth  = 80
)

// formatEvent renders an event as one line, or with full, as the whole
// text and tool output, continuation lines indented under the text.
func formatEvent(e transcript.Event, full bool) string {
	ts := "--:--:--"
	if t := e.Time(); !t.IsZero() {
		ts = t.Local().Format("15:04:05")
	}
	head := fmt.Sprintf("%s %-6s ", ts, e.Kind)
	text := func(s string) string {
		if full {
			return indent(s)
		}
		return cutLine(s, eventTextWidth)
	}
	switch {
	case e.Prompt != nil:
		p := e.Prompt
		s := text(p.Text)
		if summary := notificationSummary(p); summary != "" && !full {
			// A task notification's markup says nothing at a glance.
			s = cutLine(summary, eventTextWidth)
		}
		if p.Origin != transcript.OriginHuman && p.Origin != transcript.OriginCommand {
			s = "(" + p.Origin + ") " + s
		}
		if len(p.Images) > 0 {
			s += fmt.Sprintf(" [%s]", plural(len(p.Images), "image"))
		}
		return head + s
	case e.Think != nil:
		if e.Think.Redacted {
			return head + "(redacted)"
		}
		return head + text(e.Think.Text)
	case e.Say != nil:
		return head + text(e.Say.Text)
	case e.Tool != nil:
		return head + formatTool(e.Tool, full)
	case e.System != nil:
		return head + formatSystem(e.System, text)
	case e.Request != nil:
		return head + fmt.Sprintf("%s, %s", e.Request.Model, plural(len(e.Request.Tail), "new message"))
	}
	return head
}

// notificationSummary returns the <summary> of a task notification
// prompt, or "".
func notificationSummary(p *transcript.PromptEvent) string {
	if p.Origin != transcript.OriginTaskNotification {
		return ""
	}
	_, rest, ok := strings.Cut(p.Text, "<summary>")
	if !ok {
		return ""
	}
	summary, _, ok := strings.Cut(rest, "</summary>")
	if !ok {
		return ""
	}
	return strings.TrimSpace(summary)
}

// formatTool renders a tool call and its result: what it called, on what,
// and how it went.
func formatTool(c *transcript.ToolEvent, full bool) string {
	call := c.Name
	about := c.Target
	if about == "" {
		about = c.Detail
	}
	if about != "" {
		if full {
			call += ": " + indent(about)
		} else {
			call += ": " + cutLine(about, toolTextWidth)
		}
	}
	if m := toolMeta(c); m != "" {
		call += " (" + m + ")"
	}
	status, output := transcript.StatusPending, ""
	if c.Result != nil {
		status, output = c.Result.Status, strings.TrimRight(c.Result.Output, "\n")
	}
	lines := 0
	if strings.TrimSpace(output) != "" {
		lines = strings.Count(output, "\n") + 1
	}
	var result string
	switch status {
	case transcript.StatusPending:
		result = "no result yet"
	case transcript.StatusBackground:
		result = "running in the background"
	default:
		result = status
	}
	switch {
	case full && lines > 0:
		return fmt.Sprintf("%s → %s (%s)\n%s", call, result, plural(lines, "line"), indentAll(output))
	case status == transcript.StatusPending || status == transcript.StatusBackground || lines == 0:
		return fmt.Sprintf("%s → %s", call, result)
	case lines == 1 || status == transcript.StatusError:
		// An error's first line usually says what failed.
		s := fmt.Sprintf("%s → %s: %s", call, result, cutLine(output, toolTextWidth))
		if lines > 1 {
			s += fmt.Sprintf(" (%s)", plural(lines, "line"))
		}
		return s
	default:
		return fmt.Sprintf("%s → %s (%s)", call, result, plural(lines, "line"))
	}
}

// toolMeta returns the numbers of an edit worth a glance, "+3 -1", or "".
func toolMeta(c *transcript.ToolEvent) string {
	if c.Action != "edit" || len(c.Meta) == 0 {
		return ""
	}
	var m struct{ Added, Removed int }
	if json.Unmarshal(c.Meta, &m) != nil {
		return ""
	}
	return fmt.Sprintf("+%d -%d", m.Added, m.Removed)
}

// formatSystem renders a harness record: its text, or what its subtype
// says.
func formatSystem(s *transcript.SystemEvent, text func(string) string) string {
	switch s.Subtype {
	case "turn_duration":
		var d struct {
			DurationMs float64 `json:"durationMs"`
		}
		if json.Unmarshal(s.Data, &d) == nil && d.DurationMs > 0 {
			return fmt.Sprintf("turn took %s", time.Duration(d.DurationMs*float64(time.Millisecond)).Round(100*time.Millisecond))
		}
	case "interrupt", "api_error":
		return text(s.Text)
	}
	if s.Text == "" {
		return s.Subtype
	}
	return s.Subtype + ": " + text(s.Text)
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
