//go:build !darwin || !cgo

package macos

// Supported reports whether SpawnDisclaimed works in this build.
const Supported = false

// SpawnDisclaimed always fails outside darwin cgo builds.
func SpawnDisclaimed(path string, argv, env []string) (int, error) {
	return 0, ErrUnsupported
}
