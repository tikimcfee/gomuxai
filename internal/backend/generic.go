package backend

import "fmt"

// GenericBackend implements Backend for custom CLI tools.
type GenericBackend struct {
	name              string
	sessionPrefix     string
	launchCommand     string
	initMessageFormat string
	idlePattern       string
	processingPattern string
}

// GenericConfig holds configuration for a generic backend.
type GenericConfig struct {
	Name              string `json:"name"`
	SessionPrefix     string `json:"session_prefix"`
	LaunchCommand     string `json:"launch_command"`
	InitMessageFormat string `json:"init_message_format"` // Use %s for task file
	IdlePattern       string `json:"idle_pattern"`
	ProcessingPattern string `json:"processing_pattern"`
}

// NewGenericBackend creates a new generic backend from config.
func NewGenericBackend(cfg GenericConfig) *GenericBackend {
	prefix := cfg.SessionPrefix
	if prefix == "" {
		prefix = "agent-"
	}

	return &GenericBackend{
		name:              cfg.Name,
		sessionPrefix:     prefix,
		launchCommand:     cfg.LaunchCommand,
		initMessageFormat: cfg.InitMessageFormat,
		idlePattern:       cfg.IdlePattern,
		processingPattern: cfg.ProcessingPattern,
	}
}

func (b *GenericBackend) Name() string {
	return b.name
}

func (b *GenericBackend) SessionPrefix() string {
	return b.sessionPrefix
}

func (b *GenericBackend) LaunchCommand() string {
	return b.launchCommand
}

func (b *GenericBackend) InitMessage(taskFile string) string {
	if taskFile != "" && b.initMessageFormat != "" {
		return fmt.Sprintf(b.initMessageFormat, taskFile)
	}
	return ""
}

func (b *GenericBackend) IdlePattern() string {
	return b.idlePattern
}

func (b *GenericBackend) ProcessingPattern() string {
	return b.processingPattern
}
