// Package util provides utility functions for the agent tool.
package util

import (
	"regexp"
	"strconv"
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

// terminalEscapePattern matches the escape sequences in a raw terminal
// stream, for TerminalLines: OSC (titles, shell integration marks), ended by
// BEL or ST; DCS, SOS, PM, and APC, ended by ST; CSI, with its parameters,
// intermediates, and final byte captured; and the rest, ESC plus optional
// intermediates and a final byte (ESC 7, ESC ( B). A sequence cut off by the
// end of the stream still matches, so a log read while the agent writes
// doesn't leave half of one behind.
var terminalEscapePattern = regexp.MustCompile(
	`\x1b\][^\x07\x1b]*(?:\x07|\x1b\\|$)` +
		`|\x1b[PX^_][^\x1b]*(?:\x1b\\|$)` +
		`|\x1b\[([0-?]*)([ -/]*)([@-~]|$)` +
		`|\x1b[ -/]*(?:[0-~]|$)`)

// maxCursorMove caps the spaces one cursor movement becomes, so
// ESC [ 999999999 C can't allocate a gigabyte.
const maxCursorMove = 512

// TerminalLines turns a raw terminal stream, such as a tmux pipe-pane log of
// a TUI, into its lines of text. TUIs redraw by moving the cursor rather than
// printing spaces, so horizontal moves become spaces (or deleting them would
// glue words together), and carriage return and vertical moves break the
// line. Every other escape sequence and control character is dropped. Blank
// lines are dropped, trailing spaces trimmed, and consecutive duplicate lines
// (redraws) collapsed into one.
//
// Columns are counted in runes, which is close enough for a log: a wide
// character before a move shifts what follows by a column.
func TerminalLines(s string) []string {
	var lines []string
	var line strings.Builder
	col := 0
	breakLine := func() {
		text := strings.TrimRight(line.String(), " \t")
		if text != "" && (len(lines) == 0 || lines[len(lines)-1] != text) {
			lines = append(lines, text)
		}
		line.Reset()
		col = 0
	}
	// moveTo pads the line out to a 0-based column; moves left are dropped.
	moveTo := func(target int) {
		if n := min(target, col+maxCursorMove) - col; n > 0 {
			line.WriteString(strings.Repeat(" ", n))
			col += n
		}
	}
	text := func(t string) {
		for _, r := range t {
			switch {
			case r == '\r' || r == '\n':
				breakLine()
			case r == '\t':
				line.WriteRune(r)
				col++
			case r < 0x20 || r == 0x7f || (r >= 0x80 && r <= 0x9f):
			default:
				line.WriteRune(r)
				col++
			}
		}
	}

	pos := 0
	for _, m := range terminalEscapePattern.FindAllStringSubmatchIndex(s, -1) {
		text(s[pos:m[0]])
		pos = m[1]
		if m[2] < 0 || m[4] != m[5] {
			continue // not a CSI, or one with intermediates
		}
		params, final := s[m[2]:m[3]], s[m[6]:m[7]]
		switch final {
		case "C": // cursor forward n
			moveTo(col + csiParam(params, 0))
		case "G", "`": // cursor to column n
			moveTo(csiParam(params, 0) - 1)
		case "A", "B", "d", "e": // cursor up/down n, to row n: same column
			c := col
			breakLine()
			moveTo(c)
		case "E", "F": // cursor to the start of the next/previous line
			breakLine()
		case "H", "f": // cursor to row;column
			breakLine()
			moveTo(csiParam(params, 1) - 1)
		}
	}
	text(s[pos:])
	breakLine()
	return lines
}

// csiParam returns the i-th parameter of a CSI sequence, or 1 when it is
// missing or 0, the default for cursor movements. Private parameters
// (ESC [ ? 25 h) aren't numbers and also give 1.
func csiParam(params string, i int) int {
	fields := strings.Split(params, ";")
	if i >= len(fields) {
		return 1
	}
	n, err := strconv.Atoi(fields[i])
	if err != nil || n < 1 {
		return 1
	}
	return n
}
