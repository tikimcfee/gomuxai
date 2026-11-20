package service_test

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/storage"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Claude's auto-mode offer ("Teach auto mode about your environment?")
// comes up when a turn ends, fires no hook, and leaves its session registry
// saying idle. A message sent then was reported "Sent (it was idle)", but
// the offer took its keys and its Enter accepted the offer (friction #29).

// atOffer returns a Claude agent "worker" whose last turn ended (reported
// idle) with the offer on screen; with holds, its messages are held (as
// for a spawned agent).
func atOffer(t *testing.T, holds bool) (*service.AgentService, *storage.SQLiteStorage, *claudeTerminal) {
	t.Helper()
	ctx := context.Background()
	term := &claudeTerminal{offer: true}
	svc, store := setupClaudeTerminal(t, term, submitTestWait)
	if holds {
		if err := store.SetState(ctx, &storage.StateEntry{AgentName: "worker", Key: service.StateKeySpawnID, Value: "s1", UpdatedAt: time.Now()}); err != nil {
			t.Fatal(err)
		}
	}
	turnStarted(t, store, "worker")
	if err := svc.Report(ctx, "worker", service.StatusReport{Status: service.AgentStatusIdle, Event: "Stop"}); err != nil {
		t.Fatal(err)
	}
	return svc, store, term
}

// The offer reads as waiting, on tool "dialog", naming it and what Enter
// would pick.
func TestDialog_StatusIsWaiting(t *testing.T) {
	svc, _, term := atOffer(t, true)
	info, err := svc.StatusInfo(context.Background(), "worker")
	if err != nil {
		t.Fatal(err)
	}
	if !info.IsDialog() || info.Tool != service.DialogTool || info.Source != service.StatusSourceScrape {
		t.Fatalf("status %s, tool %q, source %s (%s); want waiting at a dialog", info.Status, info.Tool, info.Source, info.Detail)
	}
	if info.Detail != "dialog: Teach auto mode about your environment?; Enter picks: 1. Yes" {
		t.Errorf("detail %q", info.Detail)
	}
	if title, ok := service.DialogTitle(info.Status, info.Detail); !ok || title != "Teach auto mode about your environment?" {
		t.Errorf("DialogTitle = %q, %v", title, ok)
	}

	// Gone, the agent is idle again.
	term.mu.Lock()
	term.offer = false
	term.mu.Unlock()
	if info, _ := svc.StatusInfo(context.Background(), "worker"); info.Status != service.AgentStatusIdle {
		t.Errorf("status %s (%s) with the dialog gone; want idle", info.Status, info.Detail)
	}
}

// A message to an agent that holds messages is held, untyped, until the
// dialog is gone; deny closes it with Escape, and the held message is then
// typed in.
func TestDialog_SendHoldsUntilDenyClosesIt(t *testing.T) {
	ctx := context.Background()
	svc, _, term := atOffer(t, true)
	var delivering []string
	svc.SetDelivererStarter(func(name string) error { delivering = append(delivering, name); return nil })

	res, err := svc.Send(ctx, "worker", "[from lead@hub] run the tests", service.SendOptions{From: "lead"})
	if err != nil || !res.Held || res.Typed != 0 {
		t.Fatalf("Send = %+v, %v; want held", res, err)
	}
	if !strings.Contains(res.HeldUntil, "dialog is gone") || !strings.HasPrefix(res.Detail, "dialog: ") {
		t.Errorf("held until %q (%s); want until the dialog is gone", res.HeldUntil, res.Detail)
	}
	if enters, submitted, _ := term.state(); enters != 0 || len(submitted) != 0 || len(term.offerAnswers) != 0 {
		t.Fatalf("typed into the dialog: %d Enters, submitted %q, offer answered %q", enters, submitted, term.offerAnswers)
	}
	if len(delivering) == 0 {
		t.Error("no deliverer started: no hook will deliver it")
	}

	if _, err := svc.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{ExpectTool: "Bash"}); err == nil {
		t.Error("deny --expect Bash answered a dialog")
	}
	if _, err := svc.RespondPermission(ctx, "worker", backend.PermissionApproveAlways, service.PermissionOptions{}); err == nil {
		t.Error("approve --always answered a dialog, which has no always")
	}
	if _, err := svc.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{ExpectTool: service.DialogTool}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	if strings.Join(term.offerAnswers, ",") != "Esc" {
		t.Fatalf("offer answered %q; want closed with Escape", term.offerAnswers)
	}

	n, err := svc.DeliverHeld(ctx, "worker", service.DeliverOptions{})
	if err != nil || n != 1 {
		t.Fatalf("DeliverHeld = %d, %v; want the held message typed", n, err)
	}
	if _, submitted, _ := term.state(); len(submitted) != 1 || submitted[0] != "[from lead@hub] run the tests" {
		t.Errorf("submitted %q", submitted)
	}
}

