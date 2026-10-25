package cli

import (
	"context"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/export"
	"github.com/tikimcfee/gomuxai/internal/service"
)

var sessionsCmd = &cobra.Command{
	Use:   "sessions",
	Short: "List every agent session on this host, spawned by agent or not",
	Long: `Sessions lists every agent session this host can see, newest first: the
transcripts Claude Code, agy and Kimi Code keep of their sessions, whether
"agent spawn" started the session (a workgroup agent) or a person ran the
CLI in a terminal. Both kinds have the same fields; WORKGROUP names the
agent that owns a session, or is "-". A subagent's session (a Claude Task,
an agy or Kimi subagent) is a session too, listed under the one that
started it.

  WHEN              HARNESS  PROJECT  STATUS  WORKGROUP  TITLE
  2026-10-25 13:35  claude   gomuxai  idle    lead-api   Fix the auth tests
  2026-10-25 13:30  claude   gomuxai  exited  -          Plan the export
  └ 10-25 13:31     claude   gomuxai  exited  -          Explore internal/export

WHEN is the transcript's last write, in local time. PROJECT is the last
part of the working directory the session recorded. TITLE is the newest
title the harness recorded: for Claude, a custom title (/rename) over the
AI-written one; agy's annotation title; Kimi's state.json title; a Claude
subagent's description when it has none.

STATUS (idle, processing, waiting, error, exited, unknown):
  - a workgroup agent's current session: its last report, as agent export
    shows it, except that a Claude session "claude agents --json" doesn't
    list, or an agy conversation agy no longer holds open, is exited;
  - an agent's earlier sessions (before /clear, a respawn, or cleanup):
    exited;
  - Claude, no workgroup: what "claude agents --json" says, or exited if it
    doesn't list the session (unknown if it can't be run);
  - agy, no workgroup: unknown while agy holds the conversation open (its
    presence lock; idle or busy can't be told), else exited;
  - Kimi, which records nothing about running: unknown if its transcript
    changed in the last 10 minutes, else exited;
  - a subagent: exited if it recorded finishing, or its parent session is
    exited; else unknown.

Where it looks (each harness's own default dirs that exist):
  claude  $CLAUDE_CONFIG_DIR/projects, else ~/.claude/projects
  agy     ~/.gemini/antigravity/brain (desktop), ~/.gemini/antigravity-cli/brain (CLI)
  kimi    ~/.kimi-code/sessions
In the JSON app config (--backend-config), claude_projects_dir,
agy_brain_dir and kimi_sessions_dir replace a harness's dirs with one path,
and "" turns the harness off. A workgroup agent's transcript elsewhere is
listed too.

Workgroup agents come from this binary's store and the stores its tmux
sessions point at, live and departed, as agent export --all reads them.
This host only. --json prints each session as docs/EVENTS.md describes a
Session: the "session" line agent events opens with.`,
	Example: `  agent sessions
  agent sessions --workgroup
  agent sessions --local --harness claude
  agent sessions --json`,
	Args: cobra.NoArgs,
	RunE: runSessions,
}

var (
	sessionsJSON      bool
	sessionsHarness   string
	sessionsWorkgroup bool
	sessionsLocal     bool
)

// claudeSessions lists the Claude sessions running on this host; a
// variable for tests.
var claudeSessions = namedagents.ClaudeSessions

func init() {
	sessionsCmd.Flags().BoolVar(&sessionsJSON, "json", false, "Print the sessions as a JSON array")
	sessionsCmd.Flags().StringVar(&sessionsHarness, "harness", "", "Only this harness's sessions: claude, agy, or kimi")
	sessionsCmd.Flags().BoolVar(&sessionsWorkgroup, "workgroup", false, "Only sessions a workgroup agent owns")
	sessionsCmd.Flags().BoolVar(&sessionsLocal, "local", false, "Only sessions no workgroup agent owns")
}

func runSessions(cmd *cobra.Command, args []string) error {
	if sessionsWorkgroup && sessionsLocal {
		return fmt.Errorf("--workgroup and --local each leave out what the other keeps: use one (or neither, for all)")
	}
	if sessionsHarness != "" && !slices.Contains(discovery.Harnesses, sessionsHarness) {
		return fmt.Errorf("--harness %s: not a harness agent sessions reads; use %s", sessionsHarness, strings.Join(discovery.Harnesses, ", "))
	}
	ctx := getContext()
	dirs := discovery.Dirs(discovery.Overrides{Claude: cfg.ClaudeProjectsDir, Agy: cfg.AgyBrainDir, Kimi: cfg.KimiSessionsDir})
	agents, err := workgroupAgents(ctx)
	if err != nil {
		return err
	}
	live := discovery.Live{Now: time.Now()}
	claudeErr := error(nil)
	if statuses, err := claudeSessions(ctx); err == nil {
		live.Claude = map[string]string{}
		for id, st := range statuses {
			live.Claude[id] = st.String()
		}
	} else {
		claudeErr = err
	}

	sessions := discovery.Resolve(discovery.Scan(dirs), agents, live)
	sessions = slices.DeleteFunc(sessions, func(s discovery.Session) bool {
		return (sessionsHarness != "" && s.Harness != sessionsHarness) ||
			(sessionsWorkgroup && s.Workgroup == nil) || (sessionsLocal && s.Workgroup != nil)
	})
	if claudeErr != nil && slices.ContainsFunc(sessions, func(s discovery.Session) bool { return s.Harness == discovery.HarnessClaude }) {
		hint("Claude sessions' status is unknown or their last report: %v", claudeErr)
	}
	if sessionsJSON {
		return printJSON(nonNilSessions(sessions))
	}
	home, _ := os.UserHomeDir()
	printSessions(cmd.OutOrStdout(), sessions, discovery.DescribeDirs(dirs, home))
	return nil
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
	history := map[string][]string{} // by store and name: one list per name
	var agents []discovery.Agent
	for _, a := range snap.Agents {
		key := a.Store + "\x00" + a.Name
		if _, ok := history[key]; !ok {
			for _, ref := range service.ReadTranscriptRefs(filepath.Join(a.Store, "logs", a.Name+".transcripts")) {
				history[key] = append(history[key], ref.Path)
			}
		}
		agents = append(agents, discovery.Agent{Name: a.Name, Parent: a.Parent, Store: a.Store, Host: a.Host,
			Backend: a.Backend, Live: a.Live, CreatedAt: a.CreatedAt, EndedAt: a.EndedAt, Status: a.Status,
			StatusAt: a.StatusAt, SessionID: a.SessionID, Transcript: a.Transcript, History: history[key]})
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
	format := "%-19s %-8s %-18s %-10s %-20s %s\n"
	fmt.Fprintf(out, "\n"+format, "WHEN", "HARNESS", "PROJECT", "STATUS", "WORKGROUP", "TITLE")
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
		fmt.Fprintf(out, format, when, s.Harness, orDash(s.Project), s.Status, group, title)
	}
	for _, s := range top {
		line(sessionWhen(s.Mtime, "2006-01-02 15:04"), s)
		kids := subs[s.Harness+"\x00"+s.SessionID]
		slices.Reverse(kids) // oldest first, the order they were started in
		for _, k := range kids {
			line("└ "+sessionWhen(k.Mtime, "01-02 15:04"), k)
		}
	}
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
