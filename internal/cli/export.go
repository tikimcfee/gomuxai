package cli

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/export"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/tmux"
)

var exportCmd = &cobra.Command{
	Use:   "export",
	Short: "Print the agent tree for a program to read, and follow its changes",
	Long: `Export prints every agent this binary's store records, with what a display
needs: parent, backend, tmux session and whether it exists, cwd, the last
reported status, and the CLI transcript's path, size, and time. --json
prints it as one JSON object; "agent list" is the view for a person.

--follow then prints one JSON line per change until Ctrl-C: spawned,
changed, and removed agents, read from the store's agent events (which
spawn, cleanup, and every status report record), plus tmux sessions and
transcripts checked every second. Each event line carries its id; --since
<id> resumes after it, without a snapshot, if the store still keeps the
events after it (the newest 20000). docs/ARCHITECTURE.md ("Export") has
every line's shape.

Status is the last report from the agent's hooks, not the resolved status
"agent status" shows: export runs no backend CLI. An interrupted
permission prompt stays "waiting", and a crashed CLI keeps its last status,
until the next hook.

Stores: an agent spawned with another agent binary (a worktree's
./bin/agent) is recorded in that binary's store. Export finds those stores
through the PATH of each agent-* tmux session no store records, and reads
them read-only; --store adds one. Sessions it still can't match are listed
as unrecorded.

--all adds departed agents (cleaned up), with when they ended. --hosts adds
other hosts' agents through the bus ("all" for every trusted peer), with
fewer fields: no store, cwd, or transcript. Following re-lists them every
30s.

Export only reads, and records one row in the command log per run however
long it follows.`,
	Example: `  agent export
  agent export --json
  agent export --json --follow
  agent export --json --follow --since 1234
  agent export --json --all --store ~/src/gomuxai/bin
  agent export --json --hosts all`,
	Args: cobra.NoArgs,
	RunE: runExport,
}

var (
	exportJSON   bool
	exportFollow bool
	exportAll    bool
	exportSince  int64
	exportStores []string
	exportHosts  string
)

// How often a follower reads agent events, checks sessions and
// transcripts, and re-lists other hosts. Variables for tests.
var (
	exportPollInterval    = 250 * time.Millisecond
	exportSessionInterval = time.Second
	exportHostInterval    = 30 * time.Second
)

func init() {
	exportCmd.Flags().BoolVar(&exportJSON, "json", false, "Print JSON (with --follow, one object per line)")
	exportCmd.Flags().BoolVar(&exportFollow, "follow", false, "After the snapshot, print each change until Ctrl-C")
	exportCmd.Flags().BoolVar(&exportAll, "all", false, "Include departed (cleaned up) agents")
	exportCmd.Flags().Int64Var(&exportSince, "since", 0, "With --follow: resume after this event id, without a snapshot")
	exportCmd.Flags().StringArrayVar(&exportStores, "store", nil, "Also read this store (a .gomuxai dir, the dir holding one, or an agent binary); repeatable")
	exportCmd.Flags().StringVar(&exportHosts, "hosts", "", `Also list these hosts' agents over the bus: "all" (every trusted peer) or host,host`)
}

func runExport(cmd *cobra.Command, args []string) error {
	if cmd.Flags().Changed("since") && !exportFollow {
		return fmt.Errorf("--since resumes a follow: use it with --follow (agent export --json --follow --since %d)", exportSince)
	}
	dirs := []string{filepath.Dir(cfg.DatabasePath)}
	for _, p := range exportStores {
		dir := dataDirFor(p)
		if dir == "" {
			return fmt.Errorf("--store %s: no agent store there (looked for %s and %s)", p,
				filepath.Join(p, "gomuxai.db"), filepath.Join(p, config.StorageDirName, "gomuxai.db"))
		}
		dirs = append(dirs, dir)
	}
	hosts, err := exportHostList(exportHosts)
	if err != nil {
		return err
	}

	tree := export.New(exportDeps(), dirs, hosts, exportAll)
	defer tree.Close()
	ctx := getContext()

	out := cmd.OutOrStdout()
	if !exportFollow {
		snap, err := tree.Snapshot(ctx)
		if err != nil {
			return err
		}
		if exportJSON {
			return printJSON(snap)
		}
		printExport(out, snap)
		return nil
	}

	emit := lineWriter(out)
	resumed := false
	if cmd.Flags().Changed("since") {
		if resumed, err = tree.Resume(ctx, exportSince); err != nil {
			return err
		}
		if !resumed {
			notice("Events after %d are no longer all kept (or not written yet): starting from a snapshot.", exportSince)
		}
	}
	if !resumed {
		snap, err := tree.Snapshot(ctx)
		if err != nil {
			return err
		}
		if !exportJSON {
			printExport(out, snap)
		} else if err := emit(snap); err != nil {
			return err
		}
	}
	notice("Following the agent tree (%s). Ctrl-C stops.", followScope(dirs, hosts))
	return followExport(ctx, tree, emit)
}

