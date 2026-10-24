package cli

import (
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
)

func TestTailLogLines(t *testing.T) {
	tests := []struct {
		name     string
		padding  int // bytes of escape sequences after each line's text
		lines    int
		n        int
		expected []string
	}{
		{name: "small file", lines: 3, n: 50, expected: []string{"line 0", "line 1", "line 2"}},
		{name: "last n of a small file", lines: 10, n: 2, expected: []string{"line 8", "line 9"}},
		// Larger than the first window: the line cut off at its start must
		// not show up as a line of its own.
		{name: "tail of a large file", lines: 100_000, n: 3, expected: []string{"line 99997", "line 99998", "line 99999"}},
		// So much padding that the first window holds fewer than n lines.
		{name: "window grows", padding: 10_000, lines: 200, n: 40, expected: lineRange(160, 200)},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			var b strings.Builder
			pad := strings.Repeat("\x1b[K", tt.padding/3)
			for i := range tt.lines {
				fmt.Fprintf(&b, "\x1b[38;5;246mline %d\x1b[39m%s\r\n", i, pad)
			}
			path := filepath.Join(t.TempDir(), "agent.log")
			if err := os.WriteFile(path, []byte(b.String()), 0o600); err != nil {
				t.Fatal(err)
			}

			got, err := tailLogLines(path, tt.n)
			if err != nil {
				t.Fatalf("tailLogLines: %v", err)
			}
			if !slices.Equal(got, tt.expected) {
				t.Errorf("tailLogLines(n=%d) = %q, want %q", tt.n, got, tt.expected)
			}
		})
	}
}

func TestTailLogLinesMissingFile(t *testing.T) {
	if _, err := tailLogLines(filepath.Join(t.TempDir(), "none.log"), 10); !os.IsNotExist(err) {
		t.Errorf("err = %v, want a not-exist error", err)
	}
}

func lineRange(from, to int) []string {
	var lines []string
	for i := from; i < to; i++ {
		lines = append(lines, fmt.Sprintf("line %d", i))
	}
	return lines
}
