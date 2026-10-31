package cli

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

var hookCmd = &cobra.Command{
	Use:   "hook",
	Short: "The hook Claude Code runs on each event (installed by agent hooks install)",
	Long: `Hook is what Claude Code runs for every hook event of every session, once
"agent hooks install" has registered it in Claude Code's user settings. It
reads the event's payload on stdin and works out, from the running
processes, which session fired it: the Claude Code CLI above it (proven by
Claude's session registry: pid and start time), and the workgroup agent
whose tmux pane runs that CLI as its own session, if any.

Every session's report is recorded (agent sessions shows how each ended).
An agent's session also gets the agent's report, as agent status reads it,
and only there does hook print anything: held messages for Claude's Stop,
and the approval of a safe agent command for PreToolUse. A session no agent
owns, such as one a person started, never sees output from it.

It always exits 0, so a reporting problem can't disturb a session; what went
wrong goes to stderr and to logs/hook-errors.log in the data dir, which
"agent hooks status" shows.`,
	Example: `  agent hook < payload.json`,
	Args:    cobra.NoArgs,
	RunE:    runHook,
}

var hooksCmd = &cobra.Command{
	Use:   "hooks <install|remove|status>",
	Short: "Register gomuxai's hook with Claude Code for every session",
	Long: `Hooks manages gomuxai's entry in Claude Code's user settings
($CLAUDE_CONFIG_DIR/settings.json, else ~/.claude/settings.json): for each
hook event, a command running this binary's "agent hook".

  install  adds the entry where it is missing. Every other hook and
           setting stays as it was; the file is copied to backups/ in the
           data dir first, and written atomically. A file that isn't JSON,
           or whose hooks Claude Code couldn't read, is left alone. It
           changes nothing when all are current.
  remove   takes gomuxai's entries out.
  status   prints, per event, whether the entry is installed, and the last
           errors the hook logged.

One hook serves one agent binary: the hook records the sessions it places
in the store beside the binary it runs, so a Claude agent another binary
spawned isn't heard. install points the entry of an agent binary that is
gone (moved, deleted) at this one, but refuses to take over one that still
exists unless --force: that binary's agents would stop reporting. Spawn
refuses a Claude agent while the hook runs another binary.

Claude Code reads hooks when a session starts, so sessions already running
pick the change up when restarted (or through /hooks).`,
	Example: `  agent hooks install
  agent hooks install --force
  agent hooks status
  agent hooks remove`,
	Args:      cobra.ExactArgs(1),
	ValidArgs: []string{"install", "remove", "status"},
	RunE:      runHooks,
}

var (
	hooksSettings string
	hooksForce    bool
)

func init() {
	hooksCmd.Flags().StringVar(&hooksSettings, "settings", "", "Settings file to edit (default: Claude Code's user settings)")
	hooksCmd.Flags().BoolVar(&hooksForce, "force", false, "Point the hook at this binary even if it runs another agent binary that still exists")
}

func hookErrorsPath() string {
	return filepath.Join(cfg.LogDir, "hook-errors.log")
}

func runHook(cmd *cobra.Command, args []string) error {
	at := time.Now()
	err := handleHook(at)
	if err != nil {
		fmt.Fprintf(os.Stderr, "agent hook: %v\n", err)
		if f, ferr := os.OpenFile(hookErrorsPath(), os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600); ferr == nil {
			fmt.Fprintf(f, "%s %s\n", at.UTC().Format(time.RFC3339), strings.ReplaceAll(err.Error(), "\n", " "))
			f.Close()
		}
	}
	return nil
}

