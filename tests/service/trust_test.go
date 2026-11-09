package service_test

import (
	"context"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// fakeTrustCLI stands in for Claude Code at its folder trust dialog: it
// draws the dialog as Claude Code v2.1.310 does (tests/backend's
// claudeTrustScreen has a real capture), "No, exit" highlighted, moves the
// highlight with Up and Down, and takes Enter. "Yes" writes yes to $1 and
// leaves it sitting there as a started CLI; "No, exit" writes no and exits,
// leaving its last frame on screen under the shell's prompt, as Claude does.
const fakeTrustCLI = `
sel=no
draw() {
  printf '\033[2J\033[H Accessing workspace:\n\n %s\n\n' "$2"
  printf ' Quick safety check: Is this a project you created or one you trust? (Like your\n own code, a well-known open source project, or work from your team).\n\n'
  printf " Claude Code'll be able to read, edit, and execute files here.\n\n"
  if [ $sel = no ]; then printf ' ❯ No, exit\n   Yes, I trust this folder\n'
  else printf '   No, exit\n ❯ Yes, I trust this folder\n'; fi
  printf '\n Enter to confirm · Esc to cancel\n'
}
draw "$@"
while IFS= read -rsn1 k; do
  case $k in
    $'\033') IFS= read -rsn2 -t 1 rest
      case $rest in '[A') sel=no ;; '[B') sel=yes ;; esac
      draw "$@" ;;
    '') printf '%s\n' $sel > "$1"
      if [ $sel = yes ]; then printf '\033[2J\033[H● trusted\n'; exec sleep 600; fi
      exit 0 ;;
  esac
done
`

// trustFolder is the folder the fake asks about: long enough to wrap in an
// 80-column pane, as real ones do.
const trustFolder = "/srv/projects/a-folder-claude-has-never-seen-before/with-a-path-long-enough-to-wrap-at-80-columns"

// fakeTrustBackend is the claude backend launching fakeTrustCLI. Like
// Claude's registry, its native status doesn't list the session until the
// folder is trusted, and then says idle.
type fakeTrustBackend struct {
	*namedagents.ClaudeBackend
	script, answer string
}

func (f *fakeTrustBackend) Name() string                { return "faketrust" }
func (f *fakeTrustBackend) HooksInstalled(string) error { return nil }

func (f *fakeTrustBackend) LaunchCommand() string {
	return "bash " + util.ShellQuote(f.script) + " " + util.ShellQuote(f.answer) + " " + util.ShellQuote(trustFolder)
}

func (f *fakeTrustBackend) LaunchCommandWithSession(string) string { return f.LaunchCommand() }

// answered is what the fake was answered: "yes", "no", or "".
func (f *fakeTrustBackend) answered() string {
	data, _ := os.ReadFile(f.answer)
	return strings.TrimSpace(string(data))
}

func (f *fakeTrustBackend) NativeStatus(ctx context.Context, sessionID string) (backend.Status, bool, error) {
	if f.answered() == "yes" {
		return backend.StatusIdle, true, nil
	}
	return backend.StatusUnknown, false, nil
}

func (f *fakeTrustBackend) PaneSession(ctx context.Context, panePID int) (backend.PaneSession, bool, error) {
	return backend.PaneSession{}, false, nil
}

// spawnAtTrustPrompt spawns agent name on the fake, and waits until its
// dialog is drawn.
func spawnAtTrustPrompt(t *testing.T, env *testEnv, opts service.SpawnOptions) *fakeTrustBackend {
	t.Helper()
	dir := t.TempDir()
	fake := &fakeTrustBackend{ClaudeBackend: namedagents.NewClaudeBackend(), script: filepath.Join(dir, "claude.sh"), answer: filepath.Join(dir, "answer")}
	if err := os.WriteFile(fake.script, []byte(fakeTrustCLI), 0o600); err != nil {
		t.Fatal(err)
	}
	env.backends.Register(fake)
	env.backends.RegisterFactory("faketrust", func(backend.ProviderConfig) backend.Provider { return fake })
	// Spawn waits for the CLI's input box, or a dialog in its place, before
	// typing its first message, as send does.
	env.cfg.SubmitWait = 300 * time.Millisecond
	env.cfg.AgentBinary = "/opt/gomuxai/agent" // spawn pins a session id, as for real
	opts.Backend = "faketrust"
	if _, err := env.service.Spawn(context.Background(), opts); err != nil {
		t.Fatalf("spawn: %v", err)
	}
	waitPane(t, env, env.cfg.SessionName(opts.Name), "the trust dialog drawn", func(screen string) bool {
		return hasLine(screen, "Enter to confirm · Esc to cancel")
	})
	return fake
}

// statusIs waits until name's status is want.
func statusIs(t *testing.T, env *testEnv, name string, want service.AgentStatus) *service.StatusInfo {
	t.Helper()
	var info *service.StatusInfo
	await.Until(t, name+" "+string(want), func() (string, bool) {
		var err error
		info, err = env.service.StatusInfo(context.Background(), name)
		if err != nil {
			return err.Error(), false
		}
		return string(info.Status) + " (" + info.Detail + ")", info.Status == want
	})
	return info
}

// A Claude agent at its folder trust dialog read unknown: wait --for
// waiting never returned, approve refused it, and the spawner had to send
// raw keys (friction #4). It is waiting now, on tool "trust", and approve
// answers it.
func TestTrustPrompt_WaitingThenApproved(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnAtTrustPrompt(t, env, service.SpawnOptions{Name: "worker"})

	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatal(err)
	}
	if info.Status != service.AgentStatusWaiting || info.Tool != service.TrustTool || info.Detail != "trust: "+trustFolder || !info.IsTrustPrompt() {
		t.Fatalf("status = %s, tool %q, detail %q; want waiting on trust: %s", info.Status, info.Tool, info.Detail, trustFolder)
	}

	// wait --for waiting (and --any, which checks the same status) returns.
	waitCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	got, err := natsbus.Wait(waitCtx, nil, natsbus.WaitOptions{
		Host: "h", Agent: "worker", For: []string{"idle", "waiting"}, Interval: 20 * time.Millisecond,
		Current: func(ctx context.Context) (natsbus.Observation, error) { return observe(ctx, env, "worker") },
	})
	if err != nil || got != "waiting" {
		t.Fatalf("wait = %q, %v; want waiting", got, err)
	}

	// A message's Enter would answer it.
	if _, err := env.service.Send(ctx, "worker", "hello", service.SendOptions{}); err == nil || !strings.Contains(err.Error(), "agent approve worker trusts the folder") {
		t.Errorf("send at the trust prompt: err = %v", err)
	}

	// --expect guards it like any prompt, and it has no "always".
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{ExpectTool: "Bash"}); err == nil {
		t.Error("approved the trust prompt while expecting Bash")
	}
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApproveAlways, service.PermissionOptions{}); err == nil || !strings.Contains(err.Error(), "no \"always\" answer") {
		t.Errorf("approve --always: err = %v", err)
	}
	if fake.answered() != "" {
		t.Fatalf("a refused approve answered the prompt: %q", fake.answered())
	}

	answered, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{ExpectTool: service.TrustTool})
	if err != nil {
		t.Fatalf("approve: %v", err)
	}
	if !answered.IsTrustPrompt() {
		t.Errorf("approve answered %q (%s), want the trust prompt", answered.Tool, answered.Detail)
	}
	// "No, exit" was highlighted: approve moved to "Yes" before Enter.
	await.Until(t, "the fake answered", func() (string, bool) { return fake.answered(), fake.answered() != "" })
	if fake.answered() != "yes" {
		t.Fatalf("the fake was answered %q, want yes", fake.answered())
	}
	// Starting, then listed idle by its CLI.
	statusIs(t, env, "worker", service.AgentStatusIdle)

	// Nothing to answer now.
	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{}); err == nil {
		t.Error("approve with no prompt went through")
	}
}

