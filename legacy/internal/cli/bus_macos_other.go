//go:build !darwin

package cli

import (
	"context"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// The bus only needs a disclaimed process on macOS; see bus_macos.go.

func runBusDisclaimed() (bool, error)                               { return false, nil }
func watchBusParent(ctx context.Context, cancel context.CancelFunc) {}
func warnLocalNetwork(peers []natsbus.PeerBundle)                   {}
