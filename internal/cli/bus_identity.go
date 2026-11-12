package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// Changing trust, and renaming this host. nats-server can't reload a leaf
// listener's pinned certificates or a leaf's remotes (its config reload
// refuses changes to the leafnode block), so a trust change applies when
// the bus restarts: untrust and trust --renames restart it themselves.

var (
	busNoRestart bool
	busRenames   string
)

var busUntrustCmd = &cobra.Command{
	Use:   "untrust <host>",
	Short: "Stop trusting a peer host",
	Long: `Untrust removes a peer's bundle (bus/peers/<host>.json) and its ssh
destination (agent bus ssh), then restarts the bus so the peer is dropped
at once (--no-restart leaves that to agent bus restart). Friction links
and agents' spawned_by that name it stay, as history.

Untrusting on one side is enough to cut the link: the hub refuses a peer
it doesn't pin, and a leaf no longer dials a hub it doesn't trust. The
other side keeps listing this host until it untrusts it too.`,
	Example: `  agent bus untrust old-laptop
  agent bus untrust hub --no-restart`,
	Args: cobra.ExactArgs(1),
	RunE: runBusUntrust,
}

var busRenameCmd = &cobra.Command{
	Use:   "rename <new-name>",
	Short: "Rename this host on the bus, keeping its keys",
	Long: `Rename binds this host's bus identity to a new name. The keys stay:
peers pin the certificate's public key and the nkey, neither of which
names the host, so they can check the renamed host is the same machine.
The certificate is signed again under the new name with the same key,
the old name is kept in bus/host.previous, and bus/<new>.pub.json
replaces bus/<old>.pub.json (its url is --url, else the old bundle's).

A running bus is stopped, and started again as the new name with the
same settings. Agents spawned before the rename keep AGENT_HOST=<old>,
which now means the new name, and replies to <name>@<old> stay here.

Each peer then re-trusts this host: agent bus trust <new>.pub.json
--renames <old>, which rename prints. Until the hub has, this host's link
comes up (its keys are the same), but requests to <name>@<new> don't
reach it.`,
	Example: `  agent bus rename studio
  agent bus rename hub2 --url tls://hub2.lan:7422   # on the hub`,
	Args: cobra.ExactArgs(1),
	RunE: runBusRename,
}

var busRestartCmd = &cobra.Command{
	Use:   "restart",
	Short: "Restart the background bus with the settings it runs with",
	Long: `Restart stops the bus in tmux session ` + busSession + ` and starts it
again with the --listen, --client-port and --remote-ops it was running
with: what agent bus trust needs to apply.

Give any of those flags to change just that setting, e.g. --remote-ops
with the whole list peers may call. The settings it restarts with are
saved, as agent bus start saves them, so a later agent bus start (after a
reboot, say) runs the same way.`,
	Example: `  agent bus restart
  agent bus restart --remote-ops list,status,capture,send,respond,friction,logs,resume`,
	Args: cobra.NoArgs,
	RunE: runBusRestart,
}

// The flags restart takes: their own variables, since bus run reads
// busListen, busClientPort and busRemoteOps, and these have no defaults
// (unset means "as it runs").
var (
	restartListen     string
	restartClientPort int
	restartRemoteOps  []string
)

func initBusIdentityCmds() {
	busRestartCmd.Flags().StringVar(&restartListen, "listen", "", "Accept peer links on this address instead (\"\" stops accepting them)")
	busRestartCmd.Flags().IntVar(&restartClientPort, "client-port", 0, "Local client port (127.0.0.1) instead")
	busRestartCmd.Flags().StringSliceVar(&restartRemoteOps, "remote-ops", nil, "The ops peers may call on this host instead: the whole list")
	busUntrustCmd.Flags().BoolVar(&busNoRestart, "no-restart", false, "Don't restart the bus; agent bus restart applies the change")
	busTrustCmd.Flags().BoolVar(&busNoRestart, "no-restart", false, "With --renames: don't restart the bus")
	busTrustCmd.Flags().StringVar(&busRenames, "renames", "", "The peer's former name: its bundle has the same keys under a new name (agent bus rename)")
	busRenameCmd.Flags().StringVar(&busURL, "url", "", "Where this host accepts peer links, if it is the hub (default: the old bundle's)")
	busCmd.AddCommand(busUntrustCmd, busRenameCmd, busRestartCmd)
}

// busSettings is how a bus runs: what agent bus start was given. agent bus
// start and restart save them (bus/settings.json), and start reuses them
// for the flags it isn't given.
type busSettings struct {
	Listen     string   `json:"listen,omitempty"`
	ClientPort int      `json:"client_port"`
	RemoteOps  []string `json:"remote_ops"`
}

