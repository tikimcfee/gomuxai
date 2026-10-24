// Package util provides utility functions for the agent tool.
package util

import (
	"regexp"
	"strings"
)

// ansiPattern matches ANSI escape sequences.
var ansiPattern = regexp.MustCompile(`\x1b\[[0-9;]*[a-zA-Z]`)

// StripANSI removes ANSI escape codes from a string.
func StripANSI(s string) string {
	return ansiPattern.ReplaceAllString(s, "")
}

// SanitizeTerminal removes everything that could act on a terminal rather
// than print: ESC (and so every escape sequence), other C0 controls except
// newline and tab, carriage return, DEL, and C1 controls. For text from
// another host, which may be hostile.
func SanitizeTerminal(s string) string {
	return strings.Map(func(r rune) rune {
		switch {
		case r == '\n' || r == '\t':
			return r
		case r < 0x20 || r == 0x7f || (r >= 0x80 && r <= 0x9f):
			return -1
		}
		return r
	}, s)
}