// Until the CLI lists the session it was just trusted to start, status says
// it is starting, not that it died.
func TestTrustPrompt_ApprovedCLIIsStarting(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnAtTrustPrompt(t, env, service.SpawnOptions{Name: "worker"})
	// As if the fake's registry entry hadn't appeared yet.
	fake.answer = filepath.Join(t.TempDir(), "never-written")

	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{}); err != nil {
		t.Fatalf("approve: %v", err)
	}
	waitPane(t, env, env.cfg.SessionName("worker"), "the fake started", func(screen string) bool { return hasLine(screen, "● trusted") })
	info, err := env.service.StatusInfo(ctx, "worker")
	if err != nil {
		t.Fatal(err)
	}
	if info.Status != service.AgentStatusProcessing || info.Event != service.EventTrusted || !strings.Contains(info.Detail, trustFolder) {
		t.Errorf("status = %s (%s: %s), want processing (trusted)", info.Status, info.Event, info.Detail)
	}
}

// deny picks "No, exit": the CLI exits, and status says it ended because
// of that.
func TestTrustPrompt_Denied(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	fake := spawnAtTrustPrompt(t, env, service.SpawnOptions{Name: "worker"})

	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{ExpectTool: service.TrustTool}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	await.Until(t, "the fake answered", func() (string, bool) { return fake.answered(), fake.answered() != "" })
	if fake.answered() != "no" {
		t.Fatalf("the fake was answered %q, want no", fake.answered())
	}
	info := statusIs(t, env, "worker", service.AgentStatusEnded)
	if info.EndReason != backend.EndQuit || info.Event != service.EventTrustDeclined {
		t.Errorf("ended (%s), event %q; want quit, %s", info.EndReason, info.Event, service.EventTrustDeclined)
	}
}