func handleHook(at time.Time) error {
	payload, err := io.ReadAll(os.Stdin)
	if err != nil {
		return fmt.Errorf("reading the payload: %w", err)
	}
	ctx := getContext()
	var head struct {
		SessionID string `json:"session_id"`
	}
	_ = json.Unmarshal(payload, &head) // a payload that doesn't decode fails below, where it is recorded
	hs, err := agentService.FindHookSession(ctx, os.Getpid(), head.SessionID, os.Getenv("CLAUDE_PID"))
	if err != nil {
		// The session's own report needs no owner: record it, then say why
		// no agent got one.
		_, rerr := agentService.HandleClaudeHook(ctx, hs, payload, at)
		return errors.Join(fmt.Errorf("placing the session: %w", err), rerr)
	}
	out, err := agentService.HandleClaudeHook(ctx, hs, payload, at)
	if out != nil {
		if _, werr := os.Stdout.Write(out); werr != nil {
			err = errors.Join(err, fmt.Errorf("writing the hook's output: %w", werr))
		}
	}
	return err
}

func runHooks(cmd *cobra.Command, args []string) error {
	path := hooksSettings
	if path == "" {
		var err error
		if path, err = namedagents.ClaudeSettingsPath(); err != nil {
			return err
		}
	}
	bin := cfg.AgentBinary
	if bin == "" {
		return fmt.Errorf("can't tell where this agent binary is, so there is nothing to register")
	}
	backups := filepath.Join(filepath.Dir(cfg.DatabasePath), "backups")
	out := cmd.OutOrStdout()
	switch args[0] {
	case "install":
		r, err := namedagents.InstallClaudeHooks(path, bin, backups, hooksForce)
		if err != nil {
			return err
		}
		added, repointed := 0, 0
		for _, e := range namedagents.ClaudeHookEvents {
			switch r.Events[e] {
			case namedagents.HookMissing:
				added++
			case namedagents.HookStale:
				repointed++
			}
		}
		if !r.Changed {
			fmt.Fprintf(out, "Already installed in %s: every event runs %s.\n", path, namedagents.ClaudeHookCommand(bin))
			return nil
		}
		fmt.Fprintf(out, "Installed in %s: %d event(s) added, %d pointed at this binary (was: %s). Backup: %s\n",
			path, added, repointed, orNone(r.Others), orNone(nonEmpty(r.Backup)))
		hint("sessions already running pick it up when restarted, or through /hooks")
		return nil
	case "remove":
		r, err := namedagents.RemoveClaudeHooks(path, bin, backups)
		if err != nil {
			return err
		}
		if !r.Changed {
			fmt.Fprintf(out, "Nothing to remove from %s.\n", path)
			return nil
		}
		fmt.Fprintf(out, "Removed gomuxai's hook from %s. Backup: %s\n", path, orNone(nonEmpty(r.Backup)))
		return nil
	case "status":
		r, err := namedagents.ClaudeHooksStatus(path, bin)
		if err != nil {
			return err
		}
		fmt.Fprintf(out, "%s (this binary: %s)\n", path, bin)
		for _, e := range r.SortedEvents() {
			fmt.Fprintf(out, "  %-19s %s\n", e, namedagents.HookStateWord(r.Events[e]))
		}
		if len(r.Others) > 0 {
			fmt.Fprintf(out, "Other agent binaries' entries: %s\n", strings.Join(r.Others, ", "))
		}
		printHookErrors(out)
		return nil
	}
	return usageError(cmd, fmt.Errorf("unknown action %q: install, remove, or status", args[0]))
}

// printHookErrors prints the last lines of the hook's error log.
func printHookErrors(out io.Writer) {
	data, err := os.ReadFile(hookErrorsPath())
	if err != nil || len(data) == 0 {
		fmt.Fprintln(out, "No hook errors logged.")
		return
	}
	lines := strings.Split(strings.TrimRight(string(data), "\n"), "\n")
	if len(lines) > 5 {
		lines = lines[len(lines)-5:]
	}
	fmt.Fprintf(out, "Last hook errors (%s):\n", hookErrorsPath())
	for _, l := range lines {
		fmt.Fprintln(out, "  "+l)
	}
}

func nonEmpty(s string) []string {
	if s == "" {
		return nil
	}
	return []string{s}
}

func orNone(list []string) string {
	if len(list) == 0 {
		return "none"
	}
	return strings.Join(list, ", ")
}
