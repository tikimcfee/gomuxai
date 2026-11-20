package namedagents

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"slices"
	"sort"
	"strings"
	"time"

	"github.com/tikimcfee/gomuxai/internal/util"
)

// Claude Code's hooks for every session on the machine live in its user
// settings file. gomuxai's entry runs `<agent binary> hook` on each event
// (see agent hook): whichever Claude Code session fires it, a workgroup
// agent's or one a person started, the hook works out which it is from the
// running processes.

// ClaudeHookEvents are the events gomuxai's hook is registered on.
var ClaudeHookEvents = []string{
	"SessionStart",
	"UserPromptSubmit",
	"PreToolUse",
	"PermissionRequest",
	"PostToolUse",
	"PostToolUseFailure",
	"PreCompact",
	"Notification",
	"SubagentStart",
	"SubagentStop",
	"Stop",
	"StopFailure",
	"SessionEnd",
}

// ClaudeSettingsPath returns Claude Code's user settings file:
// $CLAUDE_CONFIG_DIR/settings.json, else ~/.claude/settings.json.
func ClaudeSettingsPath() (string, error) {
	if d := os.Getenv("CLAUDE_CONFIG_DIR"); d != "" {
		return filepath.Join(d, "settings.json"), nil
	}
	home, err := os.UserHomeDir()
	if err != nil {
		return "", fmt.Errorf("finding Claude Code's settings: %w", err)
	}
	return filepath.Join(home, ".claude", "settings.json"), nil
}

// ClaudeHookCommand is the command gomuxai's entry runs.
func ClaudeHookCommand(binary string) string {
	return util.ShellQuote(binary) + " hook"
}

// HookState is gomuxai's entry for one event.
type HookState int

const (
	HookMissing HookState = iota // no entry of gomuxai's
	HookCurrent                  // an entry running this binary
	HookStale                    // an entry running another agent binary (moved, or another build)
)

// HooksReport says, per event, what the settings held (before a change).
type HooksReport struct {
	Path   string
	Events map[string]HookState
	// Others names the stale entries' commands, once each, so a person can
	// see what an install re-pointed or a remove took out.
	Others []string
	// Changed says the file was written; Backup is where the old one was
	// copied ("" if unchanged, or there was no file).
	Changed bool
	Backup  string
}

// ClaudeHooksStatus reads which events carry gomuxai's entry.
func ClaudeHooksStatus(path, binary string) (*HooksReport, error) {
	settings, err := readSettings(path)
	if err != nil {
		return nil, err
	}
	r, _, err := scanHooks(settings, binary, path)
	return r, err
}

// InstallClaudeHooks makes every event in ClaudeHookEvents run binary's
// hook, in the settings file at path: it points a stale entry at binary,
// adds a missing one, and leaves every other hook and setting as it was. It
// changes nothing when all are current. A file that isn't a JSON object, or
// whose hooks aren't shaped as Claude Code reads them, is an error, never
// overwritten. The old file is copied into backupDir first.
//
// One hook serves one agent binary: the store beside it records the
// sessions it places. An entry of another agent binary that still exists is
// re-pointed only with force, since that binary's agents would stop
// reporting.
func InstallClaudeHooks(path, binary, backupDir string, force bool) (*HooksReport, error) {
	return editHooks(path, binary, backupDir, true, force)
}

// RemoveClaudeHooks takes gomuxai's entries out of the settings file, from
// every event, dropping an event (and the hooks key) left empty.
func RemoveClaudeHooks(path, binary, backupDir string) (*HooksReport, error) {
	return editHooks(path, binary, backupDir, false, true)
}

func editHooks(path, binary, backupDir string, install, force bool) (*HooksReport, error) {
	settings, err := readSettings(path)
	if err != nil {
		return nil, err
	}
	report, hooks, err := scanHooks(settings, binary, path)
	if err != nil {
		return nil, err
	}
	if install && !force {
		if live := liveOthers(report.Others); len(live) > 0 {
			return nil, fmt.Errorf("Claude Code's hook in %s runs %s, which is still there: one hook serves one agent binary (its store records the sessions it places), so re-pointing it here would stop that binary's agents reporting. Run agent hooks install from that binary, or pass --force to point it here", path, strings.Join(live, ", "))
		}
	}
	changed := false
	want := ClaudeHookCommand(binary)
	for event, groups := range hooks {
		var kept []any
		for _, g := range groups {
			group, _ := g.(map[string]any)
			inner, _ := group["hooks"].([]any)
			keepInner := []any{}
			for _, h := range inner {
				cmd, _ := h.(map[string]any)
				c, _ := cmd["command"].(string)
				if !isOurHook(c, binary) {
					keepInner = append(keepInner, h)
					continue
				}
				changed = changed || !install || c != want
				if install {
					cmd["command"] = want
					keepInner = append(keepInner, cmd)
				}
			}
			if len(inner) > 0 && len(keepInner) == 0 {
				continue // the group held only our entry
			}
			group["hooks"] = keepInner
			kept = append(kept, group)
		}
		hooks[event] = kept
	}
	if install {
		for _, event := range ClaudeHookEvents {
			if report.Events[event] == HookMissing {
				hooks[event] = append(hooks[event], map[string]any{
					"matcher": "",
					"hooks":   []any{map[string]any{"type": "command", "command": want, "timeout": hookTimeoutSec}},
				})
				changed = true
			}
		}
	}
	if !changed {
		return report, nil
	}
	out := map[string]any{}
	for event, groups := range hooks {
		if len(groups) > 0 {
			out[event] = groups
		}
	}
	if len(out) > 0 {
		settings["hooks"] = out
	} else {
		delete(settings, "hooks")
	}
	report.Changed = true
	if report.Backup, err = backup(path, backupDir); err != nil {
		return nil, err
	}
	data, err := marshalSettings(settings)
	if err != nil {
		return nil, err
	}
	if err := util.WriteFileAtomic(path, data, 0o600); err != nil {
		return nil, fmt.Errorf("writing %s: %w", path, err)
	}
	return report, nil
}

