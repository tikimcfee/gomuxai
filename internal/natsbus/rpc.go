package natsbus

import (
	"context"
	"encoding/json"
	"fmt"
	"sync"

	"github.com/nats-io/nats.go"
)

// RPCHandler executes RPC ops on the local host, one method per op.
// Implemented over service.AgentService; natsbus can't import service.
type RPCHandler interface {
	List(ctx context.Context) (*ListResponse, error)
	Status(ctx context.Context, req StatusRequest) (*StatusResponse, error)
	Capture(ctx context.Context, req CaptureRequest) (*CaptureResponse, error)
	Send(ctx context.Context, req SendRequest) error
	Respond(ctx context.Context, req RespondRequest) error
	Keys(ctx context.Context, req KeysRequest) error
}

// opFunc decodes an op's request, runs it, and returns the reply's data
// (nil for ops with none).
type opFunc func(ctx context.Context, h RPCHandler, data []byte) (any, error)

var rpcOps = map[string]opFunc{
	OpList: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req ListRequest
		if err := decode(OpList, data, &req); err != nil {
			return nil, err
		}
		return h.List(ctx)
	},
	OpStatus: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req StatusRequest
		if err := decodeAgent(OpStatus, data, &req, &req.Agent); err != nil {
			return nil, err
		}
		return h.Status(ctx, req)
	},
	OpCapture: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req CaptureRequest
		if err := decodeAgent(OpCapture, data, &req, &req.Agent); err != nil {
			return nil, err
		}
		return h.Capture(ctx, req)
	},
	OpSend: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req SendRequest
		if err := decodeAgent(OpSend, data, &req, &req.Agent); err != nil {
			return nil, err
		}
		return nil, h.Send(ctx, req)
	},
	OpRespond: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req RespondRequest
		if err := decodeAgent(OpRespond, data, &req, &req.Agent); err != nil {
			return nil, err
		}
		return nil, h.Respond(ctx, req)
	},
	OpKeys: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req KeysRequest
		if err := decodeAgent(OpKeys, data, &req, &req.Agent); err != nil {
			return nil, err
		}
		if req.Text == "" && req.Key == "" {
			return nil, fmt.Errorf("keys request has neither text nor key")
		}
		return nil, h.Keys(ctx, req)
	},
}

// decode parses an op's request into req. An empty body is the zero request.
func decode(op string, data []byte, req any) error {
	if len(data) == 0 {
		return nil
	}
	if err := json.Unmarshal(data, req); err != nil {
		return fmt.Errorf("malformed %s request: %w", op, err)
	}
	return nil
}

// decodeAgent is decode for a request naming an agent: agent points at the
// decoded request's name, which is checked before the request runs.
func decodeAgent(op string, data []byte, req any, agent *string) error {
	if err := decode(op, data, req); err != nil {
		return err
	}
	return validateAgent(*agent)
}

// MaxInFlight bounds the requests a bus serves at once; more get a "busy" reply.
const MaxInFlight = 32

// ServeRPC answers requests for each op in allowedOps on SubjectRPC(host, op),
// running them through h. Each request runs in its own goroutine, so a slow
// status query doesn't hold up the next request. Which peers may reach which
// ops is enforced by the bus's subject permissions, not here.
//
// The returned stop function unsubscribes, cancels in-flight requests, and
// waits for their handlers to return. It does not close nc.
func ServeRPC(nc *nats.Conn, host string, h RPCHandler, allowedOps []string) (stop func(), err error) {
	if err := ValidateHost(host); err != nil {
		return nil, err
	}
	for _, op := range allowedOps {
		if _, ok := rpcOps[op]; !ok {
			return nil, fmt.Errorf("unknown RPC op %q", op)
		}
	}

	ctx, cancel := context.WithCancel(context.Background())
	var (
		mu      sync.Mutex
		stopped bool
		wg      sync.WaitGroup
		subs    []*nats.Subscription
	)
	unsubscribe := func() {
		for _, sub := range subs {
			_ = sub.Unsubscribe()
		}
	}

	// Bound concurrent requests: some ops run the backend CLI, and a peer
	// flooding requests mustn't be able to start unbounded processes.
	inflight := make(chan struct{}, MaxInFlight)
	for _, op := range allowedOps {
		run := rpcOps[op]
		sub, err := nc.Subscribe(SubjectRPC(host, op), func(msg *nats.Msg) {
			select {
			case inflight <- struct{}{}:
			default:
				reply(msg, RPCResponse{Error: fmt.Sprintf("busy: %d requests already in flight", MaxInFlight)})
				return
			}
			// Registering under the lock keeps wg.Add from racing stop's wg.Wait.
			mu.Lock()
			if stopped {
				mu.Unlock()
				<-inflight
				return
			}
			wg.Add(1)
			mu.Unlock()
			go func() {
				defer wg.Done()
				defer func() { <-inflight }()
				reply(msg, serveOne(ctx, h, op, run, msg.Data))
			}()
		})
		if err != nil {
			unsubscribe()
			cancel()
			return nil, fmt.Errorf("subscribing to %s: %w", SubjectRPC(host, op), err)
		}
		subs = append(subs, sub)
	}
	// Make sure the subscriptions reached the server before callers send.
	if err := nc.Flush(); err != nil {
		unsubscribe()
		cancel()
		return nil, fmt.Errorf("flushing subscriptions: %w", err)
	}

	var once sync.Once
	return func() {
		once.Do(func() {
			mu.Lock()
			stopped = true
			mu.Unlock()
			unsubscribe()
			cancel()
			wg.Wait()
		})
	}, nil
}

// serveOne runs one request and builds its reply. A panicking handler is
// reported as an error instead of taking the bus down; the stack stays out
// of the reply, which may go to another host.
func serveOne(ctx context.Context, h RPCHandler, op string, run opFunc, data []byte) (resp RPCResponse) {
	defer func() {
		if r := recover(); r != nil {
			resp = RPCResponse{Error: fmt.Sprintf("internal error in %s: %v", op, r)}
		}
	}()
	result, err := run(ctx, h, data)
	if err != nil {
		return RPCResponse{Error: err.Error()}
	}
	if result == nil {
		return RPCResponse{}
	}
	raw, err := json.Marshal(result)
	if err != nil {
		return RPCResponse{Error: fmt.Sprintf("encoding %s response: %v", op, err)}
	}
	return RPCResponse{Data: raw}
}

func reply(msg *nats.Msg, resp RPCResponse) {
	if msg.Reply == "" {
		return // published, not requested: nobody to answer
	}
	data, err := json.Marshal(resp)
	if err != nil {
		data = []byte(`{"error":"encoding response"}`)
	}
	_ = msg.Respond(data)
}
