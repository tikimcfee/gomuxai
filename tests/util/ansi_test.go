package util_test

import (
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
