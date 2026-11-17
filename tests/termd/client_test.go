package termd_test

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/termd"
	"github.com/tikimcfee/gomuxai/internal/termdtest"
)

func newTestClient(t *testing.T, ns string) *termd.Client {
	t.Helper()
	termdtest.Require(t)
	c, err := termd.NewClient(termd.Options{Namespace: ns})
	if err != nil {
		if strings.Contains(err.Error(), "no g3d-termd binary") {
			t.Skipf("g3d-termd not installed (%v)", err)
		}
		t.Fatalf("NewClient(%q): %v", ns, err)
	}
	t.Cleanup(func() { _ = c.StopDaemon() })
	return c
}

func uniqueSession(t *testing.T, prefix string) string {
	t.Helper()
	return fmt.Sprintf("%s-%d", prefix, time.Now().UnixNano()%1_000_000_000)
}

func waitLine(t *testing.T, c *termd.Client, session, want string) string {
	t.Helper()
	return await.Until(t, fmt.Sprintf("line %q on %s", want, session), func() (string, bool) {
		out, err := c.Capture(session, 50)
		if err != nil {
			return "capture error: " + err.Error(), false
		}
		for _, line := range strings.Split(out, "\n") {
			if strings.TrimSpace(line) == want {
				return out, true
			}
		}
		return out, false
	})
}

func newShell(t *testing.T, c *termd.Client, prefix string) string {
	t.Helper()
	sess := uniqueSession(t, prefix)
	if err := c.CreateSession(sess, nil); err != nil {
		t.Fatalf("CreateSession: %v", err)
	}
	t.Cleanup(func() { _ = c.KillSession(sess) })
	if err := c.SendKeys(sess, "echo ready-$((40+2))"); err != nil {
		t.Fatalf("SendKeys: %v", err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatalf("SendSpecialKey Enter: %v", err)
	}
	waitLine(t, c, sess, "ready-42")
	return sess
}

func TestNamespaceRefusesDefaultAndWall(t *testing.T) {
	termdtest.Require(t)
	for _, reserved := range []string{"default", "wall"} {
		if _, err := termd.NewClient(termd.Options{Namespace: reserved}); err == nil {
			t.Errorf("NewClient(%q) succeeded; must refuse reserved namespaces", reserved)
		}
	}
}

func TestSessionLifecycleStatusAndProcessTreeKill(t *testing.T) {
	c := newTestClient(t, "agents-life")

	// Empty before daemon starts: no error, zero sessions.
	sessions, err := c.ListSessions()
	if err != nil || len(sessions) != 0 {
		t.Fatalf("initial ListSessions = %v, %v; want empty", sessions, err)
	}

	dir := t.TempDir()
	sess := uniqueSession(t, "agent-w")
	env := map[string]string{
		"AGENT_NAME": "w",
		"CUSTOM_KEY": "alpha-beta",
	}
	if err := c.CreateSessionIn(sess, dir, env); err != nil {
		t.Fatalf("CreateSessionIn: %v", err)
	}

	// Duplicate creation must fail naming "already exists".
	if err := c.CreateSession(sess, nil); err == nil || !strings.Contains(err.Error(), "already exists") {
		t.Fatalf("duplicate CreateSession = %v, want already exists", err)
	}

	exists, err := c.SessionExists(sess)
	if err != nil || !exists {
		t.Fatalf("SessionExists = %v, %v; want true", exists, err)
	}

	// Status document reports daemon + terminal + pid.
	st, err := c.Status()
	if err != nil {
		t.Fatalf("Status: %v", err)
	}
	if st.Daemon.NS != "agents-life" || len(st.Terminals) != 1 || st.Terminals[0].ID != sess {
		t.Fatalf("Status = %+v", st)
	}

	pane, err := c.FirstPane(sess)
	if err != nil || pane.PID <= 0 {
		t.Fatalf("FirstPane = %+v, %v", pane, err)
	}
	pids, err := c.PanePIDs()
	if err != nil || pids[pane.PID] != sess {
		t.Fatalf("PanePIDs = %v, %v; want %d -> %s", pids, err, pane.PID, sess)
	}
	if got := c.SessionEnv(sess, "CUSTOM_KEY"); got != "alpha-beta" {
		t.Errorf("SessionEnv(CUSTOM_KEY) = %q, want alpha-beta", got)
	}

	// Verify env and cwd inside the running shell, and start a child process
	// so KillSession proves termd's process-tree close rule.
	pidFile := filepath.Join(dir, "child.pid")
	cmd := fmt.Sprintf(`echo "ENV=[$CUSTOM_KEY]"; pwd -P; sleep 300 & echo $! > %s; echo child-$((40+2))`, pidFile)
	if err := c.SendKeys(sess, cmd); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatal(err)
	}
	out := waitLine(t, c, sess, "child-42")
	if !strings.Contains(out, "ENV=[alpha-beta]") {
		t.Errorf("shell missing CUSTOM_KEY:\n%s", out)
	}
	realDir, _ := filepath.EvalSymlinks(dir)
	if !strings.Contains(out, realDir) {
		t.Errorf("shell cwd missing %q:\n%s", realDir, out)
	}

	rawPID, err := os.ReadFile(pidFile)
	if err != nil {
		t.Fatalf("reading child pid: %v", err)
	}
	var childPID int
	fmt.Sscanf(strings.TrimSpace(string(rawPID)), "%d", &childPID)
	if childPID <= 0 {
		t.Fatalf("invalid child pid %q", rawPID)
	}

	if err := c.KillSession(sess); err != nil {
		t.Fatalf("KillSession: %v", err)
	}
	if exists, _ := c.SessionExists(sess); exists {
		t.Fatal("session still exists after KillSession")
	}

	// Both shell and background sleep child must be gone.
	await.Until(t, "child sleep killed by termd close rule", func() (string, bool) {
		err := syscall.Kill(childPID, 0)
		if err != nil {
			return "gone", true
		}
		return fmt.Sprintf("pid %d still alive", childPID), false
	})
}

