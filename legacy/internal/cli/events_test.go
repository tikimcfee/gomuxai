package cli

import (
	"strings"
	"testing"
	"time"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// Events can come from other hosts, so `agent events` must not write their
// fields to the terminal raw: an escape sequence could retitle the window,
// set the clipboard (OSC 52), or redraw what the user sees.
func TestFormatStatusEvent_EscapesControlCharacters(t *testing.T) {
	ev := natsbus.StatusEvent{
		Host: "peer", Agent: "worker", At: time.Now(),
		Status: "idle\x1b[2J",
		Event:  "Stop\x1b]52;c;ZWNobyBwd25lZA==\x07",
		Tool:   "Bash\r\nfake line",
		Detail: "\x1b[31m",
	}
	line := formatStatusEvent(ev)
	if strings.ContainsAny(line, "\x1b\x07\r\n") {
		t.Errorf("control characters reached the terminal: %q", line)
	}

	plain := formatStatusEvent(natsbus.StatusEvent{Host: "peer", Agent: "worker", Status: "waiting", Event: "PermissionRequest", Tool: "Bash"})
	if !strings.HasSuffix(plain, "worker@peer waiting event=PermissionRequest tool=Bash") {
		t.Errorf("plain event changed: %q", plain)
	}
}
