// Package macos holds what the agent bus needs to reach the local network
// on macOS.
//
// macOS (15+) blocks connections to the local network unless the process's
// "responsible" app has the Local Network permission. A process started
// inside tmux has no responsible app that can ever be granted it: tmux
// detaches from the terminal app that launched it, so a bus dialing its hub
// over the LAN fails with "no route to host" and macOS shows no alert.
//
// The fix has two halves. The build (make macos) embeds an Info.plist in the
// binary and signs it with an Apple-issued identity, which lets the binary
// hold the permission itself; macOS keys the grant on the bundle identifier,
// so rebuilds keep it. And the bus re-spawns itself with responsibility
// disclaimed (SpawnDisclaimed), so macOS asks about this binary instead of
// whatever tmux's chain resolves to. See TN3179.
package macos

import (
	"debug/macho"
	"errors"
)

// ErrUnsupported means this build can't spawn disclaimed processes: it isn't
// a darwin build, or it was built without cgo.
var ErrUnsupported = errors.New("disclaimed spawn needs a darwin build with cgo")

// HasInfoPlist reports whether the Mach-O executable at path embeds an
// Info.plist (a __TEXT,__info_plist section), which is what lets a
// standalone binary hold its own Local Network permission. It is false for
// anything that isn't a Mach-O file.
func HasInfoPlist(path string) bool {
	f, err := macho.Open(path)
	if err != nil {
		return false
	}
	defer f.Close()
	for _, s := range f.Sections {
		if s.Seg == "__TEXT" && s.Name == "__info_plist" {
			return true
		}
	}
	return false
}
