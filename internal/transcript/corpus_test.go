package transcript

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"reflect"
	"regexp"
	"slices"
	"strings"
	"testing"
)

// corpusDir holds the conformance corpus (see its README.md): transcripts, and the
// events each must produce.
const corpusDir = "testdata/corpus"

// TestCorpusClaude runs the Claude reader over every claude case, its subagents'
// transcripts included, and compares what it emits to the case's expected stream,
// line by line, as README.md's "Comparing" says: as JSON values, an absent field
// equal to its zero value, the session line's mtime and status left out.
func TestCorpusClaude(t *testing.T) {
	var cases []string
	err := filepath.WalkDir(filepath.Join(corpusDir, "claude"), func(path string, d fs.DirEntry, err error) error {
		if err == nil && strings.HasSuffix(path, ".events.jsonl") {
			cases = append(cases, path)
		}
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(cases) == 0 {
		t.Fatal("no claude cases")
	}
	for _, want := range cases {
		t.Run(strings.TrimPrefix(want, corpusDir+"/"), func(t *testing.T) {
			expected := readJSONLines(t, want)
			var session struct{ Path string }
			if err := json.Unmarshal(expected[0], &session); err != nil {
				t.Fatal(err)
			}
			data, err := os.ReadFile(filepath.Join(corpusDir, session.Path))
			if err != nil {
				t.Fatal(err)
			}
			// As ReadFile reads it: a subagent's turns numbered from its parent's transcript.
			p := Claude{}.NewParser(Options{
				Requests: strings.HasSuffix(want, ".requests.events.jsonl"),
				Turns:    ClaudeSubagentTurns(filepath.Join(corpusDir, session.Path)),
			})
			events := p.Feed(data)
			s := p.Session()
			s.file(filepath.Join(corpusDir, session.Path), int64(len(data)))
			s.Path = session.Path

			got := [][]byte{marshalLine(t, "session", s)}
			for _, e := range events {
				got = append(got, marshalLine(t, "event", e))
			}
			for i := range max(len(got), len(expected)) {
				switch {
				case i >= len(got):
					t.Errorf("line %d: missing; want\n%s", i, expected[i])
				case i >= len(expected):
					t.Errorf("line %d: unexpected\n%s", i, got[i])
				case !reflect.DeepEqual(comparable(t, got[i]), comparable(t, expected[i])):
					t.Errorf("line %d:\n got %s\nwant %s", i, got[i], expected[i])
				}
			}
		})
	}
}

// readJSONLines returns the lines of the JSON-lines file at path.
func readJSONLines(t *testing.T, path string) [][]byte {
	t.Helper()
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var lines [][]byte
	for line := range bytes.SplitSeq(bytes.TrimSpace(data), []byte{'\n'}) {
		lines = append(lines, line)
	}
	return lines
}

// marshalLine frames v as a line of an event stream: {"type": typ, ...v}.
func marshalLine(t *testing.T, typ string, v any) []byte {
	t.Helper()
	body, err := json.Marshal(v)
	if err != nil {
		t.Fatal(err)
	}
	return append([]byte(`{"type":"`+typ+`",`), body[1:]...)
}

// comparable decodes a stream line for comparison: fields with zero values
// dropped, and on a session line, the fields the corpus doesn't compare.
func comparable(t *testing.T, line []byte) any {
	t.Helper()
	var v map[string]any
	if err := json.Unmarshal(line, &v); err != nil {
		t.Fatalf("%s: %v", line, err)
	}
	if v["type"] == "session" {
		delete(v, "mtime")
		delete(v, "status")
		delete(v, "status_at")
	}
	return dropZero(v)
}

// dropZero returns v without the object fields whose values are zero
// (false, 0, "", null, {} or []), at every depth.
func dropZero(v any) any {
	switch v := v.(type) {
	case map[string]any:
		for k, x := range v {
			if x = dropZero(x); isZero(x) {
				delete(v, k)
			} else {
				v[k] = x
			}
		}
		return v
	case []any:
		for i, x := range v {
			v[i] = dropZero(x)
		}
	}
	return v
}

func isZero(v any) bool {
	switch v := v.(type) {
	case nil:
		return true
	case bool:
		return !v
	case float64:
		return v == 0
	case string:
		return v == ""
	case map[string]any:
		return len(v) == 0
	case []any:
		return len(v) == 0
	}
	return false
}

// readmeCase matches a case row of the README's table: | `claude/basic-turn` | ...
var readmeCase = regexp.MustCompile("(?m)^\\| `([a-z]+/[a-z0-9-]+)` ")

func TestCorpusWellFormed(t *testing.T) {
	var files []string
	err := filepath.WalkDir(corpusDir, func(path string, d fs.DirEntry, err error) error {
		if err == nil && !d.IsDir() && strings.HasSuffix(path, ".events.jsonl") {
			files = append(files, path)
		}
		return err
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(files) == 0 {
		t.Fatalf("no .events.jsonl files under %s", corpusDir)
	}
	for _, f := range files {
		t.Run(strings.TrimPrefix(f, corpusDir+"/"), func(t *testing.T) { checkEventsFile(t, f) })
	}
}

// checkEventsFile checks one expected stream: a session line naming a transcript that
// exists with that size, then event lines with every framing field, in seq order, each
// pointing at the start of a line of that transcript.
func checkEventsFile(t *testing.T, path string) {
	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var transcript []byte
	var lineStarts []int
	sc := bufio.NewScanner(bytes.NewReader(data))
	sc.Buffer(nil, 16<<20)
	for n := 0; sc.Scan(); n++ {
		var rec map[string]json.RawMessage
		if err := json.Unmarshal(sc.Bytes(), &rec); err != nil {
			t.Fatalf("line %d: %v", n, err)
		}
		var typ string
		json.Unmarshal(rec["type"], &typ)
		if n == 0 {
			if typ != "session" {
				t.Fatalf("line 0: type %q, want session", typ)
			}
			var s struct {
				SessionID string `json:"session_id"`
				Harness   string `json:"harness"`
				Path      string `json:"path"`
				Size      int    `json:"size"`
			}
			if err := json.Unmarshal(sc.Bytes(), &s); err != nil {
				t.Fatalf("session line: %v", err)
			}
			if s.SessionID == "" || !slices.Contains([]string{"claude", "agy", "kimi"}, s.Harness) {
				t.Fatalf("session line: session_id %q, harness %q", s.SessionID, s.Harness)
			}
			if transcript, err = os.ReadFile(filepath.Join(corpusDir, s.Path)); err != nil {
				t.Fatalf("session path: %v", err)
			}
			if len(transcript) == 0 {
				t.Fatalf("transcript %s is empty", s.Path)
			}
			if len(transcript) != s.Size {
				t.Errorf("session size %d, transcript %s has %d bytes", s.Size, s.Path, len(transcript))
			}
			lineStarts = []int{0}
			for i, b := range transcript[:len(transcript)-1] {
				if b == '\n' {
					lineStarts = append(lineStarts, i+1)
				}
			}
			continue
		}
		if typ != "event" {
			t.Errorf("line %d: type %q, want event", n, typ)
			continue
		}
		for _, k := range []string{"id", "op", "seq", "at", "line", "ts", "turn", "kind"} {
			if _, ok := rec[k]; !ok {
				t.Errorf("line %d: no %q", n, k)
			}
		}
		var e struct {
			ID, Op, TS, Kind string
			Seq, At, Line    int
			Turn             struct{ ID string }
		}
		if err := json.Unmarshal(sc.Bytes(), &e); err != nil {
			t.Errorf("line %d: %v", n, err)
			continue
		}
		if e.Op != "add" && e.Op != "update" {
			t.Errorf("line %d: op %q", n, e.Op)
		}
		if e.Seq != n-1 {
			t.Errorf("line %d: seq %d, want %d", n, e.Seq, n-1)
		}
		if !slices.Contains([]string{"prompt", "think", "say", "tool", "system", "request"}, e.Kind) {
			t.Errorf("line %d: kind %q", n, e.Kind)
		} else if _, ok := rec[e.Kind]; !ok {
			t.Errorf("line %d: kind %s but no %q object", n, e.Kind, e.Kind)
		}
		if e.ID == "" || e.TS == "" || e.Turn.ID == "" {
			t.Errorf("line %d: empty id, ts or turn.id", n)
		}
		if e.Line < 0 || e.Line >= len(lineStarts) || lineStarts[e.Line] != e.At {
			t.Errorf("line %d: at %d is not the start of transcript line %d", n, e.At, e.Line)
		}
	}
	if err := sc.Err(); err != nil {
		t.Fatal(err)
	}
	if transcript == nil {
		t.Fatal("empty file")
	}
}

// TestCorpusREADME checks that every case the README lists has its expected stream, and
// that every top-level stream is a case the README lists.
func TestCorpusREADME(t *testing.T) {
	readme, err := os.ReadFile(filepath.Join(corpusDir, "README.md"))
	if err != nil {
		t.Fatal(err)
	}
	var listed []string
	for _, m := range readmeCase.FindAllStringSubmatch(string(readme), -1) {
		listed = append(listed, m[1])
		if _, err := os.Stat(filepath.Join(corpusDir, m[1]+".events.jsonl")); err != nil {
			t.Errorf("README lists %s: %v", m[1], err)
		}
	}
	if len(listed) == 0 {
		t.Fatal("README lists no cases")
	}
	streams, err := filepath.Glob(filepath.Join(corpusDir, "*", "*.events.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
	for _, s := range streams {
		c := strings.TrimSuffix(strings.TrimSuffix(strings.TrimPrefix(s, corpusDir+"/"), ".events.jsonl"), ".requests")
		if !slices.Contains(listed, c) {
			t.Errorf("%s is not a case in README.md", s)
		}
	}
}
