// Package buildinfo says which build of the agent binary is running, from
// the module and VCS information the Go toolchain embeds.
package buildinfo

import (
	"runtime"
	"runtime/debug"
	"strings"
	"sync"
)

// Build is what the toolchain recorded about this binary.
type Build struct {
	Version   string // module version, "devel" for an untagged build
	Revision  string // short VCS revision, "" if none was embedded
	Modified  bool   // built from a working tree with uncommitted changes
	Time      string // commit time (RFC 3339), "" if unknown
	GoVersion string
	Platform  string // GOOS/GOARCH
}

var current = sync.OnceValue(func() Build {
	bi, ok := debug.ReadBuildInfo()
	return fromBuildInfo(bi, ok)
})

// Current returns this binary's build.
func Current() Build { return current() }

// ID returns a one-word id for this binary's build, for recording alongside
// what it did, e.g. "c7253f9-dirty", "v1.2.0", or "devel".
func ID() string { return current().ID() }

// ID returns a one-word id for b: the short revision (with "-dirty" if
// modified), else the module version.
func (b Build) ID() string {
	if b.Revision == "" {
		return b.Version
	}
	if b.Modified {
		return b.Revision + "-dirty"
	}
	return b.Revision
}

// String describes b on one line, e.g.
// "devel c7253f9-dirty 2026-10-24T21:18:44Z go1.25.1 linux/amd64".
func (b Build) String() string {
	parts := []string{b.Version}
	if b.Revision != "" {
		parts = append(parts, b.ID())
	} else {
		parts = append(parts, "(no VCS info)")
	}
	if b.Time != "" {
		parts = append(parts, b.Time)
	}
	return strings.Join(append(parts, b.GoVersion, b.Platform), " ")
}

func fromBuildInfo(bi *debug.BuildInfo, ok bool) Build {
	b := Build{
		Version:   "devel",
		GoVersion: runtime.Version(),
		Platform:  runtime.GOOS + "/" + runtime.GOARCH,
	}
	if !ok || bi == nil {
		return b
	}
	if bi.GoVersion != "" {
		b.GoVersion = bi.GoVersion
	}
	for _, s := range bi.Settings {
		switch s.Key {
		case "vcs.revision":
			b.Revision = s.Value
		case "vcs.time":
			b.Time = s.Value
		case "vcs.modified":
			b.Modified = s.Value == "true"
		}
	}
	if len(b.Revision) > 7 {
		b.Revision = b.Revision[:7]
	}
	// Builds in a checkout get a pseudo-version made of the commit time and
	// revision, which the other fields already say; only a tag adds news.
	v := strings.TrimSuffix(bi.Main.Version, "+dirty")
	if v != "" && v != "(devel)" && !isPseudoVersion(v) {
		b.Version = v
	}
	return b
}

// isPseudoVersion reports whether v is a Go pseudo-version, which ends in
// "<yyyymmddhhmmss>-<12 hex digits>", e.g. "v0.0.0-20261024211844-c7253f98712b".
func isPseudoVersion(v string) bool {
	i := strings.LastIndexByte(v, '-')
	if i < 15 || len(v)-i-1 != 12 {
		return false
	}
	stamp := v[i-14 : i]
	return strings.Trim(stamp, "0123456789") == "" && strings.Trim(v[i+1:], "0123456789abcdef") == ""
}
