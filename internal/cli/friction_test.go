package cli

import (
	"context"
	"os"
	"strings"
	"testing"
	"time"

	"github.com/spf13/pflag"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// runAgent runs "agent args..." against the test store, without setup, and
// returns its stdout. Flags go back to their defaults afterwards.
func runAgent(t *testing.T, args ...string) (string, error) {
	t.Helper()
	cmd, rest, err := rootCmd.Find(args)
	if err != nil {
		t.Fatal(err)
	}
	defer cmd.Flags().VisitAll(func(f *pflag.Flag) {
		f.Value.Set(f.DefValue)
		f.Changed = false
	})
	if err := cmd.ParseFlags(rest); err != nil {
		return "", err
	}
	if err := cmd.ValidateArgs(cmd.Flags().Args()); err != nil {
		return "", err
	}
	if err := cmd.ValidateFlagGroups(); err != nil {
		return "", err
	}
	var out strings.Builder
	cmd.SetOut(&out)
	defer cmd.SetOut(nil)
	err = cmd.RunE(cmd, cmd.Flags().Args())
	return out.String(), err
}

func mustRun(t *testing.T, args ...string) string {
	t.Helper()
	out, err := runAgent(t, args...)
	if err != nil {
		t.Fatalf("agent %s: %v", strings.Join(args, " "), err)
	}
	return out
}

func logFailure(t *testing.T, agent string, args ...string) {
	t.Helper()
	cwd := t.TempDir()
	if agent == "" {
		cwd = mustGetwd(t)
	}
	if err := store.LogCommand(context.Background(), &storage.CommandRecord{
		StartedAt: time.Now(), Agent: agent, Cwd: cwd, Args: args, ExitCode: 1, Error: "it failed",
	}); err != nil {
		t.Fatal(err)
	}
}

func mustGetwd(t *testing.T) string {
	t.Helper()
	wd, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	return wd
}

func TestSummarySimilarity(t *testing.T) {
	tests := []struct {
		a, b string
		same bool
	}{
		{"wait --next returns immediately when the agent is already idle",
			"agent wait --next returns at once if already idle", true},
		{"status says unknown for aider agents", "status is unknown for aider", true},
		{"wait --timeout doesn't say what status it last saw", "wait timeout doesn't say the last status it saw", true},
		// Different commands, same complaint.
		{"send's help doesn't say messages are queued", "queue's help doesn't say messages are queued", false},
		{"help for wait is confusing", "help for send is confusing", false},
		{"send help doesn't say messages are queued", "queue help doesn't say a watcher is needed", false},
		{"no way to wait for several agents at once", "wait --timeout exit code is undocumented", false},
		{"cleanup refuses my own helper", "cleanup error is unclear", false},
		{"bug", "bug", false}, // one word in common is not enough
	}
	for _, tt := range tests {
		score := summarySimilarity(summaryWords(tt.a), summaryWords(tt.b))
		if same := score >= similarMinScore; same != tt.same {
			t.Errorf("%q vs %q: score %.2f, want same = %v", tt.a, tt.b, score, tt.same)
		}
	}
}

func TestFrictionAdd(t *testing.T) {
	setupStore(t)
	t.Setenv("AGENT_NAME", "lead")

	out := mustRun(t, "friction", "add", "wait --timeout doesn't say what status it last saw", "--kind", "error", "--detail", " more ")
	if out != "Filed friction #1 (error).\n" {
		t.Errorf("add printed %q", out)
	}
	f, _ := store.GetFriction(context.Background(), 1)
	if f.FiledBy != "lead" || f.Kind != "error" || f.Detail != "more" || f.Status != "open" || f.Host != "local" ||
		f.Build == "" || !strings.HasPrefix(f.UID, "local-") || f.Command != nil {
		t.Errorf("filed %+v", f)
	}

	out, err := runAgent(t, "friction", "add", "wait timeout doesn't say the last status it saw")
	if ExitCode(err) != frictionSimilarExitCode || !strings.Contains(err.Error(), "agent friction +1 1") || !strings.Contains(err.Error(), "--new") {
		t.Errorf("similar summary: err = %v (exit %d), want exit %d naming +1 1 and --new", err, ExitCode(err), frictionSimilarExitCode)
	}
	if !strings.Contains(out, "#1 error +0 wait --timeout") {
		t.Errorf("similar summary: printed %q, want the match listed", out)
	}

	out = mustRun(t, "friction", "add", "wait timeout doesn't say the last status it saw", "--new")
	if out != "Filed friction #2 (other).\n" {
		t.Errorf("add --new printed %q", out)
	}

	for _, args := range [][]string{
		{"friction", "add", "  "},
		{"friction", "add", "two\nlines"},
		{"friction", "add", "x", "--kind", "nope"},
		{"friction", "add", "x", "--detail", "a", "--detail-file", "b"},
	} {
		if _, err := runAgent(t, args...); err == nil {
			t.Errorf("agent %q: no error", args)
		}
	}
	if last, _ := store.LastFrictionID(context.Background()); last != 2 {
		t.Errorf("refused adds filed entries: latest is #%d", last)
	}
}

func TestFrictionAttachesTheLastFailedCommand(t *testing.T) {
	setupStore(t)
	t.Setenv("AGENT_NAME", "lead")
	logFailure(t, "lead", "wait", "helper", "--for", "idel")
	logFailure(t, "other", "send", "x", "hi")             // someone else's
	logFailure(t, "lead", "friction", "add", "something") // friction's own refusal

	out := mustRun(t, "friction", "add", "wait --for takes no typos")
	if want := "Filed friction #1 (other), with your failed command from just now: agent wait helper --for idel (exit 1).\n"; out != want {
		t.Errorf("add printed %q, want %q", out, want)
	}
	f, _ := store.GetFriction(context.Background(), 1)
	if f.Command == nil || strings.Join(f.Command.Args, " ") != "wait helper --for idel" || f.Command.Error != "it failed" {
		t.Errorf("attached %+v", f.Command)
	}

	out = mustRun(t, "friction", "+1", "1", "--no-command")
	if out != "Voted on friction #1 (other, now +1).\n" {
		t.Errorf("+1 --no-command printed %q", out)
	}

	// A person's failures count from the same directory only.
	t.Setenv("AGENT_NAME", "")
	out = mustRun(t, "friction", "+1", "#1", "me too")
	if out != "Voted on friction #1 (other, now +2).\n" {
		t.Errorf("person's +1 printed %q, want nothing attached", out)
	}
	logFailure(t, "", "list", "--host", "nowhere")
	out = mustRun(t, "friction", "+1", "1")
	if !strings.Contains(out, "agent list --host nowhere (exit 1)") {
		t.Errorf("person's +1 printed %q, want their failure from this directory attached", out)
	}
	votes, _ := store.ListFrictionVotes(context.Background(), 1)
	if len(votes) != 3 || votes[1].Voter != "user" || votes[1].Note != "me too" {
		t.Errorf("votes = %+v", votes)
	}
}

func TestFrictionLifecycle(t *testing.T) {
	setupStore(t)
	t.Setenv("AGENT_NAME", "")
	mustRun(t, "friction", "add", "send's help doesn't say it types into the terminal", "--kind", "help")
	mustRun(t, "friction", "add", "no way to wait for several agents at once", "--kind", "missing")
	mustRun(t, "friction", "add", "wait for many agents", "--new")
	mustRun(t, "friction", "+1", "3")

	if out := mustRun(t, "friction", "accept", "1", "--note", "soon"); out != "Accepted friction #1 (help): send's help doesn't say it types into the terminal\n" {
		t.Errorf("accept printed %q", out)
	}
	if _, err := runAgent(t, "friction", "accept", "1"); err == nil || !strings.Contains(err.Error(), "already accepted (by user)") {
		t.Errorf("accepting twice: err = %v", err)
	}
	if out := mustRun(t, "friction", "resolve", "3", "--dup", "2"); out != "Closed friction #3 as a duplicate of #2 (now +2).\n" {
		t.Errorf("resolve --dup printed %q", out)
	}
	// A vote on the duplicate goes to the original.
	if out := mustRun(t, "friction", "+1", "3"); out != "Voted on friction #2 (missing, now +3; #3 is a duplicate of it).\n" {
		t.Errorf("+1 on a duplicate printed %q", out)
	}
	if _, err := runAgent(t, "friction", "resolve", "1"); err == nil || !strings.Contains(err.Error(), "--note is required") {
		t.Errorf("resolve without --note: err = %v", err)
	}
	if _, err := runAgent(t, "friction", "resolve", "2", "--dup", "3"); err == nil || !strings.Contains(err.Error(), "itself") {
		t.Errorf("resolve as a duplicate of its own duplicate: err = %v", err)
	}
	if out := mustRun(t, "friction", "resolve", "1", "--note", "send's help says so (abc1234)"); out != "Resolved friction #1 (help).\n" {
		t.Errorf("resolve printed %q", out)
	}
	if out := mustRun(t, "friction", "resolve", "2", "--wontfix", "--note", "use a loop"); out != "Closed friction #2 (missing) as wontfix.\n" {
		t.Errorf("resolve --wontfix printed %q", out)
	}
	if _, err := runAgent(t, "friction", "resolve", "1", "--note", "again"); err == nil || !strings.Contains(err.Error(), "already closed as resolved (by user)") {
		t.Errorf("resolving twice: err = %v", err)
	}
	if _, err := runAgent(t, "friction", "+1", "1"); err == nil || !strings.Contains(err.Error(), "closed as resolved") {
		t.Errorf("+1 on a resolved entry: err = %v", err)
	}

	if out := mustRun(t, "friction", "list"); out != "No friction entries match.\n" {
		t.Errorf("list with nothing open printed %q", out)
	}
	out := mustRun(t, "friction", "list", "--status", "all")
	if lines := strings.Split(strings.TrimSpace(out), "\n"); len(lines) != 4 || !strings.HasPrefix(lines[1], "#2 wontfix") {
		t.Errorf("list --status all printed:\n%s", out)
	}
	if _, err := runAgent(t, "friction", "list", "--status", "closed"); err == nil || !strings.Contains(err.Error(), `unknown status "closed"`) {
		t.Errorf("list --status closed: err = %v", err)
	}

	export := mustRun(t, "friction", "export")
	for _, want := range []string{"# Friction on local", "## Resolved\n\n### #1 send's help", "Resolved by user", ": send's help says so (abc1234).", "## Won't fix", "## Duplicates\n\n### #3"} {
		if !strings.Contains(export, want) {
			t.Errorf("export lacks %q:\n%s", want, export)
		}
	}
	if strings.Contains(export, "## Open") {
		t.Errorf("export has an empty section:\n%s", export)
	}

	show := mustRun(t, "friction", "show", "2")
	for _, want := range []string{"#2 missing, wontfix, +3: no way", "Won't fix, by user", "Votes:\n", "Duplicates:\n  #3 +1 by user: wait for many agents"} {
		if !strings.Contains(show, want) {
			t.Errorf("show lacks %q:\n%s", want, show)
		}
	}
}

func TestFrictionIDErrors(t *testing.T) {
	setupStore(t)
	if _, err := runAgent(t, "friction", "show", "1"); err == nil || !strings.Contains(err.Error(), "nothing has been filed") {
		t.Errorf("show on an empty box: err = %v", err)
	}
	mustRun(t, "friction", "add", "x y z")
	if _, err := runAgent(t, "friction", "show", "99"); err == nil || err.Error() != "no friction #99 (latest is #1); agent friction list --status all lists them" {
		t.Errorf("show 99: err = %v", err)
	}
	if _, err := runAgent(t, "friction", "show", "twelve"); err == nil || !strings.Contains(err.Error(), "ids are numbers") {
		t.Errorf("show twelve: err = %v", err)
	}
}

func TestFormatFrictionList(t *testing.T) {
	now := time.Date(2026, 10, 24, 14, 0, 0, 0, time.UTC)
	entries := []*storage.Friction{
		{ID: 12, Kind: "help", Votes: 3, CreatedAt: now.Add(-2 * time.Hour), FiledBy: "lead", Summary: "a", Status: "open"},
		{ID: 9, Kind: "missing", Votes: 0, CreatedAt: now.Add(-72 * time.Hour), FiledBy: "user", Summary: "b", Status: "open"},
	}
	want := `ID  KIND    VOTES  AGE BY   SUMMARY
#12 help       +3   2h lead a
#9  missing    +0   3d user b
`
	if got := formatFrictionList(entries, now); got != want {
		t.Errorf("all open:\n%s\nwant:\n%s", got, want)
	}
	entries[1].Status = "accepted"
	want = `ID  STATUS    KIND    VOTES  AGE BY   SUMMARY
#12           help       +3   2h lead a
#9  accepted  missing    +0   3d user b
`
	if got := formatFrictionList(entries, now); got != want {
		t.Errorf("one accepted:\n%s\nwant:\n%s", got, want)
	}
}

func TestMarkdownCode(t *testing.T) {
	for s, want := range map[string]string{
		"agent list":           "`agent list`",
		"agent send x 'a`b'":   "``agent send x 'a`b'``",
		"`x`":                  "`` `x` ``",
		"agent send x 'a``b`'": "```agent send x 'a``b`'```",
	} {
		if got := markdownCode(s); got != want {
			t.Errorf("markdownCode(%q) = %q, want %q", s, got, want)
		}
	}
}
