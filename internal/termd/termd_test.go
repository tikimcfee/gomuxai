package termd_test

import (
	"slices"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/termd"
)

func TestValidateNamespace(t *testing.T) {
	for _, valid := range []string{"agents", "agents-dev", "a1.b_c-2"} {
		if err := termd.ValidateNamespace(valid); err != nil {
			t.Errorf("ValidateNamespace(%q) = %v, want nil", valid, err)
		}
	}
	for _, reserved := range []string{"default", "wall"} {
		err := termd.ValidateNamespace(reserved)
		if err == nil || !strings.Contains(err.Error(), "reserved") {
			t.Errorf("ValidateNamespace(%q) = %v, want reserved refusal", reserved, err)
		}
	}
	for _, bad := range []string{"", "has space", "slash/ns", "../up", strings.Repeat("a", 65)} {
		if err := termd.ValidateNamespace(bad); err == nil {
			t.Errorf("ValidateNamespace(%q) = nil, want error", bad)
		}
	}
}

func TestCheckTestServer(t *testing.T) {
	t.Setenv(termd.TestServerEnv, "")
	t.Setenv("G3D_TERMD_DIR", "")
	if err := termd.CheckTestServer(); err == nil || !strings.Contains(err.Error(), "termdtest") {
		t.Fatalf("CheckTestServer without %s = %v, want error naming termdtest", termd.TestServerEnv, err)
	}

	dir := t.TempDir()
	t.Setenv(termd.TestServerEnv, dir)
	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv("G3D_TERMD_SOCKET", "")
	if err := termd.CheckTestServer(); err != nil {
		t.Fatalf("CheckTestServer with matching dir = %v, want nil", err)
	}

	t.Setenv("G3D_TERMD_SOCKET", dir+"/termd-wall.sock")
	if err := termd.CheckTestServer(); err == nil || !strings.Contains(err.Error(), "G3D_TERMD_SOCKET") {
		t.Fatalf("CheckTestServer with G3D_TERMD_SOCKET set = %v, want error", err)
	}
}

func TestParseSpecialKey(t *testing.T) {
	tests := []struct {
		in   string
		code string
		key  string
		mods []string
		text string
	}{
		{"Enter", "Enter", "Enter", nil, ""},
		{"Escape", "Escape", "Escape", nil, ""},
		{"Tab", "Tab", "Tab", nil, ""},
		{"BTab", "Tab", "Tab", []string{"Shift"}, ""},
		{"BSpace", "Backspace", "Backspace", nil, ""},
		{"Space", "Space", " ", nil, " "},
		{"Up", "ArrowUp", "ArrowUp", nil, ""},
		{"Down", "ArrowDown", "ArrowDown", nil, ""},
		{"Left", "ArrowLeft", "ArrowLeft", nil, ""},
		{"Right", "ArrowRight", "ArrowRight", nil, ""},
		{"PgUp", "PageUp", "PageUp", nil, ""},
		{"PPage", "PageUp", "PageUp", nil, ""},
		{"PgDn", "PageDown", "PageDown", nil, ""},
		{"NPage", "PageDown", "PageDown", nil, ""},
		{"DC", "Delete", "Delete", nil, ""},
		{"IC", "Insert", "Insert", nil, ""},
		{"Home", "Home", "Home", nil, ""},
		{"End", "End", "End", nil, ""},
		{"F5", "F5", "F5", nil, ""},
		{"C-c", "KeyC", "c", []string{"Control"}, "c"},
		{"c-d", "KeyD", "d", []string{"Control"}, "d"},
		{"M-b", "KeyB", "b", []string{"Alt"}, "b"},
		{"shift+enter", "Enter", "Enter", []string{"Shift"}, ""},
		{"ctrl+c", "KeyC", "c", []string{"Control"}, "c"},
		{"alt+left", "ArrowLeft", "ArrowLeft", []string{"Alt"}, ""},
		{"1", "Digit1", "1", nil, "1"},
		{"!", "Digit1", "!", []string{"Shift"}, "!"},
		{"A", "KeyA", "A", []string{"Shift"}, "A"},
		{"x", "KeyX", "x", nil, "x"},
		{"+", "Equal", "+", []string{"Shift"}, "+"},
		{"ctrl++", "Equal", "+", []string{"Control", "Shift"}, "+"},
	}
	for _, tt := range tests {
		ev, ok := termd.ParseSpecialKey(tt.in)
		if !ok {
			t.Errorf("ParseSpecialKey(%q): ok = false, want true", tt.in)
			continue
		}
		if ev.Code != tt.code || ev.Key != tt.key || !slices.Equal(ev.Mods, tt.mods) || ev.Text != tt.text {
			t.Errorf("ParseSpecialKey(%q) = (%q, %q, %v, %q), want (%q, %q, %v, %q)",
				tt.in, ev.Code, ev.Key, ev.Mods, ev.Text, tt.code, tt.key, tt.mods, tt.text)
		}
	}

	for _, literal := range []string{"-N2", "not-a-key", ""} {
		if _, ok := termd.ParseSpecialKey(literal); ok {
			t.Errorf("ParseSpecialKey(%q): ok = true, want false (literal fallback)", literal)
		}
	}
}

func TestCallerEnv(t *testing.T) {
	got := termd.CallerEnv([]string{
		"FOO=bar=baz",
		"EMPTY=",
		"BASH_FUNC_fn%%=() { :; }",
		"BAD-NAME=1",
		"INVALID_UTF8=\xff",
		"HUGE=" + strings.Repeat("x", 130*1024),
	})
	if got["FOO"] != "bar=baz" {
		t.Errorf("FOO = %q, want bar=baz", got["FOO"])
	}
	if v, ok := got["EMPTY"]; !ok || v != "" {
		t.Errorf("EMPTY = %q, %v; want empty present", v, ok)
	}
	for _, rejected := range []string{"BASH_FUNC_fn%%", "BAD-NAME", "INVALID_UTF8", "HUGE"} {
		if _, ok := got[rejected]; ok {
			t.Errorf("%s should be excluded from CallerEnv", rejected)
		}
	}
}

func TestResolveTerminalCwd(t *testing.T) {
	dir := t.TempDir()
	// macOS zsh OSC 7 reports file://<host>/<escaped-path>.
	osc7 := "file://some-host" + strings.ReplaceAll(dir, " ", "%20")
	if got := termd.ResolveTerminalCwd(osc7, "/tmp"); got != dir {
		t.Errorf("ResolveTerminalCwd(%q) = %q, want %q", osc7, got, dir)
	}
	if got := termd.ResolveTerminalCwd("file://some-host/nonexistent/path/xyz", dir); got != dir {
		t.Errorf("fallback on nonexistent OSC 7 = %q, want %q", got, dir)
	}
}
