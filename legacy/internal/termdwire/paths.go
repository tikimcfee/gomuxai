package termdwire

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

// ValidateName checks that s is 1 to 64 bytes of [A-Za-z0-9._-].
func ValidateName(what, s string) error {
	if len(s) == 0 || len(s) > 64 {
		return fmt.Errorf("%s %q must be 1 to 64 bytes of [A-Za-z0-9._-]", what, s)
	}
	for i := 0; i < len(s); i++ {
		b := s[i]
		ok := (b >= 'A' && b <= 'Z') || (b >= 'a' && b <= 'z') || (b >= '0' && b <= '9') ||
			b == '.' || b == '_' || b == '-'
		if !ok {
			return fmt.Errorf("%s %q must be 1 to 64 bytes of [A-Za-z0-9._-]", what, s)
		}
	}
	return nil
}

// SocketDir returns the directory where g3d-termd sockets live: $G3D_TERMD_DIR
// when set, else SocketDirFor(UserRuntimeDir(), os.Getuid()). It never consults
// $XDG_RUNTIME_DIR or $TMPDIR so clients and daemons in stripped environments
// resolve the same socket directory as desktop sessions.
func SocketDir() string {
	if d := os.Getenv("G3D_TERMD_DIR"); d != "" {
		return d
	}
	return SocketDirFor(UserRuntimeDir(), os.Getuid())
}

// SocketDirFor returns <runtime>/g3d when runtime is non-empty, else /tmp/g3d-<uid>.
func SocketDirFor(runtime string, uid int) string {
	if runtime != "" {
		return filepath.Join(runtime, "g3d")
	}
	return fmt.Sprintf("/tmp/g3d-%d", uid)
}

// SocketPath returns the Unix socket path for namespace ns.
func SocketPath(ns string) string {
	return filepath.Join(SocketDir(), "termd-"+ns+".sock")
}

// LogPath returns the daemon log path for namespace ns.
func LogPath(ns string) string {
	if d := os.Getenv("G3D_TERMD_DIR"); d != "" {
		return filepath.Join(d, "termd-"+ns+".log")
	}
	if s := os.Getenv("XDG_STATE_HOME"); s != "" {
		return filepath.Join(s, "g3d", "termd-"+ns+".log")
	}
	if h := os.Getenv("HOME"); h != "" {
		return filepath.Join(h, ".local", "state", "g3d", "termd-"+ns+".log")
	}
	return filepath.Join(SocketDir(), "termd-"+ns+".log")
}

// DaemonEnv returns environ with G3D_TERMD_SOCKET, G3D_TERMD_ID, and any
// G3D_RELAY_* variables stripped so a spawned g3d-termd daemon never inherits
// terminal handles or relay capability tokens/controls.
func DaemonEnv(environ []string) []string {
	out := make([]string, 0, len(environ))
	for _, kv := range environ {
		k, _, _ := strings.Cut(kv, "=")
		if k == "G3D_TERMD_SOCKET" || k == "G3D_TERMD_ID" || strings.HasPrefix(k, "G3D_RELAY_") {
			continue
		}
		out = append(out, kv)
	}
	return out
}
