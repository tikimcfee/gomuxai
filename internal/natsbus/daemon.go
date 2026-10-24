package natsbus

import (
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"time"

	"github.com/nats-io/jwt/v2"
	"github.com/nats-io/nats-server/v2/logger"
	natsserver "github.com/nats-io/nats-server/v2/server"
	"github.com/nats-io/nats.go"
)

// Topology: a host started with a leaf listener is the hub and accepts links
// from trusted peers; every other host dials each trusted peer whose bundle
// advertises a URL. v1 supports one hub.

const (
	busURLFile = "bus.url" // client URL of the running bus (not secret)
	busLogFile = "bus.log"

	// DefaultClientPort is where the bus listens for local clients (127.0.0.1 only).
	DefaultClientPort = 7420
	// DefaultLeafPort is where a hub accepts links from peers.
	DefaultLeafPort = 7422
)

// DaemonConfig configures a host's bus.
type DaemonConfig struct {
	Dir        string   // bus dir: identity, peers, runtime files
	Host       string   // this host's name; must match the identity
	ClientPort int      // local client port on 127.0.0.1; -1 = random (tests)
	LeafListen string   // "addr:port" to accept peer links on; "" = not a hub
	RemoteOps  []string // ops peers may call on this host; nil = DefaultRemoteOps
}

// Daemon is a running bus.
type Daemon struct {
	cfg    DaemonConfig
	id     *Identity
	peers  []PeerBundle
	server *natsserver.Server
}

// StartDaemon starts this host's bus and records its client URL so local
// clients can find it. Stop it with Shutdown.
func StartDaemon(cfg DaemonConfig) (*Daemon, error) {
	id, err := LoadIdentity(cfg.Dir, cfg.Host)
	if err != nil {
		return nil, err
	}
	peers, err := LoadPeers(cfg.Dir)
	if err != nil {
		return nil, err
	}
	opts, err := serverOptions(cfg, id, peers)
	if err != nil {
		return nil, err
	}

	ns, err := natsserver.NewServer(opts)
	if err != nil {
		return nil, fmt.Errorf("creating bus server: %w", err)
	}
	ns.SetLoggerV2(logger.NewFileLogger(filepath.Join(cfg.Dir, busLogFile), true, false, false, true), false, false, false)
	ns.Start()
	if !ns.ReadyForConnections(5 * time.Second) {
		ns.Shutdown()
		return nil, fmt.Errorf("bus server not ready within 5s (see %s)", filepath.Join(cfg.Dir, busLogFile))
	}
	if err := os.WriteFile(filepath.Join(cfg.Dir, busURLFile), []byte(ns.ClientURL()), 0644); err != nil {
		ns.Shutdown()
		return nil, fmt.Errorf("recording bus URL: %w", err)
	}
	return &Daemon{cfg: cfg, id: id, peers: peers, server: ns}, nil
}

// Shutdown stops the bus and removes its URL file.
func (d *Daemon) Shutdown() {
	os.Remove(filepath.Join(d.cfg.Dir, busURLFile))
	d.server.Shutdown()
	d.server.WaitForShutdown()
}

// ClientURL is where local clients connect.
func (d *Daemon) ClientURL() string { return d.server.ClientURL() }

// LeafAddr is where the hub accepts peer links ("host:port"), or "" if it
// doesn't. The port is resolved when the config asked for a random one.
func (d *Daemon) LeafAddr() string {
	if d.cfg.LeafListen == "" {
		return ""
	}
	v, err := d.server.Varz(nil)
	if err != nil {
		return ""
	}
	return net.JoinHostPort(v.LeafNode.Host, strconv.Itoa(v.LeafNode.Port))
}

// LinkedPeers counts the currently established peer links.
func (d *Daemon) LinkedPeers() int { return d.server.NumLeafNodes() }

