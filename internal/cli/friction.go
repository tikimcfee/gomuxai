package cli

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/buildinfo"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// frictionKinds are the kinds an entry can have; the first is the default
// for none given.
var frictionKinds = []string{"other", "help", "error", "missing", "bug", "docs"}

// frictionStatuses in lifecycle order; export groups by them.
var frictionStatuses = []string{
	storage.FrictionOpen, storage.FrictionAccepted, storage.FrictionResolved,
	storage.FrictionWontfix, storage.FrictionDuplicate,
}

// frictionSimilarExitCode is add's exit code when it doesn't file because
// the summary looks like an open entry's.
const frictionSimilarExitCode = 3

// frictionAttachWindow is how far back add and +1 look for the caller's
// failed command.
const frictionAttachWindow = 15 * time.Minute

var frictionCmd = &cobra.Command{
	Use:   "friction",
	Short: "File what got in your way using agent, and read what others filed",
	Long: `Friction is this host's box of what got in the way of using agent. If agent
confuses you, gets in your way, or lacks something you need, file it, then
carry on with your work. The people working on the tool fix what's filed
and resolve the entries, so resolved entries record what changed and who
asked.

Kinds (--kind): help (a command's help confused you or left something out),
error (an error that didn't say what went wrong or what to do), missing (a
feature you needed), bug, docs (README, briefing), other (the default).

Lifecycle: open -> accepted (someone means to fix it) -> resolved (fixed;
the note says how), wontfix, or duplicate (its votes count toward the
original).

Check "agent friction list" first, and +1 an entry that's already there.
add won't file a summary close to an open or accepted entry's: it lists
the matches and exits 3, and --new files it anyway. add and +1 attach your
last failed agent command from the past 15 minutes; --no-command skips it.`,
	Example: `  agent friction list
  agent friction add "wait --timeout doesn't say what status it last saw" --kind error
  agent friction +1 12 "hit it again with --next"
  agent friction show 12
  agent friction resolve 12 --note "wait names the last status (abc1234)"`,
	RunE: runGroup,
}

var (
	frictionAddKind    string
	frictionKind       string // list's and export's filter
	frictionDetail     string
	frictionDetailFile string
	frictionNew        bool
	frictionNoCommand  bool
	frictionNote       string
	frictionWontfix    bool
	frictionDup        string
	frictionStatus     string
	frictionBy         string
	frictionJSON       bool
)

var frictionAddCmd = &cobra.Command{
	Use:   `add "summary"`,
	Short: "File a friction entry",
	Long: `Add files a friction entry: a one-line summary of what got in your way,
optionally with a kind and more detail.

If your last failed agent command (same AGENT_NAME, or for a person, same
directory) ran in the past 15 minutes, it's copied into the entry, and add
says so; --no-command skips it.

If the summary is close to an open or accepted entry's, add doesn't file:
it lists the matches and exits 3. "agent friction +1 ID" adds your vote to
one of them; --new files yours anyway.`,
	Example: `  agent friction add "send's help doesn't say the message is typed into the terminal" --kind help
  agent friction add "no way to wait for several agents at once" --kind missing --detail "..."
  agent friction add "status says unknown for aider agents" --kind bug --detail-file notes.md --new`,
	Args: cobra.ExactArgs(1),
	RunE: runFrictionAdd,
}

var frictionVoteCmd = &cobra.Command{
	Use:     `+1 <id> ["note"]`,
	Aliases: []string{"vote"},
	Short:   "Add your vote to a friction entry",
	Long: `+1 records that an entry got in your way too, with an optional note, and
your last failed agent command from the past 15 minutes (--no-command skips
it). A vote on a duplicate goes to its original.`,
	Example: `  agent friction +1 12
  agent friction +1 12 "same with --next"`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runFrictionVote,
}

var frictionListCmd = &cobra.Command{
	Use:   "list",
	Short: "List friction entries",
	Long: `List shows friction entries, one line each: id, status (unless open), kind,
votes, age, who filed it, and the summary. Most votes first, then newest.
By default it lists open and accepted entries.

--status takes open, accepted, resolved, wontfix, duplicate, or all, or
several separated by commas.`,
	Example: `  agent friction list
  agent friction list --status all --kind help
  agent friction list --by lead --json`,
	Args: cobra.NoArgs,
	RunE: runFrictionList,
}

