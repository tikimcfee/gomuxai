//go:build linux || darwin

package cli

import (
	"os"
	"path/filepath"
	"testing"

	"golang.org/x/sys/unix"
)

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

	// Each end, before and after its reader closes. A full pipe has no room
	// to write, which macOS reports only alongside the hangup.
	pipe := func(full bool) (*os.File, func()) {
		var p [2]int
		if err := unix.Pipe(p[:]); err != nil {
			t.Fatal(err)
		}
		if full {
			if err := unix.SetNonblock(p[1], true); err != nil {
				t.Fatal(err)
			}
			for buf := make([]byte, 1<<16); ; {
				if _, err := unix.Write(p[1], buf); err != nil {
					break
				}
			}
		}
		// The os.File owns the write end from here: closing it is its job
		// (a finalizer would close the fd again, by then perhaps another
		// file's).
		w := os.NewFile(uintptr(p[1]), "pipe")
		t.Cleanup(func() { w.Close() })
		return w, func() { unix.Close(p[0]) }
	}
	socket := func() (*os.File, func()) {
		sp, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM, 0)
		if err != nil {
			t.Fatal(err)
		}
		w := os.NewFile(uintptr(sp[0]), "socket")
		t.Cleanup(func() { w.Close() })
		return w, func() { unix.Close(sp[1]) }
	}
	for name, open := range map[string]func() (*os.File, func()){
		"pipe":      func() (*os.File, func()) { return pipe(false) },
		"full pipe": func() (*os.File, func()) { return pipe(true) },
		"socket":    socket,
	} {
		w, closeReader := open()
		if readerGone(w) {
			t.Errorf("readerGone on a %s whose reader is open", name)
		}
		closeReader()
		if !readerGone(w) {
			t.Errorf("readerGone on a %s whose reader closed", name)
		}
	}
	// Neither has a reader to lose.
	file, err := os.Create(filepath.Join(t.TempDir(), "out"))
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	null, err := os.OpenFile(os.DevNull, os.O_WRONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer null.Close()
	for _, f := range []*os.File{file, null} {
		if readerGone(f) {
			t.Errorf("readerGone on %s", f.Name())
		}
	}
}
