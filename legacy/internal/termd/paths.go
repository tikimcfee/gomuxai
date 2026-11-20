package termd

import (
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/termdwire"
)

// fallbackDirs are common installation locations checked when PATH omits
// package-manager or user-local bin directories (e.g. non-login ssh shells).
var fallbackDirs = []string{
	"/opt/homebrew/bin",
	"/usr/local/bin",
	"/opt/local/bin",
	"/home/linuxbrew/.linuxbrew/bin",
	"/run/current-system/sw/bin",
	"/nix/var/nix/profiles/default/bin",
}

// ValidateNamespace checks that ns is 1..64 bytes of [A-Za-z0-9._-] and is
// neither "default" nor "wall", which belong to interactive and wall sessions.
func ValidateNamespace(ns string) error {
	if err := termdwire.ValidateName("termd namespace", ns); err != nil {
		return err
	}
	if ns == "default" || ns == "wall" {
		return fmt.Errorf("termd namespace %q is reserved for interactive and wall sessions; use a dedicated namespace such as %q", ns, DefaultNamespace)
	}
	return nil
}

// ValidateTerminalID checks that id is 1..64 bytes of [A-Za-z0-9._-].
func ValidateTerminalID(id string) error {
	return termdwire.ValidateName("terminal id", id)
}

// CheckTestServer refuses to touch the user's real termd sockets from a test
// binary unless internal/termdtest configured a private G3D_TERMD_DIR.
func CheckTestServer() error {
	if !testing.Testing() {
		return nil
	}
	dir := os.Getenv(TestServerEnv)
	switch {
	case dir == "":
		return errors.New("a test tried to use termd without a private socket directory: " +
			"call termdtest.Run from TestMain or termdtest.Setup(t) (internal/termdtest)")
	case os.Getenv("G3D_TERMD_SOCKET") != "":
		return fmt.Errorf("a test ran termd with $G3D_TERMD_SOCKET set (%s): "+
			"unset G3D_TERMD_SOCKET in tests using termd", os.Getenv("G3D_TERMD_SOCKET"))
	case os.Getenv("G3D_TERMD_DIR") != dir:
		return fmt.Errorf("a test ran termd with G3D_TERMD_DIR %q, not its private dir %q",
			os.Getenv("G3D_TERMD_DIR"), dir)
	}
	return nil
}

// SocketDir returns the directory where g3d-termd sockets live.
func SocketDir() string {
	return termdwire.SocketDir()
}

// SocketPath returns the Unix socket path for namespace ns.
func SocketPath(ns string) string {
	return termdwire.SocketPath(ns)
}

// LogPath returns the daemon log path for namespace ns.
func LogPath(ns string) string {
	return termdwire.LogPath(ns)
}

// DaemonBinary locates g3d-termd: explicit, $G3D_TERMD_BIN, beside the running
// executable, PATH, or common install directories.
func DaemonBinary(explicit string) (string, error) {
	if explicit != "" {
		if isExecutable(explicit) {
			return explicit, nil
		}
		return "", fmt.Errorf("termd binary %s: not an executable file", explicit)
	}
	if envBin := os.Getenv("G3D_TERMD_BIN"); envBin != "" {
		if isExecutable(envBin) {
			return envBin, nil
		}
		return "", fmt.Errorf("G3D_TERMD_BIN=%s: not an executable file", envBin)
	}
	return findTool("g3d-termd", "")
}

// AttachBinary locates g3d: explicit, $G3D_BIN, beside daemonBin, beside the
// running executable, PATH, or common install directories.
func AttachBinary(explicit, daemonBin string) (string, error) {
	if explicit != "" {
		if isExecutable(explicit) {
			return explicit, nil
		}
		return "", fmt.Errorf("g3d binary %s: not an executable file", explicit)
	}
	if envBin := os.Getenv("G3D_BIN"); envBin != "" {
		if isExecutable(envBin) {
			return envBin, nil
		}
		return "", fmt.Errorf("G3D_BIN=%s: not an executable file", envBin)
	}
	siblingDir := ""
	if daemonBin != "" {
		siblingDir = filepath.Dir(daemonBin)
	} else if d, err := DaemonBinary(""); err == nil {
		siblingDir = filepath.Dir(d)
	}
	return findTool("g3d", siblingDir)
}

func findTool(name, extraDir string) (string, error) {
	var looked []string
	checkDir := func(dir string) string {
		if dir == "" {
			return ""
		}
		p := filepath.Join(dir, name)
		looked = append(looked, p)
		if isExecutable(p) {
			return p
		}
		return ""
	}
	if p := checkDir(extraDir); p != "" {
		return p, nil
	}
	if exe, err := os.Executable(); err == nil {
		if p := checkDir(filepath.Dir(exe)); p != "" {
			return p, nil
		}
	}
	if p, err := exec.LookPath(name); err == nil {
		return p, nil
	}
	looked = append(looked, "$PATH")
	var dirs []string
	if home, err := os.UserHomeDir(); err == nil && home != "" {
		dirs = append(dirs, filepath.Join(home, ".local", "bin"), filepath.Join(home, ".cargo", "bin"))
	}
	dirs = append(dirs, fallbackDirs...)
	for _, dir := range dirs {
		p := filepath.Join(dir, name)
		if isExecutable(p) {
			return p, nil
		}
	}
	return "", fmt.Errorf("no %s binary found (looked at %s and %s)", name, strings.Join(looked, ", "), strings.Join(dirs, ", "))
}

func isExecutable(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.Mode().IsRegular() && info.Mode()&0o111 != 0
}
