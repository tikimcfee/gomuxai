package claudereg

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

// A registry file as Claude Code 2.1.316 wrote it on Linux (ids shortened).
const entryJSON = `{"pid":886208,"sessionId":"d298e0d9-329f","cwd":"/home/u/dev/p","startedAt":1793455699056,
"procStart":"61266397","version":"2.1.316","peerProtocol":1,"peerFeatures":["notify_idle"],"kind":"interactive",
"entrypoint":"cli","pidDomain":"linux:b24d:pid:[4026531836]","tmux":"agent-x:@9241.%9241",
"messagingSocketPath":"/run/user/1000/cc-socks/886208.sock","name":"p-97","nameSource":"derived",
"status":"busy","updatedAt":1793464592054,"statusUpdatedAt":1793464592054}`

func TestParseEntry(t *testing.T) {
	e, err := ParseEntry([]byte(entryJSON))
	if err != nil {
		t.Fatal(err)
	}
	if e.PID != 886208 || e.SessionID != "d298e0d9-329f" || e.Kind != KindInteractive || e.Status != StatusBusy ||
		e.ProcStart != "61266397" || e.Entrypoint != "cli" || e.TmuxHint != "agent-x:@9241.%9241" ||
		!e.UpdatedAt.Equal(time.UnixMilli(1793464592054)) {
		t.Fatalf("parsed %+v", e)
	}
}

func TestParseEntryRefusesBrokenFiles(t *testing.T) {
	for _, tc := range []struct{ json, want string }{
		{`{"sessionId":"s"}`, "no pid"},
		{`{"pid":5}`, "no sessionId"},
		{`[1]`, "not a registry entry"},
	} {
		_, err := ParseEntry([]byte(tc.json))
		if err == nil || !strings.Contains(err.Error(), tc.want) {
			t.Errorf("%s: err %v, want %q", tc.json, err, tc.want)
		}
	}
	// A missing kind is interactive and a missing status is not yet known,
	// as Claude Code reads them.
	e, err := ParseEntry([]byte(`{"pid":5,"sessionId":"s"}`))
	if err != nil || e.Kind != KindInteractive || e.Status != "" {
		t.Fatalf("got %+v, %v", e, err)
	}
}