// followExport prints the tree's changes until interrupted.
func followExport(ctx context.Context, tree *export.Tree, emit func(any) error) error {
	interrupt := make(chan os.Signal, 1)
	signal.Notify(interrupt, os.Interrupt, syscall.SIGTERM)
	defer signal.Stop(interrupt)

	poll := time.NewTicker(exportPollInterval)
	defer poll.Stop()
	sessions := time.NewTicker(exportSessionInterval)
	defer sessions.Stop()
	hosts := time.NewTicker(exportHostInterval)
	defer hosts.Stop()

	for {
		var lines []any
		select {
		case <-interrupt:
			return nil
		case <-poll.C:
			var err error
			if lines, err = tree.Poll(ctx); err != nil {
				fmt.Fprintf(os.Stderr, "agent export: %v\n", err)
			}
		case <-sessions.C:
			lines = tree.PollSessions(ctx)
		case <-hosts.C:
			lines = tree.PollHosts(ctx)
		}
		for _, l := range lines {
			if err := emit(l); err != nil {
				return err
			}
		}
	}
}

// lineWriter returns how follow prints a line: compact JSON with --json,
// a line of text otherwise.
func lineWriter(out io.Writer) func(any) error {
	if exportJSON {
		enc := json.NewEncoder(out)
		enc.SetEscapeHTML(false)
		return func(v any) error { return enc.Encode(v) }
	}
	return func(v any) error {
		if s := formatExportLine(v); s != "" {
			_, err := fmt.Fprintln(out, s)
			return err
		}
		return nil
	}
}

func followScope(dirs, hosts []string) string {
	s := plural(len(dirs), "store")
	if len(dirs) == 1 {
		s = "this store"
	}
	s += " and the stores its sessions point at"
	if len(hosts) > 0 {
		s += ", and " + strings.Join(hosts, ", ")
	}
	return s
}

// exportDeps wires the tree to tmux, the bus, and this host.
func exportDeps() export.Deps {
	d := export.Deps{
		Host:          cfg.Host,
		SessionPrefix: cfg.SessionPrefix,
		ListHost: func(ctx context.Context, host string) ([]natsbus.AgentInfo, error) {
			var agents []natsbus.AgentInfo
			err := callRemote(host, func(c *natsbus.RPCClient) (err error) {
				agents, err = c.List(ctx, host)
				return err
			})
			return agents, err
		},
	}
	if c, ok := tmuxClient.(*tmux.Client); ok {
		d.Sessions = c.SessionNames
		d.SessionStore = func(session string) string {
			// spawn puts its binary's dir first on the session's PATH.
			first, _, _ := strings.Cut(c.SessionEnv(session, "PATH"), string(os.PathListSeparator))
			return dataDirFor(first)
		}
	} else if tmuxClient != nil {
		d.Sessions = func() ([]string, error) {
			sessions, err := tmuxClient.ListSessions()
			var names []string
			for _, s := range sessions {
				names = append(names, s.Name)
			}
			return names, err
		}
	}
	return d
}

// exportHostList reads --hosts: "all" is every trusted peer.
func exportHostList(flag string) ([]string, error) {
	if flag == "" {
		return nil, nil
	}
	var hosts []string
	if flag == "all" {
		peers, err := natsbus.LoadPeers(cfg.BusDir())
		if err != nil {
			return nil, fmt.Errorf("--hosts all: reading the trusted peers: %w", err)
		}
		for _, p := range peers {
			hosts = append(hosts, p.Host)
		}
		if len(hosts) == 0 {
			return nil, fmt.Errorf("--hosts all: this host trusts no peers (agent bus trust <host>.pub.json adds one), so there are no other hosts to list")
		}
	} else {
		hosts = strings.Split(flag, ",")
	}
	var out []string
	for _, h := range hosts {
		h = strings.TrimSpace(h)
		if h == cfg.Host || slices.Contains(out, h) {
			continue // this host's agents are in its store
		}
		if err := natsbus.ValidateHost(h); err != nil {
			return nil, fmt.Errorf("--hosts: %w", err)
		}
		out = append(out, h)
	}
	slices.Sort(out)
	return out, nil
}

// dataDirFor maps a data dir, the dir holding one, or an agent binary to
// its data dir, or "" if no store is there.
func dataDirFor(p string) string {
	if p == "" {
		return ""
	}
	if r, err := filepath.EvalSymlinks(p); err == nil {
		p = r
	}
	cands := []string{p, filepath.Join(p, config.StorageDirName)}
	if info, err := os.Stat(p); err == nil && !info.IsDir() {
		cands = []string{filepath.Join(filepath.Dir(p), config.StorageDirName)}
	}
	for _, c := range cands {
		if info, err := os.Stat(filepath.Join(c, "gomuxai.db")); err == nil && !info.IsDir() {
			return c
		}
	}
	return ""
}

