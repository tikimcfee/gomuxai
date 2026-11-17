//go:build !unix

package termd

import "os"

func statOwnerUID(info os.FileInfo) (int, bool) {
	return 0, false
}
