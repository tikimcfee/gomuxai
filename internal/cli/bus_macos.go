//go:build darwin

package cli

import (
	"context"
	"errors"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/tikimcfee/gomuxai/internal/macos"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// disclaimedEnv marks the bus process that runBusDisclaimed spawned, so it
// doesn't spawn again.
const disclaimedEnv = "GOMUX_BUS_DISCLAIMED"

// runBusDisclaimed runs the bus as its own responsible process when this is
// a macOS build that can hold the Local Network permission (make macos), so
// links to peers on the LAN work from inside tmux (see package macos). It
// returns false when the bus should run in this process instead.
//
// This process stays as the child's parent: it waits for it, exits with its
// status, and forwards SIGTERM and SIGHUP. It ignores SIGINT, which a
// terminal's Ctrl-C (and `agent bus stop`) delivers to the child directly,
// since they share a process group; forwarding it too would land a second
// interrupt mid-shutdown.
func runBusDisclaimed() (bool, error) {
	if !macos.Supported || os.Getenv(disclaimedEnv) != "" {
		return false, nil
	}
	exe, err := os.Executable()
	if err != nil || !macos.HasInfoPlist(exe) {
		return false, nil
	}

	signal.Ignore(os.Interrupt)
	sigs := make(chan os.Signal, 1)
	signal.Notify(sigs, syscall.SIGTERM, syscall.SIGHUP)
	defer signal.Stop(sigs)

	pid, err := macos.SpawnDisclaimed(exe, os.Args, append(os.Environ(), disclaimedEnv+"=1"))
	if err != nil {
		return true, err
	}
	go func() {
		for sig := range sigs {
			syscall.Kill(pid, sig.(syscall.Signal))
		}
	}()

	var ws syscall.WaitStatus
	for {
		_, err = syscall.Wait4(pid, &ws, 0, nil)
		if !errors.Is(err, syscall.EINTR) {
			break
		}
	}
	switch {
	case err != nil:
		return true, fmt.Errorf("waiting for the bus: %w", err)
	case ws.Signaled():
		return true, fmt.Errorf("bus killed by %v", ws.Signal())
	case ws.ExitStatus() != 0:
		return true, fmt.Errorf("bus exited with status %d", ws.ExitStatus())
	}
	return true, nil
}

// watchBusParent cancels ctx if the process that spawned this disclaimed bus
// goes away (e.g. killed with SIGKILL), so the bus doesn't outlive its tmux
// session holding the ports.
func watchBusParent(ctx context.Context, cancel context.CancelFunc) {
	if os.Getenv(disclaimedEnv) == "" {
		return
	}
	parent := os.Getppid()
	go func() {
		t := time.NewTicker(time.Second)
		defer t.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-t.C:
				if os.Getppid() != parent {
					fmt.Println("agent bus: parent process exited")
					cancel()
					return
				}
			}
		}
	}()
}

// warnLocalNetwork points at the fix when this bus will dial a peer over
// the LAN from inside tmux on macOS with a binary that can't hold the Local
// Network permission: the link would otherwise fail with only "no route to
// host" in the log.
func warnLocalNetwork(peers []natsbus.PeerBundle) {
	if os.Getenv("TMUX") == "" || os.Getenv(disclaimedEnv) != "" || busListen != "" {
		return
	}
	for _, p := range peers {
		if p.URL != "" {
			fmt.Fprintln(os.Stderr, "note: macOS blocks LAN links from processes in tmux unless the agent binary "+
				"carries its own Local Network permission. If linking fails with \"no route to host\", "+
				"build with `make macos` on this Mac (see CLAUDE.md).")
			return
		}
	}
}