func busSettingsPath() string { return filepath.Join(cfg.BusDir(), "settings.json") }

// loadBusSettings reads the saved settings: nil if none were saved.
func loadBusSettings() (*busSettings, error) {
	data, err := os.ReadFile(busSettingsPath())
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, fmt.Errorf("reading the bus's saved settings: %w", err)
	}
	var s busSettings
	if err := json.Unmarshal(data, &s); err != nil {
		return nil, fmt.Errorf("reading the bus's saved settings in %s: %w (fix or delete it; agent bus start then saves new ones)", busSettingsPath(), err)
	}
	return &s, nil
}

func saveBusSettings(s busSettings) error {
	data, err := json.MarshalIndent(s, "", "  ")
	if err != nil {
		return err
	}
	return os.WriteFile(busSettingsPath(), append(data, '\n'), 0o644)
}

// checkBusSettings refuses settings the bus would fail to start with,
// before a restart stops the running one.
func checkBusSettings(s busSettings) error {
	for _, op := range s.RemoteOps {
		if !slices.Contains(natsbus.AllOps, op) {
			return fmt.Errorf("unknown op %q in --remote-ops: the ops are %s", op, strings.Join(natsbus.AllOps, ", "))
		}
	}
	if s.ClientPort < -1 || s.ClientPort > 65535 {
		return fmt.Errorf("--client-port %d isn't a port", s.ClientPort)
	}
	return nil
}

func (s busSettings) String() string {
	out := fmt.Sprintf("--client-port %d --remote-ops %s", s.ClientPort, strings.Join(s.RemoteOps, ","))
	if s.Listen != "" {
		out = "--listen " + s.Listen + " " + out
	}
	return out
}

// runningBusSettings asks this host's bus how it runs: nil if no bus is
// running.
func runningBusSettings() (*busSettings, error) {
	info, err := queryBus(2 * time.Second)
	if errors.Is(err, natsbus.ErrNoBus) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	s := &busSettings{Listen: info.Listen, ClientPort: info.ClientPort, RemoteOps: info.RemoteOps}
	// A bus built before the reply carried them: where it accepts links,
	// and the port of its client URL, are what it was started with
	// (--client-port -1 aside).
	if s.Listen == "" {
		s.Listen = info.LeafAddr
	}
	if s.ClientPort == 0 {
		if u, err := url.Parse(info.ClientURL); err == nil {
			s.ClientPort, _ = strconv.Atoi(u.Port())
		}
	}
	return s, nil
}

// restartBus stops the bus in its tmux session and starts it again with s.
// A bus that runs elsewhere (agent bus run) isn't this command's to stop.
func restartBus(s busSettings) error {
	if err := checkBusInSession(); err != nil {
		return err
	}
	if err := stopBusSession(); err != nil {
		return err
	}
	return startBusWith(s)
}

func startBusWith(s busSettings) error {
	busListen, busClientPort, busRemoteOps = s.Listen, s.ClientPort, s.RemoteOps
	return startBusSession()
}

// checkBusInSession fails unless this data dir's bus runs in tmux session
// busSession, where restart can stop and start it.
func checkBusInSession() error {
	exists, err := tmuxClient.SessionExists(busSession)
	if err != nil {
		return err
	}
	if !exists {
		return fmt.Errorf("the bus is running, but not in tmux session %s (started with agent bus run?): restart it where it runs (Ctrl-C, then the same command)", busSession)
	}
	return checkOwnBusSession()
}

// restartBusFn restarts the bus for a trust change; tests swap it.
var restartBusFn = restartBus

func runBusRestart(cmd *cobra.Command, args []string) error {
	s, err := runningBusSettings()
	if err != nil {
		return err
	}
	if s == nil {
		return fmt.Errorf("no bus is running on host %s, so there is nothing to restart: agent bus start (with the saved settings, if any; add --listen 0.0.0.0:7422 on the hub)", cfg.Host)
	}
	f := cmd.Flags()
	if f.Changed("listen") {
		s.Listen = restartListen
	}
	if f.Changed("client-port") {
		s.ClientPort = restartClientPort
	}
	if f.Changed("remote-ops") {
		s.RemoteOps = restartRemoteOps
	}
	if err := checkBusSettings(*s); err != nil {
		return err
	}
	if err := restartBusFn(*s); err != nil {
		return err
	}
	if err := saveBusSettings(*s); err != nil {
		return fmt.Errorf("the bus restarted (%s), but saving its settings failed: %w", s, err)
	}
	fmt.Printf("Bus restarted in tmux session %q (%s); settings saved for agent bus start\n", busSession, s)
	return busStatus(cmd, args)
}

