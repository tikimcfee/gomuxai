package util

import (
	"strings"

	"mvdan.cc/sh/v3/shell"
)

// ShellQuote quotes s as a single word for POSIX shells (and fish).
func ShellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// ShellJoin joins words into a command line a shell would split back into
// them, quoting only the words that need it.
func ShellJoin(words []string) string {
	quoted := make([]string, len(words))
	for i, w := range words {
		quoted[i] = w
		if w == "" || strings.IndexFunc(w, needsQuote) >= 0 {
			quoted[i] = ShellQuote(w)
		}
	}
	return strings.Join(quoted, " ")
}

func needsQuote(r rune) bool {
	switch {
	case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9':
		return false
	}
	return !strings.ContainsRune("@%+=:,./_-", r)
}

// SplitShellWords splits a command line into words as a POSIX shell would
// (quotes and escapes honored, nothing expanded), or nil if it doesn't
// parse.
func SplitShellWords(line string) []string {
	words, err := shell.Fields(line, func(string) string { return "" })
	if err != nil {
		return nil
	}
	return words
}
