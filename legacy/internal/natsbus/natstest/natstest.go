// Package natstest starts throwaway in-process NATS servers for tests.
package natstest

import (
	"testing"
	"time"

	natsserver "github.com/nats-io/nats-server/v2/server"
	"github.com/nats-io/nats.go"
)

// Start runs a NATS server on a random localhost port for the duration of
// the test and returns a client connection to it. Additional clients can
// connect to the returned connection's ConnectedUrl().
func Start(t testing.TB) *nats.Conn {
	t.Helper()
	ns, err := natsserver.NewServer(&natsserver.Options{
		Host:   "127.0.0.1",
		Port:   -1, // random
		NoLog:  true,
		NoSigs: true,
	})
	if err != nil {
		t.Fatalf("creating NATS server: %v", err)
	}
	ns.Start()
	if !ns.ReadyForConnections(5 * time.Second) {
		ns.Shutdown()
		t.Fatal("NATS server not ready")
	}
	t.Cleanup(ns.Shutdown)

	return Connect(t, ns.ClientURL())
}

// Connect opens another client to url, closed when the test ends.
func Connect(t testing.TB, url string) *nats.Conn {
	t.Helper()
	nc, err := nats.Connect(url)
	if err != nil {
		t.Fatalf("connecting to %s: %v", url, err)
	}
	t.Cleanup(nc.Close)
	return nc
}