// ConnectLocal opens an authenticated client connection to the bus running
// on this host. It returns ErrNoBus quickly when none is running, since hook
// processes call it on every agent event.
func ConnectLocal(dir, host string, opts ...nats.Option) (*nats.Conn, error) {
	data, err := os.ReadFile(filepath.Join(dir, busURLFile))
	if errors.Is(err, os.ErrNotExist) {
		return nil, ErrNoBus
	} else if err != nil {
		return nil, err
	}
	id, err := LoadIdentity(dir, host)
	if err != nil {
		return nil, err
	}
	pub := id.LocalPublicKey()
	opts = append([]nats.Option{
		nats.Nkey(pub, id.LocalKey.Sign),
		nats.Timeout(500 * time.Millisecond),
		nats.Name("agent " + host),
	}, opts...)
	nc, err := nats.Connect(strings.TrimSpace(string(data)), opts...)
	if errors.Is(err, nats.ErrNoServers) {
		return nil, ErrNoBus // stale URL file: the bus died without cleaning up
	}
	return nc, err
}

// serverOptions builds the bus server's configuration. All security policy
// for links lives here and in peerPermissions/remoteLeaf.
func serverOptions(cfg DaemonConfig, id *Identity, peers []PeerBundle) (*natsserver.Options, error) {
	if cfg.Host != id.Host {
		return nil, fmt.Errorf("config host %q doesn't match identity host %q", cfg.Host, id.Host)
	}
	remoteOps := cfg.RemoteOps
	if remoteOps == nil {
		remoteOps = DefaultRemoteOps
	}
	for _, op := range remoteOps {
		if !slices.Contains(allOps, op) {
			return nil, fmt.Errorf("unknown op %q in remote ops", op)
		}
	}

	opts := &natsserver.Options{
		ServerName: "gomux-" + cfg.Host,
		Host:       "127.0.0.1", // local clients only; peers come in over leaf links
		Port:       cfg.ClientPort,
		NoSigs:     true,
		// No JetStream, monitoring, or system account exposure in v1.
		Nkeys: []*natsserver.NkeyUser{{
			Nkey:                   id.LocalPublicKey(),
			AllowedConnectionTypes: connTypes(jwt.ConnectionTypeStandard),
		}},
	}

	if cfg.LeafListen != "" {
		host, port, err := splitHostPort(cfg.LeafListen)
		if err != nil {
			return nil, fmt.Errorf("leaf listen address: %w", err)
		}
		pins := natsserver.PinnedCertSet{}
		for _, p := range peers {
			pins[p.CertSHA] = struct{}{}
			opts.Nkeys = append(opts.Nkeys, &natsserver.NkeyUser{
				Nkey:                   p.NKey,
				AllowedConnectionTypes: connTypes(jwt.ConnectionTypeLeafnode),
				Permissions:            peerPermissions(cfg.Host, p.Host, peerHosts(peers), remoteOps),
			})
		}
		opts.LeafNode.Host = host
		opts.LeafNode.Port = port
		opts.LeafNode.TLSConfig = &tls.Config{
			Certificates: []tls.Certificate{id.Cert},
			// Peers present self-signed certs; nats-server checks them
			// against TLSPinnedCerts, so chain verification is skipped here.
			ClientAuth: tls.RequireAnyClientCert,
			MinVersion: tls.VersionTLS13,
		}
		opts.LeafNode.TLSPinnedCerts = pins // empty set: no peer can link
		opts.LeafNode.TLSTimeout = 2
	} else {
		for _, p := range peers {
			if p.URL == "" {
				continue
			}
			remote, err := remoteLeaf(cfg.Host, id, p, remoteOps)
			if err != nil {
				return nil, err
			}
			opts.LeafNode.Remotes = append(opts.LeafNode.Remotes, remote)
		}
	}
	return opts, nil
}

var allOps = []string{OpList, OpStatus, OpCapture, OpSend, OpRespond, OpKeys}

