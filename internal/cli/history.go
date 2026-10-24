package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"slices"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/buildinfo"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Caps on what the command log stores: messages can be long.
const (
	commandArgCap   = 300
	commandErrorCap = 300
)

// recordWait bounds how long recording can hold up the process exit when
// the database is busy.
const recordWait = 500 * time.Millisecond

// recordCommand adds a run of cmd with args, which ended with runErr, to the
// command log. It is best effort: if the database can't be opened or
// written, the run goes unrecorded and nothing is said. Help, and commands
// that failed in setup, never opened the store: it opens it itself.
func recordCommand(cmd *cobra.Command, args []string, start time.Time, runErr error) {
	if !recorded(cmd) {
		return
	}
	duration := time.Since(start)
	c := cfg
	if c == nil {
		c = config.DefaultConfig()
	}
	s := store
	if s == nil {
		opened, err := storage.NewSQLiteStorage(c.DatabasePath)
		if err != nil {
			return
		}
		defer opened.Close()
		s = opened
	}
	ctx, cancel := context.WithTimeout(getContext(), recordWait)
	defer cancel()
	s.LogCommand(ctx, newCommandRecord(c.Host, args, start, duration, runErr))
}

// recorded reports whether runs of cmd go in the command log. Hook reports
// run on every tool call of every agent, and bus run is a daemon: neither
// is worth a row.
func recorded(cmd *cobra.Command) bool {
	switch {
	case cmd == nil, cmd == busRunCmd:
		return false
	case cmd == reportCmd && reportHook:
		return false
	}
	return true
}

func newCommandRecord(host string, args []string, start time.Time, duration time.Duration, runErr error) *storage.CommandRecord {
	cwd, _ := os.Getwd()
	rec := &storage.CommandRecord{
		StartedAt:  start,
		DurationMS: duration.Milliseconds(),
		Host:       host,
		Agent:      os.Getenv("AGENT_NAME"),
		Cwd:        cwd,
		Args:       make([]string, len(args)),
		ExitCode:   ExitCode(runErr),
		Build:      buildinfo.ID(),
	}
	for i, arg := range args {
		rec.Args[i] = capBytes(arg, commandArgCap)
	}
	if runErr != nil {
		first, _, _ := strings.Cut(runErr.Error(), "\n")
		rec.Error = capBytes(first, commandErrorCap)
	}
	return rec
}

// capBytes cuts s to at most n bytes, at a character boundary, and marks
// how much it cut.
func capBytes(s string, n int) string {
	if len(s) <= n {
		return s
	}
	cut := n
	for cut > 0 && !utf8.RuneStart(s[cut]) {
		cut--
	}
	return fmt.Sprintf("%s…[+%d bytes]", s[:cut], len(s)-cut)
}

// clip cuts s to at most n characters, ending in "…" if it cut.
func clip(s string, n int) string {
	if utf8.RuneCountInString(s) <= n {
		return s
	}
	r := []rune(s)
	return string(r[:n-1]) + "…"
}

var (
	historyAgent  string
	historyFailed bool
	historyLines  int
	historyJSON   bool
)

var historyCmd = &cobra.Command{
	Use:   "history",
	Short: "List recent agent commands run on this host",
	Long: `History lists the agent commands run on this host, newest last, one per
line: time, who ran it (an agent's name, or "-" for a person), exit code, the
command line, and for a failure, its error after "#".

Every run is recorded, help included, except hook reports and the bus daemon.
The newest 20000 are kept.`,
	Example: `  agent history
  agent history --agent lead --failed
  agent history --agent - -n 50      # a person's commands
  agent history --json`,
	Args: cobra.NoArgs,
	RunE: runHistory,
}

func init() {
	historyCmd.Flags().StringVar(&historyAgent, "agent", "", `Only this agent's commands ("-" for a person's)`)
	historyCmd.Flags().BoolVar(&historyFailed, "failed", false, "Only commands that failed")
	historyCmd.Flags().IntVarP(&historyLines, "lines", "n", 20, "Number of commands to list")
	historyCmd.Flags().BoolVar(&historyJSON, "json", false, "Print the commands as a JSON array")
}

func runHistory(cmd *cobra.Command, args []string) error {
	if historyLines < 1 {
		return fmt.Errorf("--lines must be at least 1")
	}
	f := storage.CommandFilter{Failed: historyFailed, Limit: historyLines}
	if cmd.Flags().Changed("agent") {
		f.ByAgent, f.Agent = true, historyAgent
		if historyAgent == "-" {
			f.Agent = ""
		}
	}
	recs, err := store.ListCommands(getContext(), f)
	if err != nil {
		return err
	}
	slices.Reverse(recs)

	out := cmd.OutOrStdout()
	if historyJSON {
		if recs == nil {
			recs = []*storage.CommandRecord{}
		}
		return json.NewEncoder(out).Encode(recs)
	}
	if len(recs) == 0 {
		if f.ByAgent || f.Failed {
			fmt.Fprintln(out, "No recorded commands match (agent history without --agent or --failed lists them all).")
		} else {
			fmt.Fprintf(out, "No commands recorded on host %s yet.\n", cfg.Host)
		}
		return nil
	}
	fmt.Fprint(out, formatHistory(recs))
	return nil
}

// History lines are cut to keep a long message from flooding the screen.
const (
	historyCommandWidth = 100
	historyErrorWidth   = 80
)

// formatHistory renders recs one per line, e.g.
// "10-24 14:03:11 lead   1 wait helper --for idel  # invalid status".
func formatHistory(recs []*storage.CommandRecord) string {
	width := 1
	for _, r := range recs {
		width = max(width, len(r.Agent))
	}
	var b strings.Builder
	for _, r := range recs {
		who := r.Agent
		if who == "" {
			who = "-"
		}
		fmt.Fprintf(&b, "%s %-*s %3d %s", r.StartedAt.Local().Format("01-02 15:04:05"), width, who,
			r.ExitCode, clip(util.ShellJoin(r.Args), historyCommandWidth))
		if r.Error != "" {
			fmt.Fprintf(&b, "  # %s", clip(r.Error, historyErrorWidth))
		}
		b.WriteByte('\n')
	}
	return b.String()
}