var frictionShowCmd = &cobra.Command{
	Use:   "show <id>",
	Short: "Show everything about a friction entry",
	Long: `Show prints a friction entry in full: who filed it, where, and with which
build, its detail, attached command, votes, duplicates, and resolution.`,
	Example: `  agent friction show 12
  agent friction show 12 --json`,
	Args: cobra.ExactArgs(1),
	RunE: runFrictionShow,
}

var frictionAcceptCmd = &cobra.Command{
	Use:   "accept <id>",
	Short: "Mark an open friction entry as accepted",
	Long: `Accept marks an open entry as accepted: someone means to fix it. It records
who accepted it, and an optional note.`,
	Example: `  agent friction accept 12 --note "will fold into the wait rework"`,
	Args:    cobra.ExactArgs(1),
	RunE:    runFrictionAccept,
}

var frictionResolveCmd = &cobra.Command{
	Use:   "resolve <id> (--note TEXT | --wontfix --note TEXT | --dup OTHER_ID)",
	Short: "Close a friction entry: fixed, won't fix, or duplicate",
	Long: `Resolve closes an open or accepted entry, recording who closed it:

  --note TEXT              fixed; the note says how (a commit, a command)
  --wontfix --note TEXT    not going to change; the note says why
  --dup OTHER_ID           the same as another entry, whose votes then
                           include this one's`,
	Example: `  agent friction resolve 12 --note "wait names the last status (abc1234)"
  agent friction resolve 13 --wontfix --note "keys is raw by design; use send"
  agent friction resolve 14 --dup 12`,
	Args: cobra.ExactArgs(1),
	RunE: runFrictionResolve,
}

var frictionExportCmd = &cobra.Command{
	Use:   "export",
	Short: "Write the friction box as Markdown",
	Long: `Export writes friction entries as Markdown to stdout, one section per
entry, grouped by status. The resolved section is the tool's record of what
changed and who asked. By default it writes every entry; --status as for
list.`,
	Example: `  agent friction export
  agent friction export --status resolved`,
	Args: cobra.NoArgs,
	RunE: runFrictionExport,
}

func init() {
	frictionAddCmd.Flags().StringVar(&frictionAddKind, "kind", "other", "Kind: "+strings.Join(frictionKinds, ", "))
	frictionAddCmd.Flags().StringVar(&frictionDetail, "detail", "", "More detail")
	frictionAddCmd.Flags().StringVar(&frictionDetailFile, "detail-file", "", `Read the detail from a file ("-" for stdin)`)
	frictionAddCmd.Flags().BoolVar(&frictionNew, "new", false, "File even if it looks like an open entry")
	frictionAddCmd.MarkFlagsMutuallyExclusive("detail", "detail-file")
	for _, c := range []*cobra.Command{frictionAddCmd, frictionVoteCmd} {
		c.Flags().BoolVar(&frictionNoCommand, "no-command", false, "Don't attach your last failed command")
	}
	frictionAcceptCmd.Flags().StringVar(&frictionNote, "note", "", "Why, or what the plan is")
	frictionResolveCmd.Flags().StringVar(&frictionNote, "note", "", "How it was fixed, or why it won't be")
	frictionResolveCmd.Flags().BoolVar(&frictionWontfix, "wontfix", false, "Close it as won't fix")
	frictionResolveCmd.Flags().StringVar(&frictionDup, "dup", "", "Close it as a duplicate of this entry")
	frictionResolveCmd.MarkFlagsMutuallyExclusive("wontfix", "dup")
	for _, c := range []*cobra.Command{frictionListCmd, frictionExportCmd} {
		c.Flags().StringVar(&frictionStatus, "status", "", "open, accepted, resolved, wontfix, duplicate, all; comma-separated (default: list open,accepted; export all)")
		c.Flags().StringVar(&frictionKind, "kind", "", "Only this kind")
		c.Flags().StringVar(&frictionBy, "by", "", `Only entries filed by this agent ("user" for people)`)
	}
	for _, c := range []*cobra.Command{frictionListCmd, frictionShowCmd} {
		c.Flags().BoolVar(&frictionJSON, "json", false, "Print as JSON")
	}

	frictionCmd.AddCommand(frictionAddCmd, frictionVoteCmd, frictionListCmd, frictionShowCmd,
		frictionAcceptCmd, frictionResolveCmd, frictionExportCmd)
}

