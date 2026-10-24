package natsbus

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"time"

	"github.com/nats-io/nats.go"
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
	return resp.Agents, nil
}

// Status returns the status of a remote agent.
func (c *RPCClient) Status(ctx context.Context, ref AgentRef) (*StatusResponse, error) {
	var resp StatusResponse
	if err := c.callAgent(ctx, ref, OpStatus, StatusRequest{Agent: ref.Name}, &resp); err != nil {
		return nil, err
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
	return resp.Output, nil
}

// Send types message into a remote agent's terminal, followed by Enter.
func (c *RPCClient) Send(ctx context.Context, ref AgentRef, message string) error {
	return c.callAgent(ctx, ref, OpSend, SendRequest{Agent: ref.Name, Message: message}, nil)
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
		return &RemoteError{Host: host, Op: op, Msg: resp.Error}
	}
	if out == nil {
		return nil
	}
	if len(resp.Data) == 0 {
		return fmt.Errorf("empty %s reply from host %q", op, host)
	}
	if err := json.Unmarshal(resp.Data, out); err != nil {
		return fmt.Errorf("malformed %s reply from host %q: %w", op, host, err)
	}
	return nil
}
