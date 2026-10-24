package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/spf13/cobra"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
	"github.com/tikimcfee/gomuxai/internal/service"
	"github.com/tikimcfee/gomuxai/internal/util"
)

// busConnect opens a client connection to this host's agent bus. It returns
// natsbus.ErrNoBus when no bus is running, so callers can fall back to
// local-only behavior. It must fail fast in that case: hooks call the CLI on
// every agent event. Tests may swap it for a natstest connection.
var busConnect natsbus.LocalConnector = func() (*nats.Conn, error) {
	return natsbus.ConnectLocal(cfg.BusDir(), cfg.Host)
}

// busSession is the tmux session `agent bus start` runs the bus in.
const busSession = "gomux-bus"

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
status, capture, send, respond); raw keys stays local-only unless allowed.`,
}

var busInitCmd = &cobra.Command{
	Use:   "init",
	Short: "Create this host's bus keys and certificate",
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
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
	Args:  cobra.NoArgs,
	RunE: func(cmd *cobra.Command, args []string) error {
		id, err := natsbus.LoadIdentity(cfg.BusDir(), cfg.Host)
		if err != nil {
			return err
		}
		data, err := json.MarshalIndent(id.PublicBundle(busURL), "", "  ")
		if err != nil {
			return err
		}
		fmt.Println(string(data))
		return nil
	},
}

var busTrustCmd = &cobra.Command{
	Use:   "trust <bundle.json | ->",
	Short: "Trust a peer host's public bundle",
	Long: `Trust installs a peer's public bundle (from its "agent bus init" or
"agent bus export"). Check the key and pin it prints against the peer's own
output before relying on the link. Restart the bus to apply.`,
	Args: cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
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
		if b.Host == cfg.Host {
			return fmt.Errorf("bundle is for this host (%q), not a peer", b.Host)
		}
		if err := natsbus.TrustPeer(cfg.BusDir(), b); err != nil {
			return err
		}
		fmt.Printf("Trusted %q: key %s, certificate pin %s\n", b.Host, b.NKey, b.CertSHA)
		if b.URL != "" {
			fmt.Printf("This host will link to it at %s (unless it is the hub).\n", b.URL)
		}
		fmt.Println("Restart the bus to apply: agent bus stop && agent bus start ...")
		return nil
	},
}

var busRunCmd = &cobra.Command{
	Use:   "run",
	Short: "Run the bus in the foreground",
	Long:  `Run serves the bus until interrupted. "agent bus start" runs it in a tmux session.`,
	Args:  cobra.NoArgs,
	RunE:  runBus,
}

var busStartCmd = &cobra.Command{
	Use:   "start",
	Short: "Start the bus in the background (tmux session " + busSession + ")",
	Args:  cobra.NoArgs,
	RunE:  startBus,
}

var busStopCmd = &cobra.Command{
	Use:   "stop",
	Short: "Stop the background bus",
	Args:  cobra.NoArgs,
	RunE:  stopBus,
}

var busStatusCmd = &cobra.Command{
	Use:   "status",
	Short: "Show whether the bus is running and which peers are linked",
	Args:  cobra.NoArgs,
	RunE:  busStatus,
}

func init() {
	for _, c := range []*cobra.Command{busInitCmd, busExportCmd} {
		c.Flags().StringVar(&busURL, "url", "", "Where this host accepts peer links, if it is the hub (tls://host:7422)")
	}
	for _, c := range []*cobra.Command{busRunCmd, busStartCmd} {
		c.Flags().StringVar(&busListen, "listen", "", "Accept peer links on this address (makes this host the hub), e.g. 0.0.0.0:7422")
		c.Flags().StringSliceVar(&busRemoteOps, "remote-ops", natsbus.DefaultRemoteOps, "Ops peers may call on this host")
		c.Flags().IntVar(&busClientPort, "client-port", natsbus.DefaultClientPort, "Local client port (127.0.0.1)")
	}
	busCmd.AddCommand(busInitCmd, busExportCmd, busTrustCmd, busRunCmd, busStartCmd, busStopCmd, busStatusCmd)
}

// writeBundle saves this host's public bundle next to its identity.
func writeBundle(id *natsbus.Identity, url string) (string, error) {
	data, err := json.MarshalIndent(id.PublicBundle(url), "", "  ")
	if err != nil {
		return "", err
	}
	path := filepath.Join(cfg.BusDir(), id.Host+".pub.json")
	return path, os.WriteFile(path, append(data, '\n'), 0644)
}

// busInfo is the bus's answer on its local-only status subject.
type busInfo struct {
	Host        string   `json:"host"`
	ClientURL   string   `json:"client_url"`
	LeafAddr    string   `json:"leaf_addr,omitempty"`
	LinkedPeers int      `json:"linked_peers"`
	Trusted     []string `json:"trusted"`
	RemoteOps   []string `json:"remote_ops"`
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
	stopRPC, err := natsbus.ServeRPC(nc, cfg.Host, service.NewRPCHandler(agentService), natsbus.AllOps)
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
		return fmt.Errorf("bus session %q already exists (agent bus stop first)", busSession)
	}
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
			break
		}
		if time.Now().After(deadline) {
			out, _ := tmuxClient.Capture(busSession, 20, true)
			return fmt.Errorf("bus didn't come up within 5s; session output:\n%s", strings.TrimSpace(out))
		}
		time.Sleep(100 * time.Millisecond)
	}
	fmt.Printf("Bus started in tmux session %q\n", busSession)
	return busStatus(cmd, args)
}

func stopBus(cmd *cobra.Command, args []string) error {
	exists, err := tmuxClient.SessionExists(busSession)
	if err != nil {
		return err
	}
	if !exists {
		return fmt.Errorf("no bus session %q", busSession)
	}
	// Interrupt first so the bus shuts down cleanly and removes its URL file.
	tmuxClient.SendSpecialKey(busSession, "c-c")
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(filepath.Join(cfg.BusDir(), "bus.url")); errors.Is(err, os.ErrNotExist) {
			break
		}
		time.Sleep(100 * time.Millisecond)
	}
	if err := tmuxClient.KillSession(busSession); err != nil {
		return err
	}
	fmt.Println("Bus stopped")
	return nil
}

func busStatus(cmd *cobra.Command, args []string) error {
	nc, err := busConnect()
	if errors.Is(err, natsbus.ErrNoBus) {
		fmt.Printf("%s: bus not running\n", cfg.Host)
		return nil
	}
	if err != nil {
		return err
	}
	defer nc.Close()
	msg, err := nc.Request(natsbus.SubjectLocal(cfg.Host, "bus"), nil, 2*time.Second)
	if err != nil {
		return fmt.Errorf("bus not answering: %w", err)
	}
	var info busInfo
	if err := json.Unmarshal(msg.Data, &info); err != nil {
		return err
	}
	fmt.Printf("%s: bus running (clients on %s)\n", info.Host, info.ClientURL)
	if info.LeafAddr != "" {
		fmt.Printf("  hub, accepting peer links on %s\n", info.LeafAddr)
	}
	fmt.Printf("  linked peers: %d; trusted: %s\n", info.LinkedPeers, strings.Join(info.Trusted, ", "))
	fmt.Printf("  remote ops allowed: %s\n", strings.Join(info.RemoteOps, ", "))
	return nil
}