// frictionCaller returns who is running the command, as friction records
// it: the agent's name, or "user" for a person; and the agent's recorded
// parent.
func frictionCaller(ctx context.Context) (who, parent string) {
	name := os.Getenv("AGENT_NAME")
	if name == "" {
		return "user", ""
	}
	if a, err := store.GetAgent(ctx, name); err == nil && a != nil {
		parent = a.SpawnedBy
	}
	return name, parent
}

// newFrictionUID returns an id unique across hosts, so boxes can be merged.
func newFrictionUID() string {
	b := make([]byte, 6)
	rand.Read(b)
	return cfg.Host + "-" + hex.EncodeToString(b)
}

// lastFailedCommand returns the caller's newest failed agent command from
// the past frictionAttachWindow (by AGENT_NAME, or for a person, by
// directory), or nil. Friction's own commands don't count: add exits
// non-zero when it doesn't file.
func lastFailedCommand(ctx context.Context) *storage.AttachedCommand {
	if frictionNoCommand {
		return nil
	}
	f := storage.CommandFilter{
		ByAgent: true,
		Agent:   os.Getenv("AGENT_NAME"),
		Failed:  true,
		Since:   time.Now().Add(-frictionAttachWindow),
		Limit:   20,
	}
	if f.Agent == "" {
		f.Cwd, _ = os.Getwd()
	}
	recs, err := store.ListCommands(ctx, f)
	if err != nil {
		return nil
	}
	for _, r := range recs {
		if cmd, _, err := rootCmd.Find(r.Args); err == nil && isFrictionCommand(cmd) {
			continue
		}
		return &storage.AttachedCommand{Args: r.Args, ExitCode: r.ExitCode, Error: r.Error, At: r.StartedAt}
	}
	return nil
}

func isFrictionCommand(cmd *cobra.Command) bool {
	for c := cmd; c != nil; c = c.Parent() {
		if c == frictionCmd {
			return true
		}
	}
	return false
}

// describeAttached says what was attached, for add's and +1's line, e.g.
// ", with your failed command from 3m ago: agent wait x --for idel (exit 1)".
func describeAttached(c *storage.AttachedCommand) string {
	if c == nil {
		return ""
	}
	when := "just now"
	if a := age(time.Since(c.At)); a != "now" {
		when = a + " ago"
	}
	return fmt.Sprintf(", with your failed command from %s: %s (exit %d)",
		when, clip("agent "+util.ShellJoin(c.Args), 80), c.ExitCode)
}