// readSettings reads the settings file; a missing or empty one is {}.
func readSettings(path string) (map[string]any, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) || (err == nil && len(bytes.TrimSpace(data)) == 0) {
		return map[string]any{}, nil
	}
	if err != nil {
		return nil, fmt.Errorf("reading %s: %w", path, err)
	}
	var settings map[string]any
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.UseNumber() // numbers come back as written, not through float64
	if err := dec.Decode(&settings); err != nil || settings == nil {
		return nil, fmt.Errorf("%s isn't a JSON object, so it was left alone (fix it, then run this again): %v", path, err)
	}
	return settings, nil
}

// scanHooks reports gomuxai's entry per event, and returns the hooks as
// event -> groups (each {"matcher", "hooks": [{"type", "command", ...}]}).
// Hooks shaped otherwise are an error: editing what Claude Code wouldn't
// read could only make it worse.
func scanHooks(settings map[string]any, binary, path string) (*HooksReport, map[string][]any, error) {
	report := &HooksReport{Path: path, Events: map[string]HookState{}}
	hooks := map[string][]any{}
	bad := func(where, what string) (*HooksReport, map[string][]any, error) {
		return nil, nil, fmt.Errorf("%s: %s isn't %s, so the file was left alone (fix it, then run this again)", path, where, what)
	}
	var raw map[string]any
	switch v := settings["hooks"].(type) {
	case nil:
	case map[string]any:
		raw = v
	default:
		return bad("hooks", "an object")
	}
	want := ClaudeHookCommand(binary)
	seen := map[string]bool{}
	for event, v := range raw {
		groups, ok := v.([]any)
		if !ok {
			return bad("hooks."+event, "an array")
		}
		hooks[event] = groups
		for i, g := range groups {
			group, ok := g.(map[string]any)
			if !ok {
				return bad(fmt.Sprintf("hooks.%s[%d]", event, i), "an object")
			}
			inner, ok := group["hooks"].([]any)
			if !ok && group["hooks"] != nil {
				return bad(fmt.Sprintf("hooks.%s[%d].hooks", event, i), "an array")
			}
			for j, h := range inner {
				cmd, ok := h.(map[string]any)
				if !ok {
					return bad(fmt.Sprintf("hooks.%s[%d].hooks[%d]", event, i, j), "an object")
				}
				c, _ := cmd["command"].(string)
				switch {
				case c == want:
					report.Events[event] = HookCurrent
				case isOurHook(c, binary):
					if report.Events[event] != HookCurrent {
						report.Events[event] = HookStale
					}
					if !seen[c] {
						seen[c] = true
						report.Others = append(report.Others, c)
					}
				}
			}
		}
	}
	return report, hooks, nil
}

// isOurHook reports whether a hook command is an agent binary's hook: two
// words, an executable named agent (or as this binary is named) and "hook".
func isOurHook(command, binary string) bool {
	f := util.SplitShellWords(command)
	if len(f) != 2 || f[1] != "hook" {
		return false
	}
	name := filepath.Base(f[0])
	return name == "agent" || name == filepath.Base(binary)
}

// liveOthers returns the binaries of the stale hook commands that still
// exist as executables.
func liveOthers(commands []string) []string {
	var live []string
	for _, c := range commands {
		f := util.SplitShellWords(c)
		if len(f) == 0 {
			continue
		}
		if info, err := os.Stat(f[0]); err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0 {
			live = append(live, f[0])
		}
	}
	return live
}

func backup(path, dir string) (string, error) {
	data, err := os.ReadFile(path)
	if errors.Is(err, fs.ErrNotExist) {
		return "", nil
	}
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", fmt.Errorf("making the backup dir: %w", err)
	}
	dst := filepath.Join(dir, filepath.Base(path)+"."+time.Now().UTC().Format("20060102T150405.000000000Z"))
	if err := os.WriteFile(dst, data, 0o600); err != nil {
		return "", fmt.Errorf("backing up %s: %w", path, err)
	}
	return dst, nil
}

// marshalSettings writes settings the way Claude Code does: two-space
// indents, keys sorted, slashes and other HTML characters unescaped.
func marshalSettings(settings map[string]any) ([]byte, error) {
	var b bytes.Buffer
	enc := json.NewEncoder(&b)
	enc.SetEscapeHTML(false)
	enc.SetIndent("", "  ")
	if err := enc.Encode(settings); err != nil {
		return nil, err
	}
	return b.Bytes(), nil
}

// HookStateWord is a HookState as status prints it.
func HookStateWord(s HookState) string {
	switch s {
	case HookCurrent:
		return "installed"
	case HookStale:
		return "another agent binary's"
	}
	return "missing"
}

// SortedEvents returns ClaudeHookEvents, then any other event gomuxai's
// entry sits on, for printing.
func (r *HooksReport) SortedEvents() []string {
	out := append([]string(nil), ClaudeHookEvents...)
	var extra []string
	for e := range r.Events {
		if !slices.Contains(out, e) {
			extra = append(extra, e)
		}
	}
	sort.Strings(extra)
	return append(out, extra...)
}
