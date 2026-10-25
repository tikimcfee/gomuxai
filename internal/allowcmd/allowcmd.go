// Package allowcmd decides whether a shell command line is exactly one run
// of a given program with a safe subcommand, so a hook can approve it
// without a permission prompt. It errs toward "no": anything it can't prove
// safe from the text alone (a chain, an expansion, a variable, a path that
// isn't the program) is refused, and the caller's normal permission flow
// decides.
package allowcmd

import (
	"errors"
	"fmt"
	"os/exec"
	"path/filepath"
	"slices"
	"strings"

	"mvdan.cc/sh/v3/syntax"
)

// Checker checks command lines against one program and its safe subcommands.
type Checker struct {
	// Program is the path of the binary that may run. Symlinks to it, and a
	// bare name that LookPath finds it under, count as the same program.
	Program string
	// Subcommands are the safe subcommands, one or two words each
	// ("send", "state get").
	Subcommands []string
	// LookPath finds a bare command name; nil means exec.LookPath.
	LookPath func(name string) (string, error)
}

// leadingFlags are the root flags allowed before the subcommand. Others
// (--backend-config reads a file) are refused there.
var leadingFlags = map[string]bool{"-q": true, "--quiet": true}

// Check returns nil if command, run in the directory cwd, is a single run of
// c.Program with one of c.Subcommands, and otherwise an error saying why not.
func (c Checker) Check(command, cwd string) error {
	words, err := literalWords(command)
	if err != nil {
		return err
	}
	if len(words) == 0 {
		return errors.New("no command")
	}
	if err := c.sameProgram(words[0], cwd); err != nil {
		return err
	}
	args := words[1:]
	for len(args) > 0 && leadingFlags[args[0]] {
		args = args[1:]
	}
	for _, sub := range c.Subcommands {
		parts := strings.Fields(sub)
		if len(args) >= len(parts) && slices.Equal(args[:len(parts)], parts) {
			return nil
		}
	}
	if len(args) == 0 {
		return errors.New("no subcommand")
	}
	return fmt.Errorf("subcommand %q isn't a safe one", args[0])
}

// sameProgram reports whether prog, as the shell would find it from cwd,
// is c.Program.
func (c Checker) sameProgram(prog, cwd string) error {
	path := prog
	if !strings.Contains(prog, "/") {
		look := c.LookPath
		if look == nil {
			look = exec.LookPath
		}
		found, err := look(prog)
		if err != nil {
			return fmt.Errorf("%s isn't on PATH: %w", prog, err)
		}
		path = found
	}
	if !filepath.IsAbs(path) {
		if cwd == "" || !filepath.IsAbs(cwd) {
			return fmt.Errorf("relative path %s with no working directory", path)
		}
		path = filepath.Join(cwd, path)
	}
	got, err := filepath.EvalSymlinks(path)
	if err != nil {
		return fmt.Errorf("resolving %s: %w", path, err)
	}
	want, err := filepath.EvalSymlinks(c.Program)
	if err != nil {
		return fmt.Errorf("resolving %s: %w", c.Program, err)
	}
	if got != want {
		return fmt.Errorf("%s is %s, not %s", prog, got, want)
	}
	return nil
}