// A first message typed at the trust prompt was lost (Claude's dialog
// ignores a paste), or its Enter would pick "No, exit". Spawn holds it
// instead, and approving the prompt has it delivered.
func TestTrustPrompt_SpawnHoldsTheTask(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	var delivering []string
	env.service.SetDelivererStarter(func(name string) error { delivering = append(delivering, name); return nil })
	task := filepath.Join(t.TempDir(), "task.md")
	if err := os.WriteFile(task, []byte("say ok"), 0o600); err != nil {
		t.Fatal(err)
	}
	fake := spawnAtTrustPrompt(t, env, service.SpawnOptions{Name: "worker", TaskFile: task})

	held, err := env.store.GetPendingMessages(ctx, "worker", 10)
	if err != nil || len(held) != 1 || !strings.Contains(held[0].Content, task) {
		t.Fatalf("held = %v (err %v), want the task's message", held, err)
	}
	if fake.answered() != "" {
		t.Fatalf("the task's Enter answered the prompt: %q", fake.answered())
	}
	info := statusIs(t, env, "worker", service.AgentStatusWaiting)
	if !info.IsTrustPrompt() || info.Held != 1 {
		t.Errorf("status %s (%s), %d held; want the trust prompt with the task held", info.Status, info.Detail, info.Held)
	}

	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionApprove, service.PermissionOptions{}); err != nil {
		t.Fatalf("approve: %v", err)
	}
	if len(delivering) != 1 || delivering[0] != "worker" {
		t.Errorf("deliverers started for %v, want worker", delivering)
	}
}

// approve answers what status described: a trust dialog on screen where
// status said permission prompt is refused, not sent "1".
func TestRespondPermission_RefusesATrustPromptForAPermissionPrompt(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	spawnFake(t, env, "worker")
	payload := `{"hook_event_name":"PermissionRequest","session_id":"s","tool_name":"Bash","tool_input":{"command":"ls"}}`
	if err := env.service.ReportHook(ctx, "worker", "", "", []byte(payload)); err != nil {
		t.Fatal(err)
	}
	if _, err := env.service.SendUnchecked(ctx, "worker", "printf ' Quick safety check: Is this a project you created or one you trust?\\n\\n ❯ No, exit\\n   Yes, I trust this folder\\n\\n Enter to confirm\\n'; read -r -n 1 _"); err != nil {
		t.Fatal(err)
	}
	waitPane(t, env, env.cfg.SessionName("worker"), "the trust dialog printed", func(screen string) bool {
		return hasLine(screen, "Enter to confirm")
	})
	for _, resp := range []backend.PermissionResponse{backend.PermissionApprove, backend.PermissionDeny} {
		if _, err := env.service.RespondPermission(ctx, "worker", resp, service.PermissionOptions{}); err == nil || !strings.Contains(err.Error(), "workspace trust prompt") {
			t.Errorf("%v: err = %v, want a refusal naming the trust prompt", resp, err)
		}
	}
	// The report still describes the Bash prompt: nothing was answered.
	if entry, err := env.store.GetState(ctx, "worker", service.StateKeyTool); err != nil || entry == nil || entry.Value != "Bash" {
		t.Errorf("tool = %v (err %v), want Bash still pending", entry, err)
	}
}
