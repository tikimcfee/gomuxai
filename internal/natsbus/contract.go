// Package natsbus is the cross-host agent bus: one NATS server per host,
// linked to the others as leaf nodes. See "Revision 2026-10" in
// NATS_INTEGRATION_PLAN.md for the design.
//
// SQLite stays each host's source of truth. The bus adds two things:
//
//   - RPC: request/reply on gomux.<host>.rpc.<op>. The host's bus executes
//     the op through its local AgentService and replies with an RPCResponse.
//   - Events: every status report is published on
//     gomux.<host>.agent.<name>.status as a StatusEvent (core NATS, not
//     persisted), so watchers and `agent wait` need no polling.
//
// This file is the contract between hosts: subjects, ops, and wire types.
package natsbus

import (
	"encoding/json"
	"errors"
	"fmt"
	"regexp"
	"strings"
	"time"

	"github.com/nats-io/nats.go"
)

// SubjectRoot prefixes every v1 subject, so bus permissions can be scoped to it.
const SubjectRoot = "gomux"

// RPC operations a bus serves. Each host's config allowlists which ones
// remote callers may use; OpKeys is off for remote callers by default.
const (
	OpList    = "list"    // ListRequest -> ListResponse
	OpStatus  = "status"  // StatusRequest -> StatusResponse
	OpCapture = "capture" // CaptureRequest -> CaptureResponse
	OpSend    = "send"    // SendRequest -> (no data)
	OpRespond = "respond" // RespondRequest -> (no data): approve / deny
	OpKeys    = "keys"    // KeysRequest -> (no data): raw keys, local-only by default
)

// DefaultRemoteOps are the ops a bus serves to other hosts unless configured otherwise.
var DefaultRemoteOps = []string{OpList, OpStatus, OpCapture, OpSend, OpRespond}

// namePattern matches host and agent names. Both become subject tokens, so
// they must not contain '.', '*', '>' or whitespace. Agent names are already
// restricted to this by service.ValidateAgentName.
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

// SubjectLocal is a subject only local clients of host can reach: peer links
// never carry gomux.<host>.local.> (see serverOptions and remoteLeaf).
func SubjectLocal(host, name string) string {
	return fmt.Sprintf("%s.%s.local.%s", SubjectRoot, host, name)
}

// SubjectStatus is where host publishes agent's status events.
func SubjectStatus(host, agent string) string {
	return fmt.Sprintf("%s.%s.agent.%s.status", SubjectRoot, host, agent)
}

// SubjectAllStatus matches every status event on host ("*" for all hosts).
func SubjectAllStatus(host string) string {
	return fmt.Sprintf("%s.%s.agent.*.status", SubjectRoot, host)
}

// AgentRef addresses an agent, optionally on another host.
type AgentRef struct {
	Name string
	Host string // empty: the local host
}

// ParseAgentRef parses "name" or "name@host".
func ParseAgentRef(s string) (AgentRef, error) {
	name, host, remote := strings.Cut(s, "@")
	if remote {
		if err := ValidateHost(host); err != nil {
			return AgentRef{}, err
		}
	}
	if !namePattern.MatchString(name) {
		return AgentRef{}, fmt.Errorf("invalid agent name %q", name)
	}
	return AgentRef{Name: name, Host: host}, nil
}

func (r AgentRef) String() string {
	if r.Host == "" {
		return r.Name
	}
	return r.Name + "@" + r.Host
}

// RPCResponse wraps every reply. Error is set when the op failed; Data holds
// the op's response type otherwise (absent for ops with no data).
type RPCResponse struct {
	Error string          `json:"error,omitempty"`
	Data  json.RawMessage `json:"data,omitempty"`
}

type ListRequest struct{}

type ListResponse struct {
	Agents []AgentInfo `json:"agents"`
}

type AgentInfo struct {
	Name      string    `json:"name"`
	Session   string    `json:"session,omitempty"` // tmux session on the agent's host
	Backend   string    `json:"backend"`
	CreatedAt time.Time `json:"created_at"`
}

type StatusRequest struct {
	Agent string `json:"agent"`
}

// StatusResponse mirrors service.StatusInfo.
type StatusResponse struct {
	Status    string    `json:"status"`
	Source    string    `json:"source"`
	Event     string    `json:"event,omitempty"`
	Detail    string    `json:"detail,omitempty"`
	Tool      string    `json:"tool,omitempty"`
	UpdatedAt time.Time `json:"updated_at,omitempty"`

	// The last recorded report, which Status may lag or override.
	ReportedStatus string    `json:"reported_status,omitempty"`
	ReportedAt     time.Time `json:"reported_at,omitempty"`
}

type CaptureRequest struct {
	Agent string `json:"agent"`
	Lines int    `json:"lines"`
	Raw   bool   `json:"raw"`
}

type CaptureResponse struct {
	Output string `json:"output"`
}

type SendRequest struct {
	Agent   string `json:"agent"`
	Message string `json:"message"`
}

// RespondRequest answers a permission prompt (agent approve / deny).
type RespondRequest struct {
	Agent      string `json:"agent"`
	Response   string `json:"response"` // "approve", "approve_always", or "deny"
	Force      bool   `json:"force,omitempty"`
	ExpectTool string `json:"expect_tool,omitempty"`
}

type KeysRequest struct {
	Agent string `json:"agent"`
	Text  string `json:"text,omitempty"` // literal text, sent before Key
	Key   string `json:"key,omitempty"`  // special key, e.g. "Enter", "c-c"
}

// StatusEvent is published on SubjectStatus whenever an agent's reported
// status is recorded. It mirrors the report, not the resolved StatusInfo.
type StatusEvent struct {
	Host   string    `json:"host"`
	Agent  string    `json:"agent"`
	Status string    `json:"status"`
	Event  string    `json:"event,omitempty"`
	Detail string    `json:"detail,omitempty"`
	Tool   string    `json:"tool,omitempty"`
	At     time.Time `json:"at"`
}

// ErrNoBus means no bus is running on this host. Callers fall back to
// local-only behavior for local agents and fail for remote ones.
var ErrNoBus = errors.New("no agent bus running on this host (start one with `agent bus start`)")

// LocalConnector opens a client connection to this host's bus, authenticated
// as a local client: ConnectLocal in production, natstest in tests.
type LocalConnector func() (*nats.Conn, error)
