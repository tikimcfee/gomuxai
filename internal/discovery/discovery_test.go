package discovery

import (
	"bytes"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"testing"
	"time"
)

// Every fixture here is synthesized, in a temp dir standing in for the
// home directory; no test reads the real one.

func write(t *testing.T, path, content string) {
	t.Helper()
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
		t.Fatal(err)
	}
}

func touch(t *testing.T, path string, at time.Time) {
	t.Helper()
	if err := os.Chtimes(path, at, at); err != nil {
		t.Fatal(err)
	}
}

func lines(ls ...string) string { return strings.Join(ls, "\n") + "\n" }

func ptr(s string) *string { return &s }

func TestResolveDirs(t *testing.T) {
	home := t.TempDir()
	for _, d := range []string{".claude/projects", ".gemini/antigravity-cli/brain"} {
		if err := os.MkdirAll(filepath.Join(home, d), 0o755); err != nil {
			t.Fatal(err)
		}
	}
	cases := []struct {
		name      string
		o         Overrides
		configDir string
		want      []Dir
	}{
		{"defaults that exist", Overrides{}, "", []Dir{
			{HarnessClaude, filepath.Join(home, ".claude/projects"), OriginDefault},
			{HarnessAgy, filepath.Join(home, ".gemini/antigravity-cli/brain"), OriginDefault},
		}},
		{"CLAUDE_CONFIG_DIR replaces ~/.claude", Overrides{Agy: ptr("")}, filepath.Join(home, "missing"), nil},
		{"config wins, kept when missing; empty turns off", Overrides{Claude: ptr(""), Agy: ptr("~/elsewhere"), Kimi: ptr("/k")}, "", []Dir{
			{HarnessAgy, filepath.Join(home, "elsewhere"), OriginConfig},
			{HarnessKimi, "/k", OriginConfig},
		}},
	}
	for _, c := range cases {
		got := ResolveDirs(c.o, home, c.configDir)
		if !slices.Equal(got, c.want) {
			t.Errorf("%s: dirs = %+v, want %+v", c.name, got, c.want)
		}
	}
	if got := ResolveDirs(Overrides{}, "", ""); got != nil {
		t.Errorf("no home: dirs = %+v, want none", got)
	}

	desc := DescribeDirs([]Dir{{HarnessClaude, filepath.Join(home, ".claude/projects"), OriginDefault}, {HarnessKimi, filepath.Join(home, "k"), OriginConfig}}, home)
	if want := "claude ~/.claude/projects (default); kimi ~/k (config, missing)"; desc != want {
		t.Errorf("describe = %q, want %q", desc, want)
	}
	if desc := DescribeDirs(nil, home); !strings.Contains(desc, "claude_projects_dir") {
		t.Errorf("describe of nothing = %q, want it to name the config keys", desc)
	}
}

// eachLineBackward yields the same lines as a forward split, in reverse,
// with their offsets, whatever the chunk size, including lines longer than
// a chunk and a last line without a newline.
func TestEachLineBackward(t *testing.T) {
	data := []byte("a\n" + strings.Repeat("b", 40) + "\n\nccc\n" + strings.Repeat("d", 7) + "\nlast")
	var want []string
	var wantAt []int64
	off := int64(0)
	for _, l := range bytes.Split(data, []byte{'\n'}) {
		if len(l) > 0 {
			want = append(want, string(l))
			wantAt = append(wantAt, off)
		}
		off += int64(len(l)) + 1
	}
	slices.Reverse(want)
	slices.Reverse(wantAt)
	for _, chunk := range []int64{1, 3, 5, 16, 1000} {
		var got []string
		var gotAt []int64
		eachLineBackward(bytes.NewReader(data), int64(len(data)), chunk, 1<<20, func(line []byte, at int64) bool {
			got, gotAt = append(got, string(line)), append(gotAt, at)
			return true
		})
		if !slices.Equal(got, want) || !slices.Equal(gotAt, wantAt) {
			t.Errorf("chunk %d: lines %q at %v, want %q at %v", chunk, got, gotAt, want, wantAt)
		}
	}
	n := 0
	eachLineBackward(bytes.NewReader(data), int64(len(data)), 4, 1<<20, func([]byte, int64) bool { n++; return n < 2 })
	if n != 2 {
		t.Errorf("stopping after 2 lines read %d", n)
	}
	// With a limit, only lines that start within the last limit bytes.
	for _, chunk := range []int64{1, 3, 1000} {
		var got []string
		eachLineBackward(bytes.NewReader(data), int64(len(data)), chunk, 15, func(line []byte, _ int64) bool {
			got = append(got, string(line))
			return true
		})
		if want := []string{"last", "ddddddd"}; !slices.Equal(got, want) {
			t.Errorf("chunk %d, limit 15: lines %q, want %q", chunk, got, want)
		}
	}
}

