// Identity and peer bundle loading (read side only).
// Copied from the Go agent's legacy/internal/natsbus/identity.go.
package main

import (
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"

	"github.com/nats-io/nkeys"
)

const (
	hostNameFile  = "host"
	hostSeedFile  = "host.nk"
	localSeedFile = "local.nk"
	tlsKeyFile    = "tls.key"
	tlsCertFile   = "tls.crt"
	peersDir      = "peers"
	prevHostsFile = "host.previous"
)

// ErrNoIdentity means `agent bus init` hasn't been run on this host.
var ErrNoIdentity = errors.New("no bus identity on this host (run `agent bus init`)")

// Identity is a host's bus credentials.
type Identity struct {
	Host     string
	HostKey  nkeys.KeyPair // signs leaf connections to peers
	LocalKey nkeys.KeyPair // authenticates local clients
	Cert     tls.Certificate
	CertSHA  string // hex SHA-256 of the certificate's SubjectPublicKeyInfo
	hostSeed []byte // remoteLeaf hands the seed itself to nats-server
}

// PeerBundle is the public half of a host's identity: everything another
// host needs to trust it, and nothing secret.
type PeerBundle struct {
	Host    string   `json:"host"`
	NKey    string   `json:"nkey"`          // public user nkey ("U...")
	CertSHA string   `json:"cert_sha256"`   // pinned TLS public key
	URL     string   `json:"url,omitempty"` // leaf URL if the host accepts links, e.g. tls://box:7422
	AKA     []string `json:"aka,omitempty"`
}

// Validate checks a bundle's fields before it is trusted.
func (b PeerBundle) Validate() error {
	if err := ValidateHost(b.Host); err != nil {
		return err
	}
	if !nkeys.IsValidPublicUserKey(b.NKey) {
		return fmt.Errorf("peer %s: invalid public nkey", b.Host)
	}
	if raw, err := hex.DecodeString(b.CertSHA); err != nil || len(raw) != sha256.Size {
		return fmt.Errorf("peer %s: cert_sha256 must be 64 hex characters", b.Host)
	}
	if b.URL != "" && !strings.HasPrefix(b.URL, "tls://") {
		return fmt.Errorf("peer %s: url must use tls://", b.Host)
	}
	for _, name := range b.AKA {
		if err := ValidateHost(name); err != nil {
			return fmt.Errorf("peer %s: former name: %w", b.Host, err)
		}
	}
	return nil
}

// LoadIdentity reads the identity in dir, checking secret file permissions.
func LoadIdentity(dir, host string) (*Identity, error) {
	read := func(name string, secret bool) ([]byte, error) {
		path := filepath.Join(dir, name)
		info, err := os.Stat(path)
		if errors.Is(err, os.ErrNotExist) {
			return nil, ErrNoIdentity
		} else if err != nil {
			return nil, err
		}
		if secret && info.Mode().Perm()&0077 != 0 {
			return nil, fmt.Errorf("%s is readable by other users (mode %o); run chmod 600", path, info.Mode().Perm())
		}
		return os.ReadFile(path)
	}

	// The identity belongs to one host name. Loading it under another would
	// sign links and publish events as a host the peers don't know.
	name, err := read(hostNameFile, false)
	if err != nil {
		return nil, err
	}
	if recorded := strings.TrimSpace(string(name)); recorded != host {
		if slices.ContainsFunc(PreviousHosts(dir), func(n string) bool { return strings.EqualFold(n, host) }) {
			return nil, fmt.Errorf("bus identity in %s was renamed from %q to %q (agent bus rename): run as %s", dir, host, recorded, recorded)
		}
		return nil, fmt.Errorf("bus identity in %s is for host %q, not %q (set AGENT_HOST=%s, or init a new identity)", dir, recorded, host, recorded)
	}

	id := &Identity{Host: host}
	if id.hostSeed, err = read(hostSeedFile, true); err != nil {
		return nil, err
	}
	localSeed, err := read(localSeedFile, true)
	if err != nil {
		return nil, err
	}
	keyPEM, err := read(tlsKeyFile, true)
	if err != nil {
		return nil, err
	}
	certPEM, err := read(tlsCertFile, false)
	if err != nil {
		return nil, err
	}
	if id.HostKey, err = nkeys.FromSeed(id.hostSeed); err != nil {
		return nil, fmt.Errorf("reading %s: %w", hostSeedFile, err)
	}
	if id.LocalKey, err = nkeys.FromSeed(localSeed); err != nil {
		return nil, fmt.Errorf("reading %s: %w", localSeedFile, err)
	}
	if id.Cert, err = tls.X509KeyPair(certPEM, keyPEM); err != nil {
		return nil, fmt.Errorf("loading TLS key pair: %w", err)
	}
	leaf, err := x509.ParseCertificate(id.Cert.Certificate[0])
	if err != nil {
		return nil, fmt.Errorf("parsing TLS certificate: %w", err)
	}
	id.CertSHA = spkiSHA256(leaf)
	return id, nil
}

// LocalPublicKey is the nkey local clients authenticate as.
func (id *Identity) LocalPublicKey() string {
	pub, _ := id.LocalKey.PublicKey()
	return pub
}

// LoadPeers reads every trusted peer bundle in dir.
func LoadPeers(dir string) ([]PeerBundle, error) {
	paths, err := filepath.Glob(filepath.Join(dir, peersDir, "*.json"))
	if err != nil {
		return nil, err
	}
	peers := make([]PeerBundle, 0, len(paths))
	for _, path := range paths {
		data, err := os.ReadFile(path)
		if err != nil {
			return nil, err
		}
		var b PeerBundle
		if err := json.Unmarshal(data, &b); err != nil {
			return nil, fmt.Errorf("reading %s: %w", path, err)
		}
		if err := b.Validate(); err != nil {
			return nil, fmt.Errorf("%s: %w", path, err)
		}
		if filepath.Base(path) != b.Host+".json" {
			return nil, fmt.Errorf("%s: bundle is for host %q", path, b.Host)
		}
		peers = append(peers, b)
	}
	return peers, nil
}

// PreviousHosts lists the names the identity in dir had before (agent bus
// rename), oldest first.
func PreviousHosts(dir string) []string {
	data, err := os.ReadFile(filepath.Join(dir, prevHostsFile))
	if err != nil {
		return nil
	}
	var names []string
	for _, line := range strings.Split(string(data), "\n") {
		if name := strings.TrimSpace(line); name != "" {
			names = append(names, name)
		}
	}
	return names
}

// spkiSHA256 is the pin format nats-server checks (TLSPinnedCerts).
func spkiSHA256(cert *x509.Certificate) string {
	sum := sha256.Sum256(cert.RawSubjectPublicKeyInfo)
	return hex.EncodeToString(sum[:])
}
