package cli

import (
	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// busConnect opens a client connection to this host's agent bus. It returns
// natsbus.ErrNoBus when no bus is running, so callers can fall back to
// local-only behavior. It must fail fast in that case: hooks call the CLI on
// every agent event. Replaced by the bus daemon's connector (track A); tests
// may swap it for a natstest connection.
var busConnect natsbus.LocalConnector = func() (*nats.Conn, error) {
	return nil, natsbus.ErrNoBus
}
