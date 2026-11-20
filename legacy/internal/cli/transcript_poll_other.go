//go:build !linux && !darwin

package cli

import "os"

// readerGone can't tell here: a closed reader is noticed at the next
// write.
func readerGone(*os.File) bool { return false }
