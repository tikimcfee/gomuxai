package config

import (
	"os/exec"
	"strings"
)

// localHostName returns the LocalHostName macOS keeps in its system
// configuration (Sharing settings), or "". The kernel's hostname, which
// os.Hostname reads, follows the network instead: DHCP or reverse DNS can
// make it "Mac" or another machine's name.
func localHostName() string {
	out, err := exec.Command("/usr/sbin/scutil", "--get", "LocalHostName").Output()
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(out))
}