// approve presses Enter, picking the highlighted option.
func TestDialog_ApprovePicksTheHighlightedOption(t *testing.T) {
	svc, _, term := atOffer(t, true)
	info, err := svc.RespondPermission(context.Background(), "worker", backend.PermissionApprove, service.PermissionOptions{ExpectTool: service.DialogTool})
	if err != nil || !info.IsDialog() {
		t.Fatalf("approve = %+v, %v", info, err)
	}
	if strings.Join(term.offerAnswers, ",") != "Yes" {
		t.Errorf("offer answered %q; want Yes", term.offerAnswers)
	}
	if _, err := svc.RespondPermission(context.Background(), "worker", backend.PermissionApprove, service.PermissionOptions{}); err == nil {
		t.Error("approve with the dialog gone pressed a key")
	}
}

// An agent that doesn't hold messages has the send refused, saying how to
// get past the dialog, with nothing typed.
func TestDialog_SendRefusedWhereNotHeld(t *testing.T) {
	svc, _, term := atOffer(t, false)
	_, err := svc.Send(context.Background(), "worker", "[from lead@hub] run the tests", service.SendOptions{})
	if err == nil || !strings.Contains(err.Error(), "shows a dialog (Teach auto mode about your environment?)") ||
		!strings.Contains(err.Error(), "agent deny worker") || !strings.Contains(err.Error(), `"1. Yes"`) {
		t.Fatalf("err %v; want the dialog named, and how to close it", err)
	}
	if enters, _, _ := term.state(); enters != 0 || len(term.offerAnswers) != 0 {
		t.Errorf("typed into the dialog: %d Enters, answers %q", enters, term.offerAnswers)
	}
}

// A dialog that comes up between send's look at the screen and the typing
// takes the text: no Enter goes after it, which would accept it.
func TestSubmit_NoEnterIntoADialogThatTookTheText(t *testing.T) {
	term := &claudeTerminal{offerOnRead: true}
	svc, store := setupClaudeTerminal(t, term, submitTestWait)
	turnStarted(t, store, "worker")
	_, err := svc.Send(context.Background(), "worker", "[from lead@hub] run the tests", service.SendOptions{})
	var took *service.DialogTookError
	if !errors.As(err, &took) || took.Dialog.Title != "Teach auto mode about your environment?" {
		t.Fatalf("err %v; want DialogTookError", err)
	}
	if enters, _, _ := term.state(); enters != 0 || len(term.offerAnswers) != 0 {
		t.Errorf("%d Enters, offer answered %q; want no Enter", enters, term.offerAnswers)
	}
}

// A box that never shows the message, with no dialog to blame, still gets
// its Enter, but send doesn't call it sent: it says the box never showed
// it, and status keeps saying so until a turn starts.
func TestSubmit_SaysWhenTheBoxNeverShowedTheMessage(t *testing.T) {
	ctx := context.Background()
	term := &claudeTerminal{deafType: true}
	svc, store := setupClaudeTerminal(t, term, submitTestWait)
	turnStarted(t, store, "worker")
	res, err := svc.Send(ctx, "worker", "[from lead@hub] run the tests", service.SendOptions{Now: true})
	if err != nil || res.Typed != 1 {
		t.Fatalf("Send = %+v, %v; want typed", res, err)
	}
	if !strings.Contains(res.Unchecked, "never showed the message") {
		t.Errorf("Unchecked = %q; want that the box never showed it", res.Unchecked)
	}
	if why, _, ok := svc.Unchecked(ctx, "worker"); !ok || why != res.Unchecked {
		t.Errorf("Unchecked = %q, %v; want it recorded", why, ok)
	}
}

// fakeDialogCLI stands in for Claude Code starting with a dialog up, drawn
// above its input box as the fullscreen renderer draws the auto-mode offer
// (tests/backend/testdata/claude-offer-above-box.txt is a real one). Typed
// text goes to the dialog ($1.typed); Escape closes it ($1: esc) and leaves
// the bare input box; Enter accepts it ($1: yes).
const fakeDialogCLI = `
rule=$(printf '─%.0s' $(seq 60))
printf '\033[2J\033[H● ok\n\n%s\n' "$rule"
printf '  Teach auto mode about your environment?\n\n'
printf '  ❯ 1. Yes\n    2. Not now\n    3. Don'"'"'t show again\n\n'
printf '  Enter to confirm · Esc to cancel\n%s\n❯ \n%s\n  ⏵⏵ auto mode on\n' "$rule" "$rule"
while IFS= read -rsn1 k; do
  case $k in
    $'\033') IFS= read -rsn2 -t 1 rest
      if [ -z "$rest" ]; then
        printf 'esc\n' > "$1"
        printf '\033[2J\033[H● ok\n\n%s\n❯ \n%s\n  ⏵⏵ auto mode on\n' "$rule" "$rule"
        exec sleep 600
      fi ;;
    '') printf 'yes\n' > "$1"; exit 0 ;;
    *) printf '%s' "$k" >> "$1.typed" ;;
  esac
done
`

