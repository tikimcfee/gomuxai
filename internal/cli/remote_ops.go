package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"unicode/utf8"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
)

// busHandler is the RPC handler this binary's bus serves: the service's
// ops, plus logs and resume, whose logic lives here in the CLI.
func busHandler(svc *service.AgentService) natsbus.RPCHandler {
	return service.NewRPCHandlerWith(svc, service.RPCExtras{Logs: serveLogs, Resume: serveResume})
}

// serveLogs answers another host's agent logs name@host: what agent logs
// prints here, cut to fit a bus message.
func serveLogs(_ context.Context, req natsbus.LogsRequest) (*natsbus.LogsResponse, error) {
	opts := logsOptions{Lines: req.Lines, Full: req.Full, JSON: req.JSON, Terminal: req.Terminal}
	if err := opts.check(); err != nil {
		return nil, err
	}
	chunks, why, err := buildLogs(req.Agent, opts)
	if err != nil {
		return nil, err
	}
	resp := &natsbus.LogsResponse{Hint: why}
	resp.Output, resp.Truncated = fitLogs(chunks, natsbus.MaxLogsReply-jsonSize(why)-128, !opts.JSON)
	return resp, nil
}

// fitLogs joins chunks into at most budget bytes once JSON-encoded,
// keeping the newest: it leaves out whole chunks from the start, and puts
// back the session line or separator the first one kept belongs to. With
// cutText, a single chunk too big on its own (an event printed --full)
// keeps its end; otherwise it is left out, since half a JSON line is no
// line. It returns the output and how many bytes were left out.
func fitLogs(chunks []logsChunk, budget int, cutText bool) (string, int) {
	sizes := make([]int, len(chunks))
	total := 0
	for i, c := range chunks {
		sizes[i] = jsonSize(c.text)
		total += sizes[i]
	}
	if total <= budget {
		return joinChunks(chunks), 0
	}
	// header is the session chunk chunks[first] belongs to, if it isn't
	// one itself and was left out; -1 for none.
	header := func(first int) int {
		if first >= len(chunks) || chunks[first].session {
			return -1
		}
		for i := first - 1; i >= 0; i-- {
			if chunks[i].session {
				return i
			}
		}
		return -1
	}
	first, kept := len(chunks), 0
	for first > 0 && kept+sizes[first-1] <= budget {
		first--
		kept += sizes[first]
	}
	for first < len(chunks) {
		h := header(first)
		if h < 0 || kept+sizes[h] <= budget {
			break
		}
		kept -= sizes[first]
		first++
	}

	var b strings.Builder
	if h := header(first); h >= 0 {
		b.WriteString(chunks[h].text)
	}
	if first == len(chunks) && cutText && len(chunks) > 0 {
		// The newest chunk alone is too big: keep its end. JSON escaping
		// grows text at most sixfold.
		last := chunks[len(chunks)-1].text
		tail := last[len(last)-min(len(last), budget/6):]
		for len(tail) > 0 && !utf8.RuneStart(tail[0]) {
			tail = tail[1:]
		}
		b.WriteString(tail)
	}
	b.WriteString(joinChunks(chunks[first:]))
	out := b.String()
	return out, max(rawSize(chunks)-len(out), 0)
}

func joinChunks(chunks []logsChunk) string {
	var b strings.Builder
	for _, c := range chunks {
		b.WriteString(c.text)
	}
	return b.String()
}

func rawSize(chunks []logsChunk) int {
	n := 0
	for _, c := range chunks {
		n += len(c.text)
	}
	return n
}

// jsonSize is how many bytes s takes in a JSON string, quotes left out.
func jsonSize(s string) int {
	b, _ := json.Marshal(s)
	return len(b) - 2
}

// runRemoteLogs prints what agent logs prints on ref.Host for ref.Name.
func runRemoteLogs(ref natsbus.AgentRef, opts logsOptions) error {
	req := natsbus.LogsRequest{Lines: opts.Lines, Full: opts.Full, JSON: opts.JSON, Terminal: opts.Terminal}
	var resp *natsbus.LogsResponse
	err := callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
		resp, err = c.Logs(getContext(), ref, req)
		return err
	})
	if err != nil {
		if noBus := (*natsbus.NoRespondersError)(nil); errors.As(err, &noBus) {
			hint("%s serves logs from an agent built with it; an older one doesn't (agent capture %s shows its screen)", ref.Host, ref)
		}
		return err
	}
	if resp.Truncated > 0 {
		notice("%s's logs from %s were cut to fit a bus message: the oldest %s left out (--lines asks for fewer).", ref, ref.Host, byteSize(int64(resp.Truncated)))
	}
	if resp.Hint != "" {
		hint("%s", resp.Hint)
	}
	fmt.Print(resp.Output)
	return nil
}

// serveResume answers another host's agent resume name@host: this host's
// own resume, with its own refusals.
func serveResume(ctx context.Context, req natsbus.ResumeRequest) (*natsbus.ResumeResponse, error) {
	if ref, err := natsbus.ParseAgentRef(req.ResumedBy); err == nil && strings.EqualFold(ref.Host, cfg.Host) {
		return nil, fmt.Errorf("resumed_by %s names an agent on this host (%s), but the resume came over the bus", req.ResumedBy, cfg.Host)
	}
	return resumeAgent(ctx, req.Agent, req.ResumedBy, false, false)
}

// runRemoteResume resumes ref.Name on ref.Host through the agent bus.
func runRemoteResume(ref natsbus.AgentRef) error {
	if resumeAttach {
		return fmt.Errorf("--attach for an agent on another host isn't supported yet: agent resume %s, then attach to it on %s", ref, ref.Host)
	}
	for _, f := range []struct {
		set  bool
		flag string
	}{
		{resumeSkipPermissions, "--skip-permissions"},
		{resumeTrustWorkspace, "--trust-workspace"},
	} {
		if f.set {
			return fmt.Errorf("%s works only for an agent on this host: to resume %s with it, run agent resume on %s", f.flag, ref.Name, ref.Host)
		}
	}
	var req natsbus.ResumeRequest
	if me := nonInteractiveCallerAgent(); me != "" {
		req.ResumedBy = natsbus.AgentRef{Name: me, Host: cfg.Host}.String()
	}
	var resp *natsbus.ResumeResponse
	err := callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
		resp, err = c.Resume(getContext(), ref, req)
		return err
	})
	var re *natsbus.RemoteError
	switch {
	case errors.As(err, &re):
		return fmt.Errorf("%s couldn't resume %s: %w", ref.Host, ref.Name, err)
	case err != nil:
		if noBus := (*natsbus.NoRespondersError)(nil); errors.As(err, &noBus) {
			hint("%s serves resume to other hosts only if its bus runs with --remote-ops listing resume (agent bus status there shows what it allows)", ref.Host)
		}
		return err
	}
	printResumed(ref.String(), resp)
	return nil
}
