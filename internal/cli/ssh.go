package cli

import (
	"encoding/json"
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"regexp"
	"slices"
	"strings"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// Attaching to another host's agent: the bus can't carry a terminal, so
// agent runs ssh. Where to (a destination per peer, plain config in
// bus/ssh.json, defaulting to the host's name) is recorded here; what to
// run there (that host's agent binary) the host says in its list reply.
// ssh is its own trust path: the person's ssh keys and ~/.ssh/config, not
// the bus's pinned keys.

var busSSHClear bool

var busSSHCmd = &cobra.Command{
	Use:   "ssh <host> [<destination>]",
	Short: "Show or set where agent attach name@host runs ssh to",
	Long: `The bus can't carry a terminal, so "agent attach name@host" (and
"agent resume name@host --attach") runs ssh to that host, and runs the
host's own agent binary there: ssh -t <destination> <its binary> attach
<name>. The host says where its binary is, through the bus.

The destination defaults to the host's name. Set one per host when ssh
knows it by another name or needs a user: "me@hub.lan", or an alias from
~/.ssh/config, which still applies (ports, keys, jump hosts). It is plain
config, not a secret, kept in this host's bus/ssh.json; agent bus status
shows it.

ssh is a separate trust path from the bus: it uses your ssh keys, not the
bus's pinned ones, so attaching to another host needs ssh to work there
first ("ssh <destination>" logs in).

With only a host, it prints the destination. --clear goes back to the
default.`,
	Example: `  agent bus ssh hub
  agent bus ssh hub me@hub.lan
  agent bus ssh hub --clear`,
	Args: cobra.RangeArgs(1, 2),
	RunE: runBusSSH,
}

func init() {
	busSSHCmd.Flags().BoolVar(&busSSHClear, "clear", false, "Go back to the default destination: the host's name")
	busCmd.AddCommand(busSSHCmd)
}

func runBusSSH(cmd *cobra.Command, args []string) error {
	if err := natsbus.ValidateHost(args[0]); err != nil {
		return err
	}
	host := canonicalHost(args[0])
	if host == cfg.Host {
		return fmt.Errorf("%s is this host: agent attach <name> opens its agents without ssh", host)
	}
	dests, err := loadSSHDests()
	if err != nil {
		return err
	}
	switch {
	case busSSHClear && len(args) == 2:
		return fmt.Errorf("--clear goes back to the default: give it no destination")
	case busSSHClear:
		delete(dests, host)
		if err := saveSSHDests(dests); err != nil {
			return err
		}
		fmt.Printf("agent attach name@%s runs ssh to %s (the default: its host name).\n", host, host)
	case len(args) == 2:
		if err := validateSSHDest(args[1]); err != nil {
			return err
		}
		dests[host] = args[1]
		if err := saveSSHDests(dests); err != nil {
			return err
		}
		fmt.Printf("agent attach name@%s runs ssh to %s.\n", host, args[1])
	default:
		if d, ok := dests[host]; ok {
			fmt.Printf("%s: ssh to %s\n", host, d)
		} else {
			fmt.Printf("%s: ssh to %s (the default: its host name)\n", host, host)
		}
		return nil
	}
	if !slices.Contains(trustedHosts(), host) {
		hint("this host's bus doesn't trust %s yet, and attach finds its agent binary through the bus (agent bus trust <bundle>)", host)
	}
	return nil
}

// sshDestPattern is what a destination may be: a host name, user@host, an
// alias, or an IPv6 address in brackets. It never starts with "-", which
// ssh would take for an option.
var sshDestPattern = regexp.MustCompile(`^[A-Za-z0-9_\[][A-Za-z0-9_.@:%\[\]-]*$`)

func validateSSHDest(dest string) error {
	if !sshDestPattern.MatchString(dest) {
		return fmt.Errorf("invalid ssh destination %q: give a host, user@host, or an alias from ~/.ssh/config (letters, digits and ._@:%%[]-, not starting with -)", dest)
	}
	return nil
}

func sshDestsPath() string { return filepath.Join(cfg.BusDir(), "ssh.json") }

// loadSSHDests reads the destinations set per host; none set is empty.
func loadSSHDests() (map[string]string, error) {
	dests := map[string]string{}
	data, err := os.ReadFile(sshDestsPath())
	if errors.Is(err, fs.ErrNotExist) {
		return dests, nil
	}
	if err != nil {
		return nil, fmt.Errorf("reading the ssh destinations: %w", err)
	}
	if err := json.Unmarshal(data, &dests); err != nil {
		return nil, fmt.Errorf("reading the ssh destinations in %s: %w", sshDestsPath(), err)
	}
	return dests, nil
}

func saveSSHDests(dests map[string]string) error {
	if err := os.MkdirAll(cfg.BusDir(), 0o700); err != nil {
		return err
	}
	data, err := json.MarshalIndent(dests, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(sshDestsPath(), append(data, '\n'), 0o644)
}

// sshDestFor is where ssh to host goes: its destination if one is set
// (and still valid), else its name.
func sshDestFor(host string) string {
	dests, _ := loadSSHDests()
	if d, ok := dests[host]; ok && validateSSHDest(d) == nil {
		return d
	}
	return host
}

// attachTerminal reports whether this terminal can be handed over;
// stdinIsTerminal, a variable for tests.
var attachTerminal = stdinIsTerminal

// sshAttach replaces this process with ssh to ref.Host, running that
// host's agent binary there with verb and ref.Name, then extra: "attach
// <name>", or "resume <name> --attach". The host says through the bus
// where its binary is, and that the agent exists.
func sshAttach(ref natsbus.AgentRef, verb string, extra ...string) error {
	if !attachTerminal() {
		return fmt.Errorf("%s %s opens its tmux session in this terminal, over ssh, so it needs one: run it in a terminal (agent capture %s shows its screen from here)", verb, ref, ref)
	}
	var resp *natsbus.ListResponse
	err := callRemote(ref.Host, func(c *natsbus.RPCClient) (err error) {
		resp, err = c.ListHost(getContext(), ref.Host)
		return err
	})
	if err != nil {
		return fmt.Errorf("asking %s where its agent binary is: %w", ref.Host, err)
	}
	if !slices.ContainsFunc(resp.Agents, func(a natsbus.AgentInfo) bool { return a.Name == ref.Name }) {
		return fmt.Errorf("agent %q not found on host %s (%s there: agent list --host %s)", ref.Name, ref.Host, plural(len(resp.Agents), "agent"), ref.Host)
	}
	bin := resp.AgentBinary
	if bin == "" {
		return fmt.Errorf("%s's agent predates attach over ssh and doesn't say where its binary is: update it there, or ssh %s and run agent %s %s", ref.Host, sshDestFor(ref.Host), verb, ref.Name)
	}
	if err := natsbus.ValidateAgentBinary(bin); err != nil {
		return fmt.Errorf("won't run %s's agent over ssh: %w", ref.Host, err)
	}
	dest := sshDestFor(ref.Host)
	remote := strings.Join(append([]string{util.ShellQuote(bin), verb, util.ShellQuote(ref.Name)}, extra...), " ")
	notice("Attaching to %s over ssh to %s. That is ssh, not the bus: it needs \"ssh %s\" to work from here (agent bus ssh %s <destination> changes where). Ctrl-b d detaches.",
		ref, dest, dest, ref.Host)
	if err := execInto("ssh", []string{"ssh", "-t", "--", dest, remote}); err != nil {
		return fmt.Errorf("attaching to %s runs ssh to %s, which didn't start: %w", ref, dest, err)
	}
	return nil
}
