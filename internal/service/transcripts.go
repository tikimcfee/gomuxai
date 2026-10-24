package service

import (
	"context"
	"fmt"
	"os"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/transcript"
)

// TranscriptRef is a CLI transcript an agent's session was recorded in.
type TranscriptRef struct {
	Backend string // the agent's backend, which knows the format
	Path    string
}

// recordTranscript adds path to the agent's list of transcripts, unless it
// is already the last one. The list lives beside the agent's log, one
// "<backend>\t<path>" line per transcript, so like the log it outlives
// cleanup and spans respawns under the same name; _transcript only knows
// the current one.
func (s *AgentService) recordTranscript(name, backendName, path string) error {
	listPath := s.cfg.TranscriptsPath(name)
	if refs := readTranscriptRefs(listPath); len(refs) > 0 && refs[len(refs)-1].Path == path {
		return nil
	}
	f, err := os.OpenFile(listPath, os.O_CREATE|os.O_APPEND|os.O_WRONLY, 0o600)
	if err != nil {
		return fmt.Errorf("recording transcript path: %w", err)
	}
	defer f.Close()
	if _, err := fmt.Fprintf(f, "%s\t%s\n", backendName, path); err != nil {
		return fmt.Errorf("recording transcript path: %w", err)
	}
	return nil
}

// Transcripts returns the transcripts the named agent's sessions were
// recorded in, oldest first. It reads the list beside the agent's log, so
// it works after cleanup; for an agent with no list yet, it falls back to
// the _transcript state key.
func (s *AgentService) Transcripts(ctx context.Context, name string) []TranscriptRef {
	if refs := readTranscriptRefs(s.cfg.TranscriptsPath(name)); len(refs) > 0 {
		return refs
	}
	agent, err := s.storage.GetAgent(ctx, name)
	if err != nil || agent == nil {
		return nil
	}
	if path := s.stateValue(ctx, name, StateKeyTranscript); path != "" {
		return []TranscriptRef{{Backend: agent.Backend, Path: path}}
	}
	return nil
}

// readTranscriptRefs reads a transcript list, dropping malformed lines and
// repeats (two hooks can record the same new path at once).
func readTranscriptRefs(listPath string) []TranscriptRef {
	data, err := os.ReadFile(listPath)
	if err != nil {
		return nil
	}
	var refs []TranscriptRef
	for line := range strings.SplitSeq(string(data), "\n") {
		be, path, ok := strings.Cut(line, "\t")
		if !ok || path == "" || (len(refs) > 0 && refs[len(refs)-1].Path == path) {
			continue
		}
		refs = append(refs, TranscriptRef{Backend: be, Path: path})
	}
	return refs
}

// TranscriptReader returns the reader for the transcripts of backendName's
// CLI, or nil if the backend has none (or isn't built in).
func (s *AgentService) TranscriptReader(backendName string) transcript.Reader {
	provider, err := s.backends.CreateProvider(backendName, backend.ProviderConfig{})
	if err != nil {
		return nil
	}
	if tp, ok := provider.(backend.TranscriptProvider); ok {
		return tp.TranscriptReader()
	}
	return nil
}
