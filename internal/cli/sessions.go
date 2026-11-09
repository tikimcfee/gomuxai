package cli

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"text/tabwriter"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/export"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var sessionsCmd = &cobra.Command{
	Use:   "sessions",
	Short: "List the agent sessions on this host, spawned by agent or not: the newest 25, or filtered",
	Long: `Sessions lists the agent sessions this host can see, newest first: the
transcripts Claude Code, agy and Kimi Code keep of their sessions, whether
"agent spawn" started the session (a workgroup agent) or a person ran the
CLI in a terminal. Both kinds have the same fields; WORKGROUP names the
agent that owns a session, or is "-". A subagent's session (a Claude Task,
an agy or Kimi subagent) is a session too, listed under the one that
started it.

  WHEN              HARNESS  PROJECT  STATUS  WORKGROUP  TITLE
  2026-10-25 13:35  claude   gomuxai  idle    lead-api   Fix the auth tests
  2026-10-25 13:30  claude   gomuxai  ended (quit)      -          Plan the export
  └ 10-25 13:31     claude   gomuxai  ended (finished)  -          Explore internal/export

The table shows the newest 25 sessions, each with its subagents, and a
hint on stderr says how many older ones it left out: --limit N shows the
newest N, --all every one. --json prints every session unless --limit
says how many, so a script or display reading it is never cut short.

Filters (they combine, and apply to --json too):
  --since WHEN     written since then: a duration back from now (30m, 2h,
                   3d, 1w) or a local date and time (2026-11-01,
                   "2026-11-01 13:30", 2026-11-01T13:30:00Z)
  --project P      a path (it has a slash, or is ., .. or ~): run in that
                   directory or one inside it, symlinks resolved; anything
                   else: PROJECT contains P, ignoring case
  --harness H      claude, agy, kimi, or a workgroup agent's backend that
                   keeps no transcript (aider)
  --workgroup      owned by a workgroup agent; --local: not
A filter and the limit pick sessions by the one at the top of their
group: a subagent goes with the session that started it. A live agent's
session with no transcript yet counts as written now.

WHEN is the transcript's last write, in local time. PROJECT is the last
part of the working directory the session recorded. TITLE is the newest
title the harness recorded: for Claude, a custom title (/rename) over the
AI-written one; agy's annotation title; Kimi's state.json title; a Claude
subagent's description when it has none.

STATUS (idle, processing, waiting, error, ended, unknown; an ended session
shows its end reason, as in "ended (gone)"; "agent statuses" says what each
means):
  - a workgroup agent's current session: its last report, reconciled with
    Claude Code's session registry for Claude by the rule agent status uses:
    the registry's status when a running process holds the session (except
    a "waiting" within 5s of a deny), ended (gone) when none does but the
    report says it is alive. agy: ended (gone) once agy no longer holds the
    conversation open. agent status also reads the screen; sessions doesn't;
  - an agent's earlier sessions (before /clear, a resume, a respawn, or a
    CLI started anew in its pane): ended (replaced); its last one, once
    cleanup removed it: ended (closed);
  - Claude, no workgroup: the registry's status, or ended (gone) if no
    running process holds the session (unknown if the registry can't be
    read);
  - agy, no workgroup: unknown while agy holds the conversation open (its
    presence lock; idle or busy can't be told), else ended (gone);
  - Kimi, which records nothing about running: unknown if its transcript
    changed in the last 10 minutes, else ended (gone);
  - a subagent: ended (finished) if it recorded finishing, ended (parent)
    if its parent session ended; else unknown.
Subagents are listed under their parent, oldest last write first.

Where it looks (each harness's own default dirs that exist):
  claude  $CLAUDE_CONFIG_DIR/projects, else ~/.claude/projects
  agy     ~/.gemini/antigravity/brain (desktop), ~/.gemini/antigravity-cli/brain (CLI)
  kimi    ~/.kimi-code/sessions
In the JSON app config (--backend-config), claude_projects_dir,
agy_brain_dir and kimi_sessions_dir replace a harness's dirs with one path,
and "" turns the harness off. A workgroup agent's transcript elsewhere is
listed too. --dirs prints the directories it scans (with --json: each one's
harness, path, origin, config or default, and whether it exists).

It sorts by the files' last writes from directory listings, then reads
only the transcripts it shows (their head and tail), and for --project the
Claude ones whose project folder, named for the directory the session
started in, could match.

Workgroup agents come from this binary's store and the stores its tmux
sessions point at, live and departed, as agent export --all reads them.
This host only. --json prints each session as docs/EVENTS.md describes a
Session: the "session" line agent transcript opens with.`,
	Example: `  agent sessions
  agent sessions --all
  agent sessions --since 2h --project .
  agent sessions --project gomuxai --limit 5
  agent sessions --workgroup
  agent sessions --local --harness claude
  agent sessions --json --since 1d`,
	Args: cobra.NoArgs,
	RunE: runSessions,
}