// claudeFixture writes a Claude projects dir: a session in a folder whose
// slug doesn't match its cwd, with retitles and a subagent; one with only
// AI titles; one at the top level; and an empty file.
func claudeFixture(t *testing.T, home string) string {
	t.Helper()
	projects := filepath.Join(home, ".claude", "projects")
	main := filepath.Join(projects, "-slug-not-the-project", "s-main.jsonl")
	write(t, main, lines(
		`{"type":"permission-mode","permissionMode":"default","sessionId":"s-main"}`,
		`{"type":"user","sessionId":"s-main","cwd":"/home/user/src/shop","version":"2.1.300","gitBranch":"main","message":{"role":"user","content":"make the cart work"}}`,
		`{"type":"ai-title","aiTitle":"Cart fixes","sessionId":"s-main"}`,
		`{"type":"assistant","sessionId":"s-main","cwd":"/home/user/src/shop","version":"2.1.300","gitBranch":"main","message":{"role":"assistant","model":"claude-old","content":[]}}`,
		`{"type":"custom-title","customTitle":"Cart (renamed)","sessionId":"s-main"}`,
		`{"type":"ai-title","aiTitle":"Cart fixes, part two","sessionId":"s-main"}`,
		`{"type":"custom-title","customTitle":"Cart (renamed again)","sessionId":"s-main"}`,
		`{"type":"assistant","sessionId":"s-main","cwd":"/home/user/src/shop/wt","version":"2.1.310","gitBranch":"feature","message":{"role":"assistant","model":"claude-new","content":[]}}`,
		`{"type":"assistant","sessionId":"s-main","cwd":"/home/user/src/shop","version":"2.1.310","gitBranch":"feature","message":{"role":"assistant","model":"<synthetic>","content":[]}}`,
		`{"type":"continued-in","continuedInSessionId":"s-next","sessionId":"s-main","timestamp":"2026-10-20T10:00:00Z"}`,
	))
	sub := filepath.Join(projects, "-slug-not-the-project", "s-main", "subagents", "agent-x1.jsonl")
	write(t, sub, lines(`{"type":"user","sessionId":"s-main","agentId":"x1","isSidechain":true,"cwd":"/home/user/src/shop","message":{"role":"user","content":"look around"}}`))
	write(t, strings.TrimSuffix(sub, ".jsonl")+".meta.json", `{"agentType":"Explore","description":"Look around the cart code","toolUseId":"toolu_01","finished":true}`)
	sub2 := filepath.Join(projects, "-slug-not-the-project", "s-main", "subagents", "agent-x2.jsonl")
	write(t, sub2, lines(`{"type":"user","sessionId":"s-main","agentId":"x2","isSidechain":true,"cwd":"/home/user/src/shop","message":{"role":"user","content":"keep going"}}`))
	write(t, strings.TrimSuffix(sub2, ".jsonl")+".meta.json", `{"agentType":"general-purpose","description":"Keep going","toolUseId":"toolu_02","finished":false}`)
	write(t, filepath.Join(projects, "-other", "s-ai.jsonl"), lines(
		`{"type":"user","sessionId":"s-ai","cwd":"/home/user/notes","message":{"role":"user","content":"hi"}}`,
		`{"type":"ai-title","aiTitle":"First","sessionId":"s-ai"}`,
		`{"type":"ai-title","aiTitle":"Second","sessionId":"s-ai"}`,
	))
	write(t, filepath.Join(projects, "s-top.jsonl"), lines(`{"type":"user","sessionId":"s-top","cwd":"/home/user","message":{"role":"user","content":"x"}}`))
	write(t, filepath.Join(projects, "-other", "s-empty.jsonl"), "")
	return projects
}

func byID(sessions []Session) map[string]Session {
	m := map[string]Session{}
	for _, s := range sessions {
		m[s.SessionID] = s
	}
	return m
}

