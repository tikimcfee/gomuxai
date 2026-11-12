package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"maps"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strings"
	"syscall"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// busConnect opens a client connection to this host's agent bus. It returns
// natsbus.ErrNoBus when no bus is running, so callers can fall back to
// local-only behavior. It must fail fast in that case: hooks call the CLI on
// every agent event. Tests may swap it for a natstest connection.
var busConnect natsbus.LocalConnector = func() (*nats.Conn, error) {
	return natsbus.ConnectLocal(cfg.BusDir(), cfg.Host)
}

// busSession is the tmux session `agent bus start` runs the bus in. Tests
// may rename it, to stay clear of a real bus.
var busSession = "gomux-bus"

var (
	busURL        string
	busListen     string
	busRemoteOps  []string
	busClientPort int
)

var busCmd = &cobra.Command{
	Use:   "bus",
	Short: "Run and pair this host's agent bus",
	Long: `The agent bus links hosts, so "name@host" commands, status events, and
"agent wait" work across machines. Each host runs one bus; one host (the hub)
accepts links from the others.

Pairing works like SSH keys: each host has its own keys and certificate, and
trusts a peer by installing the peer's public bundle. Nothing secret is copied.

  # on both hosts
  agent bus init                          # the hub adds: --url tls://<hub>:7422
  # exchange the printed .pub.json files (e.g. with scp), then on each host
  agent bus trust <peer>.pub.json
  # start: the hub listens for peers, the others connect to it
  agent bus start --listen 0.0.0.0:7422   # hub
  agent bus start                         # other hosts

Peers may only call the ops in --remote-ops on this host (default: list,
status, capture, send, respond, friction, logs). keys, spawn and resume
stay local-only unless listed: spawn lets every linked peer start agents
here (agent spawn name@<this host>), and resume lets them start an ended
agent again (agent resume name@<this host>), which amounts to letting them
run commands
as this user (start an agent, then approve its prompts); list them only
for peers you'd give a shell. E.g. --remote-ops
list,status,capture,send,respond,friction,logs,spawn,resume.

Changing trust:

  agent bus untrust <peer>       # drop a peer; restarts the bus
  agent bus rename <new-name>    # rename this host, keeping its keys,
                                 # and print what each peer runs:
  agent bus trust <new>.pub.json --renames <old>
  agent bus restart              # apply a trust, with the same settings

nats-server can't reload trust, so a change applies when the bus restarts.

Only reaching other hosts needs a bus: everything on one host works without.`,
	Example: `  agent bus status
  agent bus init && agent bus start
  agent bus untrust old-laptop
  agent bus rename studio`,
	RunE: runGroup,
}

var busInitCmd = &cobra.Command{
	Use:   "init",
	Short: "Create this host's bus keys and certificate",
	Example: `  agent bus init
  agent bus init --url tls://hub.example:7422   # on the hub`,
	Args: cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		if busSetUp() {
			return fmt.Errorf("this host (%s) already has a bus identity in %s: agent bus export prints its public bundle", cfg.Host, cfg.BusDir())
		}
		id, err := natsbus.InitIdentity(cfg.BusDir(), cfg.Host)
		if err != nil {
			return err
		}
		path, err := writeBundle(id, busURL)
		if err != nil {
			return err
		}
		fmt.Printf("Created bus identity for host %q in %s\n", cfg.Host, cfg.BusDir())
		fmt.Printf("Public bundle: %s\n", path)
		fmt.Printf("  key %s, certificate pin %s\n", id.PublicBundle("").NKey, id.CertSHA)
		fmt.Println("Copy it to each peer and run `agent bus trust` there.")
		return nil
	},
}

var busExportCmd = &cobra.Command{
	Use:   "export",
	Short: "Print this host's public bundle",
	Example: `  agent bus export > hub.pub.json
  agent bus export --url tls://hub.example:7422`,
	Args: cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		id, err := natsbus.LoadIdentity(cfg.BusDir(), cfg.Host)
		if err != nil {
			return err
		}
		data, err := bundleJSON(id, busURL)
		if err != nil {
			return err
		}
		fmt.Print(string(data))
		return nil
	},
}

