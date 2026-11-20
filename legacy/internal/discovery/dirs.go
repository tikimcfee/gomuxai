// Package discovery finds every agent session this host can see: the
// transcripts Claude Code, agy and Kimi Code keep in their own directories,
// whether gomuxai spawned the session or a person ran the CLI in a
// terminal, and joins the ones a gomuxai agent records to that agent.
//
// Where sessions live is one table (Dirs), ported from glyph3d-rs
// crates/glyph-session-dirs: a configured path per harness wins, an empty
// one turns the harness off, and unset means the app's default locations
// that exist. What each session is follows glyph3d-rs
// agent_transcript/discovery.rs and glyph3d-js cli/sessions.go, with the
// fields docs/EVENTS.md names for a Session.
//
// It reads only the metadata a listing needs (the head and tail of a
// transcript, and the harnesses' small index files), never a whole
// conversation; internal/transcript parses those.
package discovery

import (
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// Harnesses whose sessions discovery reads.
const (
	HarnessClaude = "claude"
	HarnessAgy    = "agy"
	HarnessKimi   = "kimi"
)

// Harnesses is every harness, in scan and report order.
var Harnesses = []string{HarnessClaude, HarnessAgy, HarnessKimi}

// Dir origins: why a directory is scanned.
const (
	OriginConfig  = "config"  // named in the app config
	OriginDefault = "default" // the app's own default location, found on this machine
)

// Dir is one directory to scan for one harness's sessions.
type Dir struct {
	Harness string `json:"harness"`
	Path    string `json:"path"`
	Origin  string `json:"origin"`
}

// Overrides are the per-harness paths from the app config, as written (a
// leading ~ not yet expanded). Nil is unset, so the app defaults are used;
// an empty string turns the harness off.
type Overrides struct {
	Claude *string // claude_projects_dir
	Agy    *string // agy_brain_dir
	Kimi   *string // kimi_sessions_dir
}

func (o Overrides) get(harness string) *string {
	switch harness {
	case HarnessClaude:
		return o.Claude
	case HarnessAgy:
		return o.Agy
	case HarnessKimi:
		return o.Kimi
	}
	return nil
}

// ConfigKey names the app config field that overrides harness's location.
func ConfigKey(harness string) string {
	switch harness {
	case HarnessClaude:
		return "claude_projects_dir"
	case HarnessAgy:
		return "agy_brain_dir"
	case HarnessKimi:
		return "kimi_sessions_dir"
	}
	return ""
}

// DefaultCandidates returns the app-default locations of harness's
// sessions under home, in scan order. Each app keeps a dot-directory in the
// user's home on every OS it ships for. claudeConfigDir is
// $CLAUDE_CONFIG_DIR, which Claude Code honours in place of ~/.claude.
func DefaultCandidates(harness, home, claudeConfigDir string) []string {
	switch harness {
	case HarnessClaude:
		if claudeConfigDir != "" {
			return []string{filepath.Join(claudeConfigDir, "projects")}
		}
		return []string{filepath.Join(home, ".claude", "projects")}
	case HarnessAgy:
		return []string{
			filepath.Join(home, ".gemini", "antigravity", "brain"),     // the desktop app
			filepath.Join(home, ".gemini", "antigravity-cli", "brain"), // the CLI
		}
	case HarnessKimi:
		return []string{filepath.Join(home, ".kimi-code", "sessions")}
	}
	return nil
}

// Dirs resolves the scan list against this machine's home directory and
// $CLAUDE_CONFIG_DIR.
func Dirs(o Overrides) []Dir {
	home, _ := os.UserHomeDir()
	return ResolveDirs(o, home, os.Getenv("CLAUDE_CONFIG_DIR"))
}

// ResolveDirs is Dirs with the machine-dependent inputs passed in. A
// configured path is the only one scanned for its harness, and is kept even
// if it doesn't exist, so a report can say so; a default is kept only if it
// exists.
func ResolveDirs(o Overrides, home, claudeConfigDir string) []Dir {
	var dirs []Dir
	for _, h := range Harnesses {
		switch p := o.get(h); {
		case p != nil && *p == "":
		case p != nil:
			dirs = append(dirs, Dir{Harness: h, Path: expandHome(*p, home), Origin: OriginConfig})
		case home == "" && (h != HarnessClaude || claudeConfigDir == ""):
		default:
			for _, path := range DefaultCandidates(h, home, claudeConfigDir) {
				if isDir(path) {
					dirs = append(dirs, Dir{Harness: h, Path: path, Origin: OriginDefault})
				}
			}
		}
	}
	return dirs
}

// DescribeDirs says in one line what is scanned, e.g.
// "claude ~/.claude/projects (default); kimi ~/k (config, missing)".
func DescribeDirs(dirs []Dir, home string) string {
	if len(dirs) == 0 {
		return "no session directories: none configured and no app default found (set " +
			strings.Join([]string{ConfigKey(HarnessClaude), ConfigKey(HarnessAgy), ConfigKey(HarnessKimi)}, ", ") +
			" in the app config)"
	}
	parts := make([]string, len(dirs))
	for i, d := range dirs {
		missing := ""
		if !isDir(d.Path) {
			missing = ", missing"
		}
		parts[i] = fmt.Sprintf("%s %s (%s%s)", d.Harness, ContractHome(d.Path, home), d.Origin, missing)
	}
	return strings.Join(parts, "; ")
}

// ContractHome writes a path under home as ~/....
func ContractHome(path, home string) string {
	if home == "" {
		return path
	}
	if path == home {
		return "~"
	}
	if rest, ok := strings.CutPrefix(path, strings.TrimSuffix(home, "/")+"/"); ok {
		return "~/" + rest
	}
	return path
}

// expandHome expands a leading ~ or ~/ to home.
func expandHome(path, home string) string {
	if home == "" {
		return path
	}
	if path == "~" {
		return home
	}
	if rest, ok := strings.CutPrefix(path, "~/"); ok {
		return filepath.Join(home, rest)
	}
	return path
}

func isDir(path string) bool {
	info, err := os.Stat(path)
	return err == nil && info.IsDir()
}

// HarnessOf names the harness that writes a transcript at path, from the
// shape of the path alone, or "".
func HarnessOf(path string) string {
	slash := filepath.ToSlash(path)
	switch {
	case strings.HasSuffix(slash, "/agents/main/wire.jsonl"), strings.Contains(slash, "/.kimi-code/"):
		return HarnessKimi
	case transcript.AgyConversationDir(path) != "", strings.Contains(slash, "/antigravity-cli/brain/"),
		strings.Contains(slash, "/antigravity/brain/"):
		return HarnessAgy
	case strings.HasSuffix(slash, ".jsonl"):
		return HarnessClaude
	}
	return ""
}
