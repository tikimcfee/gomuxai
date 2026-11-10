package tmux

import (
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
)

// fallbackDirs are where package managers install tmux, looked in when PATH
// has no tmux. A command ssh runs gets a non-login shell, which skips the
// profile that puts these on PATH: on macOS, "ssh host agent attach x"
// runs with PATH=/usr/bin:/bin:/usr/sbin:/sbin, without Homebrew's
// /opt/homebrew/bin.
var fallbackDirs = []string{
	"/opt/homebrew/bin",              // Homebrew, Apple silicon
	"/usr/local/bin",                 // Homebrew, Intel Macs; source installs
	"/opt/local/bin",                 // MacPorts
	"/home/linuxbrew/.linuxbrew/bin", // Homebrew on Linux
	"/run/current-system/sw/bin",     // NixOS
	"/nix/var/nix/profiles/default/bin",
}

// Binary is the tmux to run: the one on PATH, else the first found in
// fallbackDirs. It looks each time, as exec.Command("tmux") would.
func Binary() (string, error) {
	return findBinary(exec.LookPath, fallbackDirs)
}

func findBinary(lookPath func(string) (string, error), dirs []string) (string, error) {
	if path, err := lookPath("tmux"); err == nil {
		return path, nil
	}
	for _, dir := range dirs {
		path := filepath.Join(dir, "tmux")
		if info, err := os.Stat(path); err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0 {
			return path, nil
		}
	}
	return "", fmt.Errorf("tmux isn't installed: it isn't on PATH (%s) or in %s: install it, or put its directory on PATH",
		os.Getenv("PATH"), strings.Join(dirs, ", "))
}