var (
	sessionsJSON      bool
	sessionsHarness   string
	sessionsWorkgroup bool
	sessionsLocal     bool
	sessionsDirs      bool
	sessionsLimit     int
	sessionsAll       bool
	sessionsSince     string
	sessionsProject   string
)

// sessionsDefaultLimit is how many sessions the table shows without
// --limit or --all.
const sessionsDefaultLimit = 25

// claudeSessions lists the Claude sessions running on this host; a
// variable for tests.
var claudeSessions = namedagents.ClaudeSessions

func init() {
	sessionsCmd.Flags().BoolVar(&sessionsJSON, "json", false, "Print the sessions as a JSON array")
	sessionsCmd.Flags().StringVar(&sessionsHarness, "harness", "", "Only this harness's sessions: claude, agy, kimi, or the backend of a workgroup agent with no transcript (aider)")
	sessionsCmd.Flags().BoolVar(&sessionsWorkgroup, "workgroup", false, "Only sessions a workgroup agent owns")
	sessionsCmd.Flags().BoolVar(&sessionsLocal, "local", false, "Only sessions no workgroup agent owns")
	sessionsCmd.Flags().BoolVar(&sessionsDirs, "dirs", false, "Print the directories sessions scans instead of the sessions (--json: harness, path, origin, exists)")
	sessionsCmd.Flags().IntVarP(&sessionsLimit, "limit", "n", 0, fmt.Sprintf("Only the newest N sessions, each with its subagents (the table's default: %d; --json's: all)", sessionsDefaultLimit))
	sessionsCmd.Flags().BoolVar(&sessionsAll, "all", false, fmt.Sprintf("Every session, however many (the table shows the newest %d without it)", sessionsDefaultLimit))
	sessionsCmd.Flags().StringVar(&sessionsSince, "since", "", "Only sessions written since then: a duration back from now (30m, 2h, 3d, 1w) or a local date and time (2026-11-01, \"2026-11-01 13:30\")")
	sessionsCmd.Flags().StringVar(&sessionsProject, "project", "", "Only sessions run in this directory or inside it (a path: ., ~/src/app, /srv/x), or whose PROJECT contains this text, ignoring case")
}

// sessionDir is a directory agent sessions scans, as --dirs --json prints
// it.
type sessionDir struct {
	discovery.Dir
	Exists bool `json:"exists"`
}

// printDirs prints the directories agent sessions scans: the line its
// table opens with, or a JSON array.
func printDirs(out io.Writer, dirs []discovery.Dir) error {
	if sessionsJSON {
		list := make([]sessionDir, len(dirs))
		for i, d := range dirs {
			info, err := os.Stat(d.Path)
			list[i] = sessionDir{Dir: d, Exists: err == nil && info.IsDir()}
		}
		return printJSON(list)
	}
	home, _ := os.UserHomeDir()
	fmt.Fprintf(out, "Looked in: %s\n", discovery.DescribeDirs(dirs, home))
	return nil
}

