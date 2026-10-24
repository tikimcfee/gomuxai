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

	_ backend.HookProvider        = (*namedagents.AgyBackend)(nil)
	_ backend.PermissionResponder = (*namedagents.AgyBackend)(nil)
	_ backend.PromptFlagProvider  = (*namedagents.AgyBackend)(nil)
)
