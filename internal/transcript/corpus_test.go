package transcript

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"
	"testing"
)

// corpusDir holds the conformance corpus (see its README.md). This test only checks
// that the corpus is well formed; the conformance test that parses its transcripts
// comes with the parser.
const corpusDir = "testdata/corpus"

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
