// Package natsbus provides an embedded NATS server and client for agent coordination.
package natsbus

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"time"

	natsserver "github.com/nats-io/nats-server/v2/server"
	"github.com/nats-io/nats.go"
	"github.com/nats-io/nats.go/jetstream"
)

// Bus is the embedded NATS server + client.
// If this process started the server, server will be non-nil.
// If it connected to an existing server, server will be nil.
type Bus struct {
	server   *natsserver.Server
	conn     *nats.Conn
	js       jetstream.JetStream
	storeDir string
}

// BusConfig configures the embedded NATS server.
type BusConfig struct {
	StoreDir string // e.g., ".gomuxai/nats" -- JetStream file store
	Port     int    // 0 = random available port
}

// NewBus connects to an existing NATS server (via .gomuxai/nats.url) or starts
// an embedded one. The first process to start becomes the server; subsequent
// processes connect as clients.
func NewBus(cfg BusConfig) (*Bus, error) {
	if cfg.StoreDir == "" {
		return nil, fmt.Errorf("StoreDir is required")
	}

	// Ensure store directory exists
	if err := os.MkdirAll(cfg.StoreDir, 0755); err != nil {
		return nil, fmt.Errorf("creating store dir: %w", err)
	}

	urlFile := filepath.Join(cfg.StoreDir, "nats.url")

	// Try to connect to an existing server
	if data, err := os.ReadFile(urlFile); err == nil {
		url := string(bytes.TrimSpace(data))
		if url != "" {
			conn, err := nats.Connect(url, nats.Timeout(500*time.Millisecond))
			if err == nil {
				js, err := jetstream.New(conn)
				if err != nil {
					conn.Close()
					// Fall through to start our own server
				} else {
					return &Bus{conn: conn, js: js, storeDir: cfg.StoreDir}, nil
				}
			}
			// Connection failed -- stale URL file, remove it
			os.Remove(urlFile)
		}
	}

	// Start embedded server. If it fails (e.g., another process raced us and
	// started a server first), retry connecting to the URL file.
	bus, err := startEmbeddedServer(cfg, urlFile)
	if err != nil {
		// Another process may have won the race -- retry connecting
		time.Sleep(100 * time.Millisecond)
		if data, readErr := os.ReadFile(urlFile); readErr == nil {
			url := string(bytes.TrimSpace(data))
			if url != "" {
				conn, connErr := nats.Connect(url, nats.Timeout(2*time.Second))
				if connErr == nil {
					js, jsErr := jetstream.New(conn)
					if jsErr == nil {
						return &Bus{conn: conn, js: js, storeDir: cfg.StoreDir}, nil
					}
					conn.Close()
				}
			}
		}
		return nil, err
	}
	return bus, nil
}

// startEmbeddedServer starts a NATS server and connects to it.
func startEmbeddedServer(cfg BusConfig, urlFile string) (*Bus, error) {
	opts := &natsserver.Options{
		Host:           "127.0.0.1",
		Port:           cfg.Port,
		StoreDir:       filepath.Join(cfg.StoreDir, "jetstream"),
		JetStream:      true,
		NoLog:          true,
		NoSigs:         true,
		MaxControlLine: 4096,
	}

	ns, err := natsserver.NewServer(opts)
	if err != nil {
		return nil, fmt.Errorf("creating NATS server: %w", err)
	}

	ns.Start()
	if !ns.ReadyForConnections(5 * time.Second) {
		ns.Shutdown()
		return nil, fmt.Errorf("NATS server failed to start within 5s")
	}

	clientURL := ns.ClientURL()

	// Write URL file so other processes can connect
	if err := os.WriteFile(urlFile, []byte(clientURL), 0644); err != nil {
		ns.Shutdown()
		return nil, fmt.Errorf("writing URL file: %w", err)
	}

	// Connect our own client
	conn, err := nats.Connect(clientURL)
	if err != nil {
		os.Remove(urlFile)
		ns.Shutdown()
		return nil, fmt.Errorf("connecting to embedded server: %w", err)
	}

	js, err := jetstream.New(conn)
	if err != nil {
		conn.Close()
		os.Remove(urlFile)
		ns.Shutdown()
		return nil, fmt.Errorf("creating JetStream context: %w", err)
	}

	return &Bus{
		server:   ns,
		conn:     conn,
		js:       js,
		storeDir: cfg.StoreDir,
	}, nil
}

// Close shuts down the connection and, if this process owns the server, the server.
func (b *Bus) Close() {
	if b.conn != nil {
		b.conn.Close()
	}
	if b.server != nil {
		b.server.Shutdown()
		b.server.WaitForShutdown()
		// Clean up URL file since we own the server
		urlFile := filepath.Join(b.storeDir, "nats.url")
		os.Remove(urlFile)
	}
}

// Conn returns the NATS client connection.
func (b *Bus) Conn() *nats.Conn {
	return b.conn
}

// JS returns the JetStream context.
func (b *Bus) JS() jetstream.JetStream {
	return b.js
}

// ClientURL returns the NATS client connection URL.
func (b *Bus) ClientURL() string {
	if b.server != nil {
		return b.server.ClientURL()
	}
	if b.conn != nil {
		return b.conn.ConnectedUrl()
	}
	return ""
}

// IsServer returns true if this Bus instance is hosting the embedded NATS server.
func (b *Bus) IsServer() bool {
	return b.server != nil
}