func TestLiteralSendKeysAndSpecialKeys(t *testing.T) {
	c := newTestClient(t, "agents-keys")
	sess := newShell(t, c, "keys")

	// C-c cancels a half-typed command.
	if err := c.SendKeys(sess, "echo never-''runs"); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "C-c"); err != nil {
		t.Fatal(err)
	}
	if err := c.SendKeys(sess, "echo after-$((40+2))"); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatal(err)
	}
	screen := waitLine(t, c, sess, "after-42")
	if strings.Contains(screen, "\nnever-runs\n") {
		t.Errorf("C-c did not cancel line:\n%s", screen)
	}

	// Literal strings that look like tmux flags or key names arrive verbatim.
	if err := c.SendKeys(sess, "echo cat-$((40+2)); cat -v"); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatal(err)
	}
	waitLine(t, c, sess, "cat-42")

	for _, lit := range []string{"Escape", "C-c", "-R", "--"} {
		if err := c.SendKeys(sess, lit); err != nil {
			t.Fatal(err)
		}
		if err := c.SendSpecialKey(sess, "Enter"); err != nil {
			t.Fatal(err)
		}
	}
	if err := c.SendSpecialKey(sess, "-N2"); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatal(err)
	}
	waitLine(t, c, sess, "-N2")
}

func TestLargeBracketedPasteAndCaptureStyled(t *testing.T) {
	c := newTestClient(t, "agents-paste")
	sess := newShell(t, c, "paste")

	// 66 KB paste (over 60x macOS's 1024-byte PTY MAX_CANON) with embedded
	// bracket-end sequence stripped so it arrives as one bracketed paste.
	text := strings.Repeat("0123456789", 3300) + "\x1b[201~" + strings.Repeat("abcdefghij", 3300) + "\nlast line"
	stripped := strings.ReplaceAll(text, "\x1b[201~", "")
	want := "\x1b[200~" + stripped + "\x1b[201~"

	outPath := filepath.Join(t.TempDir(), "pasted.bin")
	if err := c.SendKeys(sess, fmt.Sprintf(`stty raw -echo; printf '\033[?2004h'; echo RE''ADY; cat > %s`, outPath)); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatal(err)
	}
	await.Until(t, "app READY", func() (string, bool) {
		s, _ := c.Capture(sess, 0)
		return s, strings.Contains(s, "READY")
	})

	if err := c.PasteText(sess, text); err != nil {
		t.Fatalf("PasteText: %v", err)
	}
	got := await.Until(t, "full 66KB bracketed paste", func() (string, bool) {
		b, _ := os.ReadFile(outPath)
		return fmt.Sprintf("%d/%d bytes", len(b), len(want)), len(b) >= len(want)
	})
	_ = got
	rawBytes, err := os.ReadFile(outPath)
	if err != nil {
		t.Fatal(err)
	}
	if string(rawBytes) != want {
		t.Fatalf("got %d bytes, want %d bytes", len(rawBytes), len(want))
	}

	// Styled capture preserves dim SGR with \n line separators so ClaudeBackend.InputText
	// ignores dim placeholder text and reads real typed input.
	sess2 := newShell(t, c, "styled")
	boxCmd := `printf '────────────────────\n❯ \033[2mTry "fix lint"\033[0m\n────────────────────\nDONE-%d\n' $((40+2))`
	if err := c.SendKeys(sess2, boxCmd); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess2, "Enter"); err != nil {
		t.Fatal(err)
	}
	waitLine(t, c, sess2, "DONE-42")

	plain, err := c.Capture(sess2, 0)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(plain, "\x1b[") {
		t.Errorf("plain Capture should not contain ANSI escapes: %q", plain)
	}

	styled, err := c.CaptureStyled(sess2, 0)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(styled, "\r\n") {
		t.Errorf("CaptureStyled must normalize \\r\\n to \\n: %q", styled)
	}
	claude := namedagents.NewClaudeBackend()
	// Trim trailing shell prompt lines below DONE-42 so the simulated box sits at the bottom.
	idx := strings.Index(styled, "DONE-42")
	if idx < 0 {
		t.Fatalf("missing DONE-42 in styled capture:\n%s", styled)
	}
	inputText, found := claude.InputText(styled[:idx])
	if !found || inputText != "" {
		t.Errorf("ClaudeBackend.InputText on dim placeholder = %q, %v; want empty, true", inputText, found)
	}
}

