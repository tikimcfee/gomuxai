package cli

import "errors"

// exitError is an error that exits with a code other than 1, e.g. wait's
// timeout. Commands return it rather than calling os.Exit, so Execute still
// records the command and flushes the bus publisher.
type exitError struct {
	code int
	err  error
}

func (e *exitError) Error() string { return e.err.Error() }
func (e *exitError) Unwrap() error { return e.err }

// ExitCode returns the process exit code for Execute's error.
func ExitCode(err error) int {
	if err == nil {
		return 0
	}
	if e, ok := errors.AsType[*exitError](err); ok {
		return e.code
	}
	return 1
}
