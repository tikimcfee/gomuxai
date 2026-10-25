package cli

import (
	"bytes"
	"context"
	"encoding/json"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/discovery"
	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// corpusTranscript returns the path of a transcript of the conformance
// corpus, and its bytes.
func corpusTranscript(t *testing.T, name string) (string, []byte) {
	t.Helper()
	path, err := filepath.Abs(filepath.Join("..", "transcript", "testdata", "corpus", "claude", name))
	if err != nil {
		t.Fatal(err)
	}
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	return path, data
}

// fileTarget is a target for the Claude transcript at path that no
// workgroup owns.
func fileTarget(path string, size int64) *transcriptTarget {
	return &transcriptTarget{path: path, reader: transcript.Claude{},
		session: discovery.Session{SessionID: "s", Harness: discovery.HarnessClaude, Path: path, Size: size, Status: discovery.StatusExited}}
}

// wholeStream is the event lines a whole read of data makes, as the
// stream writes them.
func wholeStream(t *testing.T, path string, data []byte, requests bool) []string {
	t.Helper()
	opts := transcript.FileOptions("claude", path)
	opts.Requests = requests
	var lines []string
	for _, e := range (transcript.Claude{}).NewParser(opts).Feed(data) {
		line, err := json.Marshal(transcriptEventLine{"event", e})
		if err != nil {
			t.Fatal(err)
		}
		lines = append(lines, string(line))
	}
	return lines
}

// streamLines runs a stream that doesn't follow and returns its session
// line and event lines.
func streamLines(t *testing.T, target *transcriptTarget, opts streamOptions) (string, []string) {
	t.Helper()
	var out bytes.Buffer
	if err := streamTranscript(context.Background(), target, &out, opts); err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSuffix(out.String(), "\n"), "\n")
	return lines[0], lines[1:]
}

// lineStarts returns the offset each line of data starts at.
func lineStarts(data []byte) []int64 {
	starts := []int64{0}
	for i, b := range data[:len(data)-1] {
		if b == '\n' {
			starts = append(starts, int64(i+1))
		}
	}
	return starts
}

// eventAt is the at of an event line.
func eventAt(t *testing.T, line string) int64 {
	t.Helper()
	var e struct{ At int64 }
	if err := json.Unmarshal([]byte(line), &e); err != nil {
		t.Fatal(err)
	}
	return e.At
}

// A window holds exactly the whole read's events of its lines, numbered
// and turned as the whole read numbers them.
func TestTranscript_Window(t *testing.T) {
	for _, requests := range []bool{false, true} {
		path, data := corpusTranscript(t, "requests.jsonl")
		whole := wholeStream(t, path, data, requests)
		target := fileTarget(path, int64(len(data)))
		session, got := streamLines(t, target, streamOptions{from: -1, until: -1, requests: requests})
		if !strings.HasPrefix(session, `{"type":"session","session_id":"s",`) {
			t.Errorf("session line %s", session)
		}
		if strings.Join(got, "\n") != strings.Join(whole, "\n") {
			t.Fatalf("whole read (requests %v):\n%s\nwant:\n%s", requests, strings.Join(got, "\n"), strings.Join(whole, "\n"))
		}
		starts := lineStarts(data)
		for _, from := range append([]int64{-1}, starts...) {
			for _, until := range append(starts, -1) {
				if until >= 0 && until <= from {
					continue
				}
				var want []string
				for _, line := range whole {
					if at := eventAt(t, line); at > from && (until < 0 || at < until) {
						want = append(want, line)
					}
				}
				_, got := streamLines(t, target, streamOptions{from: from, until: until, requests: requests})
				if len(got) == 1 && got[0] == "" {
					got = nil
				}
				if strings.Join(got, "\n") != strings.Join(want, "\n") {
					t.Errorf("--from %d --until %d (requests %v):\n%s\nwant:\n%s", from, until, requests, strings.Join(got, "\n"), strings.Join(want, "\n"))
				}
			}
		}
	}

	// The conversation is numbered the same with or without the request
	// channel, which numbers its own.
	path, data := corpusTranscript(t, "requests.jsonl")
	var plain, conversation, requests []int
	for _, line := range wholeStream(t, path, data, false) {
		var e transcript.Event
		json.Unmarshal([]byte(line), &e)
		plain = append(plain, e.Seq)
	}
	for _, line := range wholeStream(t, path, data, true) {
		var e transcript.Event
		json.Unmarshal([]byte(line), &e)
		if e.Kind == transcript.KindRequest {
			requests = append(requests, e.Seq)
		} else {
			conversation = append(conversation, e.Seq)
		}
	}
	if !equalInts(plain, conversation) || !equalInts(requests, []int{0, 1}) {
		t.Errorf("seq without requests %v; with, conversation %v and requests %v", plain, conversation, requests)
	}
}

