package util_test

import (
	"slices"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/util"
)

func TestStripANSI(t *testing.T) {
	tests := []struct {
		name     string
		input    string
		expected string
	}{
		{
			name:     "no ansi codes",
			input:    "hello world",
			expected: "hello world",
		},
		{
			name:     "color codes",
			input:    "\x1b[32mgreen\x1b[0m text",
			expected: "green text",
		},
		{
			name:     "cursor movement",
			input:    "\x1b[2J\x1b[Hstart",
			expected: "start",
		},
		{
			name:     "complex sequence",
			input:    "\x1b[1;31;40mBold Red on Black\x1b[0m",
			expected: "Bold Red on Black",
		},
		{
			name:     "empty string",
			input:    "",
			expected: "",
		},
		{
			name:     "only ansi codes",
			input:    "\x1b[0m\x1b[32m\x1b[0m",
			expected: "",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			result := util.StripANSI(tt.input)
			if result != tt.expected {
				t.Errorf("StripANSI(%q) = %q, want %q", tt.input, result, tt.expected)
			}
		})
	}
}

func TestSanitizeTerminal(t *testing.T) {
	tests := []struct {
		name     string
		input    string
		expected string
	}{
		{name: "plain text", input: "hello world", expected: "hello world"},
		{name: "keeps newline and tab", input: "a\tb\nc", expected: "a\tb\nc"},
		{name: "unicode", input: "❯ héllo", expected: "❯ héllo"},
		{name: "drops escape, keeps its printable tail", input: "\x1b[31mred\x1b[0m", expected: "[31mred[0m"},
		{name: "drops carriage return", input: "fake\rreal", expected: "fakereal"},
		{name: "drops bell and backspace", input: "a\x07b\x08c", expected: "abc"},
		{name: "drops DEL", input: "a\x7fb", expected: "ab"},
		{name: "drops C1 CSI", input: "a\u009b31mb", expected: "a31mb"},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if result := util.SanitizeTerminal(tt.input); result != tt.expected {
				t.Errorf("SanitizeTerminal(%q) = %q, want %q", tt.input, result, tt.expected)
			}
		})
	}
}

func TestTerminalLines(t *testing.T) {
	tests := []struct {
		name     string
		input    string
		expected []string
	}{
		{name: "empty", input: "", expected: nil},
		{name: "plain lines", input: "one\ntwo\n", expected: []string{"one", "two"}},
		{name: "carriage return breaks the line", input: "one\rtwo\r\nthree", expected: []string{"one", "two", "three"}},
		{
			name:     "cursor forward becomes spaces",
			input:    "\x1b[1mBash\x1b[22m(run\x1b[1Cthe\x1b[Ctests\x1b[3Cnow)",
			expected: []string{"Bash(run the tests   now)"},
		},
		{
			// Claude's TUI writes "  ⎿  Sent message to ..." this way.
			name:     "cursor to column pads to it",
			input:    "\x1b[38;5;246m  ⎿  \x1b[39mSent\x1b[11Gmessage\x1b[19Gto\x1b[22G\"air1\"",
			expected: []string{"  ⎿  Sent message to \"air1\""},
		},
		{name: "cursor to an earlier column is dropped", input: "abcdef\x1b[2Gx", expected: []string{"abcdefx"}},
		{
			name:     "cursor down keeps the column",
			input:    "filepath.Join(tmpDir,\x1b[K\r\x1b[2C\x1b[1B   \"logs\"),",
			expected: []string{"filepath.Join(tmpDir,", `     "logs"),`},
		},
		{
			name:     "cursor position starts a line at its column",
			input:    "top\x1b[22;3Hprompt",
			expected: []string{"top", "  prompt"},
		},
		{name: "huge cursor forward is capped", input: "a\x1b[999999999Cb", expected: []string{"a" + strings.Repeat(" ", 512) + "b"}},
		{
			name: "OSC sequences",
			input: "\x1b]0;✳ Claude Code\x07title" +
				"\x1b]3008;start=3c07;type=shell\x1b\\mark" +
				"\x1b]7501;cut off",
			expected: []string{"titlemark"},
		},
		{
			name:     "private modes, charsets, and save/restore",
			input:    "\x1b[?2026h\x1b[?25l\x1b(B\x1b7text\x1b8\x1b[?25h\x1b[>4;2m\x1b[2 q",
			expected: []string{"text"},
		},
		{name: "sequence cut off at the end", input: "done\x1b[38;5", expected: []string{"done"}},
		{name: "other controls", input: "a\x07b\x08c\x7fd\u009be", expected: []string{"abcde"}},
		{
			name:     "blank lines and trailing spaces",
			input:    "one   \r\n\r\n   \x1b[K\r\ntwo\x1b[5C\n",
			expected: []string{"one", "two"},
		},
		{
			name: "redraws collapse",
			input: "\x1b[H\r\x1b[18B✻ Thinking…\x1b[24;1H" +
				"\x1b[H\r\x1b[18B✻ Thinking…\x1b[24;1H" +
				"\x1b[H\r\x1b[18B✶ Thinking…\x1b[24;1H" +
				"\x1b[H\r\x1b[18B✻ Thinking…",
			expected: []string{"✻ Thinking…", "✶ Thinking…", "✻ Thinking…"},
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if result := util.TerminalLines(tt.input); !slices.Equal(result, tt.expected) {
				t.Errorf("TerminalLines(%q) = %q, want %q", tt.input, result, tt.expected)
			}
		})
	}
}