var busTrustCmd = &cobra.Command{
	Use:   "trust <bundle.json | ->",
	Short: "Trust a peer host's public bundle",
	Long: `Trust installs a peer's public bundle (from its "agent bus init",
"agent bus export" or "agent bus rename"). Check the key and pin it prints
against the peer's own output before relying on the link. Re-trusting a
host replaces its bundle. Restart the bus to apply: agent bus restart.

A peer renamed with "agent bus rename" keeps its keys: trust its new
bundle with --renames <old name>. That checks the bundle has the old
name's key and certificate pin, replaces the old bundle, rewrites this
host's references to the old name (its agents' spawned_by, friction
links, its ssh destination), keeps the old name as an alias, and restarts
the bus (--no-restart skips that).`,
	Example: `  agent bus trust hub.pub.json
  agent bus trust - < hub.pub.json
  agent bus trust new-name.pub.json --renames old-name`,
	Args: cobra.ExactArgs(1),
	RunE: runBusTrust,
}

var busRunCmd = &cobra.Command{
	Use:     "run",
	Short:   "Run the bus in the foreground",
	Long:    `Run serves the bus until interrupted. "agent bus start" runs it in a tmux session.`,
	Example: `  agent bus run --listen 0.0.0.0:7422`,
	Args:    cobra.NoArgs,
	RunE:    runBus,
}

var busStartCmd = &cobra.Command{
	Use:   "start",
	Short: "Start the bus in the background (tmux session " + busSession + ")",
	Long: `Start runs the bus in tmux session ` + busSession + `.

It saves the --listen, --client-port and --remote-ops it starts with (in
bus/settings.json), and a later start without them uses the saved ones: on
the hub, agent bus start after a reboot listens for peers again, and peers
may call the same ops. A flag given changes just that setting, and is
saved. agent bus restart takes the same flags.`,
	Example: `  agent bus start
  agent bus start --listen 0.0.0.0:7422   # on the hub, once; saved after
  agent bus start --remote-ops list,status,capture,send,respond,friction,logs,resume`,
	Args: cobra.NoArgs,
	RunE: startBus,
}

var busStopCmd = &cobra.Command{
	Use:     "stop",
	Short:   "Stop the background bus",
	Example: `  agent bus stop`,
	Args:    cobra.NoArgs,
	RunE:    stopBus,
}

var busStatusCmd = &cobra.Command{
	Use:   "status",
	Short: "Show whether the bus is running and which peers are linked",
	Example: `  agent bus status
  agent bus status --json`,
	Args: cobra.NoArgs,
	RunE: busStatus,
}

var busStatusJSON bool

func init() {
	for _, c := range []*cobra.Command{busInitCmd, busExportCmd} {
		c.Flags().StringVar(&busURL, "url", "", "Where this host accepts peer links, if it is the hub (tls://host:7422)")
	}
	for _, c := range []*cobra.Command{busRunCmd, busStartCmd} {
		c.Flags().StringVar(&busListen, "listen", "", "Accept peer links on this address (makes this host the hub), e.g. 0.0.0.0:7422")
		c.Flags().StringSliceVar(&busRemoteOps, "remote-ops", natsbus.DefaultRemoteOps, "Ops peers may call on this host")
		c.Flags().IntVar(&busClientPort, "client-port", natsbus.DefaultClientPort, "Local client port (127.0.0.1)")
	}
	busStatusCmd.Flags().BoolVar(&busStatusJSON, "json", false, "Print the bus status as JSON")
	busCmd.AddCommand(busInitCmd, busExportCmd, busTrustCmd, busRunCmd, busStartCmd, busStopCmd, busStatusCmd)
	initBusIdentityCmds()
}

// writeBundle saves this host's public bundle next to its identity.
func writeBundle(id *natsbus.Identity, url string) (string, error) {
	data, err := bundleJSON(id, url)
	if err != nil {
		return "", err
	}
	path := filepath.Join(cfg.BusDir(), id.Host+".pub.json")
	return path, os.WriteFile(path, data, 0644)
}

// bundleJSON encodes this host's public bundle as "agent bus trust" reads it.
func bundleJSON(id *natsbus.Identity, url string) ([]byte, error) {
	data, err := json.MarshalIndent(id.PublicBundle(url), "", "  ")
	if err != nil {
		return nil, err
	}
	return append(data, '\n'), nil
}

