//go:build !linux && !darwin

package claudereg

// procStart: no start time gomuxai can match to Claude Code's here, so
// entries are Unproven.
func procStart(int) (string, bool) { return "", false }

func pidDomain() string { return "" }

func zombie(int) bool { return false }
