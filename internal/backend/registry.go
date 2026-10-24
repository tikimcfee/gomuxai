package backend

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"sort"
	"strings"
	"sync"

	"github.com/tikimcfee/gomuxai/internal/backend/namedagents"
)

// ProviderConfig holds the per-agent options a Provider is created with.
type ProviderConfig struct {
	// SkipPermissions makes Claude and Agy pass --dangerously-skip-permissions.
	SkipPermissions bool

	// AgentDir is the per-agent bundle directory under .gomuxai/agents/<name>
	AgentDir string
}

// ProviderFactory creates a new Provider instance with the given config.
type ProviderFactory func(cfg ProviderConfig) Provider

// Registry holds the available backends. Built-in ones have a Provider
// factory, so each agent gets an instance configured for it. Every backend,
// custom JSON ones included, is also registered as a Backend: custom ones
// spawn from it, and status scraping reads its patterns.
type Registry struct {
	mu        sync.RWMutex
	backends  map[string]Backend
	factories map[string]ProviderFactory
}

// NewRegistry creates a new backend registry with built-in backends.
func NewRegistry() *Registry {
	r := &Registry{
		backends:  make(map[string]Backend),
		factories: make(map[string]ProviderFactory),
	}

	r.Register(namedagents.NewClaudeBackend())
	r.Register(namedagents.NewAiderBackend())
	r.Register(namedagents.NewAgyBackend())

	r.RegisterFactory("claude", func(cfg ProviderConfig) Provider {
		return namedagents.NewClaudeBackendWithConfig(namedagents.ClaudeConfig{
			SkipPermissions: cfg.SkipPermissions,
		})
	})
	r.RegisterFactory("aider", func(cfg ProviderConfig) Provider {
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

// ErrNotFound is wrapped by Get's error for a name with no backend.
var ErrNotFound = errors.New("not found")

// Get retrieves a backend by name.
func (r *Registry) Get(name string) (Backend, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	b, ok := r.backends[name]
	if !ok {
		names := make([]string, 0, len(r.backends))
		for n := range r.backends {
			names = append(names, n)
		}
		sort.Strings(names)
		return nil, fmt.Errorf("backend %q %w (known: %s; or define one with --backend-config <file>)", name, ErrNotFound, strings.Join(names, ", "))
	}
	return b, nil
}

// RegisterFactory registers a factory function for creating providers.
func (r *Registry) RegisterFactory(name string, factory ProviderFactory) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.factories[name] = factory
}

// CreateProvider returns a fresh instance of the named built-in backend,
// configured with cfg.
func (r *Registry) CreateProvider(name string, cfg ProviderConfig) (Provider, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()

	factory, ok := r.factories[name]
	if !ok {
		return nil, fmt.Errorf("no factory registered for provider %q", name)
	}
	return factory(cfg), nil
}

// LoadFromFile registers the custom backend a JSON file configures, and
// returns it.
func (r *Registry) LoadFromFile(path string) (Backend, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("reading backend config: %w", err)
	}

	var cfg GenericConfig
	if err := json.Unmarshal(data, &cfg); err != nil {
		return nil, fmt.Errorf("parsing backend config: %w", err)
	}

	if cfg.Name == "" {
		return nil, fmt.Errorf("backend config must have a name")
	}

	if cfg.LaunchCommand == "" {
		return nil, fmt.Errorf("backend config must have a launch_command")
	}

	b := NewGenericBackend(cfg)
	r.Register(b)
	return b, nil
}