func TestScanClaude(t *testing.T) {
	home := t.TempDir()
	projects := claudeFixture(t, home)
	got := byID(Scan([]Dir{{HarnessClaude, projects, OriginDefault}}))
	if _, ok := got["s-empty"]; ok || len(got) != 5 {
		t.Fatalf("sessions = %v, want s-main, its 2 subagents, s-ai, s-top (not the empty file)", got)
	}
	m := got["s-main"]
	if m.Harness != HarnessClaude || m.Cwd != "/home/user/src/shop" || m.Project != "shop" {
		t.Errorf("s-main cwd/project = %q %q, want the first recorded cwd, not the folder slug", m.Cwd, m.Project)
	}
	if m.Title != "Cart (renamed again)" {
		t.Errorf("s-main title = %q, want the newest custom title over the newer AI one", m.Title)
	}
	if m.Model != "claude-new" || m.Version != "2.1.310" || m.GitBranch != "feature" || m.ContinuedIn != "s-next" {
		t.Errorf("s-main model/version/branch/continued = %q %q %q %q", m.Model, m.Version, m.GitBranch, m.ContinuedIn)
	}
	if m.Size == 0 || m.Mtime.IsZero() || m.ParentSession != nil || m.Workgroup != nil {
		t.Errorf("s-main = %+v", m)
	}
	if s := got["s-ai"]; s.Title != "Second" || s.Project != "notes" {
		t.Errorf("s-ai title/project = %q %q, want the newest AI title", s.Title, s.Project)
	}
	x1 := got["agent-x1"]
	if x1.ParentSession == nil || *x1.ParentSession != (ParentSession{"s-main", "toolu_01"}) || x1.Title != "Look around the cart code" || x1.state != subFinished {
		t.Errorf("agent-x1 = %+v parent %+v", x1, x1.ParentSession)
	}
	if x2 := got["agent-x2"]; x2.state != subRunning {
		t.Errorf("agent-x2 state = %v, want running (finished: false)", x2.state)
	}
}

// countingReader counts the bytes read through it.
type countingReader struct {
	r io.ReaderAt
	n int64
}

func (c *countingReader) ReadAt(p []byte, off int64) (int, error) {
	n, err := c.r.ReadAt(p, off)
	c.n += int64(n)
	return n, err
}

var filler = `{"type":"attachment","sessionId":"s","attachment":{"content":"` + strings.Repeat("x", 4000) + `"}}`

// fillers is n filler lines, about 4 KB each.
func fillers(n int) []string { return slices.Repeat([]string{filler}, n) }

// readCounted runs readClaude over data and returns the session and the
// bytes it read.
func readCounted(data string, subagent bool) (Session, int64) {
	var s Session
	r := &countingReader{r: strings.NewReader(data)}
	readClaude(&s, r, int64(len(data)), subagent)
	return s, r.n
}

// A listing never reads a large transcript whole: at most the head and the
// last claudeTail bytes, whether it has no title, is a subagent's (which
// never has one), or ends in one huge line.
func TestReadClaude_Bounded(t *testing.T) {
	head := `{"type":"user","sessionId":"s","cwd":"/home/user/big","message":{"role":"user","content":"go"}}`
	meta := `{"type":"assistant","sessionId":"s","version":"2.1.310","gitBranch":"","message":{"role":"assistant","model":"m","content":[]}}`
	limit := int64(claudeHead + claudeTail + claudeTailChunk)
	cases := map[string]struct {
		data     string
		subagent bool
	}{
		"no title":     {lines(append(append([]string{head}, fillers(5000)...), meta)...), false},
		"subagent":     {lines(append(append([]string{head, meta}, fillers(5000)...), meta)...), true},
		"one big line": {lines(head, meta, `{"type":"attachment","x":"`+strings.Repeat("y", 20<<20)+`"}`), false},
	}
	for name, c := range cases {
		s, n := readCounted(c.data, c.subagent)
		if n > limit {
			t.Errorf("%s: read %d of %d bytes, want at most %d", name, n, len(c.data), limit)
		}
		if s.Cwd != "/home/user/big" {
			t.Errorf("%s: cwd = %q, want the head's", name, s.Cwd)
		}
	}
	// A titled transcript stops soon after its newest title.
	data := lines(append(append([]string{head, `{"type":"ai-title","aiTitle":"Old","sessionId":"s"}`}, fillers(5000)...),
		`{"type":"ai-title","aiTitle":"New","sessionId":"s"}`, meta)...)
	if s, n := readCounted(data, false); s.Title != "New" || n > claudeHead+claudeTitleSlack+2*claudeTailChunk {
		t.Errorf("titled: title %q after reading %d bytes, want New within the slack", s.Title, n)
	}
}

