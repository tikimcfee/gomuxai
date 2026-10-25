// Package natsbus is the cross-host agent bus: one NATS server per host,
// linked to the others as leaf nodes. See "The bus" in docs/ARCHITECTURE.md
// for the design ("Revision 2026-10" in docs/history/NATS_INTEGRATION_PLAN.md
// for how it got there).
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

// AllOps lists every RPC op a bus serves to local clients.
var AllOps = []string{OpList, OpStatus, OpCapture, OpSend, OpRespond, OpKeys}

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

// validateAgent reports whether name can be used as an agent name on the bus.
func validateAgent(name string) error {
	if !namePattern.MatchString(name) {
		return fmt.Errorf("invalid agent name %q", name)
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

// InboxPrefix is where host's clients receive replies. Each host has its
// own, so the hub can forward a peer the replies to its requests without
// also forwarding everyone else's (capture output, status details).
func InboxPrefix(host string) string {
	return "_INBOX." + host
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
	if err := validateAgent(name); err != nil {
		return AgentRef{}, err
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
	SpawnedBy string    `json:"spawned_by,omitempty"` // parent agent; "" when a person spawned it
	CreatedAt time.Time `json:"created_at"`
	// Status is the agent's resolved status, as StatusResponse.Status;
	// empty from hosts that predate it.
	Status string `json:"status,omitempty"`
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
	// ToolInput is the full input (JSON) of the tool call a permission
	// prompt asks about; empty otherwise, and from hosts that predate it.
	ToolInput string `json:"tool_input,omitempty"`

	// The last recorded report, which Status may lag or override.
	ReportedStatus string    `json:"reported_status,omitempty"`
	ReportedAt     time.Time `json:"reported_at,omitempty"`
	// When a message was last sent to the agent, and when its last turn
	// started (see Observation); zero from hosts that predate them.
	SentAt time.Time `json:"sent_at,omitempty"`
	TurnAt time.Time `json:"turn_at,omitempty"`
	// Held is how many messages wait for the agent (see service.Send);
	// zero from hosts that predate it.
	Held int `json:"held,omitempty"`
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
	// Now types the message even while the agent is busy, rather than
	// holding it for the end of its turn. Hosts that predate holding
	// ignore it: they always type.
	Now bool `json:"now,omitempty"`
}

// SendResponse says what the receiving host did with a message. Hosts that
// predate it reply with no data.
type SendResponse struct {
	Status    string `json:"status"`
	Detail    string `json:"detail,omitempty"`
	Held      bool   `json:"held,omitempty"`
	HeldCount int    `json:"held_count,omitempty"`
	Typed     int    `json:"typed,omitempty"`
}

// RespondRequest answers a permission prompt (agent approve / deny).
type RespondRequest struct {
	Agent      string `json:"agent"`
	Response   string `json:"response"` // one of the Response* values
	Force      bool   `json:"force,omitempty"`
	ExpectTool string `json:"expect_tool,omitempty"`
}

// Permission responses carried by RespondRequest.Response.
const (
	ResponseApprove       = "approve"
	ResponseApproveAlways = "approve_always"
	ResponseDeny          = "deny"
)

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