// applyTrustChange restarts a running bus so a trust change takes effect,
// unless --no-restart, and says what happened.
func applyTrustChange() error {
	s, err := runningBusSettings()
	switch {
	case err != nil:
		fmt.Printf("Couldn't ask the bus how it runs (%v): agent bus restart applies the change.\n", err)
	case s == nil:
		fmt.Println("The bus isn't running; agent bus start applies the change.")
	case busNoRestart:
		fmt.Println("Restart the bus to apply: agent bus restart")
	default:
		if err := restartBusFn(*s); err != nil {
			return fmt.Errorf("the change is saved, but restarting the bus failed: %w", err)
		}
		fmt.Printf("Restarted the bus (%s), so the change applies now.\n", s)
	}
	return nil
}

func runBusTrust(cmd *cobra.Command, args []string) error {
	var data []byte
	var err error
	if args[0] == "-" {
		data, err = io.ReadAll(os.Stdin)
	} else {
		data, err = os.ReadFile(args[0])
	}
	if err != nil {
		return err
	}
	var b natsbus.PeerBundle
	if err := json.Unmarshal(data, &b); err != nil {
		return fmt.Errorf("reading bundle: %w", err)
	}
	if strings.EqualFold(b.Host, cfg.Host) {
		return fmt.Errorf("bundle is for this host (%q), not a peer", b.Host)
	}
	b.AKA = nil // only --renames gives a peer another name
	if busRenames != "" {
		return trustRenamed(args[0], b)
	}
	if busNoRestart {
		return fmt.Errorf("--no-restart goes with --renames: a plain trust doesn't restart the bus (agent bus restart does)")
	}

	if err := natsbus.TrustPeer(cfg.BusDir(), b); err != nil {
		if same := (*natsbus.SameKeysError)(nil); errors.As(err, &same) {
			return fmt.Errorf("%w: agent bus trust %s --renames %s", err, args[0], same.Peer)
		}
		return err
	}
	fmt.Printf("Trusted %q: key %s, certificate pin %s\n", b.Host, b.NKey, b.CertSHA)
	if b.URL != "" {
		fmt.Printf("This host will link to it at %s (unless it is the hub).\n", b.URL)
	}
	fmt.Println("Restart the bus to apply: agent bus restart")
	return nil
}

// trustRenamed is trust --renames: b is a trusted peer's bundle under the
// name it was renamed to.
func trustRenamed(file string, b natsbus.PeerBundle) error {
	if err := natsbus.ValidateHost(busRenames); err != nil {
		return err
	}
	installed, replaced, err := natsbus.RenamePeer(cfg.BusDir(), busRenames, b)
	if err != nil {
		return err
	}
	old := replaced.Host
	fmt.Printf("Trusted %q as %s renamed: same key %s, certificate pin %s\n", b.Host, old, b.NKey, b.CertSHA)
	if b.URL != "" {
		fmt.Printf("This host will link to it at %s (unless it is the hub).\n", b.URL)
	}

	n, err := store.RenameHost(context.Background(), old, b.Host)
	if err != nil {
		return fmt.Errorf("the new bundle is trusted, but updating references to %s failed: %w", old, err)
	}
	moved, err := moveSSHDest(old, b.Host)
	if err != nil {
		return fmt.Errorf("the new bundle is trusted, but moving %s's ssh destination failed: %w", old, err)
	}
	var updated []string
	if n.Agents > 0 {
		updated = append(updated, "the spawned_by of "+plural(n.Agents, "agent"))
	}
	if n.FrictionLinks > 0 {
		updated = append(updated, plural(n.FrictionLinks, "friction link"))
	}
	if moved != "" {
		updated = append(updated, "its ssh destination ("+moved+")")
	}
	if len(updated) > 0 {
		fmt.Printf("Now naming %s instead of %s: %s\n", b.Host, old, strings.Join(updated, ", "))
	}
	if len(installed.AKA) > 0 {
		fmt.Printf("%s still reaches it (an alias).\n", strings.Join(installed.AKA, ", "))
	}
	return applyTrustChange()
}

