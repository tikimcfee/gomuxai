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
	var resp ListResponse
	if err := c.call(ctx, host, OpList, ListRequest{}, &resp); err != nil {
		return nil, err
	}
	for i := range resp.Agents {
		a := &resp.Agents[i]
		a.Name, a.Session, a.Backend = clean(a.Name), clean(a.Session), clean(a.Backend)
	}
	return resp.Agents, nil
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

// Capture returns a remote agent's terminal output.
func (c *RPCClient) Capture(ctx context.Context, ref AgentRef, lines int, raw bool) (string, error) {
	var resp CaptureResponse
	req := CaptureRequest{Agent: ref.Name, Lines: lines, Raw: raw}
	if err := c.callAgent(ctx, ref, OpCapture, req, &resp); err != nil {
		return "", err
	}
	if !raw { // raw was asked for explicitly; it keeps escape sequences
		return clean(resp.Output), nil
	}
	return resp.Output, nil
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
	resp.Status, resp.Detail = clean(resp.Status), clean(resp.Detail)
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
		return fmt.Errorf("no bus answering for host %q (is its bus running and linked, and does it allow %q?)", host, op)
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
