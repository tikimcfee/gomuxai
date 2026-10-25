package service

import (
	"context"
	"fmt"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// rpcHandler serves bus RPC ops through an AgentService.
type rpcHandler struct {
	s *AgentService
}

var _ natsbus.RPCHandler = rpcHandler{}

// NewRPCHandler returns a natsbus.RPCHandler that runs each op on s, the
// same way the CLI does for a local agent.
func NewRPCHandler(s *AgentService) natsbus.RPCHandler {
	return rpcHandler{s: s}
}

func (h rpcHandler) List(ctx context.Context) (*natsbus.ListResponse, error) {
	agents, err := h.s.List(ctx)
	if err != nil {
		return nil, err
	}
	resp := &natsbus.ListResponse{Agents: make([]natsbus.AgentInfo, 0, len(agents))}
	for _, a := range agents {
		status := AgentStatusUnknown
		if info, err := h.s.StatusInfo(ctx, a.Name); err == nil {
			status = info.Status
		}
		resp.Agents = append(resp.Agents, natsbus.AgentInfo{
			Name:      a.Name,
			Session:   a.SessionName,
			Backend:   a.Backend,
			SpawnedBy: a.SpawnedBy,
			CreatedAt: a.CreatedAt,
			Status:    string(status),
		})
	}
	return resp, nil
}

func (h rpcHandler) Status(ctx context.Context, req natsbus.StatusRequest) (*natsbus.StatusResponse, error) {
	info, err := h.s.StatusInfo(ctx, req.Agent)
	if err != nil {
		return nil, remoteErr(err)
	}
	return &natsbus.StatusResponse{
		Status:    string(info.Status),
		Source:    info.Source,
		Event:     info.Event,
		Detail:    info.Detail,
		Tool:      info.Tool,
		UpdatedAt: info.UpdatedAt,
		ToolInput: info.ToolInput,

		ReportedStatus: string(info.ReportedStatus),
		ReportedAt:     info.ReportedAt,
		SentAt:         info.SentAt,
		TurnAt:         info.TurnAt,
		Held:           info.Held,
	}, nil
}

func (h rpcHandler) Capture(ctx context.Context, req natsbus.CaptureRequest) (*natsbus.CaptureResponse, error) {
	out, err := h.s.Capture(ctx, req.Agent, req.Lines, req.Raw)
	if err != nil {
		return nil, remoteErr(err)
	}
	return &natsbus.CaptureResponse{Output: out}, nil
}

func (h rpcHandler) Send(ctx context.Context, req natsbus.SendRequest) (*natsbus.SendResponse, error) {
	res, err := h.s.Send(ctx, req.Agent, req.Message, SendOptions{Now: req.Now, From: "remote"})
	if err != nil {
		return nil, remoteErr(err)
	}
	return &natsbus.SendResponse{
		Status:    string(res.Status),
		Detail:    res.Detail,
		Held:      res.Held,
		HeldCount: res.HeldCount,
		Typed:     res.Typed,
	}, nil
}

func (h rpcHandler) Respond(ctx context.Context, req natsbus.RespondRequest) error {
	resp, err := ParsePermissionResponse(req.Response)
	if err != nil {
		return err
	}
	_, err = h.s.RespondPermission(ctx, req.Agent, resp, PermissionOptions{
		Force:      req.Force,
		ExpectTool: req.ExpectTool,
	})
	return remoteErr(err)
}

func (h rpcHandler) Keys(ctx context.Context, req natsbus.KeysRequest) error {
	return remoteErr(h.s.Keys(ctx, req.Agent, req.Text, req.Key))
}

// Keys sends text, then the special key, to an agent's session. Either may be
// empty. Unlike `agent keys <session>`, it only reaches managed agents.
func (s *AgentService) Keys(ctx context.Context, name, text, key string) error {
	agent, err := s.getAgent(ctx, name)
	if err != nil {
		return err
	}
	if text != "" {
		if err := s.tmux.SendKeys(agent.SessionName, text); err != nil {
			return fmt.Errorf("sending keys: %w", err)
		}
	}
	if key != "" {
		if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
			return fmt.Errorf("sending %q: %w", key, err)
		}
	}
	return nil
}

var permissionResponses = map[string]backend.PermissionResponse{
	natsbus.ResponseApprove:       backend.PermissionApprove,
	natsbus.ResponseApproveAlways: backend.PermissionApproveAlways,
	natsbus.ResponseDeny:          backend.PermissionDeny,
}

// ParsePermissionResponse maps a RespondRequest.Response to a backend response.
func ParsePermissionResponse(s string) (backend.PermissionResponse, error) {
	if resp, ok := permissionResponses[s]; ok {
		return resp, nil
	}
	return 0, fmt.Errorf("invalid permission response %q (want %s, %s, or %s)",
		s, natsbus.ResponseApprove, natsbus.ResponseApproveAlways, natsbus.ResponseDeny)
}

// PermissionResponseName is the RespondRequest.Response for resp.
func PermissionResponseName(resp backend.PermissionResponse) (string, error) {
	for name, r := range permissionResponses {
		if r == resp {
			return name, nil
		}
	}
	return "", fmt.Errorf("unknown permission response %d", resp)
}