func equalInts(a, b []int) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}

func TestTranscript_FromNotALineStart(t *testing.T) {
	path, data := corpusTranscript(t, "requests.jsonl")
	target := fileTarget(path, int64(len(data)))
	for from, want := range map[int64]string{
		1:                     "is not the start of a line",
		int64(len(data)):      "is past the transcript's end",
		int64(len(data)) + 10: "is past the transcript's end",
	} {
		var out bytes.Buffer
		err := streamTranscript(context.Background(), target, &out, streamOptions{from: from, until: -1})
		if err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("--from %d: %v, want %q", from, err, want)
		}
		if out.Len() > 0 {
			t.Errorf("--from %d wrote %q before failing", from, out.String())
		}
	}

	// A last line not yet whole can't be resumed after.
	partial := filepath.Join(t.TempDir(), "partial.jsonl")
	last := lineStarts(data)[len(lineStarts(data))-1]
	writeTestFile(t, partial, string(data[:len(data)-5]))
	err := streamTranscript(context.Background(), fileTarget(partial, int64(len(data)-5)), &bytes.Buffer{}, streamOptions{from: last, until: -1})
	if err == nil || !strings.Contains(err.Error(), "has no end yet") {
		t.Errorf("--from the partial last line: %v", err)
	}
}

// syncBuffer is a bytes.Buffer a stream writes while a test reads it.
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *syncBuffer) Write(p []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

// lines returns the whole lines written so far.
func (b *syncBuffer) lines() []string {
	b.mu.Lock()
	defer b.mu.Unlock()
	s := b.buf.String()
	if i := strings.LastIndexByte(s, '\n'); i >= 0 {
		return strings.Split(s[:i], "\n")
	}
	return nil
}

// waitLines waits until b holds n lines, and returns them.
func waitLines(t *testing.T, b *syncBuffer, n int) []string {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		lines := b.lines()
		if len(lines) >= n || time.Now().After(deadline) {
			if len(lines) != n {
				t.Fatalf("stream has %d lines, want %d:\n%s", len(lines), n, strings.Join(lines, "\n"))
			}
			return lines
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// followStream starts a following stream of target, and returns its
// output and a func that stops it and returns its error.
func followStream(t *testing.T, target *transcriptTarget) (*syncBuffer, func() error) {
	t.Helper()
	oldPoll, oldSession := transcriptPollInterval, transcriptSessionInterval
	transcriptPollInterval, transcriptSessionInterval = 5*time.Millisecond, time.Hour
	ctx, cancel := context.WithCancel(context.Background())
	out := &syncBuffer{}
	done := make(chan error, 1)
	go func() { done <- streamTranscript(ctx, target, out, streamOptions{from: -1, until: -1, follow: true}) }()
	stop := func() error {
		cancel()
		err := <-done
		transcriptPollInterval, transcriptSessionInterval = oldPoll, oldSession
		return err
	}
	t.Cleanup(func() { cancel() })
	return out, stop
}

// appendFile appends data to the file at path.
func appendFile(t *testing.T, path string, data []byte) {
	t.Helper()
	f, err := os.OpenFile(path, os.O_APPEND|os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer f.Close()
	if _, err := f.Write(data); err != nil {
		t.Fatal(err)
	}
}

// A transcript followed as it grows, in chunks that end mid-line, streams
// what a whole read of it gives.
func TestTranscript_Follow(t *testing.T) {
	src, data := corpusTranscript(t, "held-delivery.jsonl")
	whole := wholeStream(t, src, data, false)
	path := filepath.Join(t.TempDir(), "growing.jsonl")
	starts := lineStarts(data)
	first := starts[3] + 7 // mid-line: that line isn't read yet
	writeTestFile(t, path, string(data[:first]))
	out, stop := followStream(t, fileTarget(path, first))

	upTo := func(at int64) int {
		n := 0
		for _, line := range whole {
			if eventAt(t, line) < at {
				n++
			}
		}
		return n
	}
	waitLines(t, out, 1+upTo(starts[3]))
	for off := first; off < int64(len(data)); {
		next := min(off+97, int64(len(data)))
		appendFile(t, path, data[off:next])
		off = next
		complete := int64(len(data))
		if off < complete {
			complete = int64(bytes.LastIndexByte(data[:off], '\n') + 1)
		}
		waitLines(t, out, 1+upTo(complete))
	}
	if err := stop(); err != nil {
		t.Fatal(err)
	}
	got := out.lines()[1:]
	if strings.Join(got, "\n") != strings.Join(whole, "\n") {
		t.Errorf("followed:\n%s\nwant the whole read's:\n%s", strings.Join(got, "\n"), strings.Join(whole, "\n"))
	}

	// A transcript that shrinks was rewritten: the stream says so.
	out, stop = followStream(t, fileTarget(path, int64(len(data))))
	waitLines(t, out, 1+len(whole))
	if err := os.Truncate(path, starts[2]); err != nil {
		t.Fatal(err)
	}
	time.Sleep(50 * time.Millisecond)
	if err := stop(); err == nil || !strings.Contains(err.Error(), "shrank") {
		t.Errorf("a shrunk transcript: %v", err)
	}
}

// heldTurn is held-delivery's turn, and its two final replies.
const (
	heldTurn   = "389c7d4d-b2a5-5fba-8c0a-ad513ef44a69"
	heldFirst  = "Two TODOs: the port should come from the config (cmd/widget/main.go:22), and cmd/legacy/run.go is marked for deletion after v2."
	heldSecond = "Understood: the only TODO left to act on is reading the port from the config."
)

// stopHook reports a Stop of worker's whose reply is text.
func stopHook(t *testing.T, path, text string) {
	t.Helper()
	payload, _ := json.Marshal(map[string]string{"hook_event_name": "Stop", "transcript_path": path, "last_assistant_message": text})
	if err := agentService.ReportHook(context.Background(), "worker", "", "", payload); err != nil {
		t.Fatal(err)
	}
}

// A workgroup Claude agent's Stop sends its reply before the transcript
// holds it; the transcript's copy updates it.
func TestTranscript_Provisional(t *testing.T) {
	setupSessions(t)
	_, data := corpusTranscript(t, "held-delivery.jsonl")
	path := filepath.Join(t.TempDir(), "held.jsonl")
	firstAt := int64(bytes.Index(data, []byte(heldFirst)))
	firstAt = int64(bytes.LastIndexByte(data[:firstAt], '\n') + 1)
	secondAt := int64(bytes.Index(data, []byte(heldSecond)))
	secondAt = int64(bytes.LastIndexByte(data[:secondAt], '\n') + 1)
	writeTestFile(t, path, string(data[:firstAt]))
	hookTranscript(t, path)

	target, err := resolveTranscript(context.Background(), "worker")
	if err != nil {
		t.Fatal(err)
	}
	if !samePath(target.path, path) || target.session.Workgroup == nil || target.session.Workgroup.Name != "worker" {
		t.Fatalf("worker's target: %s, %+v", target.path, target.session.Workgroup)
	}
	out, stop := followStream(t, target)
	n := len(waitLines(t, out, 1+len(wholeStream(t, path, data[:firstAt], false))))

	says := func(lines []string) []transcript.Event {
		var says []transcript.Event
		for _, line := range lines[1:] {
			var e transcript.Event
			json.Unmarshal([]byte(line), &e)
			if e.Say != nil && e.Say.Final {
				says = append(says, e)
			}
		}
		return says
	}
	stopHook(t, path, heldFirst)
	lines := waitLines(t, out, n+1)
	p := says(lines)[0]
	var next transcript.Event
	json.Unmarshal([]byte(lines[n-1]), &next)
	if p.ID != heldTurn+"#final" || !p.Say.Provisional || p.At != firstAt || p.Seq != next.Seq+1 || p.Turn == nil || p.Turn.Index != 1 {
		t.Errorf("provisional reply %s", lines[n])
	}

	appendFile(t, path, data[firstAt:secondAt])
	n = len(waitLines(t, out, 1+len(wholeStream(t, path, data[:secondAt], false))+1))
	stopHook(t, path, heldSecond)
	waitLines(t, out, n+1)
	appendFile(t, path, data[secondAt:])
	lines = waitLines(t, out, 1+len(wholeStream(t, path, data, false))+2)
	if err := stop(); err != nil {
		t.Fatal(err)
	}

	var got []string
	for _, e := range says(lines) {
		got = append(got, e.ID+" "+e.Op+" "+e.Say.Text[:10])
	}
	want := []string{
		heldTurn + "#final add Two TODOs:", heldTurn + "#final update Two TODOs:",
		heldTurn + "#final2 add Understood", heldTurn + "#final2 update Understood",
	}
	if strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Errorf("final replies:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
	}

	// Read again, the transcript holds both replies: no provisional one.
	if target, err = resolveTranscript(context.Background(), "worker"); err != nil {
		t.Fatal(err)
	}
	_, again := streamLines(t, target, streamOptions{from: -1, until: -1})
	if strings.Join(again, "\n") != strings.Join(wholeStream(t, path, data, false), "\n") {
		t.Errorf("a whole read after both replies landed:\n%s", strings.Join(again, "\n"))
	}
}

func TestTranscript_Resolve(t *testing.T) {
	home := setupSessions(t)
	projects := filepath.Join(home, ".claude", "projects", "-home-user-shop")
	ctx := context.Background()
	for arg, want := range map[string]string{
		"worker": "sess-w", "sess-l": "sess-l", "sess-w": "sess-w", filepath.Join(projects, "sess-l.jsonl"): "sess-l",
	} {
		target, err := resolveTranscript(ctx, arg)
		if err != nil {
			t.Errorf("%s: %v", arg, err)
			continue
		}
		if target.session.SessionID != want || !samePath(target.path, filepath.Join(projects, want+".jsonl")) || target.reader.Harness() != "claude" {
			t.Errorf("%s: session %s at %s", arg, target.session.SessionID, target.path)
		}
	}
	if target, _ := resolveTranscript(ctx, "worker"); target.session.Workgroup == nil || target.session.Status != "waiting" {
		t.Errorf("worker's session: %+v", target.session)
	}
	for arg, want := range map[string]string{
		"sess-":                        `"sess-" starts the ids of 2 sessions`,
		"nope":                         `no agent or session matches "nope"`,
		"worker@hub":                   `transcripts are read per host`,
		projects:                       "is a directory",
		filepath.Join(home, "x.jsonl"): "there is no such file",
	} {
		if _, err := resolveTranscript(ctx, arg); err == nil || !strings.Contains(err.Error(), want) {
			t.Errorf("%s: %v, want %q", arg, err, want)
		}
	}
}

// A reader closing the pipe ends the command cleanly.
func TestTranscript_ClosedPipe(t *testing.T) {
	setupSessions(t)
	path, _ := corpusTranscript(t, "held-delivery.jsonl")
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	r.Close()
	old := os.Stdout
	os.Stdout = w
	defer func() { os.Stdout = old; w.Close() }()
	if err := runTranscript(transcriptCmd, []string{path}); err != nil {
		t.Errorf("a closed pipe: %v", err)
	}
}