// The custom title wins over a newer AI title within the slack past it,
// across chunk boundaries; a title older than the tail window isn't found.
func TestReadClaude_TitleWindow(t *testing.T) {
	user := `{"type":"user","sessionId":"s","cwd":"/home/user/big","message":{"role":"user","content":"go"}}`
	ls := append([]string{user, `{"type":"custom-title","customTitle":"Named by hand","sessionId":"s"}`}, fillers(10)...)
	ls = append(ls, `{"type":"ai-title","aiTitle":"Named by the model","sessionId":"s"}`)
	ls = append(ls, fillers(100)...)
	if s, _ := readCounted(lines(ls...), false); s.Title != "Named by hand" {
		t.Errorf("title = %q, want the custom one beside the AI one", s.Title)
	}
	ls = append([]string{user, `{"type":"ai-title","aiTitle":"Ancient","sessionId":"s"}`}, fillers(400)...)
	if s, _ := readCounted(lines(ls...), false); s.Title != "" {
		t.Errorf("title = %q, want none: it is older than the last %d bytes", s.Title, claudeTail)
	}
}

// In a cwd that isn't a git repo every record's gitBranch is "": the newest
// version-bearing record still settles version and branch, and older
// custom titles don't replace the newest one.
func TestReadClaude_NoGitBranch(t *testing.T) {
	data := lines(
		`{"type":"user","sessionId":"s","cwd":"/home/user/plain","version":"2.1.300","gitBranch":"","message":{"role":"user","content":"go"}}`,
		`{"type":"custom-title","customTitle":"First name","sessionId":"s"}`,
		`{"type":"ai-title","aiTitle":"AI","sessionId":"s"}`,
		`{"type":"custom-title","customTitle":"Second name","sessionId":"s"}`,
		`{"type":"assistant","sessionId":"s","version":"2.1.310","gitBranch":"","message":{"role":"assistant","model":"m","content":[]}}`,
	)
	s, _ := readCounted(data, false)
	if s.Title != "Second name" || s.Version != "2.1.310" || s.GitBranch != "" || s.Model != "m" {
		t.Errorf("session = title %q version %q branch %q model %q, want the newest of each", s.Title, s.Version, s.GitBranch, s.Model)
	}
}

// agyFixture writes an agy home: a conversation with a workspace and
// annotation title, its subagent, and one with no presence lock.
func agyFixture(t *testing.T, home string) (brain string, lock string) {
	t.Helper()
	agy := filepath.Join(home, ".gemini", "antigravity-cli")
	brain = filepath.Join(agy, "brain")
	step := `{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE","created_at":"2026-10-20T10:00:00Z","content":"hello"}`
	write(t, filepath.Join(brain, "conv-a", ".system_generated", "logs", "transcript_full.jsonl"), lines(step))
	write(t, filepath.Join(brain, "conv-a", ".system_generated", "logs", "transcript.jsonl"), lines(step))
	write(t, filepath.Join(brain, "conv-a", ".system_generated", "subagents", "conv-sub.json"),
		`{"conversationId":"conv-sub","subagentDescriptor":{"typeName":"x","role":"Test Researcher"},"state":"SUBAGENT_STATE_ALIVE","spawnStepIndex":4,"workspaceUris":["file:///home/user/src/game"]}`)
	write(t, filepath.Join(brain, "conv-sub", ".system_generated", "logs", "transcript_full.jsonl"), lines(step))
	write(t, filepath.Join(brain, "conv-old", ".system_generated", "logs", "transcript_full.jsonl"), lines(step))
	if err := os.MkdirAll(filepath.Join(brain, "conv-nosteps"), 0o755); err != nil {
		t.Fatal(err)
	}
	write(t, filepath.Join(agy, "history.jsonl"), lines(
		`{"display":"/model","timestamp":1,"workspace":"/home/user/src/game","type":"slash_command"}`,
		`{"display":"hello","timestamp":2,"workspace":"/home/user/src/game","conversationId":"conv-a"}`,
		`{"display":"later","timestamp":3,"workspace":"/home/user/src/elsewhere","conversationId":"conv-a"}`,
		`not json`,
	))
	write(t, filepath.Join(agy, "annotations", "conv-a.pbtxt"), `title:"Game \"engine\" review"`+"\n")
	lock = filepath.Join(agy, "presence", "conv-a.lock")
	write(t, lock, "")
	write(t, filepath.Join(agy, "presence", "conv-old.lock"), "")
	return brain, lock
}

