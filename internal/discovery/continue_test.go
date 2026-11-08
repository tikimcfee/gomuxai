package discovery

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestContinue(t *testing.T) {
	root := t.TempDir()
	dir := filepath.Join(root, "shop")
	child := filepath.Join(dir, "web")
	for _, d := range []string{dir, filepath.Join(child, "src"), filepath.Join(root, "other")} {
		if err := os.MkdirAll(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	at := func(min int) time.Time { return time.Date(2026, 11, 8, 12, min, 0, 0, time.UTC) }
	sess := func(id, harness, cwd string, min int) Session {
		return Session{SessionID: id, Harness: harness, Path: "/t/" + id + ".jsonl", Cwd: cwd, Mtime: at(min), Status: StatusEnded}
	}
	sub := sess("c-sub", HarnessClaude, dir, 50)
	sub.ParentSession = &ParentSession{SessionID: "c-new"}
	sessions := []Session{
		sess("c-old", HarnessClaude, dir, 10),
		sess("c-new", HarnessClaude, dir, 20),
		sub, // newest in dir, but a subagent's
		sess("c-other", HarnessClaude, filepath.Join(root, "other"), 40),
		sess("c-child", HarnessClaude, child, 45),
		sess("a-here", HarnessAgy, dir, 5),
		sess("a-child", HarnessAgy, child, 30),
		sess("k-here", HarnessKimi, dir, 59),
		{SessionID: "c-nocwd", Harness: HarnessClaude, Path: "/t/x.jsonl", Mtime: at(55)},
	}

	for _, tc := range []struct {
		harness, dir, want string
	}{
		{HarnessClaude, dir, "c-new"},                        // the newest main session here; not other dirs, not the subagent
		{HarnessClaude, dir + "/", "c-new"},                  // the same directory spelled another way
		{HarnessClaude, child, "c-child"},                    // only the child dir's own
		{HarnessClaude, filepath.Join(root, "none"), ""},     // Claude doesn't fall back to a parent or child
		{HarnessAgy, dir, "a-here"},                          // agy's own workspace first, though a child's is newer
		{HarnessAgy, root, "a-child"},                        // then the newest in a child directory
		{HarnessAgy, filepath.Join(child, "src"), "a-child"}, // or a parent one
		{HarnessAgy, filepath.Join(root, "other"), ""},
	} {
		got, ok := Continue(sessions, tc.harness, tc.dir)
		if got.SessionID != tc.want || ok != (tc.want != "") {
			t.Errorf("Continue(%s, %s) = %q, %v; want %q", tc.harness, tc.dir, got.SessionID, ok, tc.want)
		}
	}

	// Symlinked spellings of a directory are one directory.
	link := filepath.Join(root, "link")
	if err := os.Symlink(dir, link); err != nil {
		t.Fatal(err)
	}
	if got, _ := Continue(sessions, HarnessClaude, link); got.SessionID != "c-new" {
		t.Errorf("Continue via a symlink = %q; want c-new", got.SessionID)
	}
}
