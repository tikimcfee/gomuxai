package discovery

import (
	"fmt"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

// claudeSession writes a Claude session run in cwd, in the project folder
// Claude Code names for it, last written at at; and, with subs, that many
// subagents, written just before it.
func claudeSession(t *testing.T, projects, id, cwd string, at time.Time, subs int) string {
	t.Helper()
	folder := strings.Map(func(r rune) rune {
		if isAlnum(r) {
			return r
		}
		return '-'
	}, cwd)
	path := filepath.Join(projects, folder, id+".jsonl")
	write(t, path, lines(
		`{"type":"user","sessionId":"`+id+`","cwd":"`+cwd+`","message":{"role":"user","content":"hi"}}`,
		`{"type":"custom-title","customTitle":"Title of `+id+`","sessionId":"`+id+`"}`))
	touch(t, path, at)
	for i := range subs {
		sub := filepath.Join(projects, folder, id, "subagents", fmt.Sprintf("agent-%s-%d.jsonl", id, i))
		write(t, sub, lines(`{"type":"user","isSidechain":true,"agentId":"x","sessionId":"`+id+`","cwd":"`+cwd+`","message":{"role":"user","content":"sub"}}`))
		touch(t, sub, at.Add(-time.Minute))
	}
	return path
}

func ids(sessions []Session) []string {
	var out []string
	for _, s := range sessions {
		out = append(out, s.SessionID)
	}
	return out
}

// probed names the index's sessions that have been read.
func (x *Index) probedIDs() []string {
	var out []string
	for _, s := range x.j.sessions {
		if s.probed {
			out = append(out, s.SessionID)
		}
	}
	slices.Sort(out)
	return out
}

// The index lists every session without reading one; Select picks the
// newest groups (a session with its subagents) and counts the rest, and
// Resolve reads only what was picked.
func TestIndex_SelectLimit(t *testing.T) {
	projects := filepath.Join(t.TempDir(), "projects")
	now := time.Now()
	for i := range 5 {
		subs := 0
		if i == 1 {
			subs = 2
		}
		claudeSession(t, projects, fmt.Sprintf("s%d", i), "/home/u/app", now.Add(-time.Duration(i)*time.Hour), subs)
	}
	x := NewIndex([]Dir{{HarnessClaude, projects, OriginDefault}}, nil)
	if got := x.probedIDs(); len(got) != 0 {
		t.Fatalf("listing read %v, want none", got)
	}
	if got := len(x.Sessions()); got != 7 {
		t.Fatalf("listed %d sessions, want 5 and 2 subagents", got)
	}

	picked, left := x.Select(Query{Limit: 2})
	if got := ids(picked); !slices.Equal(got, []string{"s0", "s1", "agent-s1-0", "agent-s1-1"}) || left != 3 {
		t.Errorf("Select(limit 2) = %v, %d left, want s0, s1 and its subagents, 3 left", got, left)
	}
	if got := x.probedIDs(); len(got) != 0 {
		t.Errorf("Select read %v, want none", got)
	}
	resolved := x.Resolve(picked, Live{Claude: map[string]string{}})
	if got := x.probedIDs(); !slices.Equal(got, []string{"agent-s1-0", "agent-s1-1", "s0", "s1"}) {
		t.Errorf("Resolve read %v, want only the picked", got)
	}
	for _, s := range resolved {
		if s.Cwd != "/home/u/app" || s.Status != StatusEnded {
			t.Errorf("resolved %s: cwd %q status %q", s.SessionID, s.Cwd, s.Status)
		}
	}
	if resolved[0].Title != "Title of s0" {
		t.Errorf("resolved[0] = %+v, want s0 with its title", resolved[0])
	}

	if picked, left := x.Select(Query{}); len(picked) != 7 || left != 0 {
		t.Errorf("Select(no limit) = %v, %d left", ids(picked), left)
	}
	if picked, left := x.Select(Query{Since: now.Add(-90 * time.Minute), Limit: 1}); !slices.Equal(ids(picked), []string{"s0"}) || left != 1 {
		t.Errorf("Select(since 90m, limit 1) = %v, %d left, want s0, 1 left", ids(picked), left)
	}
	if picked, _ := x.Select(Query{Harness: HarnessAgy}); len(picked) != 0 {
		t.Errorf("Select(harness agy) = %v", ids(picked))
	}
}

// --project reads only the Claude sessions whose project folder could
// match, and matches by the cwd they recorded: a directory and what is
// inside it, or a part of the project name.
func TestIndex_SelectProject(t *testing.T) {
	projects := filepath.Join(t.TempDir(), "projects")
	now := time.Now()
	claudeSession(t, projects, "shop", "/home/u/src/Shop", now, 1)
	claudeSession(t, projects, "shop-wt", "/home/u/src/Shop/.claude/worktrees/fix", now.Add(-time.Minute), 0)
	claudeSession(t, projects, "notes", "/home/u/notes", now.Add(-2*time.Minute), 0)
	claudeSession(t, projects, "shopping", "/srv/shopping", now.Add(-3*time.Minute), 0)
	// A folder name that is no slug of its cwd: read, whatever the name.
	odd := filepath.Join(projects, "odd folder", "odd.jsonl")
	write(t, odd, lines(`{"type":"user","sessionId":"odd","cwd":"/home/u/src/Shop","message":{"role":"user","content":"hi"}}`))
	touch(t, odd, now.Add(-4*time.Minute))
	dirs := []Dir{{HarnessClaude, projects, OriginDefault}}

	for _, tc := range []struct {
		project string
		want    []string
		read    []string
	}{
		// shop-wt's folder holds "Shop" too: read, and its project, fix, doesn't match.
		{"shop", []string{"shop", "agent-shop-0", "shopping", "odd"}, []string{"odd", "shop", "shop-wt", "shopping"}},
		{"SHOP", []string{"shop", "agent-shop-0", "shopping", "odd"}, []string{"odd", "shop", "shop-wt", "shopping"}},
		{"/home/u/src/Shop", []string{"shop", "agent-shop-0", "shop-wt", "odd"}, []string{"odd", "shop", "shop-wt"}},
		{"/home/u/src/Shop/", []string{"shop", "agent-shop-0", "shop-wt", "odd"}, []string{"odd", "shop", "shop-wt"}},
		{"/home/u/src/Sho", nil, []string{"odd", "shop", "shop-wt"}},
		{"notes", []string{"notes"}, []string{"notes", "odd"}},
		{"nothing", nil, []string{"odd"}},
	} {
		x := NewIndex(dirs, nil)
		picked, _ := x.Select(Query{Project: tc.project})
		if got := ids(picked); !slices.Equal(got, tc.want) {
			t.Errorf("--project %s = %v, want %v", tc.project, got, tc.want)
		}
		if got := x.probedIDs(); !slices.Equal(got, tc.read) {
			t.Errorf("--project %s read %v, want %v", tc.project, got, tc.read)
		}
	}

	x := NewIndex(dirs, nil)
	if picked, left := x.Select(Query{Project: "shop", Limit: 1}); !slices.Equal(ids(picked), []string{"shop", "agent-shop-0"}) || left != 2 {
		t.Errorf("--project shop --limit 1 = %v, %d left, want shop and its subagent, 2 left", ids(picked), left)
	}
}

// Workgroup and Local pick by the run that owns the transcript, which the
// join works out without reading any.
func TestIndex_SelectWorkgroup(t *testing.T) {
	projects := filepath.Join(t.TempDir(), "projects")
	now := time.Now()
	owned := claudeSession(t, projects, "owned", "/w", now, 1)
	claudeSession(t, projects, "mine", "/w", now.Add(-time.Minute), 0)
	agents := []Agent{{Name: "worker", Store: "/s", Host: "h", Backend: "claude", Live: true, CreatedAt: now.Add(-time.Hour),
		Status: StatusIdle, SessionID: "owned", Transcript: owned, History: []string{owned}},
		{Name: "aid", Store: "/s", Host: "h", Backend: "aider", Live: true, CreatedAt: now.Add(-48 * time.Hour)}}
	x := NewIndex([]Dir{{HarnessClaude, projects, OriginDefault}}, agents)

	// The aider agent keeps no transcript: written now, so it comes first.
	picked, _ := x.Select(Query{Workgroup: true})
	if got := ids(picked); !slices.Equal(got, []string{"", "owned", "agent-owned-0"}) {
		t.Errorf("Workgroup = %v, want aid's, then owned and its subagent", got)
	}
	if picked, _ := x.Select(Query{Local: true}); !slices.Equal(ids(picked), []string{"mine"}) {
		t.Errorf("Local = %v, want mine", ids(picked))
	}
	if picked, _ = x.Select(Query{Since: now.Add(time.Hour)}); !slices.Equal(ids(picked), []string{""}) {
		t.Errorf("Since in an hour = %v, want only aid's session, which has no transcript", ids(picked))
	}
	if got := x.probedIDs(); len(got) != 0 {
		t.Errorf("Select read %v, want none", got)
	}
	resolved := x.Resolve(picked, Live{Claude: map[string]string{}})
	if len(resolved) != 1 || resolved[0].Workgroup == nil || resolved[0].Workgroup.Name != "aid" || resolved[0].Harness != "aider" {
		t.Errorf("resolved = %+v", resolved)
	}
	if hs := x.Harnesses(); !slices.Contains(hs, "aider") {
		t.Errorf("Harnesses = %v, want aider among them", hs)
	}
}

// Lookup finds a session by its transcript's path without reading any,
// adds one outside the scanned dirs, and Resolve reads it and the sessions
// above it.
func TestIndex_Lookup(t *testing.T) {
	root := t.TempDir()
	projects := filepath.Join(root, "projects")
	now := time.Now()
	parent := claudeSession(t, projects, "p", "/w", now, 1)
	claudeSession(t, projects, "other", "/w", now.Add(-time.Minute), 0)
	sub := filepath.Join(strings.TrimSuffix(parent, ".jsonl"), "subagents", "agent-p-0.jsonl")
	x := NewIndex([]Dir{{HarnessClaude, projects, OriginDefault}}, nil)

	s, ok := x.Lookup(sub)
	if !ok || s.SessionID != "agent-p-0" || s.ParentSession == nil || s.ParentSession.SessionID != "p" {
		t.Fatalf("Lookup(sub) = %+v, %v", s, ok)
	}
	resolved := x.Resolve([]Session{s}, Live{Claude: map[string]string{"p": StatusIdle}})
	if got := ids(resolved); !slices.Equal(got, []string{"p", "agent-p-0"}) {
		t.Errorf("Resolve(sub) = %v, want it and its parent", got)
	}
	if got := x.probedIDs(); !slices.Equal(got, []string{"agent-p-0", "p"}) {
		t.Errorf("read %v, want the subagent and its parent only", got)
	}
	for _, r := range resolved {
		if r.SessionID == "agent-p-0" && r.Status != StatusUnknown {
			t.Errorf("subagent status = %s, want unknown under a running parent", r.Status)
		}
	}

	elsewhere := filepath.Join(root, "elsewhere", "e1.jsonl")
	write(t, elsewhere, lines(`{"type":"user","sessionId":"e1","cwd":"/e","message":{"role":"user","content":"hi"}}`))
	if s, ok := x.Lookup(elsewhere); !ok || s.SessionID != "e1" {
		t.Errorf("Lookup(outside) = %+v, %v", s, ok)
	}
	if _, ok := x.Lookup(filepath.Join(root, "nothing.jsonl")); ok {
		t.Error("Lookup(no file) found one")
	}
	if !slices.Contains(ids(x.Sessions()), "e1") {
		t.Error("the transcript Lookup added isn't in Sessions")
	}
}

// Index.Continue picks what Continue would from a full scan, reading only
// the Claude sessions whose project folder could be dir's, and stops at the
// newest that ran there.
func TestIndex_Continue(t *testing.T) {
	projects := filepath.Join(t.TempDir(), "projects")
	now := time.Now()
	claudeSession(t, projects, "a-new", "/home/u/a", now, 0)
	claudeSession(t, projects, "a-old", "/home/u/a", now.Add(-time.Hour), 0)
	claudeSession(t, projects, "b", "/home/u/b", now.Add(time.Minute), 0)
	claudeSession(t, projects, "a-sub", "/home/u/a/sub", now.Add(2*time.Minute), 0)
	dirs := []Dir{{HarnessClaude, projects, OriginDefault}}
	x := NewIndex(dirs, nil)
	s, ok := x.Continue(HarnessClaude, "/home/u/a")
	want, _ := Continue(Scan(dirs), HarnessClaude, "/home/u/a")
	if !ok || s.SessionID != "a-new" || want.SessionID != "a-new" {
		t.Errorf("Continue = %s, %v; from a scan %s; want a-new", s.SessionID, ok, want.SessionID)
	}
	if got := x.probedIDs(); !slices.Equal(got, []string{"a-new", "a-sub"}) {
		t.Errorf("Continue read %v, want a-sub (its folder could be a's) and a-new, not b or a-old", got)
	}
	if _, ok := NewIndex(dirs, nil).Continue(HarnessClaude, "/home/u/c"); ok {
		t.Error("Continue in a dir with no sessions found one")
	}
}

func TestProjectMatch_MayMatch(t *testing.T) {
	for _, tc := range []struct {
		q, folder string
		want      bool
	}{
		{"gomuxai", "-Users-lugo-localdev-gomuxai", true},
		{"GoMux", "-Users-lugo-localdev-gomuxai", true},
		{"gomuxai", "-Users-lugo-notes", false},
		{"my_app", "-home-u-my-app", true},
		{"my_app", "-home-u-myapp", true}, // a cheap test: the read decides
		{"/Users/lugo/localdev", "-Users-lugo-localdev-gomuxai--claude-worktrees-x", true},
		{"/Users/lugo/elsewhere", "-Users-lugo-localdev-gomuxai", false},
		{"x", "", true},                        // not listed in a project folder
		{"x", "odd folder", true},              // not a slug
		{"x", strings.Repeat("-a", 100), true}, // long enough to be shortened
		{"-", "-home-u", true},                 // no letters or digits to look for
		{"é", "-home-u", true},
	} {
		if got := newProjectMatch(tc.q).mayMatch(tc.folder); got != tc.want {
			t.Errorf("mayMatch(%q, %q) = %v, want %v", tc.q, tc.folder, got, tc.want)
		}
	}
}