func runFrictionAdd(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	summary := strings.TrimSpace(args[0])
	if summary == "" {
		return fmt.Errorf("the summary is empty: say in one line what got in your way")
	}
	if strings.ContainsAny(summary, "\r\n") {
		return fmt.Errorf("the summary must be one line; put the rest in --detail or --detail-file")
	}
	if !slices.Contains(frictionKinds, frictionAddKind) {
		return unknownChoice("kind", frictionAddKind, frictionKinds)
	}
	detail := frictionDetail
	if frictionDetailFile != "" {
		var data []byte
		var err error
		if frictionDetailFile == "-" {
			data, err = io.ReadAll(os.Stdin)
		} else {
			data, err = os.ReadFile(frictionDetailFile)
		}
		if err != nil {
			return fmt.Errorf("reading --detail-file: %w", err)
		}
		detail = string(data)
	}
	detail = strings.TrimSpace(detail)

	if !frictionNew {
		open, err := store.ListFriction(ctx, storage.FrictionFilter{
			Statuses: []string{storage.FrictionOpen, storage.FrictionAccepted},
		})
		if err != nil {
			return err
		}
		if similar := similarFriction(summary, open); len(similar) > 0 {
			out := cmd.OutOrStdout()
			fmt.Fprintln(out, "Similar entries:")
			for _, f := range similar {
				fmt.Fprintf(out, "  #%d %s +%d %s\n", f.ID, f.Kind, f.Votes, clip(f.Summary, 100))
			}
			return &exitError{code: frictionSimilarExitCode, err: fmt.Errorf(
				"not filed: it looks like #%d; if it's the same, agent friction +1 %d \"your note\"; if not, re-run with --new",
				similar[0].ID, similar[0].ID)}
		}
	}

	who, parent := frictionCaller(ctx)
	cwd, _ := os.Getwd()
	now := time.Now()
	f := &storage.Friction{
		UID:       newFrictionUID(),
		Host:      cfg.Host,
		FiledBy:   who,
		Parent:    parent,
		Cwd:       cwd,
		Build:     buildinfo.ID(),
		CreatedAt: now,
		UpdatedAt: now,
		Kind:      frictionAddKind,
		Summary:   summary,
		Detail:    detail,
		Command:   lastFailedCommand(ctx),
		Status:    storage.FrictionOpen,
	}
	if err := store.AddFriction(ctx, f); err != nil {
		return err
	}
	fmt.Fprintf(cmd.OutOrStdout(), "Filed friction #%d (%s)%s.\n", f.ID, f.Kind, describeAttached(f.Command))
	return nil
}

func runFrictionVote(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	f, err := getFriction(ctx, args[0])
	if err != nil {
		return err
	}
	var dupNote string
	if f.Status == storage.FrictionDuplicate {
		dupNote = fmt.Sprintf("; #%d is a duplicate of it", f.ID)
		if f, err = getFriction(ctx, strconv.FormatInt(f.DuplicateOf, 10)); err != nil {
			return err
		}
	}
	if f.Status == storage.FrictionResolved || f.Status == storage.FrictionWontfix {
		return fmt.Errorf("friction #%d is closed as %s by %s (%s)%s; if it still gets in your way, file it again with agent friction add",
			f.ID, f.Status, f.ResolvedBy, clip(f.Resolution, 80), dupNote)
	}

	who, parent := frictionCaller(ctx)
	v := &storage.FrictionVote{
		FrictionID: f.ID,
		UID:        newFrictionUID(),
		Host:       cfg.Host,
		Voter:      who,
		Parent:     parent,
		Build:      buildinfo.ID(),
		CreatedAt:  time.Now(),
		Command:    lastFailedCommand(ctx),
	}
	if len(args) > 1 {
		v.Note = strings.TrimSpace(args[1])
	}
	if err := store.AddFrictionVote(ctx, v); err != nil {
		return err
	}
	fmt.Fprintf(cmd.OutOrStdout(), "Voted on friction #%d (%s, now +%d%s)%s.\n",
		f.ID, f.Kind, f.Votes+1, dupNote, describeAttached(v.Command))
	return nil
}

func runFrictionList(cmd *cobra.Command, args []string) error {
	entries, err := listFriction([]string{storage.FrictionOpen, storage.FrictionAccepted})
	if err != nil {
		return err
	}
	out := cmd.OutOrStdout()
	if frictionJSON {
		if entries == nil {
			entries = []*storage.Friction{}
		}
		return json.NewEncoder(out).Encode(entries)
	}
	if len(entries) == 0 {
		all, err := store.ListFriction(getContext(), storage.FrictionFilter{})
		if err != nil {
			return err
		}
		if len(all) == 0 {
			fmt.Fprintf(out, "The friction box on host %s is empty.\n", cfg.Host)
			hint(`if agent gets in your way, file it: agent friction add "what happened" --kind help|error|missing|bug|docs`)
			return nil
		}
		in := "1 entry in all"
		if len(all) != 1 {
			in = fmt.Sprintf("%d entries in all", len(all))
		}
		fmt.Fprintf(out, "No %s (%s: agent friction list --status all).\n", describeFrictionFilter(), in)
		return nil
	}
	fmt.Fprint(out, formatFrictionList(entries, time.Now()))
	return nil
}