// peerPermissions limits what a linked peer can do on the hub. A leaf
// connection's permissions are from the hub's point of view of that link:
// Publish is what the peer may send into the hub, Subscribe what the hub
// will forward to it.
//
// Events are allowed by wildcard and narrowed by denying every other known
// host: an allow of only the peer's own subject would be narrower than
// subscribers' gomux.*.agent.*.status interest, and the hub would then never
// tell the peer about that interest, so its events would never cross.
func peerPermissions(self, peer string, knownPeers []string, remoteOps []string) *natsserver.Permissions {
	deny := deniedOps(self, remoteOps)
	for _, h := range append([]string{self}, knownPeers...) {
		if h != peer {
			deny = append(deny, SubjectAllStatus(h)) // no speaking for other hosts
		}
	}
	return &natsserver.Permissions{
		Publish: &natsserver.SubjectPermission{
			Allow: []string{
				SubjectRoot + ".*.rpc.*", // requests, to any host
				"_INBOX.>",               // replies to requests
				SubjectAllStatus("*"),    // events, narrowed by deny
			},
			Deny: deny,
		},
		Subscribe: &natsserver.SubjectPermission{
			Allow: []string{
				SubjectRoot + "." + peer + ".rpc.*", // requests for it
				"_INBOX.>",
				SubjectAllStatus("*"),
			},
		},
	}
}

// remoteLeaf configures this host's link to a peer hub: TLS pinned to the
// hub's key, nkey authentication, and refusal of requests for ops this host
// doesn't serve remotely.
//
// The hub is trusted as the router. On a leaf link a deny filters a subject
// in both directions (it also suppresses the interest that lets the subject
// flow out), so a leaf can't refuse "events claiming to be me" from the hub
// without silencing its own genuine events. Consumers therefore treat events
// as hints and confirm with a status check, and the hub belongs on the most
// trusted machine. The hub does stop peers from speaking for other hosts.
func remoteLeaf(self string, id *Identity, hub PeerBundle, remoteOps []string) (*natsserver.RemoteLeafOpts, error) {
	u, err := url.Parse(hub.URL)
	if err != nil {
		return nil, fmt.Errorf("peer %s url: %w", hub.Host, err)
	}
	return &natsserver.RemoteLeafOpts{
		URLs: []*url.URL{u},
		// The server signs the hub's nonce with this seed and sends only the
		// public key and signature. (SignatureCB is for JWT auth and would
		// omit the public key.)
		Nkey: string(id.hostSeed),
		TLS:  true,
		TLSConfig: &tls.Config{
			Certificates: []tls.Certificate{id.Cert},
			// The hub's cert is self-signed; trust comes from the pin.
			InsecureSkipVerify:    true,
			VerifyPeerCertificate: pinVerifier(hub.Host, hub.CertSHA),
			MinVersion:            tls.VersionTLS13,
		},
		TLSTimeout:  2,
		DenyImports: append(deniedOps(self, remoteOps), SubjectRoot+"."+self+".local.>"),
	}, nil
}

func peerHosts(peers []PeerBundle) []string {
	hosts := make([]string, len(peers))
	for i, p := range peers {
		hosts[i] = p.Host
	}
	return hosts
}

// deniedOps lists host's RPC subjects for ops not in remoteOps.
func deniedOps(host string, remoteOps []string) []string {
	var denied []string
	for _, op := range allOps {
		if !slices.Contains(remoteOps, op) {
			denied = append(denied, SubjectRPC(host, op))
		}
	}
	return denied
}

// pinVerifier accepts only a certificate whose public key hashes to pin.
func pinVerifier(host, pin string) func([][]byte, [][]*x509.Certificate) error {
	return func(rawCerts [][]byte, _ [][]*x509.Certificate) error {
		if len(rawCerts) == 0 {
			return fmt.Errorf("peer %s presented no certificate", host)
		}
		cert, err := x509.ParseCertificate(rawCerts[0])
		if err != nil {
			return err
		}
		if got := spkiSHA256(cert); got != pin {
			return fmt.Errorf("peer %s certificate key %s doesn't match pinned %s", host, got, pin)
		}
		return nil
	}
}

func connTypes(types ...string) map[string]struct{} {
	m := make(map[string]struct{}, len(types))
	for _, t := range types {
		m[t] = struct{}{}
	}
	return m
}

func splitHostPort(addr string) (string, int, error) {
	host, portStr, err := net.SplitHostPort(addr)
	if err != nil {
		return "", 0, err
	}
	port, err := strconv.Atoi(portStr)
	if err != nil {
		return "", 0, fmt.Errorf("port %q: %w", portStr, err)
	}
	return host, port, nil
}
