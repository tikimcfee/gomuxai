package cli

import (
	"context"
	"errors"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// setupStore points cfg and store at a temp database, on host "local". Unlike
// setupCLI, it needs no tmux.
func setupStore(t *testing.T) {
	t.Helper()
	dir := t.TempDir()
	testCfg := &config.Config{DatabasePath: filepath.Join(dir, "gomuxai.db"), Host: "local"}
	testStore, err := storage.NewSQLiteStorage(testCfg.DatabasePath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { testStore.Close() })
	oldCfg, oldStore := cfg, store
	t.Cleanup(func() { cfg, store = oldCfg, oldStore })
	cfg, store = testCfg, testStore
}

func commands(t *testing.T) []*storage.CommandRecord {
	t.Helper()
	recs, err := store.ListCommands(context.Background(), storage.CommandFilter{})
	if err != nil {
		t.Fatal(err)
	}
	return recs
}

func TestRecordCommand(t *testing.T) {
	setupStore(t)
	t.Setenv("AGENT_NAME", "lead")
	long := strings.Repeat("x", 400)
	runErr := &exitError{code: 2, err: errors.New("timed out\nmore")}
	recordCommand(sendCmd, []string{"send", "helper", long}, time.Now(), runErr)

	recs := commands(t)
	if len(recs) != 1 {
		t.Fatalf("recorded %d commands, want 1", len(recs))
	}
	r := recs[0]
	if r.Agent != "lead" || r.Host != "local" || r.ExitCode != 2 || r.Error != "timed out" || r.Build == "" || r.Cwd == "" {
		t.Errorf("recorded %+v", r)
	}
	if want := strings.Repeat("x", 300) + "…[+100 bytes]"; len(r.Args) != 3 || r.Args[2] != want {
		t.Errorf("args = %q, want the long one capped", r.Args)
	}
}

func TestRecordCommand_SkipsHooksAndTheBusDaemon(t *testing.T) {
	setupStore(t)
	t.Cleanup(func() { reportHook = false })
	reportHook = true
	recordCommand(reportCmd, []string{"report", "--hook", "--as", "x"}, time.Now(), nil)
	recordCommand(busRunCmd, []string{"bus", "run"}, time.Now(), nil)
	reportHook = false
	recordCommand(reportCmd, []string{"report", "idle"}, time.Now(), nil)

	recs := commands(t)
	if len(recs) != 1 || !slices.Equal(recs[0].Args, []string{"report", "idle"}) {
		t.Errorf("recorded %d commands, want only the plain report", len(recs))
	}
}

// Help and commands that fail in setup never open the store.
func TestRecordCommand_OpensTheStoreItself(t *testing.T) {
	setupStore(t)
	opened := store
	store = nil
	recordCommand(helpCommand(), []string{"help", "wait"}, time.Now(), nil)
	store = opened
	if recs := commands(t); len(recs) != 1 || !slices.Equal(recs[0].Args, []string{"help", "wait"}) {
		t.Errorf("recorded %d commands, want help", len(recs))
	}

	cfg.DatabasePath = filepath.Join(t.TempDir(), "missing", "gomuxai.db")
	store = nil
	recordCommand(listCmd, []string{"list"}, time.Now(), nil) // must not panic or print
	store = opened
}

func helpCommand() *cobra.Command {
	cmd, _, _ := rootCmd.Find([]string{"help"})
	return cmd
}

func TestWaitTimeout_ReturnsExitCode(t *testing.T) {
	setupCLI(t)
	busConnect = func() (*nats.Conn, error) { return nil, natsbus.ErrNoBus }
	t.Cleanup(func() { waitFor, waitTimeout, waitInterval = "", 0, 0 })
	waitFor, waitTimeout, waitInterval = "idle", 50*time.Millisecond, 10*time.Millisecond

	err := runWait(waitCmd, []string{"worker"})
	if ExitCode(err) != waitTimeoutExitCode {
		t.Fatalf("err = %v, exit code %d; want %d", err, ExitCode(err), waitTimeoutExitCode)
	}
	if want := "timed out after 50ms waiting for worker to be idle (last status: waiting)"; err.Error() != want {
		t.Errorf("err = %q, want %q", err, want)
	}
}

func TestFormatHistory(t *testing.T) {
	at := time.Date(2026, 10, 24, 14, 3, 11, 0, time.Local)
	recs := []*storage.CommandRecord{
		{StartedAt: at, Args: []string{"list"}},
		{StartedAt: at, Agent: "lead", Args: []string{"send", "helper", "review auth.go"}},
		{StartedAt: at, Agent: "lead", Args: []string{"wait", "helper", "--for", "idel"}, ExitCode: 1, Error: `invalid status "idel"`},
		{StartedAt: at, Args: []string{"send", "helper", strings.Repeat("y", 200)}, ExitCode: 1, Error: strings.Repeat("e", 200)},
	}
	want := `10-24 14:03:11 -      0 list
10-24 14:03:11 lead   0 send helper 'review auth.go'
10-24 14:03:11 lead   1 wait helper --for idel  # invalid status "idel"
10-24 14:03:11 -      1 send helper ` + strings.Repeat("y", 87) + `…  # ` + strings.Repeat("e", 79) + `…
`
	if got := formatHistory(recs); got != want {
		t.Errorf("formatHistory =\n%s\nwant:\n%s", got, want)
	}
}
