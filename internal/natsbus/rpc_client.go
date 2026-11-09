package natsbus

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// DefaultRPCTimeout bounds each request. Status can run the backend CLI on
// the far side, and respond checks status and the screen before answering.
const DefaultRPCTimeout = 10 * time.Second

// RemoteError is an error the remote host's handler returned. Its message is
// the remote one, unchanged, so it reads the same as the local error would.
type RemoteError struct {
	Host string
	Op   string
	Msg  string
}

func (e *RemoteError) Error() string { return e.Msg }

// NoRespondersError is the error for a request that no bus answered:
// host's bus isn't running or linked, doesn't serve op to other hosts, or
// host isn't a name any bus has (subjects are case-sensitive, so a host is
// reached only by its own spelling).
type NoRespondersError struct {
	Host string
	Op   string
}

func (e *NoRespondersError) Error() string {
	return fmt.Sprintf("no bus answering for host %q (is its bus running and linked, and does it allow %q?)", e.Host, e.Op)
}

// RPCClient calls the RPC ops of other hosts' buses.
type RPCClient struct {
	nc      *nats.Conn
	timeout time.Duration
}

// NewRPCClient returns a client over nc. timeout bounds each request;
// zero means DefaultRPCTimeout.
func NewRPCClient(nc *nats.Conn, timeout time.Duration) *RPCClient {
	if timeout <= 0 {
		timeout = DefaultRPCTimeout
	}
	return &RPCClient{nc: nc, timeout: timeout}
}

// List returns the agents on host.
func (c *RPCClient) List(ctx context.Context, host string) ([]AgentInfo, error) {
	resp, err := c.ListHost(ctx, host)
	if err != nil {
		return nil, err
	}
	return resp.Agents, nil
}

// ListHost returns host's list reply: its agents, and the path of its
// agent binary (empty from a host that predates it), which a caller about
// to run it checks with ValidateAgentBinary.
func (c *RPCClient) ListHost(ctx context.Context, host string) (*ListResponse, error) {
	var resp ListResponse
	if err := c.call(ctx, host, OpList, ListRequest{}, &resp); err != nil {
		return nil, err
	}
	resp.AgentBinary = clean(resp.AgentBinary)
	for i := range resp.Agents {
		a := &resp.Agents[i]
		for _, f := range []*string{&a.Name, &a.Session, &a.Backend, &a.SpawnedBy, &a.Status, &a.EndReason, &a.Detail, &a.SessionID, &a.Cwd} {
			*f = clean(*f)
		}
	}
	return &resp, nil
}

// Status returns the status of a remote agent.
func (c *RPCClient) Status(ctx context.Context, ref AgentRef) (*StatusResponse, error) {
	var resp StatusResponse
	if err := c.callAgent(ctx, ref, OpStatus, StatusRequest{Agent: ref.Name}, &resp); err != nil {
		return nil, err
	}
	for _, f := range []*string{&resp.Status, &resp.Source, &resp.Event, &resp.Detail, &resp.Tool, &resp.ReportedStatus} {
		*f = clean(*f)
	}
	return &resp, nil
}

// Capture returns a remote agent's terminal output, as plain text or, with
// raw, with its colors and styling. Either way it is sanitized: raw keeps
// SGR sequences (colors, bold, dim), and nothing else that could act on the
// terminal it's printed in.
func (c *RPCClient) Capture(ctx context.Context, ref AgentRef, lines int, raw bool) (string, error) {
	var resp CaptureResponse
	req := CaptureRequest{Agent: ref.Name, Lines: lines, Raw: raw}
	if err := c.callAgent(ctx, ref, OpCapture, req, &resp); err != nil {
		return "", err
	}
	if raw {
		return util.SanitizeStyled(resp.Output), nil
	}
	return clean(resp.Output), nil
}

// Send types message into a remote agent's terminal, followed by Enter.
func (c *RPCClient) Send(ctx context.Context, ref AgentRef, message string) error {
	_, err := c.SendMessage(ctx, ref, SendRequest{Message: message})
	return err
}

// SendMessage sends a message to a remote agent, saying what its host did
// with it; nil (and no error) from a host that predates SendResponse.
// req.Agent is set from ref.
func (c *RPCClient) SendMessage(ctx context.Context, ref AgentRef, req SendRequest) (*SendResponse, error) {
	req.Agent = ref.Name
	var resp SendResponse
	var got bool
	if err := c.callAgent(ctx, ref, OpSend, req, optionalReply{&resp, &got}); err != nil {
		return nil, err
	}
	if !got {
		return nil, nil
	}
	resp.Status, resp.Detail, resp.InputBlocked = clean(resp.Status), clean(resp.Detail), clean(resp.InputBlocked)
	return &resp, nil
}

// optionalReply is a reply's data that hosts predating it don't send: got
// says whether it came.
type optionalReply struct {
	out any
	got *bool
}

// Respond answers a remote agent's permission prompt. req.Agent is set from ref.
func (c *RPCClient) Respond(ctx context.Context, ref AgentRef, req RespondRequest) error {
	req.Agent = ref.Name
	return c.callAgent(ctx, ref, OpRespond, req, nil)
}

// Keys sends text and then key (either may be empty) to a remote agent's
// terminal. Most buses don't serve this op to other hosts.
func (c *RPCClient) Keys(ctx context.Context, ref AgentRef, text, key string) error {
	return c.callAgent(ctx, ref, OpKeys, KeysRequest{Agent: ref.Name, Text: text, Key: key}, nil)
}

