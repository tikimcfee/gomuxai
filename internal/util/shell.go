package util

import "strings"

// ShellQuote quotes s as a single word for POSIX shells (and fish).
func ShellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}