// printExport prints the snapshot for a person.
func printExport(out io.Writer, snap *export.Snapshot) {
	for _, s := range snap.Stores {
		line := "Store " + s.Dir
		if s.This {
			line += " (this binary's)"
		}
		switch {
		case s.Error != "":
			line += ": unreadable: " + s.Error
		case !s.Events:
			line += ": written by an agent built before export, so --follow sees its changes only as whole-store updates"
		default:
			line += fmt.Sprintf(": events up to %d", s.Cursor)
		}
		fmt.Fprintln(out, line)
	}
	for _, h := range snap.Hosts {
		if h.Error != "" {
			fmt.Fprintf(out, "Host %s: %s\n", h.Host, h.Error)
		} else {
			fmt.Fprintf(out, "Host %s: %s\n", h.Host, plural(h.Agents, "agent"))
		}
	}
	if len(snap.Agents) == 0 {
		fmt.Fprintf(out, "No agents (agent spawn <name> --backend claude starts one).\n")
	} else {
		fmt.Fprintf(out, "\n%-24s %-11s %-18s %-24s %-19s %s\n", "NAME", "STATUS", "PARENT", "SESSION", "CREATED", "TRANSCRIPT")
		for _, a := range snap.Agents {
			name := a.Name
			if a.Host != snap.Host {
				name += "@" + a.Host
			}
			status, session, parent := a.Status, a.Session, a.Parent
			switch {
			case !a.Live:
				status = "departed"
			case status == "":
				status = "-"
			}
			if a.Attached != nil && !*a.Attached {
				session = "- (gone)"
			}
			if parent == "" {
				parent = "-"
			}
			fmt.Fprintf(out, "%-24s %-11s %-18s %-24s %-19s %s\n", name, status, parent, session,
				a.CreatedAt.Local().Format("2006-01-02 15:04:05"), transcriptSummary(a))
		}
	}
	for _, s := range snap.Unrecorded {
		where := "no store found for it"
		if s.Store != "" {
			where = "its binary's store " + s.Store + " doesn't record it"
		}
		fmt.Fprintf(out, "Unrecorded session %s: %s\n", s.Session, where)
	}
}

func transcriptSummary(a export.Agent) string {
	switch {
	case a.Transcript == "":
		return "-"
	case a.TranscriptMtime.IsZero():
		return "missing: " + a.Transcript
	}
	return fmt.Sprintf("%s, %s", byteSize(a.TranscriptSize), a.TranscriptMtime.Local().Format("15:04:05"))
}

func byteSize(n int64) string {
	switch {
	case n >= 1<<20:
		return fmt.Sprintf("%.1f MB", float64(n)/(1<<20))
	case n >= 1<<10:
		return fmt.Sprintf("%.1f KB", float64(n)/(1<<10))
	}
	return fmt.Sprintf("%d B", n)
}

// formatExportLine renders a follow line for a person; "" skips it
// (transcript growth, which would scroll by every second).
func formatExportLine(v any) string {
	switch l := v.(type) {
	case export.AgentLine:
		a := l.Agent
		name := a.Name + "@" + a.Host
		at := l.At.Local().Format("15:04:05.000")
		switch {
		case l.Type == export.LineSpawned:
			by := "a person"
			if a.Parent != "" {
				by = a.Parent
			}
			return fmt.Sprintf("%s spawned %s (%s, by %s, in %s)", at, name, a.Backend, by, a.Cwd)
		case l.Type == export.LineRemoved:
			return fmt.Sprintf("%s removed %s (last status %s)", at, name, orDash(a.Status))
		case l.Reason == export.ReasonSession:
			state := "gone"
			if a.Attached != nil && *a.Attached {
				state = "back"
			}
			return fmt.Sprintf("%s %s: session %s %s", at, name, a.Session, state)
		case l.Reason == export.ReasonStatus:
			line := fmt.Sprintf("%s %s %s", at, name, printable(a.Status))
			if a.Event != "" {
				line += " event=" + printable(a.Event)
			}
			if a.Detail != "" {
				line += " detail=" + strconv.Quote(service.OneLine(a.Detail))
			}
			return line
		}
	case export.StoreLine:
		return fmt.Sprintf("%s store %s: %s", time.Now().Format("15:04:05.000"), l.Store.Dir, plural(len(l.Agents), "agent"))
	case export.HostLine:
		if l.Host.Error != "" {
			return fmt.Sprintf("%s host %s: %s", time.Now().Format("15:04:05.000"), l.Host.Host, l.Host.Error)
		}
		return fmt.Sprintf("%s host %s: %s", time.Now().Format("15:04:05.000"), l.Host.Host, plural(len(l.Agents), "agent"))
	case export.UnrecordedLine:
		var names []string
		for _, s := range l.Sessions {
			names = append(names, s.Session)
		}
		if len(names) == 0 {
			return fmt.Sprintf("%s every agent session is recorded", time.Now().Format("15:04:05.000"))
		}
		return fmt.Sprintf("%s unrecorded sessions: %s", time.Now().Format("15:04:05.000"), strings.Join(names, ", "))
	}
	return ""
}

func orDash(s string) string {
	if s == "" {
		return "-"
	}
	return s
}