func TestReadDir(t *testing.T) {
	dir := t.TempDir()
	write := func(name, body string) {
		if err := os.WriteFile(filepath.Join(dir, name), []byte(body), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	write("886208.json", entryJSON)
	write("886208.abcdef.key", "secret") // never read
	write("12.json", `{"pid":13,"sessionId":"s"}`)
	write("7.json", `{"pid":7,"sessionId":"s","status":"dreaming"`) // cut off mid-write
	write("notes.json", `{}`)
	entries, problems, err := ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(entries) != 1 || entries[0].PID != 886208 || entries[0].Path != filepath.Join(dir, "886208.json") {
		t.Fatalf("entries %+v", entries)
	}
	if len(problems) != 2 {
		t.Fatalf("problems %v", problems)
	}
	if _, _, err := ReadDir(filepath.Join(dir, "missing")); err != nil {
		t.Fatalf("a missing registry is no entries, got %v", err)
	}
}

func TestStatStartTime(t *testing.T) {
	// A command name with spaces and a ")" in it.
	stat := "886208 (claude (x) y) S 886207 886208 886151 34816 886208 4194560 1 2 3 4 5 6 7 8 20 0 15 0 61266397 7 8"
	if got, ok := StatStartTime(stat); !ok || got != "61266397" {
		t.Fatalf("got %q %v", got, ok)
	}
	if _, ok := StatStartTime("1 (x) S 1 2"); ok {
		t.Fatal("a short line has no start time")
	}
}

// StatStartTime reads this process's own start time the way Claude Code
// writes procStart.
func TestStatStartTimeSelf(t *testing.T) {
	data, err := os.ReadFile("/proc/self/stat")
	if err != nil {
		t.Skip("no /proc")
	}
	if s, ok := StatStartTime(string(data)); !ok || s == "" || strings.Trim(s, "0123456789") != "" {
		t.Fatalf("start time %q %v", s, ok)
	}
}

type fakeHost struct {
	running map[int]string // pid -> start ("" = can't read)
	domain  string
}

func (h fakeHost) Running(pid int) bool { _, ok := h.running[pid]; return ok }
func (h fakeHost) ProcStart(pid int) (string, bool) {
	s := h.running[pid]
	return s, s != ""
}
func (h fakeHost) PIDDomain() string { return h.domain }

func TestProof(t *testing.T) {
	h := fakeHost{running: map[int]string{10: "100", 11: "", 12: "999"}, domain: "linux:m:ns"}
	for _, tc := range []struct {
		e    Entry
		want Verdict
	}{
		{Entry{PID: 10, ProcStart: "100"}, Live},
		{Entry{PID: 10, ProcStart: "100", PIDDomain: "linux:m:ns"}, Live},
		{Entry{PID: 10, ProcStart: "100", PIDDomain: "linux:other:ns"}, Foreign},
		{Entry{PID: 12, ProcStart: "100"}, Stale}, // the pid was reused
		{Entry{PID: 13, ProcStart: "100"}, Stale}, // not running
		{Entry{PID: 10}, Unproven},                // no procStart recorded
		{Entry{PID: 11, ProcStart: "100"}, Unproven},
	} {
		if got := Proof(tc.e, h); got != tc.want {
			t.Errorf("%+v: %v, want %v", tc.e, got, tc.want)
		}
	}
}

func TestSessionsPrefersTheLaterUpdate(t *testing.T) {
	at := time.Unix(1000, 0)
	s := &Snapshot{Entries: []Proven{
		{Entry: Entry{PID: 1, SessionID: "a", UpdatedAt: at}, Verdict: Live},
		{Entry: Entry{PID: 2, SessionID: "a", UpdatedAt: at.Add(time.Minute)}, Verdict: Live}, // resumed after a Ctrl-Z
		{Entry: Entry{PID: 3, SessionID: "b"}, Verdict: Stale},
		{Entry: Entry{PID: 4, SessionID: "c"}, Verdict: Unproven},
	}}
	got := s.Sessions()
	if len(got) != 2 || got["a"].PID != 2 || got["c"].PID != 4 {
		t.Fatalf("got %+v", got)
	}
}

type tree map[int]int

func (t tree) Depth(pid, root int) (int, bool) {
	for d := 0; d < 64; d++ {
		if pid == root {
			return d, true
		}
		p, ok := t[pid]
		if !ok {
			return 0, false
		}
		pid = p
	}
	return 0, false
}

// The case seen on this machine: a `claude -p` run from inside an agent's
// session registers too, with the agent's pane in its tmux field. The pane's
// session is the shallower one.
func TestUnderPaneSkipsNestedClaude(t *testing.T) {
	// pane shell 100 -> sh 101 -> claude 102 -> bash 103 -> claude -p 104
	pt := tree{101: 100, 102: 101, 103: 102, 104: 103, 200: 1}
	s := &Snapshot{Entries: []Proven{
		{Entry: Entry{PID: 104, SessionID: "nested", TmuxHint: "agent-x:@1.%1", UpdatedAt: time.Unix(2000, 0)}, Verdict: Live},
		{Entry: Entry{PID: 102, SessionID: "agent", TmuxHint: "agent-x:@1.%1", UpdatedAt: time.Unix(1000, 0)}, Verdict: Live},
		{Entry: Entry{PID: 200, SessionID: "elsewhere", TmuxHint: "agent-x:@1.%1"}, Verdict: Live},
	}}
	e, ok, err := s.UnderPane(pt, 100)
	if err != nil || !ok || e.SessionID != "agent" {
		t.Fatalf("got %+v %v %v", e, ok, err)
	}
	if _, ok, _ := s.UnderPane(pt, 999); ok {
		t.Fatal("nothing runs under pane 999")
	}
}

// A stale entry under the pane (a CLI that crashed) is not its session.
func TestUnderPaneNeedsProof(t *testing.T) {
	pt := tree{101: 100}
	s := &Snapshot{Entries: []Proven{{Entry: Entry{PID: 101, SessionID: "old"}, Verdict: Stale}}}
	if _, ok, _ := s.UnderPane(pt, 100); ok {
		t.Fatal("a stale entry is not running")
	}
}

func TestUnderPaneAmbiguous(t *testing.T) {
	pt := tree{101: 100, 102: 100}
	s := &Snapshot{Entries: []Proven{
		{Entry: Entry{PID: 101, SessionID: "a"}, Verdict: Live},
		{Entry: Entry{PID: 102, SessionID: "b"}, Verdict: Live},
	}}
	if _, _, err := s.UnderPane(pt, 100); err == nil || !strings.Contains(err.Error(), "same depth") {
		t.Fatalf("err %v", err)
	}
}

// A file gomuxai can't read at all may be a running session's: the snapshot
// says so rather than let a reader call that session gone.
func TestSnapshotErr(t *testing.T) {
	dir := t.TempDir()
	os.WriteFile(filepath.Join(dir, "7.json"), []byte(`{"pid":7,"sessionId":"s","stat`), 0o600)
	os.WriteFile(filepath.Join(dir, "8.json"), []byte(`{"pid":8,"sessionId":"t","status":"idle"}`), 0o600)
	s, err := Read(dir, fakeHost{running: map[int]string{8: ""}})
	if err != nil {
		t.Fatal(err)
	}
	if err := s.Err(); err == nil || !strings.Contains(err.Error(), "7.json") || !strings.Contains(err.Error(), "won't say what runs") {
		t.Fatalf("Err() = %v", err)
	}
	if len(s.Entries) != 1 {
		t.Errorf("the readable entry is kept: %+v", s.Entries)
	}
}

// Claude Code ships a switch that writes the start time as procStartFt.
// The same value proves the entry; another form only fails to prove it.
func TestProcStartFt(t *testing.T) {
	e, err := ParseEntry([]byte(`{"pid":10,"sessionId":"s","procStartFt":"100"}`))
	if err != nil || e.ProcStart != "100" || !e.ProcStartFt {
		t.Fatalf("%+v %v", e, err)
	}
	h := fakeHost{running: map[int]string{10: "100"}}
	if v := Proof(e, h); v != Live {
		t.Errorf("same value: %v", v)
	}
	h.running[10] = "133777000000000000"
	if v := Proof(e, h); v != Unproven {
		t.Errorf("another form: %v", v)
	}
}

// One entry with a value gomuxai doesn't know costs that session only.
func TestUnknownValueIsOneSession(t *testing.T) {
	e, err := ParseEntry([]byte(`{"pid":5,"sessionId":"s","status":"compacting","version":"2.2.0"}`))
	if err != nil || e.Status != "compacting" || !strings.Contains(e.Unknown, `status "compacting"`) || !strings.Contains(e.Unknown, "2.2.0") {
		t.Fatalf("%+v %v", e, err)
	}
}