// describeFrictionFilter names the entries list's flags select, e.g. "open
// or accepted help entries filed by lead".
func describeFrictionFilter() string {
	statuses := "open or accepted"
	if frictionStatus != "" {
		statuses = util.OrList(strings.Split(frictionStatus, ","), nil)
	}
	s := statuses + " "
	if frictionKind != "" {
		s += frictionKind + " "
	}
	s += "entries"
	if frictionBy != "" {
		s += " filed by " + frictionBy
	}
	return s
}

// unknownChoice is the error for a flag value that isn't one of choices,
// suggesting close ones.
func unknownChoice(what, value string, choices []string) error {
	if similar := util.Similar(value, choices); len(similar) > 0 {
		return fmt.Errorf("unknown %s %q; did you mean %s? (want %s)", what, value, util.OrList(similar, nil), strings.Join(choices, ", "))
	}
	return fmt.Errorf("unknown %s %q (want %s)", what, value, strings.Join(choices, ", "))
}

// listFriction returns the entries the list and export flags select, with
// these statuses if --status isn't given.
func listFriction(defaultStatuses []string) ([]*storage.Friction, error) {
	statuses := defaultStatuses
	if frictionStatus != "" {
		var err error
		if statuses, err = parseFrictionStatuses(frictionStatus); err != nil {
			return nil, err
		}
	}
	if frictionKind != "" && !slices.Contains(frictionKinds, frictionKind) {
		return nil, unknownChoice("kind", frictionKind, frictionKinds)
	}
	return store.ListFriction(getContext(), storage.FrictionFilter{Statuses: statuses, Kind: frictionKind, FiledBy: frictionBy})
}

// parseFrictionStatuses reads a comma-separated --status; "all" is every
// status (nil).
func parseFrictionStatuses(s string) ([]string, error) {
	var statuses []string
	for _, part := range strings.Split(s, ",") {
		part = strings.TrimSpace(part)
		switch {
		case part == "":
		case part == "all":
			return nil, nil
		case slices.Contains(frictionStatuses, part):
			statuses = append(statuses, part)
		default:
			return nil, unknownChoice("status", part, append(slices.Clone(frictionStatuses), "all"))
		}
	}
	return statuses, nil
}

// formatFrictionList renders entries one per line under a header. The
// STATUS column only appears if some entry isn't open.
func formatFrictionList(entries []*storage.Friction, now time.Time) string {
	idW, byW, showStatus := len("ID"), len("BY"), false
	for _, f := range entries {
		idW = max(idW, len(fmt.Sprintf("#%d", f.ID)))
		byW = max(byW, len(f.FiledBy))
		showStatus = showStatus || f.Status != storage.FrictionOpen
	}
	var b strings.Builder
	row := func(id, status, kind, votes, age, by, summary string) {
		fmt.Fprintf(&b, "%-*s ", idW, id)
		if showStatus {
			fmt.Fprintf(&b, "%-9s ", status)
		}
		fmt.Fprintf(&b, "%-7s %5s %4s %-*s %s\n", kind, votes, age, byW, by, summary)
	}
	row("ID", "STATUS", "KIND", "VOTES", "AGE", "BY", "SUMMARY")
	for _, f := range entries {
		status := f.Status
		if status == storage.FrictionOpen {
			status = ""
		}
		row(fmt.Sprintf("#%d", f.ID), status, f.Kind, fmt.Sprintf("+%d", f.Votes),
			age(now.Sub(f.CreatedAt)), f.FiledBy, clip(f.Summary, 100))
	}
	return b.String()
}

// age renders a duration as a short age: "now", "5m", "3h", "12d".
func age(d time.Duration) string {
	switch {
	case d < time.Minute:
		return "now"
	case d < time.Hour:
		return fmt.Sprintf("%dm", int(d.Minutes()))
	case d < 48*time.Hour:
		return fmt.Sprintf("%dh", int(d.Hours()))
	}
	return fmt.Sprintf("%dd", int(d.Hours()/24))
}