// literalWords parses command as one simple command and returns its words
// after quote removal. It refuses anything whose words or effect the text
// alone doesn't fix: more than one command, pipes and chains, subshells,
// assignments, expansions of any kind, globs, comments, and redirections
// other than to /dev/null or between stdout and stderr. Unquoted text is
// limited to characters that are plain in both bash and zsh.
func literalWords(command string) ([]string, error) {
	// Comments are kept to be refused: zsh without interactive_comments
	// reads "#" as text.
	file, err := syntax.NewParser(syntax.Variant(syntax.LangBash), syntax.KeepComments(true)).Parse(strings.NewReader(command), "")
	if err != nil {
		return nil, fmt.Errorf("parsing: %w", err)
	}
	if len(file.Stmts) != 1 {
		return nil, fmt.Errorf("%d commands, not one", len(file.Stmts))
	}
	if len(file.Last) > 0 {
		return nil, errors.New("has a comment")
	}
	st := file.Stmts[0]
	switch {
	case len(st.Comments) > 0:
		return nil, errors.New("has a comment")
	case st.Negated, st.Background, st.Coprocess, st.Disown:
		return nil, errors.New("not a plain command (!, &, or |&)")
	}
	for _, r := range st.Redirs {
		if !harmlessRedirect(r) {
			return nil, errors.New("has a redirection")
		}
	}
	call, ok := st.Cmd.(*syntax.CallExpr)
	if !ok {
		return nil, fmt.Errorf("not a simple command (%T)", st.Cmd)
	}
	if len(call.Assigns) > 0 {
		return nil, errors.New("sets variables")
	}
	words := make([]string, len(call.Args))
	for i, w := range call.Args {
		if words[i], err = literalWord(w); err != nil {
			return nil, err
		}
	}
	return words, nil
}

// harmlessRedirect: >/dev/null, 2>/dev/null, &>/dev/null (and >> forms),
// 2>&1, and >&2.
func harmlessRedirect(r *syntax.Redirect) bool {
	if r.Hdoc != nil {
		return false
	}
	if r.N != nil && r.N.Value != "1" && r.N.Value != "2" {
		return false
	}
	target, err := literalWord(r.Word)
	if err != nil {
		return false
	}
	switch r.Op {
	case syntax.RdrOut, syntax.AppOut, syntax.RdrAll, syntax.AppAll:
		return target == "/dev/null"
	case syntax.DplOut:
		return target == "1" || target == "2"
	}
	return false
}

// literalWord returns w after quote removal, if it is made only of plain
// unquoted text, '...' and "..." without expansions.
func literalWord(w *syntax.Word) (string, error) {
	if w == nil {
		return "", errors.New("missing word")
	}
	var b strings.Builder
	for i, part := range w.Parts {
		switch p := part.(type) {
		case *syntax.Lit:
			if err := plainUnquoted(p.Value, i == 0); err != nil {
				return "", err
			}
			b.WriteString(p.Value)
		case *syntax.SglQuoted:
			if p.Dollar {
				return "", errors.New("$'...' quoting")
			}
			b.WriteString(p.Value)
		case *syntax.DblQuoted:
			if p.Dollar {
				return "", errors.New(`$"..." quoting`)
			}
			for _, inner := range p.Parts {
				lit, ok := inner.(*syntax.Lit)
				if !ok {
					return "", errors.New("expansion inside double quotes")
				}
				b.WriteString(unescapeDouble(lit.Value))
			}
		default:
			return "", fmt.Errorf("expansion (%T)", part)
		}
	}
	return b.String(), nil
}

// plainUnquoted refuses unquoted text with anything bash or zsh might
// expand or treat specially: globs, braces, tildes, backslashes, history
// and extended-glob characters, and zsh's leading "=" (=cmd is a path).
func plainUnquoted(s string, wordStart bool) error {
	if wordStart && strings.HasPrefix(s, "=") {
		return fmt.Errorf("unquoted leading = in %q", s)
	}
	for _, r := range s {
		switch {
		case r >= 'a' && r <= 'z', r >= 'A' && r <= 'Z', r >= '0' && r <= '9':
		case strings.ContainsRune("_@%+=:,./-", r):
		default:
			return fmt.Errorf("unquoted %q", r)
		}
	}
	return nil
}

// unescapeDouble removes the backslashes that escape a character inside
// double quotes: before $, `, ", \, and newline (which goes too).
func unescapeDouble(s string) string {
	var b strings.Builder
	for i := 0; i < len(s); i++ {
		if s[i] == '\\' && i+1 < len(s) {
			switch s[i+1] {
			case '$', '`', '"', '\\':
				b.WriteByte(s[i+1])
				i++
				continue
			case '\n':
				i++
				continue
			}
		}
		b.WriteByte(s[i])
	}
	return b.String()
}
