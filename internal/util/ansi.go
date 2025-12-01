// Package util provides utility functions for the agent tool.
package util

import "regexp"

// ansiPattern matches ANSI escape sequences.
var ansiPattern = regexp.MustCompile(`\x1b\[[0-9;]*[a-zA-Z]`)

// StripANSI removes ANSI escape codes from a string.
func StripANSI(s string) string {
	return ansiPattern.ReplaceAllString(s, "")
}
