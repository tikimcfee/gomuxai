package buildinfo

import (
	"runtime"
	"runtime/debug"
	"testing"
)

func TestFromBuildInfo(t *testing.T) {
	platform := runtime.GOOS + "/" + runtime.GOARCH
	vcs := func(modified string) []debug.BuildSetting {
		return []debug.BuildSetting{
			{Key: "vcs", Value: "git"},
			{Key: "vcs.revision", Value: "c7253f98712b95668bc56c584c9696e3907be2fc"},
			{Key: "vcs.time", Value: "2026-10-24T21:18:44Z"},
			{Key: "vcs.modified", Value: modified},
		}
	}
	tests := []struct {
		name     string
		bi       *debug.BuildInfo
		id, line string
	}{
		{"no build info", nil, "devel", "devel (no VCS info) " + runtime.Version() + " " + platform},
		{"go test", &debug.BuildInfo{GoVersion: "go1.25.1", Main: debug.Module{Version: "(devel)"}},
			"devel", "devel (no VCS info) go1.25.1 " + platform},
		{"checkout, pseudo-version", &debug.BuildInfo{GoVersion: "go1.25.1",
			Main:     debug.Module{Version: "v0.0.0-20261024211844-c7253f98712b"},
			Settings: vcs("false")},
			"c7253f9", "devel c7253f9 2026-10-24T21:18:44Z go1.25.1 " + platform},
		{"checkout, modified", &debug.BuildInfo{GoVersion: "go1.25.1",
			Main:     debug.Module{Version: "v0.0.0-20261024211844-c7253f98712b+dirty"},
			Settings: vcs("true")},
			"c7253f9-dirty", "devel c7253f9-dirty 2026-10-24T21:18:44Z go1.25.1 " + platform},
		{"tag", &debug.BuildInfo{GoVersion: "go1.25.1", Main: debug.Module{Version: "v1.2.0"}, Settings: vcs("false")},
			"c7253f9", "v1.2.0 c7253f9 2026-10-24T21:18:44Z go1.25.1 " + platform},
		{"go install module@tag", &debug.BuildInfo{GoVersion: "go1.25.1", Main: debug.Module{Version: "v1.2.0"}},
			"v1.2.0", "v1.2.0 (no VCS info) go1.25.1 " + platform},
	}
	for _, tt := range tests {
		b := fromBuildInfo(tt.bi, tt.bi != nil)
		if got := b.ID(); got != tt.id {
			t.Errorf("%s: ID() = %q, want %q", tt.name, got, tt.id)
		}
		if got := b.String(); got != tt.line {
			t.Errorf("%s: String() = %q, want %q", tt.name, got, tt.line)
		}
	}
}

func TestIsPseudoVersion(t *testing.T) {
	for v, want := range map[string]bool{
		"v0.0.0-20261024211844-c7253f98712b":       true,
		"v1.2.4-0.20261024211844-c7253f98712b":     true,
		"v1.3.0-pre.0.20261024211844-c7253f98712b": true,
		"v1.2.0":       false,
		"v1.2.0-rc.1":  false,
		"v1.2.0-beta1": false,
	} {
		if got := isPseudoVersion(v); got != want {
			t.Errorf("isPseudoVersion(%q) = %v, want %v", v, got, want)
		}
	}
}
