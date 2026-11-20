package cli

import (
	"errors"
	"fmt"
	"slices"
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
	ref.Host = canonicalHost(ref.Host)
	if ref.Host == cfg.Host {
		return natsbus.AgentRef{Name: ref.Name}, false, nil
	}
	return ref, true, nil
}

// isRemoteHost reports whether host names another host.
func isRemoteHost(host string) bool {
	return host != "" && canonicalHost(host) != cfg.Host
}

// canonicalHost returns host as the bus spells it: this host's name, or a
// trusted peer's, when host matches one ignoring case, or a name either had
// before it was renamed; any other host as given. Host names are matched case-insensitively ("il-air-m2" is
// "IL-Air-M2"), but bus subjects are case-sensitive, so a request must go
// out under the host's own spelling. A host this one doesn't trust directly
// (another host linked through the hub) is only known by the spelling it is
// given.
func canonicalHost(host string) string {
	if host == "" || host == cfg.Host {
		return host
	}
	same := func(name string) bool { return strings.EqualFold(host, name) }
	if same(cfg.Host) || slices.ContainsFunc(natsbus.PreviousHosts(cfg.BusDir()), same) {
		return cfg.Host // or a name it had before agent bus rename
	}
	peers, _ := natsbus.LoadPeers(cfg.BusDir())
	for _, p := range peers {
		if same(p.Host) {
			return p.Host
		}
	}
	for _, p := range peers {
		if p.Names(host) { // a name it had before (trust --renames)
			return p.Host
		}
	}
	return host
}

// trustedHosts returns the hosts this host's bus trusts (agent bus trust),
// or none if it can't read them.
func trustedHosts() []string {
	peers, err := natsbus.LoadPeers(cfg.BusDir())
	if err != nil {
		return nil
	}
	hosts := make([]string, 0, len(peers))
	for _, p := range peers {
		hosts = append(hosts, p.Host)
	}
	return hosts
}

// knownHosts says which host names this host knows, for an error about a
// host no bus answered for.
func knownHosts() string {
	s := fmt.Sprintf("this host is %s", cfg.Host)
	if peers := trustedHosts(); len(peers) > 0 {
		slices.Sort(peers)
		s += fmt.Sprintf(", and its bus trusts %s", strings.Join(peers, ", "))
	}
	return s
}

// callRemote runs fn with an RPC client connected through this host's bus,
// for a request to an agent on another host.
func callRemote(host string, fn func(*natsbus.RPCClient) error) error {
	nc, err := busConnect()
	if errors.Is(err, natsbus.ErrNoBus) {
		return fmt.Errorf("%q is not this host (%q); reaching agents on other hosts needs a running agent bus: %w", host, cfg.Host, err)
	}
	if err != nil {
		return fmt.Errorf("connecting to the agent bus: %w", err)
	}
	defer nc.Close()
	err = fn(natsbus.NewRPCClient(nc, 0))
	if noBus := (*natsbus.NoRespondersError)(nil); errors.As(err, &noBus) {
		return fmt.Errorf("%w; %s", err, knownHosts())
	}
	return err
}
