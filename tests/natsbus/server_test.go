package natsbus_test

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// setupTestBus creates an embedded NATS server on a random port with
// a temp directory for JetStream. Returns the bus and a cleanup function.
func setupTestBus(t *testing.T) (*natsbus.Bus, string, func()) {
	t.Helper()
	tmpDir, err := os.MkdirTemp("", "natsbus-test-*")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	bus, err := natsbus.NewBus(natsbus.BusConfig{
		StoreDir: tmpDir,
		Port:     0, // random port
	})
	if err != nil {
		os.RemoveAll(tmpDir)
		t.Fatalf("creating bus: %v", err)
	}
	cleanup := func() {
		bus.Close()
		os.RemoveAll(tmpDir)
	}
	return bus, tmpDir, cleanup
}

// =============================================================================
// Bus Lifecycle Tests
// =============================================================================

func TestBus_StartAndConnect(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	// ClientURL should return a valid nats:// URL
	url := bus.ClientURL()
	if url == "" {
		t.Fatal("ClientURL() returned empty string")
	}
	if !strings.HasPrefix(url, "nats://") {
		t.Errorf("ClientURL() = %q, want nats:// prefix", url)
	}

	// Connection should be valid
	conn := bus.Conn()
	if conn == nil {
		t.Fatal("Conn() returned nil")
	}
	if !conn.IsConnected() {
		t.Error("connection is not connected")
	}
}

func TestBus_PublishSubscribe_Core(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	// Subscribe
	received := make(chan []byte, 1)
	sub, err := bus.Conn().Subscribe("test.subject", func(msg *nats.Msg) {
		received <- msg.Data
	})
	if err != nil {
		t.Fatalf("subscribing: %v", err)
	}
	defer sub.Unsubscribe()

	// Publish
	payload := []byte("hello nats")
	if err := bus.Conn().Publish("test.subject", payload); err != nil {
		t.Fatalf("publishing: %v", err)
	}
	bus.Conn().Flush()

	// Wait for delivery
	select {
	case data := <-received:
		if string(data) != "hello nats" {
			t.Errorf("received %q, want %q", string(data), "hello nats")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for message")
	}
}

func TestBus_JetStreamAvailable(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)
	defer cleanup()

	js := bus.JS()
	if js == nil {
		t.Fatal("JS() returned nil")
	}
}

func TestBus_Close_Graceful(t *testing.T) {
	bus, _, cleanup := setupTestBus(t)

	// Close should not panic
	bus.Close()

	// Connection should no longer be connected
	if bus.Conn().IsConnected() {
		t.Error("connection still connected after Close()")
	}

	// Publishing after close should fail (not panic)
	err := bus.Conn().Publish("test.subject", []byte("after close"))
	if err == nil {
		t.Error("expected error publishing after close, got nil")
	}

	// Clean up the temp dir (bus.Close already called, just remove dir)
	cleanup()
}

func TestBus_URLFileWritten(t *testing.T) {
	bus, tmpDir, cleanup := setupTestBus(t)
	defer cleanup()

	urlFile := filepath.Join(tmpDir, "nats.url")
	data, err := os.ReadFile(urlFile)
	if err != nil {
		t.Fatalf("reading nats.url: %v", err)
	}

	url := strings.TrimSpace(string(data))
	if url != bus.ClientURL() {
		t.Errorf("nats.url contains %q, want %q", url, bus.ClientURL())
	}
}

func TestBus_ConnectToExisting(t *testing.T) {
	// Start first bus (becomes the server)
	bus1, tmpDir, cleanup1 := setupTestBus(t)
	defer cleanup1()

	// Start second bus with the same StoreDir -- should connect as client
	bus2, err := natsbus.NewBus(natsbus.BusConfig{
		StoreDir: tmpDir,
		Port:     0,
	})
	if err != nil {
		t.Fatalf("creating bus2: %v", err)
	}
	defer bus2.Close()

	// bus2 should NOT be hosting a server (it connected to bus1's)
	if bus2.IsServer() {
		t.Error("bus2 should be a client, not a server")
	}

	// Cross-bus pub/sub should work
	received := make(chan []byte, 1)
	sub, err := bus2.Conn().Subscribe("cross.bus", func(msg *nats.Msg) {
		received <- msg.Data
	})
	if err != nil {
		t.Fatalf("subscribing on bus2: %v", err)
	}
	defer sub.Unsubscribe()

	// Ensure subscription is propagated before publishing
	bus2.Conn().Flush()

	if err := bus1.Conn().Publish("cross.bus", []byte("from bus1")); err != nil {
		t.Fatalf("publishing on bus1: %v", err)
	}
	bus1.Conn().Flush()

	select {
	case data := <-received:
		if string(data) != "from bus1" {
			t.Errorf("received %q, want %q", string(data), "from bus1")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("timed out waiting for cross-bus message")
	}
}

func TestBus_StaleURLFile_StartsNewServer(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "natsbus-stale-test-*")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	// Write a stale URL file pointing to a port nobody is listening on
	urlFile := filepath.Join(tmpDir, "nats.url")
	if err := os.WriteFile(urlFile, []byte("nats://127.0.0.1:19999"), 0644); err != nil {
		t.Fatalf("writing stale URL: %v", err)
	}

	// NewBus should detect the stale URL, remove it, and start its own server
	bus, err := natsbus.NewBus(natsbus.BusConfig{
		StoreDir: tmpDir,
		Port:     0,
	})
	if err != nil {
		t.Fatalf("creating bus with stale URL: %v", err)
	}
	defer bus.Close()

	if !bus.IsServer() {
		t.Error("bus should have started its own server after stale URL")
	}
	if !bus.Conn().IsConnected() {
		t.Error("bus should be connected")
	}
}

func TestBus_URLFileCleanedOnClose(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "natsbus-cleanup-test-*")
	if err != nil {
		t.Fatalf("creating temp dir: %v", err)
	}
	defer os.RemoveAll(tmpDir)

	bus, err := natsbus.NewBus(natsbus.BusConfig{
		StoreDir: tmpDir,
		Port:     0,
	})
	if err != nil {
		t.Fatalf("creating bus: %v", err)
	}

	urlFile := filepath.Join(tmpDir, "nats.url")

	// URL file should exist while server is running
	if _, err := os.Stat(urlFile); os.IsNotExist(err) {
		t.Fatal("nats.url should exist while server is running")
	}

	bus.Close()

	// URL file should be cleaned up when the server owner closes
	if _, err := os.Stat(urlFile); !os.IsNotExist(err) {
		t.Error("nats.url should be removed after server Close()")
	}
}