// fakeDialogBackend is the claude backend launching fakeDialogCLI; like
// Claude's registry, its native status says idle with the dialog up.
type fakeDialogBackend struct {
	*namedagents.ClaudeBackend
	script, answer string
}

func (f *fakeDialogBackend) Name() string                           { return "fakedialog" }
func (f *fakeDialogBackend) HooksInstalled(string) error            { return nil }
func (f *fakeDialogBackend) LaunchCommandWithSession(string) string { return f.LaunchCommand() }
func (f *fakeDialogBackend) LaunchCommand() string {
	return "bash " + util.ShellQuote(f.script) + " " + util.ShellQuote(f.answer)
}
func (f *fakeDialogBackend) NativeStatus(ctx context.Context, sessionID string) (backend.Status, bool, error) {
	return backend.StatusIdle, true, nil
}
func (f *fakeDialogBackend) PaneSession(ctx context.Context, panePID int) (backend.PaneSession, bool, error) {
	return backend.PaneSession{}, false, nil
}

// A dialog up when spawn types the first message would take it: spawn
// holds the task instead, starts the deliverer, and status says why it
// waits.
func TestDialog_SpawnHoldsTheTask(t *testing.T) {
	env := setupTestEnv(t)
	defer env.cleanup()
	ctx := context.Background()
	dir := t.TempDir()
	fake := &fakeDialogBackend{ClaudeBackend: namedagents.NewClaudeBackend(), script: filepath.Join(dir, "claude.sh"), answer: filepath.Join(dir, "answer")}
	if err := os.WriteFile(fake.script, []byte(fakeDialogCLI), 0o600); err != nil {
		t.Fatal(err)
	}
	env.backends.Register(fake)
	env.backends.RegisterFactory("fakedialog", func(backend.ProviderConfig) backend.Provider { return fake })
	// How long spawn waits for the CLI to draw its input box or a dialog:
	// the wait ends as soon as the dialog shows, so a generous one costs
	// nothing, while a short one under load (a full parallel test run)
	// looks before the fake has drawn anything.
	env.cfg.SubmitWait = 3 * time.Second
	env.cfg.AgentBinary = "/opt/gomuxai/agent"
	var delivering []string
	env.service.SetDelivererStarter(func(name string) error { delivering = append(delivering, name); return nil })
	task := filepath.Join(dir, "task.md")
	if err := os.WriteFile(task, []byte("say ok"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := env.service.Spawn(ctx, service.SpawnOptions{Name: "worker", Backend: "fakedialog", TaskFile: task}); err != nil {
		t.Fatalf("spawn: %v", err)
	}

	held, err := env.store.GetPendingMessages(ctx, "worker", 10)
	if err != nil || len(held) != 1 || !strings.Contains(held[0].Content, task) {
		t.Fatalf("held = %v (err %v), want the task's message", held, err)
	}
	if typed, _ := os.ReadFile(fake.answer + ".typed"); len(typed) > 0 {
		t.Errorf("the task was typed into the dialog: %q", typed)
	}
	if len(delivering) != 1 || delivering[0] != "worker" {
		t.Errorf("deliverers started for %v, want worker", delivering)
	}
	info := statusIs(t, env, "worker", service.AgentStatusWaiting)
	if !info.IsDialog() || info.Held != 1 {
		t.Errorf("status %s (%s), %d held; want the dialog with the task held", info.Status, info.Detail, info.Held)
	}

	if _, err := env.service.RespondPermission(ctx, "worker", backend.PermissionDeny, service.PermissionOptions{ExpectTool: service.DialogTool}); err != nil {
		t.Fatalf("deny: %v", err)
	}
	waitPane(t, env, env.cfg.SessionName("worker"), "the dialog closed", func(screen string) bool {
		return !strings.Contains(screen, "Teach auto mode")
	})
	if data, _ := os.ReadFile(fake.answer); strings.TrimSpace(string(data)) != "esc" {
		t.Errorf("dialog answered %q; want esc", data)
	}
	statusIs(t, env, "worker", service.AgentStatusIdle)
}
