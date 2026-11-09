package service

import (
	"fmt"
	"regexp"
)

// modelPattern is what spawn --model takes: a model name or alias as CLIs
// spell them ("opus", "claude-fable-5-1", "claude-opus-5-5[1m]",
// "gemini-3.8-flash-high", "openrouter/x-ai/grok"). It is shell-quoted on
// the launch line anyway; the pattern keeps a name from another host to
// something that reads as a model.
var modelPattern = regexp.MustCompile(`^[A-Za-z0-9][A-Za-z0-9._:/@\[\]+-]{0,127}$`)

// ValidateModel says why model can't be passed to a CLI's --model, or nil.
func ValidateModel(model string) error {
	if !modelPattern.MatchString(model) {
		return fmt.Errorf("invalid model %q: a model name is letters, digits and . _ : / @ [ ] + -, starting with a letter or digit", model)
	}
	return nil
}

// modelRefusal is the error for spawn --model on a backend that takes none.
func modelRefusal(backendName string) error {
	return fmt.Errorf("backend %s takes no --model: claude, agy and aider do (for another, put the model in its launch command)", backendName)
}