// busInfo is the bus's answer on its local-only status subject.
type busInfo struct {
	Host        string   `json:"host"`
	ClientURL   string   `json:"client_url"`
	LeafAddr    string   `json:"leaf_addr,omitempty"`
	LinkedPeers int      `json:"linked_peers"`
	Trusted     []string `json:"trusted"`
	RemoteOps   []string `json:"remote_ops"`
	// Listen and ClientPort are the --listen and --client-port it runs
	// with, so agent bus restart can start it again the same way. A bus
	// built before them sends neither (see runningBusSettings).
	Listen     string `json:"listen,omitempty"`
	ClientPort int    `json:"client_port,omitempty"`
}

func runBus(cmd *cobra.Command, args []string) error {
	if spawned, err := runBusDisclaimed(); spawned {
		return err
	}
	d, err := natsbus.StartDaemon(natsbus.DaemonConfig{
		Dir:        cfg.BusDir(),
		Host:       cfg.Host,
		ClientPort: busClientPort,
		LeafListen: busListen,
		RemoteOps:  busRemoteOps,
	})
	if err != nil {
		return err
	}
	defer d.Shutdown()

	nc, err := busConnect()
	if err != nil {
		return fmt.Errorf("connecting to own bus: %w", err)
	}
	defer nc.Close()

	// Local callers may use every op; peers are limited to --remote-ops by
	// the link permissions in natsbus.serverOptions.
	stopRPC, err := natsbus.ServeRPC(nc, cfg.Host, busHandler(agentService), natsbus.AllOps)
	if err != nil {
		return err
	}
	defer stopRPC()

	peers, _ := natsbus.LoadPeers(cfg.BusDir())
	trusted := make([]string, len(peers))
	for i, p := range peers {
		trusted[i] = p.Host
	}
	statusSub, err := nc.Subscribe(natsbus.SubjectLocal(cfg.Host, "bus"), func(m *nats.Msg) {
		data, _ := json.Marshal(busInfo{
			Host: cfg.Host, ClientURL: d.ClientURL(), LeafAddr: d.LeafAddr(),
			LinkedPeers: d.LinkedPeers(), Trusted: trusted, RemoteOps: busRemoteOps,
			Listen: busListen, ClientPort: busClientPort,
		})
		m.Respond(data)
	})
	if err != nil {
		return err
	}
	defer statusSub.Unsubscribe()

	fmt.Printf("agent bus for %q: clients on %s", cfg.Host, d.ClientURL())
	if addr := d.LeafAddr(); addr != "" {
		fmt.Printf(", peer links on %s", addr)
	}
	fmt.Printf("; trusted peers: %s\n", strings.Join(trusted, ", "))
	warnLocalNetwork(peers)

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	watchBusParent(ctx, cancel)
	<-ctx.Done()
	fmt.Println("agent bus stopping")
	return nil
}

func startBus(cmd *cobra.Command, args []string) error {
	if exists, _ := tmuxClient.SessionExists(busSession); exists {
		if err := checkOwnBusSession(); err != nil {
			return err
		}
		return fmt.Errorf("the bus is already running in tmux session %s (agent bus status shows it; agent bus restart restarts it)", busSession)
	}
	s, from, err := startSettings(cmd)
	if err != nil {
		return err
	}
	busListen, busClientPort, busRemoteOps = s.Listen, s.ClientPort, s.RemoteOps
	if err := startBusSession(); err != nil {
		return err
	}
	if err := saveBusSettings(s); err != nil {
		return fmt.Errorf("the bus started (%s), but saving its settings failed: %w", s, err)
	}
	fmt.Printf("Bus started in tmux session %q (%s%s)\n", busSession, s, from)
	return busStatus(cmd, args)
}

// startSettings is what agent bus start runs with: the flags given, and
// for the rest the saved settings, or the defaults if none were saved.
// from says where they came from, for start's output.
func startSettings(cmd *cobra.Command) (s busSettings, from string, err error) {
	s = busSettings{Listen: busListen, ClientPort: busClientPort, RemoteOps: busRemoteOps}
	saved, err := loadBusSettings()
	if err != nil {
		return s, "", err
	}
	f := cmd.Flags()
	if saved != nil {
		if !f.Changed("listen") {
			s.Listen = saved.Listen
		}
		if !f.Changed("client-port") {
			s.ClientPort = saved.ClientPort
		}
		if !f.Changed("remote-ops") {
			s.RemoteOps = saved.RemoteOps
		}
		from = "; saved settings"
		if f.Changed("listen") || f.Changed("client-port") || f.Changed("remote-ops") {
			from = "; saved settings, with the flags given"
		}
	}
	return s, from, checkBusSettings(s)
}