func runSessions(cmd *cobra.Command, args []string) error {
	if sessionsWorkgroup && sessionsLocal {
		return fmt.Errorf("--workgroup and --local each leave out what the other keeps: use one (or neither, for all)")
	}
	q, err := sessionsQuery(cmd, time.Now())
	if err != nil {
		return err
	}
	ctx := getContext()
	dirs := sessionDirs()
	if sessionsDirs {
		return printDirs(cmd.OutOrStdout(), dirs)
	}
	agents, err := workgroupAgents(ctx)
	if err != nil {
		return err
	}
	live, claudeErr := liveNow(ctx)
	index := discovery.NewIndex(dirs, agents)
	// A workgroup agent with no transcript is listed under its backend's
	// name (aider, say), so that is a harness to filter on too.
	if harnesses := index.Harnesses(); q.Harness != "" && !slices.Contains(harnesses, q.Harness) {
		return fmt.Errorf("--harness %s: no session has that harness; use %s", q.Harness, strings.Join(harnesses, ", "))
	}
	picked, left := index.Select(q)
	sessions := index.Resolve(picked, live)
	if claudeErr != nil && slices.ContainsFunc(sessions, func(s discovery.Session) bool { return s.Harness == discovery.HarnessClaude }) {
		hint("Claude sessions' status is unknown or their last report: %v", claudeErr)
	}
	if left > 0 {
		hint("showing the newest %d sessions and their subagents; %d older left out: --all lists every one, --limit N the newest N", q.Limit, left)
	}
	if sessionsJSON {
		return printJSON(nonNilSessions(sessions))
	}
	home, _ := os.UserHomeDir()
	printSessions(cmd.OutOrStdout(), sessions, discovery.DescribeDirs(dirs, home))
	return nil
}

// sessionsQuery is what agent sessions' flags pick, at now.
func sessionsQuery(cmd *cobra.Command, now time.Time) (discovery.Query, error) {
	q := discovery.Query{Harness: sessionsHarness, Workgroup: sessionsWorkgroup, Local: sessionsLocal}
	limitSet := cmd.Flags().Changed("limit")
	switch {
	case sessionsAll && limitSet:
		return q, fmt.Errorf("--all and --limit don't go together: --all lists every session, --limit N the newest N")
	case limitSet && sessionsLimit < 1:
		return q, fmt.Errorf("--limit %d: give 1 or more (--all lists every session)", sessionsLimit)
	case limitSet:
		q.Limit = sessionsLimit
	case !sessionsAll && !sessionsJSON:
		// Scripts and displays read --json: it is never cut unasked.
		q.Limit = sessionsDefaultLimit
	}
	if sessionsSince != "" {
		since, err := parseSince(sessionsSince, now)
		if err != nil {
			return q, err
		}
		q.Since = since
	}
	if p := sessionsProject; p != "" {
		if looksLikeDir(p) {
			home, _ := os.UserHomeDir()
			if p == "~" {
				p = home
			} else if rest, ok := strings.CutPrefix(p, "~/"); ok && home != "" {
				p = filepath.Join(home, rest)
			}
			abs, err := filepath.Abs(p)
			if err != nil {
				return q, fmt.Errorf("--project %s: %w", sessionsProject, err)
			}
			p = abs
		}
		q.Project = p
	}
	return q, nil
}

// looksLikeDir reports whether a --project value is a directory rather
// than part of a project name: it has a slash, or is ., .. or ~.
func looksLikeDir(p string) bool {
	return strings.ContainsRune(p, '/') || strings.ContainsRune(p, filepath.Separator) || p == "." || p == ".." || p == "~"
}

// parseSince reads --since: a duration back from now (a Go duration, or a
// whole number of days or weeks: 3d, 1w), or a local date, or date and
// time.
func parseSince(v string, now time.Time) (time.Time, error) {
	v = strings.TrimSpace(v)
	if n, unit := strings.TrimRight(v, "dw"), strings.TrimLeft(v, "0123456789"); len(unit) == 1 && n != "" && n != v {
		if days, err := strconv.Atoi(n); err == nil {
			if unit == "w" {
				days *= 7
			}
			return now.AddDate(0, 0, -days), nil
		}
	}
	if d, err := time.ParseDuration(v); err == nil {
		if d < 0 {
			return time.Time{}, fmt.Errorf("--since %s: give how long ago without a minus sign (2h, 3d)", v)
		}
		return now.Add(-d), nil
	}
	for _, layout := range []string{"2006-01-02", "2006-01-02 15:04", "2006-01-02T15:04", "2006-01-02 15:04:05", "2006-01-02T15:04:05"} {
		if t, err := time.ParseInLocation(layout, v, time.Local); err == nil {
			return t, nil
		}
	}
	if t, err := time.Parse(time.RFC3339, v); err == nil {
		return t, nil
	}
	return time.Time{}, fmt.Errorf("--since %s: give a duration back from now (30m, 2h, 3d, 1w) or a local date and time (2026-11-01, \"2026-11-01 13:30\")", v)
}