// SpawnTimeout bounds a spawn request: the far side waits out the CLI's
// startup (and a workspace trust prompt) before it answers.
const SpawnTimeout = 2 * time.Minute

// Spawn starts an agent on ref.Host. req.Agent is set from ref. It waits up
// to SpawnTimeout, or the client's timeout if that is longer.
func (c *RPCClient) Spawn(ctx context.Context, ref AgentRef, req SpawnRequest) (*SpawnResponse, error) {
	req.Agent = ref.Name
	long := *c
	long.timeout = max(c.timeout, SpawnTimeout)
	var resp SpawnResponse
	if err := long.callAgent(ctx, ref, OpSpawn, req, &resp); err != nil {
		return nil, err
	}
	for _, f := range []*string{&resp.Name, &resp.Session, &resp.Backend, &resp.SpawnedBy, &resp.Cwd, &resp.TaskFile, &resp.Status, &resp.EndReason} {
		*f = clean(*f)
	}
	return &resp, nil
}

// Logs returns what agent logs prints for a remote agent. req.Agent is set
// from ref. Output and Hint are sanitized: no terminal controls but newline
// and tab.
func (c *RPCClient) Logs(ctx context.Context, ref AgentRef, req LogsRequest) (*LogsResponse, error) {
	req.Agent = ref.Name
	var resp LogsResponse
	if err := c.callAgent(ctx, ref, OpLogs, req, &resp); err != nil {
		return nil, err
	}
	resp.Output, resp.Hint = clean(resp.Output), clean(resp.Hint)
	return &resp, nil
}

// Resume starts an ended agent on ref.Host again. req.Agent is set from
// ref. Like Spawn, it waits up to SpawnTimeout for the CLI to start.
func (c *RPCClient) Resume(ctx context.Context, ref AgentRef, req ResumeRequest) (*ResumeResponse, error) {
	req.Agent = ref.Name
	long := *c
	long.timeout = max(c.timeout, SpawnTimeout)
	var resp ResumeResponse
	if err := long.callAgent(ctx, ref, OpResume, req, &resp); err != nil {
		return nil, err
	}
	for _, f := range []*string{&resp.Name, &resp.Session, &resp.Backend, &resp.SessionID, &resp.Cwd, &resp.Status, &resp.EndReason} {
		*f = clean(*f)
	}
	return &resp, nil
}

// Friction sends a friction request to host and returns its answer, with
// the text in it sanitized.
func (c *RPCClient) Friction(ctx context.Context, host string, req FrictionRequest) (*FrictionResponse, error) {
	var resp FrictionResponse
	if err := c.call(ctx, host, OpFriction, req, &resp); err != nil {
		return nil, err
	}
	for i := range resp.Entries {
		e := &resp.Entries[i]
		for _, f := range []*string{&e.Host, &e.UID, &e.Status, &e.Kind, &e.Summary, &e.ResolvedBy, &e.Resolution} {
			*f = clean(*f)
		}
		if e.Requested != nil {
			e.Requested.Host, e.Requested.UID = clean(e.Requested.Host), clean(e.Requested.UID)
		}
		for j := range e.Links {
			e.Links[j].Host, e.Links[j].UID = clean(e.Links[j].Host), clean(e.Links[j].UID)
		}
	}
	return &resp, nil
}

func (c *RPCClient) callAgent(ctx context.Context, ref AgentRef, op string, req, out any) error {
	if ref.Host == "" {
		return fmt.Errorf("%s: no host given for agent %q", op, ref.Name)
	}
	return c.call(ctx, ref.Host, op, req, out)
}

// call sends one request to host and decodes the reply's data into out
// (if non-nil).
func (c *RPCClient) call(ctx context.Context, host, op string, req, out any) error {
	if err := ValidateHost(host); err != nil {
		return err
	}
	data, err := json.Marshal(req)
	if err != nil {
		return fmt.Errorf("encoding %s request: %w", op, err)
	}

	ctx, cancel := context.WithTimeout(ctx, c.timeout)
	defer cancel()
	msg, err := c.nc.RequestWithContext(ctx, SubjectRPC(host, op), data)
	switch {
	case errors.Is(err, nats.ErrNoResponders):
		return &NoRespondersError{Host: host, Op: op}
	case errors.Is(err, context.DeadlineExceeded) || errors.Is(err, nats.ErrTimeout):
		return fmt.Errorf("host %q did not answer %s within %s", host, op, c.timeout)
	case err != nil:
		return fmt.Errorf("%s request to host %q: %w", op, host, err)
	}

	var resp RPCResponse
	if err := json.Unmarshal(msg.Data, &resp); err != nil {
		return fmt.Errorf("malformed %s reply from host %q: %w", op, host, err)
	}
	if resp.Error != "" {
		return &RemoteError{Host: host, Op: op, Msg: clean(resp.Error)}
	}
	if out == nil {
		return nil
	}
	if opt, ok := out.(optionalReply); ok {
		if len(resp.Data) == 0 || string(resp.Data) == "null" {
			return nil
		}
		*opt.got, out = true, opt.out
	}
	if len(resp.Data) == 0 {
		return fmt.Errorf("empty %s reply from host %q", op, host)
	}
	if err := json.Unmarshal(resp.Data, out); err != nil {
		return fmt.Errorf("malformed %s reply from host %q: %w", op, host, err)
	}
	return nil
}

// clean strips terminal controls from text another host sent: it ends up
// printed in the user's terminal, and the other host may be hostile.
func clean(s string) string { return util.SanitizeTerminal(s) }