// frictionDetails is everything show prints about an entry.
type frictionDetails struct {
	*storage.Friction
	VoteList   []*storage.FrictionVote `json:"vote_list"`
	Duplicates []*storage.Friction     `json:"duplicates"`
}

func runFrictionShow(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	f, err := getFriction(ctx, args[0])
	if err != nil {
		return err
	}
	d := frictionDetails{Friction: f, VoteList: []*storage.FrictionVote{}, Duplicates: []*storage.Friction{}}
	if votes, err := store.ListFrictionVotes(ctx, f.ID); err != nil {
		return err
	} else if votes != nil {
		d.VoteList = votes
	}
	if dups, err := store.ListFriction(ctx, storage.FrictionFilter{DuplicateOf: f.ID}); err != nil {
		return err
	} else if dups != nil {
		d.Duplicates = dups
	}
	if frictionJSON {
		return json.NewEncoder(cmd.OutOrStdout()).Encode(d)
	}
	fmt.Fprint(cmd.OutOrStdout(), formatFrictionShow(d))
	return nil
}

func formatFrictionShow(d frictionDetails) string {
	f := d.Friction
	var b strings.Builder
	fmt.Fprintf(&b, "#%d %s, %s, +%d: %s\n", f.ID, f.Kind, f.Status, f.Votes, f.Summary)
	fmt.Fprintf(&b, "Filed by %s%s on %s, %s, build %s, in %s\n",
		f.FiledBy, parentNote(f.Parent), f.Host, stamp(f.CreatedAt), f.Build, f.Cwd)
	if f.Command != nil {
		fmt.Fprintf(&b, "Command: %s\n", formatAttached(f.Command, nil))
	}
	if f.Detail != "" {
		fmt.Fprintf(&b, "\n%s\n\n", f.Detail)
	}
	if f.AcceptedBy != "" {
		fmt.Fprintf(&b, "Accepted by %s, %s%s\n", f.AcceptedBy, stamp(f.AcceptedAt), notePart(f.AcceptNote))
	}
	switch f.Status {
	case storage.FrictionResolved:
		fmt.Fprintf(&b, "Resolved by %s, %s%s\n", f.ResolvedBy, stamp(f.ResolvedAt), notePart(f.Resolution))
	case storage.FrictionWontfix:
		fmt.Fprintf(&b, "Won't fix, by %s, %s%s\n", f.ResolvedBy, stamp(f.ResolvedAt), notePart(f.Resolution))
	case storage.FrictionDuplicate:
		fmt.Fprintf(&b, "Duplicate of #%d, by %s, %s%s\n", f.DuplicateOf, f.ResolvedBy, stamp(f.ResolvedAt), notePart(f.Resolution))
	}
	if len(d.VoteList) > 0 {
		fmt.Fprintf(&b, "Votes:\n")
		for _, v := range d.VoteList {
			fmt.Fprintf(&b, "  %s %s%s, build %s%s\n", stamp(v.CreatedAt), v.Voter, parentNote(v.Parent), v.Build, notePart(v.Note))
			if v.Command != nil {
				fmt.Fprintf(&b, "    command: %s\n", formatAttached(v.Command, nil))
			}
		}
	}
	if len(d.Duplicates) > 0 {
		fmt.Fprintf(&b, "Duplicates:\n")
		for _, dup := range d.Duplicates {
			fmt.Fprintf(&b, "  #%d +%d by %s: %s\n", dup.ID, dup.Votes, dup.FiledBy, dup.Summary)
		}
	}
	return b.String()
}

func parentNote(parent string) string {
	if parent == "" {
		return ""
	}
	return fmt.Sprintf(" (helper of %s)", parent)
}

func notePart(note string) string {
	if note == "" {
		return ""
	}
	return ": " + note
}

func stamp(t time.Time) string {
	return t.Local().Format("2006-01-02 15:04")
}

