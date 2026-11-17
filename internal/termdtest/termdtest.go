// Package termdtest runs a test binary against an isolated g3d-termd socket
// directory in /tmp, stopping every daemon started in it when the tests end.
package termdtest

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/termd"
)

// maxSocketPath keeps "<dir>/termd-<64-char-ns>.sock" within macOS's 104-byte
// sun_path limit for reasonable test namespaces.
const maxSocketPath = 103

// Run isolates m's tests in a private G3D_TERMD_DIR and stops every daemon in
// that directory before removing it.
func Run(m *testing.M) int {
	dir, err := socketDir()
	if err != nil {
		fmt.Fprintln(os.Stderr, "termdtest:", err)
		return 1
	}
	defer os.RemoveAll(dir)
	for _, k := range []string{"G3D_TERMD_SOCKET", "G3D_TERMD_ID"} {
		if err := os.Unsetenv(k); err != nil {
			fmt.Fprintln(os.Stderr, "termdtest:", err)
			return 1
		}
	}
	for k, v := range map[string]string{"G3D_TERMD_DIR": dir, termd.TestServerEnv: dir} {
		if err := os.Setenv(k, v); err != nil {
			fmt.Fprintln(os.Stderr, "termdtest:", err)
			return 1
		}
	}
	defer stopAll(dir)
	return m.Run()
}

// Require skips t if g3d-termd is not available, and fails t if this test
// binary is not running under termdtest.Run (or Setup).
func Require(t testing.TB) {
	t.Helper()
	if err := termd.CheckTestServer(); err != nil {
		t.Fatal(err)
	}
}

// Setup isolates a single test in its own private G3D_TERMD_DIR when the
// package's TestMain already runs tmuxtest.Run.
func Setup(t testing.TB) string {
	t.Helper()
	dir, err := socketDir()
	if err != nil {
		t.Fatal(err)
	}
	t.Setenv("G3D_TERMD_SOCKET", "")
	t.Setenv("G3D_TERMD_ID", "")
	t.Setenv("G3D_TERMD_DIR", dir)
	t.Setenv(termd.TestServerEnv, dir)
	t.Cleanup(func() {
		stopAll(dir)
		_ = os.RemoveAll(dir)
	})
	return dir
}

func socketDir() (string, error) {
	for _, parent := range []string{"/tmp", os.TempDir()} {
		dir, err := os.MkdirTemp(parent, "gmxt")
		if err != nil {
			continue
		}
		if err := os.Chmod(dir, 0o700); err != nil {
			_ = os.RemoveAll(dir)
			continue
		}
		sample := filepath.Join(dir, "termd-agents-test.sock")
		if len(sample) <= maxSocketPath {
			return dir, nil
		}
		_ = os.RemoveAll(dir)
	}
	return "", fmt.Errorf("no temp directory short enough for a termd socket (at most %d bytes)", maxSocketPath)
}

func stopAll(dir string) {
	entries, err := os.ReadDir(dir)
	if err != nil {
		return
	}
	for _, e := range entries {
		if strings.HasPrefix(e.Name(), "termd-") && strings.HasSuffix(e.Name(), ".sock") {
			_ = termd.StopSocket(filepath.Join(dir, e.Name()))
		}
	}
}
