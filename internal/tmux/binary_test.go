package tmux

import (
	"errors"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func noTmuxOnPath(string) (string, error) { return "", errors.New("not found") }

func TestFindBinaryPrefersPath(t *testing.T) {
	onPath := func(string) (string, error) { return "/usr/bin/tmux", nil }
	got, err := findBinary(onPath, []string{t.TempDir()})
	if err != nil || got != "/usr/bin/tmux" {
		t.Fatalf("findBinary = %q, %v; want /usr/bin/tmux", got, err)
	}
}

// A command ssh runs on a Mac has a PATH without Homebrew's directory: tmux
// is found there anyway.
func TestFindBinaryFallsBackToKnownDirs(t *testing.T) {
	empty, brew := t.TempDir(), t.TempDir()
	tmux := filepath.Join(brew, "tmux")
	if err := os.WriteFile(tmux, []byte("#!/bin/sh\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	got, err := findBinary(noTmuxOnPath, []string{empty, brew})
	if err != nil || got != tmux {
		t.Fatalf("findBinary = %q, %v; want %s", got, err, tmux)
	}
}

func TestFindBinarySkipsNonExecutable(t *testing.T) {
	dir := t.TempDir()
	if err := os.WriteFile(filepath.Join(dir, "tmux"), []byte("not a program"), 0o644); err != nil {
		t.Fatal(err)
	}
	if got, err := findBinary(noTmuxOnPath, []string{dir}); err == nil {
		t.Fatalf("findBinary = %q; want an error for a tmux that isn't executable", got)
	}
}

// The error names the PATH it searched and the directories, so one from
// another host (over ssh) shows why that host's shell didn't find it.
func TestFindBinaryErrorSaysWhereItLooked(t *testing.T) {
	t.Setenv("PATH", "/usr/bin:/bin")
	dir := t.TempDir()
	_, err := findBinary(noTmuxOnPath, []string{dir})
	if err == nil {
		t.Fatal("findBinary found a tmux in an empty directory")
	}
	for _, want := range []string{"tmux isn't installed", "PATH (/usr/bin:/bin)", dir} {
		if !strings.Contains(err.Error(), want) {
			t.Errorf("error %q doesn't say %q", err, want)
		}
	}
}
