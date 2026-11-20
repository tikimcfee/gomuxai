package cli

import (
	"context"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/config"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/termd"
	"github.com/tikimcfee/gomuxai/internal/termdtest"
)

type fakeTermdAttacher struct {
	termd.Ops
	ns  string
	bin string
}

func (f fakeTermdAttacher) AttachCommand(session string) (string, []string, error) {
	return f.bin, []string{"g3d", "--ns", f.ns, "attach", "--take", session}, nil
}

func TestAttachLocal_ExecsG3DUnderTermd(t *testing.T) {
	setupCLI(t)
	forbidBus(t)
	calls := stubExec(t)

	prevMux := tmuxClient
	t.Cleanup(func() { tmuxClient = prevMux })
	tmuxClient = fakeTermdAttacher{
		Ops: prevMux,
		ns:  "agents",
		bin: "/opt/g3d/bin/g3d",
	}

	oldCmd, oldStart := runCmd, runStart
	t.Cleanup(func() { runCmd, runStart = oldCmd, oldStart })
	runCmd, runStart, runRecorded = attachCmd, time.Now(), false

	if err := runAttach(attachCmd, []string{"worker"}); err != nil {
		t.Fatalf("runAttach: %v", err)
	}
	want := []execCall{{"/opt/g3d/bin/g3d", []string{"g3d", "--ns", "agents", "attach", "--take", "rpc-cli-worker"}}}
	if !slices.EqualFunc(*calls, want, sameCall) {
		t.Errorf("exec = %+v, want %+v", *calls, want)
	}
	rows, err := store.ListCommands(context.Background(), storage.CommandFilter{})
	if err != nil {
		t.Fatal(err)
	}
	if !runRecorded || len(rows) == 0 {
		t.Errorf("recorded = %v, rows = %d; want command recorded before exec", runRecorded, len(rows))
	}
}

func TestSetup_RefusesReservedTermdNamespace(t *testing.T) {
	oldCfg := cfg
	t.Cleanup(func() { cfg = oldCfg })
	for _, reserved := range []string{"default", "wall"} {
		t.Setenv("GOMUXAI_TERMD_NS", reserved)
		for _, cmd := range []*cobra.Command{listCmd, adoptCmd} {
			err := setup(cmd, nil)
			if err == nil || !strings.Contains(err.Error(), "reserved") {
				t.Fatalf("setup(%s) with GOMUXAI_TERMD_NS=%s = %v, want reserved refusal", cmd.Name(), reserved, err)
			}
		}
	}
}

func TestTermdLogsAndExportDeps(t *testing.T) {
	termdtest.Require(t)
	tc, err := termd.NewClient(termd.Options{Namespace: "agents-cli"})
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	t.Cleanup(func() { _ = tc.StopDaemon() })

	setupLogs(t)
	prevMux := tmuxClient
	prevSvc := agentService
	t.Cleanup(func() {
		tmuxClient = prevMux
		agentService = prevSvc
	})

	// Point another store directory on PATH to verify exportDeps().SessionStore.
	otherBinDir := filepath.Join(t.TempDir(), "bin")
	otherDataDir := filepath.Join(otherBinDir, config.StorageDirName)
	if err := os.MkdirAll(otherDataDir, 0o700); err != nil {
		t.Fatal(err)
	}
	otherDB, err := storage.NewSQLiteStorage(filepath.Join(otherDataDir, "gomuxai.db"))
	if err != nil {
		t.Fatal(err)
	}
	otherDB.Close()

	cfg.TermdNamespace = "agents-cli"
	cfg.StartupDelay = 0
	cfg.SubmitWait = 0
	cfg.AgentBinary = filepath.Join(otherBinDir, "agent")
	tmuxClient = tc
	agentService = service.NewAgentService(cfg, store, tc, backend.NewRegistry())

	ctx := context.Background()
	a, err := agentService.Spawn(ctx, service.SpawnOptions{
		Name:    "live-log",
		Backend: "shell",
	})
	if err != nil {
		t.Fatalf("Spawn: %v", err)
	}
	if _, err := agentService.Send(ctx, "live-log", "echo cli-log-$((40+2))", service.SendOptions{}); err != nil {
		t.Fatalf("Send: %v", err)
	}
	await.Until(t, "cli-log-42 on screen", func() (string, bool) {
		s, _ := agentService.Capture(ctx, "live-log", 20, false)
		return s, strings.Contains(s, "cli-log-42")
	})

	// Live logs --terminal reads directly from termd scrollback with no pipe-pane file.
	logsTerminal = true
	out, err := stdout(t, func() error { return runLogs(logsCmd, []string{"live-log"}) })
	if err != nil || !strings.Contains(out, "cli-log-42") {
		t.Fatalf("live logs --terminal = %q, %v", out, err)
	}

	// exportDeps lists live termd sessions, carries the configured namespace, and resolves SessionStore via op "env".
	deps := exportDeps()
	if deps.Namespace != "agents-cli" {
		t.Errorf("exportDeps.Namespace = %q, want agents-cli", deps.Namespace)
	}
	names, err := deps.Sessions()
	if err != nil || !slices.Contains(names, a.SessionName) {
		t.Errorf("exportDeps.Sessions() = %v, %v; want %s", names, err, a.SessionName)
	}
	realOther, _ := filepath.EvalSymlinks(otherDataDir)
	if got := deps.SessionStore(a.SessionName); got != realOther {
		t.Errorf("exportDeps.SessionStore(%q) = %q, want %q", a.SessionName, got, realOther)
	}

	// After Cleanup, logs --terminal still reads the saved scrollback snapshot.
	if err := agentService.Cleanup(ctx, "live-log"); err != nil {
		t.Fatalf("Cleanup: %v", err)
	}
	outAfter, err := stdout(t, func() error { return runLogs(logsCmd, []string{"live-log"}) })
	if err != nil || !strings.Contains(outAfter, "cli-log-42") {
		t.Errorf("post-cleanup logs --terminal = %q, %v", outAfter, err)
	}
}
