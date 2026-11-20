//go:build !linux

package discovery

// newLockProbe asks for each lock with flockHeld: there is no /proc/locks
// to read without taking one.
func newLockProbe() func(path string) bool {
	return flockHeld
}
