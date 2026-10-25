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
	"github.com/tikimcfee/gomuxai/internal/storage"
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

// streamLines runs a stream that doesn't follow and returns its opening
// session line and its event lines.
func streamLines(t *testing.T, target *transcriptTarget, opts streamOptions) (string, []string) {
	t.Helper()
	var out bytes.Buffer
	if err := streamTranscript(context.Background(), target, &out, opts); err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSuffix(out.String(), "\n"), "\n")
	return lines[0], eventLines(lines)
}

// eventLines returns the event lines among a stream's lines.
func eventLines(lines []string) []string {
	var events []string
	for _, line := range lines {
		if strings.HasPrefix(line, `{"type":"event",`) {
			events = append(events, line)
		}
	}
	return events
}

// sessionLines returns the session lines among a stream's lines, decoded.
func sessionLines(t *testing.T, lines []string) []discovery.Session {
	t.Helper()
	var sessions []discovery.Session
	for _, line := range lines {
		if strings.HasPrefix(line, `{"type":"session",`) {
			var s discovery.Session
			if err := json.Unmarshal([]byte(line), &s); err != nil {
				t.Fatal(err)
			}
			sessions = append(sessions, s)
		}
	}
	return sessions
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

// waitEvents waits until b holds n event lines, and returns them.
func waitEvents(t *testing.T, b *syncBuffer, n int) []string {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		events := eventLines(b.lines())
		if len(events) >= n || time.Now().After(deadline) {
			if len(events) != n {
				t.Fatalf("stream has %d events, want %d:\n%s", len(events), n, strings.Join(b.lines(), "\n"))
			}
			return events
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// follower is a following stream a test runs.
type follower struct {
	out    *syncBuffer
	cancel context.CancelFunc
	done   chan error
}

// followStream starts a following stream of target, polling every 5ms and
// checking an owner's report every 10ms (never in full). The stream ends
// once readerGone, if set, says so.
func followStream(t *testing.T, target *transcriptTarget, readerGone func() bool) *follower {
	t.Helper()
	oldPoll, oldSession, oldStatus := transcriptPollInterval, transcriptSessionInterval, transcriptStatusInterval
	transcriptPollInterval, transcriptSessionInterval, transcriptStatusInterval = 5*time.Millisecond, 10*time.Millisecond, time.Hour
	ctx, cancel := context.WithCancel(context.Background())
	f := &follower{out: &syncBuffer{}, cancel: cancel, done: make(chan error, 1)}
	opts := streamOptions{from: -1, until: -1, follow: true, readerGone: readerGone}
	finished := make(chan struct{})
	go func() {
		f.done <- streamTranscript(ctx, target, f.out, opts)
		close(finished)
	}()
	t.Cleanup(func() {
		cancel()
		<-finished
		transcriptPollInterval, transcriptSessionInterval, transcriptStatusInterval = oldPoll, oldSession, oldStatus
	})
	return f
}

// stop ends the stream and returns its error.
func (f *follower) stop() error {
	f.cancel()
	return <-f.done
}

// wait waits for the stream to end by itself, and returns its error.
func (f *follower) wait(t *testing.T) error {
	t.Helper()
	select {
	case err := <-f.done:
		return err
	case <-time.After(5 * time.Second):
		t.Fatalf("the stream didn't end:\n%s", strings.Join(f.out.lines(), "\n"))
		return nil
	}
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
	f := followStream(t, fileTarget(path, first), nil)

	upTo := func(at int64) int {
		n := 0
		for _, line := range whole {
			if eventAt(t, line) < at {
				n++
			}
		}
		return n
	}
	waitEvents(t, f.out, upTo(starts[3]))
	for off := first; off < int64(len(data)); {
		next := min(off+97, int64(len(data)))
		appendFile(t, path, data[off:next])
		off = next
		complete := int64(len(data))
		if off < complete {
			complete = int64(bytes.LastIndexByte(data[:off], '\n') + 1)
		}
		waitEvents(t, f.out, upTo(complete))
	}
	if err := f.stop(); err != nil {
		t.Fatal(err)
	}
	got := eventLines(f.out.lines())
	if strings.Join(got, "\n") != strings.Join(whole, "\n") {
		t.Errorf("followed:\n%s\nwant the whole read's:\n%s", strings.Join(got, "\n"), strings.Join(whole, "\n"))
	}
	// Session lines say how far the stream had read.
	sessions := sessionLines(t, f.out.lines())
	if last := sessions[len(sessions)-1]; last.Size <= first || last.Cwd != "/home/user/src/widget" {
		t.Errorf("last session line: size %d, cwd %q", last.Size, last.Cwd)
	}
}

// A followed transcript that is removed, replaced, or rewritten in place
// ends the stream with an error that says so.
func TestTranscript_FollowFileChanges(t *testing.T) {
	_, data := corpusTranscript(t, "held-delivery.jsonl")
	starts := lineStarts(data)
	for name, change := range map[string]func(path string){
		"was removed": func(path string) { os.Remove(path) },
		"was replaced by another file": func(path string) {
			other := path + ".new"
			writeTestFile(t, other, string(data))
			os.Rename(other, path)
		},
		"shrank": func(path string) { os.Truncate(path, starts[2]) },
		// Truncated and grown past where it was between two polls: the
		// last bytes read are other bytes now.
		"was rewritten": func(path string) {
			f, err := os.OpenFile(path, os.O_WRONLY, 0)
			if err != nil {
				t.Fatal(err)
			}
			f.WriteAt([]byte("XXXX"), int64(len(data))-10)
			f.WriteAt(data[starts[1]:starts[2]], int64(len(data)))
			f.Close()
		},
	} {
		path := filepath.Join(t.TempDir(), "t.jsonl")
		writeTestFile(t, path, string(data))
		f := followStream(t, fileTarget(path, int64(len(data))), nil)
		waitEvents(t, f.out, len(wholeStream(t, path, data, false)))
		change(path)
		if err := f.wait(t); err == nil || !strings.Contains(err.Error(), name) {
			t.Errorf("%s: %v", name, err)
		}
	}
}

// A reader that has gone ends the stream, cleanly, though nothing is
// written; readerGone tells a closed pipe from an open one.
func TestTranscript_ReaderGone(t *testing.T) {
	path, data := corpusTranscript(t, "held-delivery.jsonl")
	closed := make(chan struct{})
	f := followStream(t, fileTarget(path, int64(len(data))), func() bool {
		select {
		case <-closed:
			return true
		default:
			return false
		}
	})
	waitEvents(t, f.out, len(wholeStream(t, path, data, false)))
	close(closed)
	if err := f.wait(t); err != nil {
		t.Errorf("a reader gone: %v", err)
	}

	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer w.Close()
	if readerGone(w) {
		t.Error("readerGone on a pipe whose reader is open")
	}
	r.Close()
	if !readerGone(w) {
		t.Error("readerGone on a pipe whose reader closed")
	}
}

// The parser's values replace what the opening line had from agent
// sessions' read of the head and tail, and a known value is never emptied.
func TestTranscript_SessionMeta(t *testing.T) {
	path, data := corpusTranscript(t, "requests.jsonl")
	target := fileTarget(path, int64(len(data)))
	target.session.Title, target.session.Model, target.session.Project = "Spin's return", "older-model", "stale"
	var out bytes.Buffer
	if err := streamTranscript(context.Background(), target, &out, streamOptions{from: -1, until: -1}); err != nil {
		t.Fatal(err)
	}
	sessions := sessionLines(t, strings.Split(strings.TrimSpace(out.String()), "\n"))
	if len(sessions) != 2 {
		t.Fatalf("%d session lines, want the opening one and the parser's:\n%s", len(sessions), out.String())
	}
	if s := sessions[1]; s.Title != "Spin's return" || s.Model != "claude-opus-5-5" || s.Project != "widget" || s.GitBranch != "main" || s.Size != int64(len(data)) {
		t.Errorf("after the read: %+v", s)
	}
	// A window reads part of the transcript: its line stays as it opened.
	var window bytes.Buffer
	if err := streamTranscript(context.Background(), target, &window, streamOptions{from: -1, until: lineStarts(data)[5]}); err != nil {
		t.Fatal(err)
	}
	if n := strings.Count(window.String(), `"type":"session"`); n != 1 {
		t.Errorf("a window has %d session lines, want 1:\n%s", n, window.String())
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

// finalSays returns the final replies among a stream's event lines.
func finalSays(events []string) []transcript.Event {
	var says []transcript.Event
	for _, line := range events {
		var e transcript.Event
		json.Unmarshal([]byte(line), &e)
		if e.Say != nil && e.Say.Final {
			says = append(says, e)
		}
	}
	return says
}

// finalSayOps is "<id> <op> <the text's start>" of each final reply.
func finalSayOps(events []string) []string {
	var ops []string
	for _, e := range finalSays(events) {
		ops = append(ops, e.ID+" "+e.Op+" "+e.Say.Text[:10])
	}
	return ops
}

// The real held delivery: the Stop that hands over a held message
// empties _result, as the turn goes on, so the first final reply has no
// provisional copy; the second Stop's reply is the turn's second.
func TestTranscript_HeldDelivery(t *testing.T) {
	setupSessions(t)
	ctx := context.Background()
	_, data := corpusTranscript(t, "held-delivery.jsonl")
	path := filepath.Join(t.TempDir(), "held.jsonl")
	firstAt := int64(bytes.Index(data, []byte(heldFirst)))
	firstAt = int64(bytes.LastIndexByte(data[:firstAt], '\n') + 1)
	secondAt := int64(bytes.Index(data, []byte(heldSecond)))
	secondAt = int64(bytes.LastIndexByte(data[:secondAt], '\n') + 1)
	writeTestFile(t, path, string(data[:firstAt]))
	hookTranscript(t, path)
	target, err := resolveTranscript(ctx, "worker")
	if err != nil {
		t.Fatal(err)
	}
	f := followStream(t, target, nil)
	n := len(waitEvents(t, f.out, len(wholeStream(t, path, data[:firstAt], false))))

	if err := store.CreateMessage(ctx, &storage.Message{SenderID: "lead", ReceiverID: "worker", Content: "[from lead@h] only the port",
		Status: storage.MessageStatusPending, CreatedAt: time.Now()}); err != nil {
		t.Fatal(err)
	}
	stop := func(text string) []byte {
		payload, _ := json.Marshal(map[string]string{"hook_event_name": "Stop", "transcript_path": path, "last_assistant_message": text})
		out, err := agentService.HandleHook(ctx, "worker", "", "", payload)
		if err != nil {
			t.Fatal(err)
		}
		return out
	}
	if out := stop(heldFirst); !bytes.Contains(out, []byte("only the port")) {
		t.Fatalf("the first Stop delivered %s, want the held message", out)
	}
	appendFile(t, path, data[firstAt:secondAt])
	n = len(waitEvents(t, f.out, len(wholeStream(t, path, data[:secondAt], false))))
	if out := stop(heldSecond); out != nil {
		t.Fatalf("the second Stop delivered %s", out)
	}
	waitEvents(t, f.out, n+1)
	appendFile(t, path, data[secondAt:])
	events := waitEvents(t, f.out, len(wholeStream(t, path, data, false))+1)
	if err := f.stop(); err != nil {
		t.Fatal(err)
	}
	first := finalSays(wholeStream(t, path, data, false))[0].ID
	want := []string{first + " add Two TODOs:", heldTurn + "#final2 add Understood", heldTurn + "#final2 update Understood"}
	if got := finalSayOps(events); strings.Join(got, "\n") != strings.Join(want, "\n") {
		t.Errorf("final replies:\n%s\nwant:\n%s", strings.Join(got, "\n"), strings.Join(want, "\n"))
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
	f := followStream(t, target, nil)
	n := len(waitEvents(t, f.out, len(wholeStream(t, path, data[:firstAt], false))))

	stopHook(t, path, heldFirst)
	events := waitEvents(t, f.out, n+1)
	p := finalSays(events)[0]
	var last transcript.Event
	json.Unmarshal([]byte(events[n-1]), &last)
	if p.ID != heldTurn+"#final" || !p.Say.Provisional || p.At != firstAt || p.Seq != last.Seq+1 || p.Turn == nil || p.Turn.Index != 1 {
		t.Errorf("provisional reply %s", events[n])
	}

	appendFile(t, path, data[firstAt:secondAt])
	n = len(waitEvents(t, f.out, len(wholeStream(t, path, data[:secondAt], false))+1))
	stopHook(t, path, heldSecond)
	waitEvents(t, f.out, n+1)
	appendFile(t, path, data[secondAt:])
	events = waitEvents(t, f.out, len(wholeStream(t, path, data, false))+2)
	// The Stop's report reaches the session line by the report check.
	deadline := time.Now().Add(5 * time.Second)
	for s := sessionLines(t, f.out.lines()); s[len(s)-1].Status != "idle"; s = sessionLines(t, f.out.lines()) {
		if time.Now().After(deadline) {
			t.Fatalf("no session line says idle after the Stop:\n%s", strings.Join(f.out.lines(), "\n"))
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := f.stop(); err != nil {
		t.Fatal(err)
	}

	want := []string{
		heldTurn + "#final add Two TODOs:", heldTurn + "#final update Two TODOs:",
		heldTurn + "#final2 add Understood", heldTurn + "#final2 update Understood",
	}
	if got := finalSayOps(events); strings.Join(got, "\n") != strings.Join(want, "\n") {
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
