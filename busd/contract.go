// Subject and op names shared with the Rust agent-bus crate.
// Copied from the Go agent's legacy/internal/natsbus/contract.go (only what the server needs).
package main

import (
	"fmt"
	"regexp"
)

// SubjectRoot prefixes every v1 subject, so bus permissions can be scoped to it.
const SubjectRoot = "gomux"

// RPC operations a bus serves. Each host's config allowlists which ones
// remote callers may use; OpKeys, OpSpawn and OpResume are off for remote
// callers by default.
const (
	OpList     = "list"
	OpStatus   = "status"
	OpCapture  = "capture"
	OpSend     = "send"
	OpRespond  = "respond"
	OpKeys     = "keys"
	OpSpawn    = "spawn"
	OpLogs     = "logs"
	OpResume   = "resume"
	OpFriction = "friction"
	OpFleet    = "fleet"
	OpTemplate = "template"
	OpManage   = "manage"
)

// AllOps lists every RPC op a bus serves to local clients.
var AllOps = []string{OpList, OpStatus, OpCapture, OpSend, OpRespond, OpKeys, OpSpawn, OpFriction, OpLogs, OpResume, OpFleet, OpTemplate, OpManage}

// DefaultRemoteOps are the ops a bus serves to other hosts unless configured otherwise.
var DefaultRemoteOps = []string{OpList, OpStatus, OpCapture, OpSend, OpRespond, OpFriction, OpLogs, OpFleet, OpTemplate, OpManage}

// namePattern matches host and agent names. Both become subject tokens, so
// they must not contain '.', '*', '>' or whitespace.
var namePattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9_-]*$`)

// ValidateHost reports whether host can be used as a host name on the bus.
func ValidateHost(host string) error {
	if !namePattern.MatchString(host) {
		return fmt.Errorf("invalid host name %q: use letters, digits, '-' and '_'", host)
	}
	return nil
}

// SubjectRPC is the request subject for op on host.
func SubjectRPC(host, op string) string {
	return fmt.Sprintf("%s.%s.rpc.%s", SubjectRoot, host, op)
}

// SubjectAllStatus matches every status event on host ("*" for all hosts).
func SubjectAllStatus(host string) string {
	return fmt.Sprintf("%s.%s.agent.*.status", SubjectRoot, host)
}

// SubjectFocus is where host publishes focus changes ("*" for all hosts).
func SubjectFocus(host string) string {
	return fmt.Sprintf("%s.%s.focus", SubjectRoot, host)
}

// InboxPrefix is where host's clients receive replies. Each host has its
// own, so the hub can forward a peer the replies to its requests without
// also forwarding everyone else's (capture output, status details).
func InboxPrefix(host string) string {
	return "_INBOX." + host
}
