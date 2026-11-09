package cli

import (
	"fmt"
	"os"
	"os/exec"
	"syscall"
)

// execProcess replaces this process with another program; syscall.Exec,
// a variable for tests.
var execProcess = syscall.Exec

// lookPath finds a program on PATH; exec.LookPath, a variable for tests.
var lookPath = exec.LookPath

// execInto replaces this process with the program named (looked up on
// PATH) and argv, which starts with its name. Attach hands the terminal
// over this way, rather than running the program as a child: a signal to
// agent attach then reaches tmux or ssh directly, and nothing is left
// behind if either is killed. The command-log row is written first, since
// nothing runs after. It returns only if the exec failed.
func execInto(name string, argv []string) error {
	path, err := lookPath(name)
	if err != nil {
		return err
	}
	if !runRecorded {
		recordCommand(runCmd, os.Args[1:], runStart, nil)
		runRecorded = true
	}
	closeBusPublisher()
	if err := execProcess(path, argv, os.Environ()); err != nil {
		return fmt.Errorf("running %s: %w", path, err)
	}
	return nil
}