// workgroupAgents reads every run export --all sees on this host, with the
// transcript history each name recorded.
func workgroupAgents(ctx context.Context) ([]discovery.Agent, error) {
	tree := export.New(exportDeps(), []string{filepath.Dir(cfg.DatabasePath)}, nil, true)
	defer tree.Close()
	snap, err := tree.Snapshot(ctx)
	if err != nil {
		return nil, err
	}
	own := filepath.Clean(filepath.Dir(cfg.DatabasePath))
	history := map[string][]string{} // by store and name: one list per name
	var agents []discovery.Agent
	for _, a := range snap.Agents {
		key := a.Store + "\x00" + a.Name
		if _, ok := history[key]; !ok {
			// Another binary's store keeps the list where its default
			// config does: beside its database, under logs/.
			list := filepath.Join(a.Store, "logs", a.Name+".transcripts")
			if a.Store == own {
				list = cfg.TranscriptsPath(a.Name)
			}
			for _, ref := range service.ReadTranscriptRefs(list) {
				history[key] = append(history[key], ref.Path)
			}
		}
		agents = append(agents, discovery.Agent{Name: a.Name, Parent: a.Parent, Store: a.Store, Host: a.Host,
			Backend: a.Backend, Live: a.Live, CreatedAt: a.CreatedAt, EndedAt: a.EndedAt, Status: a.Status, EndReason: a.EndReason,
			Event: a.Event, StatusAt: a.StatusAt, SessionID: a.SessionID, Transcript: a.Transcript, History: history[key]})
	}
	return agents, nil
}

// printSessions prints the sessions for a person: one line each, newest
// first, with each one's subagents right under it.
func printSessions(out io.Writer, sessions []discovery.Session, looked string) {
	fmt.Fprintf(out, "Looked in: %s\n", looked)
	if len(sessions) == 0 {
		fmt.Fprintln(out, "No sessions.")
		return
	}
	subs := map[string][]discovery.Session{}
	ids := map[string]bool{}
	for _, s := range sessions {
		ids[s.Harness+"\x00"+s.SessionID] = true
	}
	var top []discovery.Session
	for _, s := range sessions {
		if p := s.ParentSession; p != nil && ids[s.Harness+"\x00"+p.SessionID] {
			key := s.Harness + "\x00" + p.SessionID
			subs[key] = append(subs[key], s)
			continue
		}
		top = append(top, s)
	}
	fmt.Fprintln(out)
	tw := tabwriter.NewWriter(out, 0, 0, 2, ' ', 0)
	format := "%s\t%s\t%s\t%s\t%s\t%s\n"
	fmt.Fprintf(tw, format, "WHEN", "HARNESS", "PROJECT", "STATUS", "WORKGROUP", "TITLE")
	owned := 0
	line := func(when string, s discovery.Session) {
		group := "-"
		if s.Workgroup != nil {
			group = s.Workgroup.Name
			owned++
		}
		title := strings.Join(strings.Fields(s.Title), " ")
		switch {
		case title != "":
		case s.Path == "":
			title = "(no transcript yet)"
		default:
			title = "(" + s.SessionID + ")"
		}
		status := s.Status
		if s.EndReason != "" {
			status += " (" + s.EndReason + ")"
		}
		fmt.Fprintf(tw, format, when, s.Harness, orDash(s.Project), status, group, title)
	}
	printed := map[string]bool{}
	var kidsOf func(s discovery.Session, depth int)
	kidsOf = func(s discovery.Session, depth int) {
		key := s.Harness + "\x00" + s.SessionID
		if printed[key] {
			return
		}
		printed[key] = true
		kids := subs[key]
		slices.Reverse(kids) // sessions are newest first; kids read oldest last write first
		for _, k := range kids {
			line(strings.Repeat("  ", depth)+"└ "+sessionWhen(k.Mtime, "01-02 15:04"), k)
			kidsOf(k, depth+1)
		}
	}
	for _, s := range top {
		line(sessionWhen(s.Mtime, "2006-01-02 15:04"), s)
		kidsOf(s, 0)
	}
	tw.Flush()
	fmt.Fprintf(out, "\n%s, %d in a workgroup.\n", plural(len(sessions), "session"), owned)
}

func sessionWhen(t time.Time, layout string) string {
	if t.IsZero() {
		return "-"
	}
	return t.Local().Format(layout)
}

func nonNilSessions(s []discovery.Session) []discovery.Session {
	if s == nil {
		return []discovery.Session{}
	}
	return s
}
