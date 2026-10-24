package cli

import (
	"errors"
	"fmt"
	"strings"

	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// parseTarget reads an agent argument that may name another host
// ("name@host"). remote is false for an argument without "@" (returned as is,
// so local behavior is untouched) and for "name@<this host>" (returned as the
// bare name).
func parseTarget(arg string) (ref natsbus.AgentRef, remote bool, err error) {
	if !strings.Contains(arg, "@") {
		return natsbus.AgentRef{Name: arg}, false, nil
	}
	ref, err = natsbus.ParseAgentRef(arg)
	if err != nil {
		return natsbus.AgentRef{}, false, err
	}
	if ref.Host == cfg.Host {
		return natsbus.AgentRef{Name: ref.Name}, false, nil
	}
	return ref, true, nil
}

// isRemoteHost reports whether host names another host.
func isRemoteHost(host string) bool {
	return host != "" && host != cfg.Host
}

// dialRemote connects to this host's bus for a request to another host.
// The caller must call closeFn when done.
func dialRemote(host string) (client *natsbus.RPCClient, closeFn func(), err error) {
	nc, err := busConnect()
	if errors.Is(err, natsbus.ErrNoBus) {
		return nil, nil, fmt.Errorf("%q is not this host (%q); reaching agents on other hosts needs a running agent bus: %w", host, cfg.Host, err)
	}
	if err != nil {
		return nil, nil, fmt.Errorf("connecting to the agent bus: %w", err)
	}
	return natsbus.NewRPCClient(nc, 0), nc.Close, nil
}
