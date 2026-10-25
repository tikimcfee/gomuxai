package backend_test

import (
	"github.com/tikimcfee/gomuxai/internal/backend"
	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// The service finds these capabilities by runtime type assertion, so a
// backend missing one method silently loses the whole capability. Pin them.
var (
	_ backend.HookProvider         = (*namedagents.ClaudeBackend)(nil)
	_ backend.NativeStatusProvider = (*namedagents.ClaudeBackend)(nil)
	_ backend.PermissionResponder  = (*namedagents.ClaudeBackend)(nil)
	_ backend.InstructionsProvider = (*namedagents.ClaudeBackend)(nil)
	_ backend.TrustPrompter        = (*namedagents.ClaudeBackend)(nil)
	_ backend.CommandAllower       = (*namedagents.ClaudeBackend)(nil)
	_ backend.HookDeliverer        = (*namedagents.ClaudeBackend)(nil)
	_ backend.InputReader          = (*namedagents.ClaudeBackend)(nil)

	_ backend.HookProvider         = (*namedagents.AgyBackend)(nil)
	_ backend.PermissionResponder  = (*namedagents.AgyBackend)(nil)
	_ backend.PromptFlagProvider   = (*namedagents.AgyBackend)(nil)
	_ backend.InstructionsProvider = (*namedagents.AgyBackend)(nil)
	_ backend.TrustPrompter        = (*namedagents.AgyBackend)(nil)
	_ backend.HookDeliverer        = (*namedagents.AgyBackend)(nil)
	_ backend.InputReader          = (*namedagents.AgyBackend)(nil)
	_ backend.TurnEndReader        = (*namedagents.AgyBackend)(nil)
	_ backend.QuestionPrompter     = (*namedagents.AgyBackend)(nil)
)