func TestPipePaneRespawnAndAttachCommand(t *testing.T) {
	c := newTestClient(t, "agents-pipe")
	sess := newShell(t, c, "pipe")

	// Malicious path with command substitution must write literal file, never run shell.
	tmp := t.TempDir()
	marker := filepath.Join(tmp, "PWNED")
	logPath := filepath.Join(tmp, fmt.Sprintf("log$(touch %s).txt", marker))
	if err := os.MkdirAll(filepath.Dir(logPath), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := c.PipePane(sess, logPath); err != nil {
		t.Fatalf("PipePane: %v", err)
	}
	if err := c.SendKeys(sess, "echo piped-$((40+2))"); err != nil {
		t.Fatal(err)
	}
	if err := c.SendSpecialKey(sess, "Enter"); err != nil {
		t.Fatal(err)
	}
	await.Until(t, "piped output in file", func() (string, bool) {
		b, _ := os.ReadFile(logPath)
		return string(b), strings.Contains(string(b), "piped-42")
	})
	if _, err := os.Stat(marker); err == nil {
		t.Fatal("PipePane executed shell injection!")
	}
	if err := c.StopPipePane(sess); err != nil {
		t.Fatalf("StopPipePane: %v", err)
	}

	// RespawnPane replaces the shell PID while keeping the session.
	before, err := c.FirstPane(sess)
	if err != nil {
		t.Fatal(err)
	}
	if err := c.RespawnPane(sess); err != nil {
		t.Fatalf("RespawnPane: %v", err)
	}
	after, err := c.FirstPane(sess)
	if err != nil {
		t.Fatal(err)
	}
	if after.PID == before.PID {
		t.Errorf("RespawnPane kept same PID %d", after.PID)
	}

	// AttachCommand builds g3d --ns <ns> attach --take <session>.
	if _, err := termd.AttachBinary("", ""); err == nil {
		bin, argv, err := c.AttachCommand(sess)
		if err != nil {
			t.Fatalf("AttachCommand: %v", err)
		}
		if bin == "" || !slices.Equal(argv, []string{"g3d", "--ns", "agents-pipe", "attach", "--take", sess}) {
			t.Errorf("AttachCommand = %q, %q", bin, argv)
		}
	}
}
