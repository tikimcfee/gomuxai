package service

import "testing"

func TestIsSlashCommand(t *testing.T) {
	for msg, want := range map[string]bool{
		"/compact":                true,
		"/model claude-fable-5-1": true,
		"/model\nopus":            true,
		"/plugin:skill args":      true,
		"/clear":                  true,
		"/tmp/out.txt is ready":   false,
		"/":                       false,
		"/ compact":               false,
		"/1abc":                   false,
		"hi /compact":             false,
		"[from lead@h1] /compact": false,
		"/etc/hosts":              false,
		"/usr/bin/env is missing": false,
	} {
		if got := IsSlashCommand(msg); got != want {
			t.Errorf("IsSlashCommand(%q) = %v, want %v", msg, got, want)
		}
	}
}
