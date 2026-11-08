package natsbus

import (
	"context"
	"encoding/json"
	"fmt"
	"slices"
	"strings"
	"sync"

	"github.com/nats-io/nats.go"
)

// RPCHandler executes RPC ops on the local host, one method per op.
// Implemented over service.AgentService; natsbus can't import service.
type RPCHandler interface {
	List(ctx context.Context) (*ListResponse, error)
	Status(ctx context.Context, req StatusRequest) (*StatusResponse, error)
	Capture(ctx context.Context, req CaptureRequest) (*CaptureResponse, error)
	Send(ctx context.Context, req SendRequest) (*SendResponse, error)
	Respond(ctx context.Context, req RespondRequest) error
	Keys(ctx context.Context, req KeysRequest) error
	Spawn(ctx context.Context, req SpawnRequest) (*SpawnResponse, error)
	Friction(ctx context.Context, req FrictionRequest) (*FrictionResponse, error)
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
		return h.Send(ctx, req)
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
	OpSpawn: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req SpawnRequest
		if err := decodeAgent(OpSpawn, data, &req, &req.Agent); err != nil {
			return nil, err
		}
		if req.Backend == "" {
			return nil, fmt.Errorf("spawn request names no backend")
		}
		// A bare name would make the new agent a helper of this host's
		// agent of that name, which could then clean it up.
		if req.SpawnedBy != "" {
			ref, err := ParseAgentRef(req.SpawnedBy)
			if err != nil {
				return nil, fmt.Errorf("spawn request's spawned_by: %w", err)
			}
			if ref.Host == "" {
				return nil, fmt.Errorf("spawn request's spawned_by %q names no host: want name@host", req.SpawnedBy)
			}
		}
		return h.Spawn(ctx, req)
	},
	OpFriction: func(ctx context.Context, h RPCHandler, data []byte) (any, error) {
		var req FrictionRequest
		if err := decode(OpFriction, data, &req); err != nil {
			return nil, err
		}
		if err := checkFrictionRequest(req); err != nil {
			return nil, err
		}
		return h.Friction(ctx, req)
	},
}

// checkFrictionRequest refuses a friction request missing what its action
// needs.
func checkFrictionRequest(req FrictionRequest) error {
	if !slices.Contains(FrictionActions, req.Action) {
		return fmt.Errorf("unknown friction action %q (want %s)", req.Action, strings.Join(FrictionActions, ", "))
	}
	if req.Action == FrictionGet {
		if len(req.IDs) == 0 {
			return fmt.Errorf("friction get request names no ids")
		}
		return nil
	}
	if req.ID < 1 {
		return fmt.Errorf("friction %s request names no entry", req.Action)
	}
	if req.Action == FrictionResolve && req.Status != "resolved" && req.Status != "wontfix" {
		return fmt.Errorf("friction resolve request's status %q isn't resolved or wontfix", req.Status)
	}
	// Every change names the entry on the calling host it's linked from:
	// resolve closes only an entry linked to it.
	if req.From == nil || req.From.ID < 1 {
		return fmt.Errorf("friction %s request names no entry it comes from", req.Action)
	}
	if err := ValidateHost(req.From.Host); err != nil {
		return fmt.Errorf("friction %s request's from: %w", req.Action, err)
	}
	return nil
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
			// As at start: once stop returns, the server has dropped the
			// subscriptions, so requests fail fast with no responders
			// instead of waiting out their timeout. Best effort: the
			// connection may already be closed.
			_ = nc.Flush()
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