// holdLock holds an exclusive flock on path, as agy does on an open
// conversation's presence file, until the test ends.
func holdLock(t *testing.T, path string) {
	t.Helper()
	f, err := os.Open(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := syscall.Flock(int(f.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { f.Close() })
}

func TestScanAgy(t *testing.T) {
	home := t.TempDir()
	brain, lock := agyFixture(t, home)
	holdLock(t, lock)
	got := byID(Scan([]Dir{{HarnessAgy, brain, OriginDefault}}))
	if len(got) != 3 {
		t.Fatalf("sessions = %v, want conv-a, conv-sub, conv-old (not one with no steps)", got)
	}
	a := got["conv-a"]
	if !strings.HasSuffix(a.Path, "transcript_full.jsonl") || a.Cwd != "/home/user/src/game" || a.Project != "game" ||
		a.Title != `Game "engine" review` || a.live != liveYes || a.ParentSession != nil {
		t.Errorf("conv-a = %+v", a)
	}
	sub := got["conv-sub"]
	if sub.ParentSession == nil || sub.ParentSession.SessionID != "conv-a" || sub.Title != "Test Researcher" ||
		sub.Cwd != "/home/user/src/game" || sub.live != liveYes {
		t.Errorf("conv-sub = %+v, want a subagent of conv-a, open while it is", sub)
	}
	if old := got["conv-old"]; old.live != liveNo || old.Cwd != "" {
		t.Errorf("conv-old = %+v, want not open, no workspace", old)
	}
}

func TestScanKimi(t *testing.T) {
	home := t.TempDir()
	kimi := filepath.Join(home, ".kimi-code")
	sess := filepath.Join(kimi, "sessions", "wd_shop_abc123", "session_k-1")
	write(t, filepath.Join(sess, "state.json"), `{"title":"Kimi cart work","workDir":"/home/user/src/shop"}`)
	write(t, filepath.Join(sess, "agents", "main", "wire.jsonl"), lines(
		`{"type":"metadata","protocol_version":"1.4","created_at":1}`,
		`{"type":"llm.request","time":2,"provider":"kimi","model":"k-old"}`,
		`{"type":"turn.prompt","input":[{"type":"text","text":"go"}]}`,
		`{"type":"llm.request","time":3,"provider":"kimi","model":"k-new"}`,
	))
	write(t, filepath.Join(sess, "agents", "agent-1", "wire.jsonl"), lines(`{"type":"turn.prompt","input":[{"type":"text","text":"sub"}]}`))
	other := filepath.Join(kimi, "sessions", "wd_notes_def456", "session_k-2")
	write(t, filepath.Join(other, "agents", "main", "wire.jsonl"), lines(`{"type":"turn.prompt","input":[{"type":"text","text":"x"}]}`))
	write(t, filepath.Join(kimi, "session_index.jsonl"), lines(`{"sessionId":"session_k-2","sessionDir":"`+other+`","workDir":"/home/user/notes"}`))

	got := byID(Scan([]Dir{{HarnessKimi, filepath.Join(kimi, "sessions"), OriginDefault}}))
	if len(got) != 3 {
		t.Fatalf("sessions = %v, want k-1, k-1:agent-1, k-2", got)
	}
	if k := got["k-1"]; k.Title != "Kimi cart work" || k.Project != "shop" || k.Model != "k-new" || k.ParentSession != nil {
		t.Errorf("k-1 = %+v", k)
	}
	if k := got["k-1:agent-1"]; k.ParentSession == nil || k.ParentSession.SessionID != "k-1" || k.Project != "shop" {
		t.Errorf("k-1:agent-1 = %+v, want a subagent of k-1", k)
	}
	if k := got["k-2"]; k.Cwd != "/home/user/notes" {
		t.Errorf("k-2 cwd = %q, want session_index.jsonl's workDir", k.Cwd)
	}
}

func statuses(sessions []Session) map[string]string {
	m := map[string]string{}
	for _, s := range sessions {
		m[s.SessionID] = s.Status
	}
	return m
}

// Sessions no workgroup owns get their status from the harness: claude
// agents for Claude, the presence lock for agy, the transcript's age for
// Kimi; subagents follow their own record and their parent.
func TestResolve_Local(t *testing.T) {
	home := t.TempDir()
	projects := claudeFixture(t, home)
	brain, lock := agyFixture(t, home)
	holdLock(t, lock)
	now := time.Now()
	k := filepath.Join(home, ".kimi-code", "sessions", "wd_a_1")
	write(t, filepath.Join(k, "session_fresh", "agents", "main", "wire.jsonl"), lines(`{"type":"turn.prompt"}`))
	write(t, filepath.Join(k, "session_stale", "agents", "main", "wire.jsonl"), lines(`{"type":"turn.prompt"}`))
	touch(t, filepath.Join(k, "session_stale", "agents", "main", "wire.jsonl"), now.Add(-time.Hour))
	dirs := []Dir{{HarnessClaude, projects, OriginDefault}, {HarnessAgy, brain, OriginDefault}, {HarnessKimi, filepath.Dir(k), OriginDefault}}

	got := statuses(Resolve(Scan(dirs), nil, Live{Claude: map[string]string{"s-main": "processing"}, Now: now}))
	want := map[string]string{
		"s-main": "processing", "s-ai": "exited", "s-top": "exited",
		"agent-x1": "exited",  // finished
		"agent-x2": "unknown", // not finished, parent running
		"conv-a":   "unknown", "conv-sub": "unknown", "conv-old": "exited",
		"fresh": "unknown", "stale": "exited",
	}
	for id, w := range want {
		if got[id] != w {
			t.Errorf("%s: status %q, want %q", id, got[id], w)
		}
	}

	got = statuses(Resolve(Scan(dirs), nil, Live{Now: now}))
	if got["s-main"] != "unknown" || got["s-ai"] != "unknown" || got["agent-x2"] != "unknown" {
		t.Errorf("without claude agents: %v, want Claude sessions unknown", got)
	}
	got = statuses(Resolve(Scan(dirs), nil, Live{Claude: map[string]string{}, Now: now}))
	if got["agent-x2"] != "exited" {
		t.Errorf("subagent of an exited session: %q, want exited", got["agent-x2"])
	}
}

// Sessions a workgroup agent records are joined to it: its current one
// gets its report (corrected by claude agents), earlier ones and departed
// runs' are exited, a subagent follows its parent's agent, a transcript
// outside the scanned dirs is probed, and a live agent with no transcript
// yet is listed.
func TestResolve_Workgroup(t *testing.T) {
	home := t.TempDir()
	projects := claudeFixture(t, home)
	store := filepath.Join(home, "bin", ".gomuxai")
	outside := filepath.Join(home, "elsewhere", "projects", "-x", "s-out.jsonl")
	write(t, outside, lines(`{"type":"user","sessionId":"s-out","cwd":"/home/user/x","message":{"role":"user","content":"x"}}`))
	old := filepath.Join(projects, "-other", "s-ai.jsonl")
	current := filepath.Join(projects, "-slug-not-the-project", "s-main.jsonl")
	top := filepath.Join(projects, "s-top.jsonl")
	now := time.Now()
	touch(t, old, now.Add(-2*time.Hour))
	touch(t, top, now.Add(-30*time.Minute))
	ended := now.Add(-10 * time.Minute)

	agents := []Agent{
		// An earlier run of lead, cleaned up; its name's history spans both runs.
		{Name: "lead", Store: store, Host: "box", Backend: "claude", CreatedAt: now.Add(-3 * time.Hour), EndedAt: ended,
			Status: "idle", Transcript: top, History: []string{old, top, current}},
		{Name: "lead", Store: store, Host: "box", Backend: "claude", Live: true, CreatedAt: now.Add(-time.Hour),
			Status: "waiting", StatusAt: now.Add(-time.Minute), SessionID: "s-main", Transcript: current, History: []string{old, top, current}},
		{Name: "lead-t", Parent: "lead", Store: store, Host: "box", Backend: "claude", Live: true, CreatedAt: now,
			Status: "idle", Transcript: outside},
		{Name: "fresh", Parent: "lead", Store: store, Host: "box", Backend: "claude", Live: true, CreatedAt: now, SessionID: "s-new"},
	}
	sessions := Resolve(Scan([]Dir{{HarnessClaude, projects, OriginDefault}}), agents, Live{Claude: map[string]string{"s-main": "waiting"}, Now: now})
	got := byID(sessions)

	m := got["s-main"]
	if m.Workgroup == nil || *m.Workgroup != (Workgroup{Name: "lead", Store: store, Host: "box"}) || m.Status != "waiting" || !m.StatusAt.Equal(now.Add(-time.Minute)) {
		t.Errorf("s-main = %+v %+v, want lead's current session with its report", m, m.Workgroup)
	}
	if s := got["agent-x2"]; s.Workgroup == nil || s.Workgroup.Name != "lead" || s.Status != "unknown" {
		t.Errorf("agent-x2 = %+v, want lead's, as its parent session is", s)
	}
	if s := got["s-top"]; s.Workgroup == nil || s.Workgroup.Name != "lead" || s.Status != "exited" || !s.StatusAt.Equal(ended) {
		t.Errorf("s-top = %+v, want the departed run's last session, exited when it ended", s)
	}
	if s := got["s-ai"]; s.Workgroup == nil || s.Status != "exited" {
		t.Errorf("s-ai = %+v, want lead's (history, written before the second run), exited", s)
	}
	if s := got["s-out"]; s.Workgroup == nil || s.Workgroup.Name != "lead-t" || s.Workgroup.Parent != "lead" || s.Project != "x" || s.Status != "exited" {
		t.Errorf("s-out = %+v, want lead-t's transcript outside the dirs, exited (claude agents doesn't list it)", s)
	}
	if s := got["s-new"]; s.Workgroup == nil || s.Workgroup.Name != "fresh" || s.Path != "" || s.Harness != "claude" || s.Status != "unknown" {
		t.Errorf("s-new = %+v, want fresh with no transcript yet, status unknown before its first report", s)
	}
	if got["agent-x1"].Workgroup == nil {
		t.Error("agent-x1 has no workgroup, want lead's")
	}

	// Claude's list wins over a report it disagrees with.
	sessions = Resolve(Scan([]Dir{{HarnessClaude, projects, OriginDefault}}), agents, Live{Claude: map[string]string{"s-main": "idle"}, Now: now})
	if s := byID(sessions)["s-main"]; s.Status != "idle" {
		t.Errorf("s-main = %q, want claude agents' idle over the waiting report", s.Status)
	}
	// Without claude agents, the report stands.
	sessions = Resolve(Scan([]Dir{{HarnessClaude, projects, OriginDefault}}), agents, Live{Now: now})
	if s := byID(sessions)["s-out"]; s.Status != "idle" {
		t.Errorf("s-out = %q, want lead-t's report", s.Status)
	}
	for i := 1; i < len(sessions); i++ {
		if sessions[i].Mtime.After(sessions[i-1].Mtime) {
			t.Errorf("not newest first at %d: %v after %v", i, sessions[i].Mtime, sessions[i-1].Mtime)
		}
	}
}

func TestReconcileNative(t *testing.T) {
	never := func() bool { return false }
	always := func() bool { return true }
	cases := []struct {
		n      Native
		report string
		denied func() bool
		want   int
	}{
		{Native{Queried: true, Listed: true, Status: "idle"}, "processing", never, NativeWins},
		{Native{Queried: true, Listed: true, Status: "waiting"}, "idle", always, ReportDenied},
		{Native{Queried: true, Listed: true, Status: "waiting"}, "idle", never, NativeWins},
		{Native{Queried: true, Listed: true, Status: "unknown"}, "idle", never, ReportStands},
		{Native{Queried: true}, "idle", never, NativeGone},
		{Native{Queried: true}, "exited", never, ReportStands},
		{Native{Queried: true}, "", never, ReportStands},
		{Native{}, "idle", never, ReportStands},
	}
	for _, c := range cases {
		if got := ReconcileNative(c.n, c.report, c.denied); got != c.want {
			t.Errorf("ReconcileNative(%+v, %q) = %d, want %d", c.n, c.report, got, c.want)
		}
	}
}

// A workgroup agent's Claude session follows agent status's rule: a waiting
// in the CLI's list right after a deny, or a state with no mapping, leaves
// the report standing.
func TestResolve_NativeRule(t *testing.T) {
	home := t.TempDir()
	projects := claudeFixture(t, home)
	now := time.Now()
	current := filepath.Join(projects, "-slug-not-the-project", "s-main.jsonl")
	agent := Agent{Name: "lead", Store: "/s", Host: "box", Backend: "claude", Live: true, CreatedAt: now.Add(-time.Hour),
		Status: "idle", Event: "denied", StatusAt: now.Add(-time.Second), SessionID: "s-main", Transcript: current}
	status := func(a Agent, native string) string {
		got := byID(Resolve(Scan([]Dir{{HarnessClaude, projects, OriginDefault}}), []Agent{a}, Live{Claude: map[string]string{"s-main": native}, Now: now}))
		return got["s-main"].Status
	}
	if got := status(agent, "waiting"); got != "idle" {
		t.Errorf("waiting just after a deny: %q, want the report's idle", got)
	}
	agent.StatusAt = now.Add(-time.Minute)
	if got := status(agent, "waiting"); got != "waiting" {
		t.Errorf("waiting a minute after a deny: %q, want waiting", got)
	}
	if got := status(agent, "unknown"); got != "idle" {
		t.Errorf("a state with no mapping: %q, want the report's idle", got)
	}
}

// A transcript only an agent's history names, outside the scanned dirs,
// goes to the run that was live when it was written, not the newest.
func TestResolve_HistoryOutsideDirs(t *testing.T) {
	home := t.TempDir()
	now := time.Now()
	old := filepath.Join(home, "elsewhere", "-x", "s-old.jsonl")
	write(t, old, lines(`{"type":"user","sessionId":"s-old","cwd":"/home/user/x","message":{"role":"user","content":"x"}}`))
	touch(t, old, now.Add(-2*time.Hour))
	agents := []Agent{
		{Name: "lead", Parent: "first", Store: "/s", Host: "box", Backend: "claude", CreatedAt: now.Add(-3 * time.Hour), EndedAt: now.Add(-90 * time.Minute), History: []string{old}},
		{Name: "lead", Parent: "second", Store: "/s", Host: "box", Backend: "claude", Live: true, CreatedAt: now.Add(-time.Hour), History: []string{old}},
	}
	got := byID(Resolve(nil, agents, Live{Now: now}))
	if s := got["s-old"]; s.Workgroup == nil || s.Workgroup.Parent != "first" || s.Status != "exited" {
		t.Errorf("s-old = %+v %+v, want the first run's, exited", s, s.Workgroup)
	}
}

// A session reached through a symlinked dir and named by its real path is
// one session, owned; so are two spellings of one store.
func TestResolve_Symlinks(t *testing.T) {
	home := t.TempDir()
	real := claudeFixture(t, filepath.Join(home, "real"))
	link := filepath.Join(home, "link")
	if err := os.Symlink(real, link); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(home, filepath.Join(home, "store-link")); err != nil {
		t.Fatal(err)
	}
	now := time.Now()
	current := filepath.Join(real, "-slug-not-the-project", "s-main.jsonl")
	agents := []Agent{
		{Name: "lead", Store: home, Host: "box", Backend: "claude", Live: true, CreatedAt: now, Status: "idle", Transcript: current},
		{Name: "lead", Store: filepath.Join(home, "store-link"), Host: "box", Backend: "claude", Live: true, CreatedAt: now, Status: "idle", Transcript: current},
	}
	sessions := Resolve(Scan([]Dir{{HarnessClaude, link, OriginDefault}}), agents, Live{Now: now})
	n := 0
	for _, s := range sessions {
		if s.SessionID == "s-main" {
			n++
			if s.Workgroup == nil || s.Workgroup.Name != "lead" {
				t.Errorf("s-main = %+v, want lead's", s)
			}
		}
	}
	if n != 1 {
		t.Errorf("s-main listed %d times, want once", n)
	}
}

// A subagent's subagent takes its workgroup and status from the chain above
// it, whatever order the sessions come in.
func TestResolve_NestedSubagents(t *testing.T) {
	now := time.Now()
	sessions := []Session{
		{SessionID: "c", Harness: HarnessAgy, Path: "/b/c", Mtime: now, ParentSession: &ParentSession{SessionID: "b"}},
		{SessionID: "b", Harness: HarnessAgy, Path: "/b/b", Mtime: now, ParentSession: &ParentSession{SessionID: "a"}},
		{SessionID: "a", Harness: HarnessAgy, Path: "/b/a", Mtime: now, live: liveNo},
	}
	agents := []Agent{{Name: "lead", Store: "/s", Host: "box", Backend: "agy", Live: true, CreatedAt: now, Status: "idle", Transcript: "/b/a"}}
	got := byID(Resolve(sessions, agents, Live{Now: now}))
	for _, id := range []string{"a", "b", "c"} {
		if s := got[id]; s.Workgroup == nil || s.Workgroup.Name != "lead" || s.Status != "exited" {
			t.Errorf("%s = %+v %+v, want lead's, exited with its closed parent", id, s, s.Workgroup)
		}
	}
}