// formatAttached renders an attached command, e.g.
// "agent wait x --for idel (exit 1, 2026-10-24 14:02): invalid status",
// with the command line wrapped in code (Markdown backticks), if given.
func formatAttached(c *storage.AttachedCommand, code func(string) string) string {
	line := "agent " + util.ShellJoin(c.Args)
	if code != nil {
		line = code(line)
	}
	s := fmt.Sprintf("%s (exit %d, %s)", line, c.ExitCode, stamp(c.At))
	if c.Error != "" {
		s += ": " + c.Error
	}
	return s
}

func runFrictionAccept(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	f, err := getFriction(ctx, args[0])
	if err != nil {
		return err
	}
	if f.Status != storage.FrictionOpen {
		return fmt.Errorf("friction #%d is already %s%s; accept takes open entries", f.ID, f.Status, byWhom(f))
	}
	now := time.Now()
	f.Status, f.UpdatedAt = storage.FrictionAccepted, now
	f.AcceptedBy, _ = frictionCaller(ctx)
	f.AcceptedAt, f.AcceptNote = now, strings.TrimSpace(frictionNote)
	if err := store.UpdateFriction(ctx, f); err != nil {
		return err
	}
	fmt.Fprintf(cmd.OutOrStdout(), "Accepted friction #%d (%s): %s\n", f.ID, f.Kind, clip(f.Summary, 100))
	return nil
}

// byWhom says who moved f to its status, e.g. " (by lead)", if anyone did.
func byWhom(f *storage.Friction) string {
	by := f.ResolvedBy
	if f.Status == storage.FrictionAccepted {
		by = f.AcceptedBy
	}
	if by == "" {
		return ""
	}
	return fmt.Sprintf(" (by %s)", by)
}

func runFrictionResolve(cmd *cobra.Command, args []string) error {
	ctx := getContext()
	note := strings.TrimSpace(frictionNote)
	if note == "" && frictionDup == "" {
		return fmt.Errorf("--note is required: say how it was fixed, or with --wontfix, why it won't be (or close it with --dup ID)")
	}
	f, err := getFriction(ctx, args[0])
	if err != nil {
		return err
	}
	if f.Status != storage.FrictionOpen && f.Status != storage.FrictionAccepted {
		return fmt.Errorf("friction #%d is already closed as %s%s", f.ID, f.Status, byWhom(f))
	}

	var original *storage.Friction
	if frictionDup != "" {
		if original, err = getFriction(ctx, frictionDup); err != nil {
			return err
		}
		// Duplicates stay one level deep: point at the original's original.
		if original.Status == storage.FrictionDuplicate {
			if original, err = getFriction(ctx, strconv.FormatInt(original.DuplicateOf, 10)); err != nil {
				return err
			}
		}
		if original.ID == f.ID {
			return fmt.Errorf("friction #%d can't be a duplicate of itself", f.ID)
		}
	}

	now := time.Now()
	f.UpdatedAt, f.ResolvedAt, f.Resolution = now, now, note
	f.ResolvedBy, _ = frictionCaller(ctx)
	switch {
	case original != nil:
		f.Status, f.DuplicateOf = storage.FrictionDuplicate, original.ID
	case frictionWontfix:
		f.Status = storage.FrictionWontfix
	default:
		f.Status = storage.FrictionResolved
	}
	if err := store.UpdateFriction(ctx, f); err != nil {
		return err
	}

	out := cmd.OutOrStdout()
	switch f.Status {
	case storage.FrictionDuplicate:
		votes := original.Votes
		if updated, err := store.GetFriction(ctx, original.ID); err == nil && updated != nil {
			votes = updated.Votes
		}
		fmt.Fprintf(out, "Closed friction #%d as a duplicate of #%d (now +%d).\n", f.ID, original.ID, votes)
	case storage.FrictionWontfix:
		fmt.Fprintf(out, "Closed friction #%d (%s) as wontfix.\n", f.ID, f.Kind)
	default:
		fmt.Fprintf(out, "Resolved friction #%d (%s).\n", f.ID, f.Kind)
	}
	return nil
}

