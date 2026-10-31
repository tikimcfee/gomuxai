package backend_test

import (
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

func readJSON(t *testing.T, path string) map[string]any {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var v map[string]any
	if err := json.Unmarshal(data, &v); err != nil {
		t.Fatalf("%s: %v\n%s", path, err, data)
	}
	return v
}

// ourCommands lists, per event, the hook commands that are an agent
// binary's.
func ourCommands(settings map[string]any) map[string][]string {
	out := map[string][]string{}
	hooks, _ := settings["hooks"].(map[string]any)
	for event, v := range hooks {
		for _, g := range v.([]any) {
			for _, h := range g.(map[string]any)["hooks"].([]any) {
				c := h.(map[string]any)["command"].(string)
				if strings.HasSuffix(c, " hook") {
					out[event] = append(out[event], c)
				}
			}
		}
	}
	return out
}

func TestInstallClaudeHooks(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "settings.json")
	backups := filepath.Join(dir, "backups")
	// A person's settings, with a hook of their own on Stop and an old
	// gomuxai binary's entry on PreToolUse.
	orig := `{"model":"opus","env":{"A":"1"},"hooks":{
		"Stop":[{"matcher":"","hooks":[{"type":"command","command":"notify-send done"}]}],
		"PreToolUse":[{"matcher":"","hooks":[{"type":"command","command":"'/old/bin/agent' hook","timeout":10}]}]}}`
	if err := os.WriteFile(path, []byte(orig), 0o600); err != nil {
		t.Fatal(err)
	}
	bin := "/home/u/.local/bin/agent"
	r, err := namedagents.InstallClaudeHooks(path, bin, backups, false)
	if err != nil {
		t.Fatal(err)
	}
	if r.Events["PreToolUse"] != namedagents.HookStale || r.Events["Stop"] != namedagents.HookMissing || !r.Changed || r.Backup == "" {
		t.Fatalf("report %+v", r)
	}
	if b, _ := os.ReadFile(r.Backup); string(b) != orig {
		t.Fatalf("backup holds %q", b)
	}
	got := readJSON(t, path)
	if got["model"] != "opus" || got["env"].(map[string]any)["A"] != "1" {
		t.Errorf("other settings changed: %v", got)
	}
	want := "'" + bin + "' hook"
	ours := ourCommands(got)
	for _, event := range namedagents.ClaudeHookEvents {
		if len(ours[event]) != 1 || ours[event][0] != want {
			t.Errorf("%s: %v, want one %q", event, ours[event], want)
		}
	}
	// The person's own hook stays, beside ours.
	if stop := got["hooks"].(map[string]any)["Stop"].([]any); len(stop) != 2 ||
		stop[0].(map[string]any)["hooks"].([]any)[0].(map[string]any)["command"] != "notify-send done" {
		t.Errorf("Stop = %v", stop)
	}

	// Again: nothing to change, nothing written.
	before, _ := os.ReadFile(path)
	r, err = namedagents.InstallClaudeHooks(path, bin, backups, false)
	if err != nil || r.Changed || r.Backup != "" {
		t.Fatalf("second install: %+v %v", r, err)
	}
	if after, _ := os.ReadFile(path); string(after) != string(before) {
		t.Error("a second install rewrote the file")
	}
	st, err := namedagents.ClaudeHooksStatus(path, bin)
	if err != nil {
		t.Fatal(err)
	}
	for _, event := range namedagents.ClaudeHookEvents {
		if st.Events[event] != namedagents.HookCurrent {
			t.Errorf("status %s = %v", event, st.Events[event])
		}
	}

	// Remove takes out only ours, and the events left empty.
	if _, err := namedagents.RemoveClaudeHooks(path, bin, backups); err != nil {
		t.Fatal(err)
	}
	got = readJSON(t, path)
	if len(ourCommands(got)) != 0 {
		t.Errorf("left %v", ourCommands(got))
	}
	hooks := got["hooks"].(map[string]any)
	if len(hooks) != 1 || len(hooks["Stop"].([]any)) != 1 {
		t.Errorf("after remove: %v", hooks)
	}
}

func TestInstallClaudeHooksRefusesABrokenFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "settings.json")
	if err := os.WriteFile(path, []byte(`{"model": "opus",`), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := namedagents.InstallClaudeHooks(path, "/bin/agent", t.TempDir(), false); err == nil || !strings.Contains(err.Error(), "left alone") {
		t.Fatalf("err %v", err)
	}
	if b, _ := os.ReadFile(path); string(b) != `{"model": "opus",` {
		t.Errorf("the file was changed: %q", b)
	}
}

