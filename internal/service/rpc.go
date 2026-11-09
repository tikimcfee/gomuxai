package service

import (
	"context"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/types"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/storage"
)

// rpcHandler serves bus RPC ops through an AgentService.
type rpcHandler struct {
	s *AgentService
	x RPCExtras
}

var _ natsbus.RPCHandler = rpcHandler{}

// RPCExtras serves the ops whose logic lives in the CLI rather than the
// service: what agent logs prints, and agent resume (which finds the
// session to continue). The bus's own process supplies them; a nil one
// refuses its op.
type RPCExtras struct {
	Logs   func(ctx context.Context, req natsbus.LogsRequest) (*natsbus.LogsResponse, error)
	Resume func(ctx context.Context, req natsbus.ResumeRequest) (*natsbus.ResumeResponse, error)
}

// NewRPCHandler returns a natsbus.RPCHandler that runs each op on s, the
// same way the CLI does for a local agent. It serves no logs or resume:
// NewRPCHandlerWith does.
func NewRPCHandler(s *AgentService) natsbus.RPCHandler {
	return rpcHandler{s: s}
}

// NewRPCHandlerWith is NewRPCHandler, serving logs and resume through x.
func NewRPCHandlerWith(s *AgentService, x RPCExtras) natsbus.RPCHandler {
	return rpcHandler{s: s, x: x}
}

func (h rpcHandler) Logs(ctx context.Context, req natsbus.LogsRequest) (*natsbus.LogsResponse, error) {
	if h.x.Logs == nil {
		return nil, fmt.Errorf("this host's bus doesn't serve %s", natsbus.OpLogs)
	}
	resp, err := h.x.Logs(ctx, req)
	return resp, remoteErr(err)
}

func (h rpcHandler) Resume(ctx context.Context, req natsbus.ResumeRequest) (*natsbus.ResumeResponse, error) {
	if h.x.Resume == nil {
		return nil, fmt.Errorf("this host's bus doesn't serve %s", natsbus.OpResume)
	}
	resp, err := h.x.Resume(ctx, req)
	return resp, remoteErr(err)
}

func (h rpcHandler) List(ctx context.Context) (*natsbus.ListResponse, error) {
	agents, err := h.s.List(ctx)
	if err != nil {
		return nil, err
	}
	resp := &natsbus.ListResponse{Agents: make([]natsbus.AgentInfo, 0, len(agents)), AgentBinary: h.s.cfg.AgentBinary}
	for _, a := range agents {
		resp.Agents = append(resp.Agents, h.s.agentInfo(ctx, a))
	}
	return resp, nil
}

// agentInfo describes an agent as the bus's list serves it: its resolved
// status, and what export shows for a local agent.
func (s *AgentService) agentInfo(ctx context.Context, a *storage.Agent) natsbus.AgentInfo {
	status, reason, detail := AgentStatusUnknown, backend.EndReason(""), ""
	if info, err := s.StatusInfo(ctx, a.Name); err == nil {
		status, reason, detail = info.Status, info.EndReason, info.Detail
	}
	sessionID := s.LastSessionID(ctx, a.Name)
	in := natsbus.AgentInfo{
		Name:      a.Name,
		Session:   a.SessionName,
		Backend:   a.Backend,
		SpawnedBy: a.SpawnedBy,
		Adopted:   a.Adopted,
		CreatedAt: a.CreatedAt,
		Status:    string(status),
		EndReason: string(reason),
		Detail:    detail,
		SessionID: sessionID,
		Cwd:       a.Cwd,
	}
	if exists, err := s.tmux.SessionExists(a.SessionName); err == nil {
		in.Attached = &exists
	}
	resumable := types.Resumable(a.Backend, a.Adopted, sessionID, status == AgentStatusEnded)
	in.Resumable = &resumable
	return in
}

func (h rpcHandler) Status(ctx context.Context, req natsbus.StatusRequest) (*natsbus.StatusResponse, error) {
	info, err := h.s.StatusInfo(ctx, req.Agent)
	if err != nil {
		return nil, remoteErr(err)
	}
	return &natsbus.StatusResponse{
		Status:    string(info.Status),
		EndReason: string(info.EndReason),
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
		HeldUntil: res.HeldUntil,
		Typed:     res.Typed,

		InputBlocked: res.InputBlocked,
		Unchecked:    res.Unchecked,
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

func (h rpcHandler) Friction(ctx context.Context, req natsbus.FrictionRequest) (*natsbus.FrictionResponse, error) {
	return h.s.ServeFriction(ctx, req)
}

// Spawn starts an agent for another host's agent spawn name@host. The
// request has no --skip-permissions, --trust-workspace or --resume: a peer
// starts an agent on the permission settings of the directory it names, on
// a backend this host knows (built in, or the bus's own --backend-config).
// Paths are this host's; with no Cwd the agent
// starts in the home directory of the user the bus runs as, since the
// bus's own working directory is wherever it happened to be started.
func (h rpcHandler) Spawn(ctx context.Context, req natsbus.SpawnRequest) (*natsbus.SpawnResponse, error) {
	if ref, err := natsbus.ParseAgentRef(req.SpawnedBy); err == nil && strings.EqualFold(ref.Host, h.s.cfg.Host) {
		return nil, fmt.Errorf("spawned_by %s names an agent on this host (%s), but the spawn came over the bus", req.SpawnedBy, h.s.cfg.Host)
	}
	cwd := req.Cwd
	if cwd == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return nil, fmt.Errorf("no working directory given, and no home directory to start in: %w", err)
		}
		cwd = home
	}
	if err := checkCwd(cwd); err != nil {
		return nil, err
	}
	taskFile := req.TaskFile
	if taskFile != "" && !filepath.IsAbs(taskFile) {
		taskFile = filepath.Join(cwd, taskFile)
	}
	agent, err := h.s.Spawn(ctx, SpawnOptions{
		Name:        req.Agent,
		Backend:     req.Backend,
		TaskFile:    taskFile,
		TaskContent: req.TaskContent,
		SpawnedBy:   req.SpawnedBy,
		Cwd:         cwd,
	})
	if err != nil {
		return nil, err
	}
	resp := &natsbus.SpawnResponse{
		Name:      agent.Name,
		Session:   agent.SessionName,
		Backend:   agent.Backend,
		SpawnedBy: agent.SpawnedBy,
		Cwd:       agent.Cwd,
		TaskFile:  taskFile,
		Status:    string(AgentStatusUnknown),
	}
	if info, err := h.s.StatusInfo(ctx, agent.Name); err == nil {
		resp.Status, resp.Detail, resp.EndReason = string(info.Status), info.Detail, string(info.EndReason)
	}
	return resp, nil
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
			return err
		}
	}
	if key != "" {
		if err := s.tmux.SendSpecialKey(agent.SessionName, key); err != nil {
			return err
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