// startBusSession starts the bus in its tmux session with the --listen,
// --client-port and --remote-ops in busListen, busClientPort and
// busRemoteOps, and waits for it to answer.
func startBusSession() error {
	if _, err := natsbus.LoadIdentity(cfg.BusDir(), cfg.Host); err != nil {
		return err
	}
	if cfg.AgentBinary == "" {
		return fmt.Errorf("can't locate the agent binary to run the bus")
	}

	// Pin the host, so the bus runs as the identity that started it even if
	// AGENT_HOST isn't set in the new session's shell.
	run := []string{"env", "AGENT_HOST=" + util.ShellQuote(cfg.Host), util.ShellQuote(cfg.AgentBinary), "bus", "run",
		"--client-port", fmt.Sprint(busClientPort),
		"--remote-ops", util.ShellQuote(strings.Join(busRemoteOps, ","))}
	if busListen != "" {
		run = append(run, "--listen", util.ShellQuote(busListen))
	}
	if err := tmuxClient.CreateSession(busSession, nil); err != nil {
		return err
	}
	if err := tmuxClient.SendKeys(busSession, strings.Join(run, " ")); err != nil {
		return err
	}
	if err := tmuxClient.SendSpecialKey(busSession, "Enter"); err != nil {
		return err
	}

	deadline := time.Now().Add(5 * time.Second)
	for {
		nc, err := busConnect()
		if err == nil {
			nc.Close()
			return nil
		}
		if time.Now().After(deadline) {
			out, _ := tmuxClient.Capture(busSession, 20)
			return fmt.Errorf("bus didn't come up within 5s; session output:\n%s", strings.TrimSpace(out))
		}
		time.Sleep(100 * time.Millisecond)
	}
}

// checkOwnBusSession fails if this data dir's bus isn't running while the
// bus session exists. The session name is the same for every data dir on
// the tmux server, while each bus records its address in its own data dir:
// the session then runs another agent binary's bus (or a dead one), which
// isn't this binary's to stop.
func checkOwnBusSession() error {
	nc, err := busConnect()
	if err == nil {
		nc.Close()
		return nil
	}
	if !errors.Is(err, natsbus.ErrNoBus) {
		return err
	}
	return fmt.Errorf("tmux session %s exists, but this data dir's bus (%s) isn't running: the session likely runs the bus of another agent binary with its own data dir, so it's left alone. If it's a dead bus of this one, agent session kill %s",
		busSession, cfg.BusDir(), busSession)
}

func stopBus(cmd *cobra.Command, args []string) error {
	exists, err := tmuxClient.SessionExists(busSession)
	if err != nil {
		return err
	}
	if !exists {
		if _, err := queryBus(time.Second); err == nil {
			return fmt.Errorf("the bus is running, but not in tmux session %s (started with agent bus run?): stop it with Ctrl-C where it runs", busSession)
		}
		return fmt.Errorf("no bus is running on host %s (no tmux session %s), so there is nothing to stop", cfg.Host, busSession)
	}
	if err := checkOwnBusSession(); err != nil {
		return err
	}
	if err := stopBusSession(); err != nil {
		return err
	}
	fmt.Println("Bus stopped")
	return nil
}