// getFriction reads the entry an id argument ("12" or "#12") names, or
// says why it can't.
func getFriction(ctx context.Context, arg string) (*storage.Friction, error) {
	id, err := strconv.ParseInt(strings.TrimPrefix(arg, "#"), 10, 64)
	if err != nil || id < 1 {
		return nil, fmt.Errorf("%q isn't a friction id: ids are numbers, like 12 or #12 (agent friction list shows them)", arg)
	}
	f, err := store.GetFriction(ctx, id)
	if err != nil {
		return nil, err
	}
	if f == nil {
		last, err := store.LastFrictionID(ctx)
		if err != nil {
			return nil, err
		}
		if last == 0 {
			return nil, fmt.Errorf("no friction #%d: nothing has been filed on this host yet", id)
		}
		return nil, fmt.Errorf("no friction #%d (latest is #%d); agent friction list --status all lists them", id, last)
	}
	return f, nil
}

func runFrictionExport(cmd *cobra.Command, args []string) error {
	entries, err := listFriction(nil)
	if err != nil {
		return err
	}
	fmt.Fprint(cmd.OutOrStdout(), formatFrictionExport(entries, cfg.Host))
	return nil
}

// markdownCode wraps s in a code span, with a fence of more backticks than
// s has in a row.
func markdownCode(s string) string {
	fence := "`"
	for strings.Contains(s, fence) {
		fence += "`"
	}
	if strings.HasPrefix(s, "`") || strings.HasSuffix(s, "`") {
		s = " " + s + " "
	}
	return fence + s + fence
}

var frictionStatusHeadings = map[string]string{
	storage.FrictionOpen:      "Open",
	storage.FrictionAccepted:  "Accepted",
	storage.FrictionResolved:  "Resolved",
	storage.FrictionWontfix:   "Won't fix",
	storage.FrictionDuplicate: "Duplicates",
}

// formatFrictionExport renders entries as Markdown, a section per status
// in lifecycle order, keeping the entries' order within each.
func formatFrictionExport(entries []*storage.Friction, host string) string {
	var b strings.Builder
	fmt.Fprintf(&b, "# Friction on %s\n", host)
	if len(entries) == 0 {
		b.WriteString("\nNo entries.\n")
	}
	for _, status := range frictionStatuses {
		var group []*storage.Friction
		for _, f := range entries {
			if f.Status == status {
				group = append(group, f)
			}
		}
		if len(group) == 0 {
			continue
		}
		fmt.Fprintf(&b, "\n## %s\n", frictionStatusHeadings[status])
		for _, f := range group {
			fmt.Fprintf(&b, "\n### #%d %s\n\n", f.ID, f.Summary)
			fmt.Fprintf(&b, "%s, +%d. Filed by %s%s, %s, build %s.\n",
				f.Kind, f.Votes, f.FiledBy, parentNote(f.Parent), stamp(f.CreatedAt), f.Build)
			if f.AcceptedBy != "" {
				fmt.Fprintf(&b, "Accepted by %s, %s%s.\n", f.AcceptedBy, stamp(f.AcceptedAt), notePart(f.AcceptNote))
			}
			switch f.Status {
			case storage.FrictionResolved:
				fmt.Fprintf(&b, "Resolved by %s, %s%s.\n", f.ResolvedBy, stamp(f.ResolvedAt), notePart(f.Resolution))
			case storage.FrictionWontfix:
				fmt.Fprintf(&b, "Won't fix (%s, %s)%s.\n", f.ResolvedBy, stamp(f.ResolvedAt), notePart(f.Resolution))
			case storage.FrictionDuplicate:
				fmt.Fprintf(&b, "Duplicate of #%d (%s, %s)%s.\n", f.DuplicateOf, f.ResolvedBy, stamp(f.ResolvedAt), notePart(f.Resolution))
			}
			if f.Command != nil {
				fmt.Fprintf(&b, "\nCommand: %s\n", formatAttached(f.Command, markdownCode))
			}
			if f.Detail != "" {
				fmt.Fprintf(&b, "\n%s\n", f.Detail)
			}
		}
	}
	return b.String()
}