// moveSSHDest moves old's ssh destination to new, unless new has its own,
// returning the destination moved ("" if none).
func moveSSHDest(old, new string) (string, error) {
	dests, err := loadSSHDests()
	if err != nil {
		return "", err
	}
	var dest string
	for h, d := range dests {
		if strings.EqualFold(h, old) {
			dest = d
			delete(dests, h)
		}
	}
	if dest == "" {
		return "", nil
	}
	if _, ok := dests[new]; !ok {
		dests[new] = dest
	}
	return dest, saveSSHDests(dests)
}

func runBusUntrust(cmd *cobra.Command, args []string) error {
	if err := natsbus.ValidateHost(args[0]); err != nil {
		return err
	}
	if canonicalHost(args[0]) == cfg.Host {
		return fmt.Errorf("%s is this host, not a peer", args[0])
	}
	b, err := natsbus.UntrustPeer(cfg.BusDir(), args[0])
	if err != nil {
		return err
	}
	fmt.Printf("Untrusted %q (key %s, certificate pin %s)\n", b.Host, b.NKey, b.CertSHA)

	dests, err := loadSSHDests()
	if err != nil {
		return err
	}
	for h, d := range dests {
		if b.Names(h) {
			delete(dests, h)
			if err := saveSSHDests(dests); err != nil {
				return err
			}
			fmt.Printf("Removed its ssh destination (%s).\n", d)
		}
	}

	if b.URL != "" {
		if s, _ := runningBusSettings(); s == nil || s.Listen == "" {
			fmt.Printf("%s was the hub this host links to (%s): this host now reaches no other host. To link again: agent bus trust <hub>.pub.json\n", b.Host, b.URL)
		}
	}
	if err := applyTrustChange(); err != nil {
		return err
	}
	fmt.Printf("%s still trusts this host until it untrusts it too: on %s, agent bus untrust %s\n", b.Host, b.Host, cfg.Host)
	return nil
}

func runBusRename(cmd *cobra.Command, args []string) error {
	old, newName := cfg.Host, args[0]
	if !busSetUp() {
		return fmt.Errorf("this host (%s) has no bus identity to rename: agent bus init --help", old)
	}
	if err := natsbus.CheckRename(cfg.BusDir(), old, newName); err != nil {
		return err
	}
	peerURL := busURL
	oldBundle := filepath.Join(cfg.BusDir(), old+".pub.json")
	if peerURL == "" {
		if data, err := os.ReadFile(oldBundle); err == nil {
			var b natsbus.PeerBundle
			if json.Unmarshal(data, &b) == nil {
				peerURL = b.URL
			}
		}
	}

	running, err := runningBusSettings()
	if err != nil {
		return err
	}
	if running != nil {
		if err := checkBusInSession(); err != nil {
			return fmt.Errorf("rename stops and starts the bus: %w", err)
		}
		if err := stopBusSession(); err != nil {
			return err
		}
	}

	id, err := natsbus.RenameIdentity(cfg.BusDir(), old, newName)
	if err != nil {
		if running != nil {
			if rerr := startBusWith(*running); rerr != nil {
				return fmt.Errorf("%w (and starting the bus again as %s failed: %v)", err, old, rerr)
			}
		}
		return err
	}
	cfg.Host = id.Host
	path, err := writeBundle(id, peerURL)
	if err != nil {
		return err
	}
	if oldBundle != path {
		os.Remove(oldBundle)
	}
	fmt.Printf("Renamed this host's bus identity from %q to %q in %s\n", old, id.Host, cfg.BusDir())
	fmt.Printf("  same key %s, certificate pin %s\n", id.PublicBundle("").NKey, id.CertSHA)
	fmt.Printf("Public bundle: %s\n", path)
	if running != nil {
		if err := startBusWith(*running); err != nil {
			return fmt.Errorf("renamed, but starting the bus as %s failed: %w", id.Host, err)
		}
		fmt.Printf("Restarted the bus as %s (%s).\n", id.Host, running)
	}
	fmt.Printf("Agents spawned before the rename keep AGENT_HOST=%s, which now means %s.\n", old, id.Host)

	peers, _ := natsbus.LoadPeers(cfg.BusDir())
	if len(peers) == 0 {
		fmt.Println("No trusted peers to tell.")
		return nil
	}
	fmt.Printf("Each peer re-trusts this host under its new name (until the hub has, requests to <name>@%s don't reach it). Copy %s to it, then run there:\n", id.Host, path)
	for _, p := range slices.SortedFunc(slices.Values(peers), func(a, b natsbus.PeerBundle) int { return strings.Compare(a.Host, b.Host) }) {
		fmt.Printf("  on %s: agent bus trust %s.pub.json --renames %s\n", p.Host, id.Host, old)
	}
	return nil
}