// stopBusSession stops the bus running in its tmux session: interrupted
// first, so it shuts down cleanly and removes its URL file, then the
// session is killed.
func stopBusSession() error {
	tmuxClient.SendSpecialKey(busSession, "c-c")
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(filepath.Join(cfg.BusDir(), "bus.url")); errors.Is(err, os.ErrNotExist) {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	return tmuxClient.KillSession(busSession)
}

// queryBus asks this host's bus for its status, waiting up to timeout for
// the answer. It returns natsbus.ErrNoBus if no bus is running.
func queryBus(timeout time.Duration) (*busInfo, error) {
	nc, err := busConnect()
	if err != nil {
		return nil, err
	}
	defer nc.Close()
	msg, err := nc.Request(natsbus.SubjectLocal(cfg.Host, "bus"), nil, timeout)
	if err != nil {
		return nil, fmt.Errorf("bus not answering: %w", err)
	}
	var info busInfo
	if err := json.Unmarshal(msg.Data, &info); err != nil {
		return nil, err
	}
	return &info, nil
}

// busStatusOut is the bus status as --json prints it.
type busStatusOut struct {
	Host        string   `json:"host"`
	SetUp       bool     `json:"set_up"` // has an identity ("agent bus init")
	Running     bool     `json:"running"`
	ClientURL   string   `json:"client_url,omitempty"`
	HubAddr     string   `json:"hub_addr,omitempty"` // where it accepts peer links, if it is the hub
	LinkedPeers int      `json:"linked_peers"`
	Trusted     []string `json:"trusted"`
	RemoteOps   []string `json:"remote_ops"`
	// SSH is where agent attach name@host runs ssh to, per trusted peer
	// (agent bus ssh): its set destination, or its name.
	SSH map[string]string `json:"ssh"`
	// Formerly lists this host's names before agent bus rename, oldest
	// first; they still reach it.
	Formerly []string `json:"formerly,omitempty"`
}

// sshByPeer is each trusted peer's ssh destination, for bus status.
func sshByPeer() map[string]string {
	out := map[string]string{}
	for _, h := range trustedHosts() {
		out[h] = sshDestFor(h)
	}
	return out
}

// busSetUp reports whether this host has a bus identity ("agent bus init").
func busSetUp() bool {
	_, err := natsbus.LoadIdentity(cfg.BusDir(), cfg.Host)
	return err == nil
}

func busStatus(cmd *cobra.Command, args []string) error {
	info, err := queryBus(2 * time.Second)
	if busStatusJSON {
		out := busStatusOut{Host: cfg.Host, SetUp: busSetUp(), Trusted: []string{}, RemoteOps: []string{}, SSH: sshByPeer(),
			Formerly: natsbus.PreviousHosts(cfg.BusDir())}
		if errors.Is(err, natsbus.ErrNoBus) {
			return printJSON(out)
		}
		if err != nil {
			return err
		}
		out.Running = true
		out.ClientURL, out.HubAddr, out.LinkedPeers = info.ClientURL, info.LeafAddr, info.LinkedPeers
		out.Trusted, out.RemoteOps = info.Trusted, info.RemoteOps
		return printJSON(out)
	}
	if errors.Is(err, natsbus.ErrNoBus) {
		fmt.Printf("%s: bus not running\n", cfg.Host)
		if exists, _ := tmuxClient.SessionExists(busSession); exists {
			hint("tmux session %s exists, but runs no bus of this data dir (%s): likely another agent binary's bus", busSession, cfg.BusDir())
		} else if busSetUp() {
			hint("start it: agent bus start (add --listen 0.0.0.0:7422 on the hub)")
		} else {
			hint("set it up first: agent bus init (see agent bus --help)")
		}
		return nil
	}
	if err != nil {
		return err
	}
	fmt.Printf("%s: bus running (clients on %s)\n", info.Host, info.ClientURL)
	if prev := natsbus.PreviousHosts(cfg.BusDir()); len(prev) > 0 {
		fmt.Printf("  formerly: %s (agent bus rename)\n", strings.Join(prev, ", "))
	}
	if info.LeafAddr != "" {
		fmt.Printf("  hub, accepting peer links on %s\n", info.LeafAddr)
	}
	fmt.Printf("  linked peers: %d; trusted: %s\n", info.LinkedPeers, strings.Join(info.Trusted, ", "))
	fmt.Printf("  remote ops allowed: %s\n", strings.Join(info.RemoteOps, ", "))
	if ssh := sshByPeer(); len(ssh) > 0 {
		var dests []string
		for _, h := range slices.Sorted(maps.Keys(ssh)) {
			dests = append(dests, h+" → "+ssh[h])
		}
		fmt.Printf("  attach over ssh (agent bus ssh): %s\n", strings.Join(dests, ", "))
	}
	return nil
}