func TestInstallClaudeHooksNewFileAndSymlink(t *testing.T) {
	dir := t.TempDir()
	// No file yet.
	path := filepath.Join(dir, "settings.json")
	if _, err := namedagents.InstallClaudeHooks(path, "/bin/agent", filepath.Join(dir, "b"), false); err != nil {
		t.Fatal(err)
	}
	if n := len(ourCommands(readJSON(t, path))); n != len(namedagents.ClaudeHookEvents) {
		t.Fatalf("%d events", n)
	}
	// A settings file kept elsewhere and linked in stays linked.
	real := filepath.Join(dir, "dotfiles.json")
	if err := os.WriteFile(real, []byte(`{}`), 0o600); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(dir, "linked.json")
	if err := os.Symlink(real, link); err != nil {
		t.Fatal(err)
	}
	if _, err := namedagents.InstallClaudeHooks(link, "/bin/agent", filepath.Join(dir, "b"), false); err != nil {
		t.Fatal(err)
	}
	if fi, err := os.Lstat(link); err != nil || fi.Mode()&os.ModeSymlink == 0 {
		t.Fatalf("the link was replaced: %v %v", fi, err)
	}
	if n := len(ourCommands(readJSON(t, real))); n != len(namedagents.ClaudeHookEvents) {
		t.Errorf("target holds %d events", n)
	}
}

// Hooks Claude Code couldn't read are an error, not something to edit
// around; numbers come back as written; a group left empty stays an array.
func TestInstallClaudeHooksOddFiles(t *testing.T) {
	dir := t.TempDir()
	for _, bad := range []string{`{"hooks":{"Stop":[1]}}`, `{"hooks":{"Stop":"nope"}}`, `{"hooks":[]}`, `{"hooks":{"Stop":[{"hooks":[7]}]}}`} {
		path := filepath.Join(dir, "bad.json")
		os.WriteFile(path, []byte(bad), 0o600)
		if _, err := namedagents.InstallClaudeHooks(path, "/bin/agent", dir, false); err == nil || !strings.Contains(err.Error(), "left alone") {
			t.Errorf("%s: err %v", bad, err)
		}
		if b, _ := os.ReadFile(path); string(b) != bad {
			t.Errorf("%s was changed to %s", bad, b)
		}
	}
	path := filepath.Join(dir, "settings.json")
	os.WriteFile(path, []byte(`{"big":12345678901234567890123,"hooks":{"Stop":[{"matcher":"","hooks":[{"type":"command","command":"'/gone/agent' hook"}]}]}}`), 0o600)
	if _, err := namedagents.InstallClaudeHooks(path, "/bin/agent", dir, false); err != nil {
		t.Fatal(err)
	}
	if b, _ := os.ReadFile(path); !strings.Contains(string(b), "12345678901234567890123") {
		t.Errorf("number rewritten: %s", b)
	}
	if _, err := namedagents.RemoveClaudeHooks(path, "/bin/agent", dir); err != nil {
		t.Fatal(err)
	}
}

// One hook serves one binary: install won't take over another binary's
// entry while that binary exists, unless forced; a binary that is gone is
// replaced freely.
func TestInstallClaudeHooksAnotherBinary(t *testing.T) {
	dir := t.TempDir()
	other := filepath.Join(dir, "installed", "agent")
	os.MkdirAll(filepath.Dir(other), 0o755)
	os.WriteFile(other, []byte("#!/bin/sh\n"), 0o755)
	path := filepath.Join(dir, "settings.json")
	if _, err := namedagents.InstallClaudeHooks(path, other, dir, false); err != nil {
		t.Fatal(err)
	}
	dev := filepath.Join(dir, "dev", "agent")
	_, err := namedagents.InstallClaudeHooks(path, dev, dir, false)
	if err == nil || !strings.Contains(err.Error(), other) || !strings.Contains(err.Error(), "--force") {
		t.Fatalf("err %v", err)
	}
	r, err := namedagents.InstallClaudeHooks(path, dev, dir, true)
	if err != nil || !r.Changed || len(r.Others) != 1 {
		t.Fatalf("forced: %+v %v", r, err)
	}
	os.Remove(dev) // the binary the hook runs is gone: re-pointing it is free
	if _, err := namedagents.InstallClaudeHooks(path, other, dir, false); err != nil {
		t.Errorf("re-pointing a gone binary's entry: %v", err)
	}
}
