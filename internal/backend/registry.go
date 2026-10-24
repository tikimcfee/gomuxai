package backend

import (
	"encoding/json"
	"fmt"
	"os"
	"sync"

	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// ProviderConfig holds configuration options passed when creating providers.
// Backend-specific options are checked via type assertion or fields.
type ProviderConfig struct {
	// SkipPermissions is used by Claude and Agy backends to pass --dangerously-skip-permissions
	SkipPermissions bool

	// AgentDir is the per-agent bundle directory under .gomuxai/agents/<name>
	AgentDir string
}

// ProviderFactory creates a new Provider instance with the given config.
type ProviderFactory func(cfg ProviderConfig) Provider

// Registry manages available backends.
type Registry struct {
	mu        sync.RWMutex
	backends  map[string]Backend
	providers map[string]Provider
	factories map[string]ProviderFactory
}

// NewRegistry creates a new backend registry with built-in backends.
func NewRegistry() *Registry {
	r := &Registry{
		backends:  make(map[string]Backend),
		providers: make(map[string]Provider),
		factories: make(map[string]ProviderFactory),
	}

	// Register built-in backends from namedagents
	claude := namedagents.NewClaudeBackend()
	aider := namedagents.NewAiderBackend()
	agy := namedagents.NewAgyBackend()

	r.Register(claude)
	r.Register(aider)
	r.Register(agy)

	// Register as providers (default instances)
	r.RegisterProvider(claude)
	r.RegisterProvider(aider)
	r.RegisterProvider(agy)

	// Register factories for creating configured instances
	r.RegisterFactory("claude", func(cfg ProviderConfig) Provider {
		return namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{
			SkipPermissions: cfg.SkipPermissions,
		})
	})
	r.RegisterFactory("aider", func(cfg ProviderConfig) Provider {
		// Aider doesn't have special config yet, return default
		return namedagents.NewAiderBackend()
	})
	r.RegisterFactory("agy", func(cfg ProviderConfig) Provider {
		return namedagents.NewAgyBackendWithConfig(namedagents.AgyConfig{
			SkipPermissions: cfg.SkipPermissions,
			AgentDir:        cfg.AgentDir,
		})
	})

	return r
}

// Register adds a backend to the registry.
func (r *Registry) Register(b Backend) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.backends[b.Name()] = b
}

// Get retrieves a backend by name.
func (r *Registry) Get(name string) (Backend, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	b, ok := r.backends[name]
	if !ok {
		return nil, fmt.Errorf("backend %q not found", name)
	}
	return b, nil
}

// List returns all registered backend names.
func (r *Registry) List() []string {
	r.mu.RLock()
	defer r.mu.RUnlock()

	names := make([]string, 0, len(r.backends))
	for name := range r.backends {
		names = append(names, name)
	}
	return names
}

// RegisterProvider adds a provider to the registry.
func (r *Registry) RegisterProvider(p Provider) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.providers[p.Name()] = p
}

// GetProvider retrieves a provider by name.
func (r *Registry) GetProvider(name string) (Provider, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	p, ok := r.providers[name]
	if !ok {
		return nil, fmt.Errorf("provider %q not found", name)
	}
	return p, nil
}

// ListProviders returns all registered provider names.
func (r *Registry) ListProviders() []string {
	r.mu.RLock()
	defer r.mu.RUnlock()

	names := make([]string, 0, len(r.providers))
	for name := range r.providers {
		names = append(names, name)
	}
	return names
}

// RegisterFactory registers a factory function for creating providers.
func (r *Registry) RegisterFactory(name string, factory ProviderFactory) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.factories[name] = factory
}

// CreateProvider creates a new provider instance with the given config.
// This returns a fresh instance configured with the provided options.
func (r *Registry) CreateProvider(name string, cfg ProviderConfig) (Provider, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	factory, ok := r.factories[name]
	if !ok {
		return nil, fmt.Errorf("no factory registered for provider %q", name)
	}
	return factory(cfg), nil
}

// LoadFromFile loads a backend configuration from a JSON file.
func (r *Registry) LoadFromFile(path string) error {
	data, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("reading backend config: %w", err)
	}

	var cfg GenericConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return fmt.Errorf("parsing backend config: %w", err)
	}

	if cfg.Name == "" {
		return fmt.Errorf("backend config must have a name")
	}

	if cfg.LaunchCommand == "" {
		return fmt.Errorf("backend config must have a launch_command")
	}

	r.Register(NewGenericBackend(cfg))
	return nil
}
