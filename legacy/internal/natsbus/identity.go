package natsbus

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"crypto/tls"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/hex"
	"encoding/json"
	"encoding/pem"
	"errors"
	"fmt"
	"math/big"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"time"

	"github.com/nats-io/nkeys"
)

// A host's bus identity lives in its bus dir (.gomuxai/bus):
//
//	host       the host name the identity is bound to
//	host.previous  names this host had before (agent bus rename), one a line
//	host.nk    nkey seed this host signs with when it links to a peer   0600
//	local.nk   nkey seed local CLIs and hooks authenticate with         0600
//	tls.key    private key for the host's self-signed TLS certificate   0600
//	tls.crt    the certificate (public)
//	peers/     one PeerBundle (<host>.json) per trusted peer
//
// Trust works like SSH known_hosts: a peer is identified by its nkey and the
// SHA-256 of its certificate's public key, both installed out of band from
// the peer's public bundle. There is no CA, and secrets never leave a host.

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
	Host    string `json:"host"`
	NKey    string `json:"nkey"`          // public user nkey ("U...")
	CertSHA string `json:"cert_sha256"`   // pinned TLS public key
	URL     string `json:"url,omitempty"` // leaf URL if the host accepts links, e.g. tls://box:7422
	// AKA lists names this peer had before it was renamed (agent bus trust
	// --renames), so references to them reach it. Only the trusting host
	// sets it: a bundle's own claim to other names isn't taken.
	AKA []string `json:"aka,omitempty"`
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

// Names reports whether name is this peer's name or one it had before,
// ignoring case.
func (b PeerBundle) Names(name string) bool {
	return strings.EqualFold(name, b.Host) || slices.ContainsFunc(b.AKA, func(n string) bool { return strings.EqualFold(n, name) })
}

// InitIdentity creates a new identity for host in dir. It refuses to replace
// an existing one: that would silently break every peer's trust.
func InitIdentity(dir, host string) (*Identity, error) {
	if err := ValidateHost(host); err != nil {
		return nil, err
	}
	if _, err := os.Stat(filepath.Join(dir, hostSeedFile)); err == nil {
		return nil, fmt.Errorf("bus identity already exists in %s", dir)
	}

	hostKey, err := nkeys.CreateUser()
	if err != nil {
		return nil, fmt.Errorf("creating host nkey: %w", err)
	}
	localKey, err := nkeys.CreateUser()
	if err != nil {
		return nil, fmt.Errorf("creating local nkey: %w", err)
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, fmt.Errorf("generating TLS key: %w", err)
	}
	certPEM, keyPEM, err := selfSignedCert(host, key)
	if err != nil {
		return nil, err
	}
	hostSeed, _ := hostKey.Seed()
	localSeed, _ := localKey.Seed()

	if err := os.MkdirAll(filepath.Join(dir, peersDir), 0700); err != nil {
		return nil, fmt.Errorf("creating bus dir: %w", err)
	}
	files := []struct {
		name string
		data []byte
		mode os.FileMode
	}{
		{hostNameFile, []byte(host + "\n"), 0644},
		{tlsCertFile, certPEM, 0644},
		{tlsKeyFile, keyPEM, 0600},
		{localSeedFile, localSeed, 0600},
		{hostSeedFile, hostSeed, 0600}, // last: its presence marks a complete identity
	}
	for _, f := range files {
		if err := writeFileExclusive(filepath.Join(dir, f.name), f.data, f.mode); err != nil {
			return nil, err
		}
	}
	return LoadIdentity(dir, host)
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

// PublicBundle returns what peers need to trust this host. url is where this
// host accepts leaf links, or "" if it only connects out.
func (id *Identity) PublicBundle(url string) PeerBundle {
	pub, _ := id.HostKey.PublicKey()
	return PeerBundle{Host: id.Host, NKey: pub, CertSHA: id.CertSHA, URL: url}
}

// LocalPublicKey is the nkey local clients authenticate as.
func (id *Identity) LocalPublicKey() string {
	pub, _ := id.LocalKey.PublicKey()
	return pub
}

// TrustPeer installs a peer's public bundle. Re-trusting a host replaces its
// bundle, which is how a peer's rotated identity is accepted; the names it
// had before (b.AKA, if b doesn't set them) are kept. A bundle with the keys
// of another trusted peer is refused (*SameKeysError): two hosts can't be
// one machine, and a renamed peer is re-trusted with RenamePeer.
func TrustPeer(dir string, b PeerBundle) error {
	if err := b.Validate(); err != nil {
		return err
	}
	peers, err := LoadPeers(dir)
	if err != nil {
		return err
	}
	var replaced *PeerBundle
	for _, p := range peers {
		if strings.EqualFold(p.Host, b.Host) {
			replaced = &p
			continue
		}
		if p.NKey == b.NKey || p.CertSHA == b.CertSHA {
			return &SameKeysError{Host: b.Host, Peer: p.Host}
		}
	}
	if replaced != nil && b.AKA == nil {
		b.AKA = replaced.AKA
	}
	return writePeer(dir, b, replaced)
}

// SameKeysError is TrustPeer refusing a bundle whose key or certificate pin
// another trusted peer already has.
type SameKeysError struct {
	Host string // the bundle's
	Peer string // the trusted peer with its keys
}

func (e *SameKeysError) Error() string {
	return fmt.Sprintf("the bundle for %s has the keys of trusted peer %s; if %s was renamed to %s, re-trust it with --renames %s", e.Host, e.Peer, e.Peer, e.Host, e.Peer)
}

// RenamePeer replaces the trusted peer old with b, the same machine under a
// new name (agent bus rename keeps a host's keys): b must carry old's key
// and certificate pin. b keeps old's names as AKA, so references to them
// still reach it. It returns the bundle installed and the one it replaced.
func RenamePeer(dir, old string, b PeerBundle) (installed, replaced PeerBundle, err error) {
	if err := b.Validate(); err != nil {
		return PeerBundle{}, PeerBundle{}, err
	}
	peers, err := LoadPeers(dir)
	if err != nil {
		return PeerBundle{}, PeerBundle{}, err
	}
	var prev *PeerBundle
	for _, p := range peers {
		if strings.EqualFold(p.Host, old) {
			prev = &p
		}
	}
	if prev == nil {
		return PeerBundle{}, PeerBundle{}, fmt.Errorf("no trusted peer %q to rename (%s)", old, trustedList(peers))
	}
	if b.NKey != prev.NKey || b.CertSHA != prev.CertSHA {
		return PeerBundle{}, PeerBundle{}, fmt.Errorf("the bundle for %s doesn't have trusted peer %s's keys (key %s, pin %s; %s has key %s, pin %s), so it isn't %s renamed: trust it without --renames, and agent bus untrust %s if that host is gone",
			b.Host, prev.Host, b.NKey, b.CertSHA, prev.Host, prev.NKey, prev.CertSHA, prev.Host, prev.Host)
	}
	for _, p := range peers {
		if p.Host != prev.Host && strings.EqualFold(p.Host, b.Host) {
			return PeerBundle{}, PeerBundle{}, fmt.Errorf("another trusted peer is already named %s: agent bus untrust %s first, if it's gone", p.Host, p.Host)
		}
	}
	var aka []string
	for _, n := range append(slices.Clone(prev.AKA), prev.Host) {
		if !strings.EqualFold(n, b.Host) && !slices.ContainsFunc(aka, func(a string) bool { return strings.EqualFold(a, n) }) {
			aka = append(aka, n)
		}
	}
	b.AKA = aka
	return b, *prev, writePeer(dir, b, prev)
}

// UntrustPeer removes the trusted peer named host (ignoring case) and
// returns its bundle.
func UntrustPeer(dir, host string) (PeerBundle, error) {
	peers, err := LoadPeers(dir)
	if err != nil {
		return PeerBundle{}, err
	}
	for _, p := range peers {
		if strings.EqualFold(p.Host, host) {
			if err := os.Remove(peerPath(dir, p.Host)); err != nil {
				return PeerBundle{}, err
			}
			return p, nil
		}
	}
	return PeerBundle{}, fmt.Errorf("no trusted peer %q (%s)", host, trustedList(peers))
}

func trustedList(peers []PeerBundle) string {
	if len(peers) == 0 {
		return "this host trusts no peers"
	}
	hosts := peerHosts(peers)
	slices.Sort(hosts)
	return "trusted: " + strings.Join(hosts, ", ")
}

func peerPath(dir, host string) string { return filepath.Join(dir, peersDir, host+".json") }

// writePeer installs b, replacing the bundle prev (nil: none) if it has
// another file name. The new bundle is written aside first and moved into
// place after prev is gone, so a case-insensitive file system, where
// prev's file may be b's, never loses it.
func writePeer(dir string, b PeerBundle, prev *PeerBundle) error {
	data, err := json.MarshalIndent(b, "", "  ")
	if err != nil {
		return err
	}
	path := peerPath(dir, b.Host)
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	tmp := path + ".tmp" // not *.json: LoadPeers skips it
	if err := os.WriteFile(tmp, append(data, '\n'), 0644); err != nil {
		return err
	}
	if prev != nil && prev.Host != b.Host {
		if err := os.Remove(peerPath(dir, prev.Host)); err != nil && !errors.Is(err, os.ErrNotExist) {
			os.Remove(tmp)
			return err
		}
	}
	return os.Rename(tmp, path)
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

// CheckRename says why the identity in dir, now old's, can't be renamed to
// new, or nil if it can: RenameIdentity's checks, which change nothing.
func CheckRename(dir, old, new string) error {
	if err := ValidateHost(new); err != nil {
		return err
	}
	if new == old {
		return fmt.Errorf("this host is already named %s", old)
	}
	if _, err := LoadIdentity(dir, old); err != nil {
		return err
	}
	peers, err := LoadPeers(dir)
	if err != nil {
		return err
	}
	for _, p := range peers {
		if p.Names(new) {
			return fmt.Errorf("trusted peer %s is (or was) named %s; pick another name", p.Host, new)
		}
	}
	return nil
}

// RenameIdentity binds the identity in dir, now old's, to the host name
// new. The keys stay: the nkey isn't tied to a name, and peers pin the
// certificate's public key, so a peer can check that the renamed host is
// the same machine (RenamePeer). The certificate is signed again with the
// same key under the new name, old is added to host.previous, and the host
// file, written last, commits the rename.
func RenameIdentity(dir, old, new string) (*Identity, error) {
	if err := CheckRename(dir, old, new); err != nil {
		return nil, err
	}

	keyPEM, err := os.ReadFile(filepath.Join(dir, tlsKeyFile))
	if err != nil {
		return nil, err
	}
	block, _ := pem.Decode(keyPEM)
	if block == nil {
		return nil, fmt.Errorf("reading %s: no PEM key", tlsKeyFile)
	}
	key, err := x509.ParseECPrivateKey(block.Bytes)
	if err != nil {
		return nil, fmt.Errorf("reading %s: %w", tlsKeyFile, err)
	}
	certPEM, _, err := selfSignedCert(new, key)
	if err != nil {
		return nil, err
	}

	prev := slices.DeleteFunc(PreviousHosts(dir), func(n string) bool {
		return strings.EqualFold(n, new) || strings.EqualFold(n, old)
	})
	prev = append(prev, old)

	for _, f := range []struct {
		name string
		data []byte
	}{
		{tlsCertFile, certPEM},
		{prevHostsFile, []byte(strings.Join(prev, "\n") + "\n")},
		{hostNameFile, []byte(new + "\n")}, // last: the rename takes effect here
	} {
		if err := writeFileAtomic(filepath.Join(dir, f.name), f.data, 0644); err != nil {
			return nil, err
		}
	}
	return LoadIdentity(dir, new)
}

// selfSignedCert creates a certificate for host with key (ECDSA P-256).
// Peers pin its public key, so its names and expiry aren't what makes it
// trusted; the long validity just avoids surprise expiry.
func selfSignedCert(host string, key *ecdsa.PrivateKey) (certPEM, keyPEM []byte, err error) {
	serial, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		return nil, nil, err
	}
	now := time.Now()
	tmpl := &x509.Certificate{
		SerialNumber:          serial,
		Subject:               pkix.Name{CommonName: "gomux bus " + host},
		DNSNames:              []string{host},
		NotBefore:             now.Add(-time.Hour),
		NotAfter:              now.AddDate(10, 0, 0),
		KeyUsage:              x509.KeyUsageDigitalSignature,
		ExtKeyUsage:           []x509.ExtKeyUsage{x509.ExtKeyUsageServerAuth, x509.ExtKeyUsageClientAuth},
		BasicConstraintsValid: true,
	}
	der, err := x509.CreateCertificate(rand.Reader, tmpl, tmpl, &key.PublicKey, key)
	if err != nil {
		return nil, nil, fmt.Errorf("creating TLS certificate: %w", err)
	}
	keyDER, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		return nil, nil, err
	}
	certPEM = pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	keyPEM = pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER})
	return certPEM, keyPEM, nil
}

// spkiSHA256 is the pin format nats-server checks (TLSPinnedCerts).
func spkiSHA256(cert *x509.Certificate) string {
	sum := sha256.Sum256(cert.RawSubjectPublicKeyInfo)
	return hex.EncodeToString(sum[:])
}

func writeFileExclusive(path string, data []byte, mode os.FileMode) error {
	f, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, mode)
	if err != nil {
		return fmt.Errorf("writing %s: %w", path, err)
	}
	if _, err := f.Write(data); err != nil {
		f.Close()
		return fmt.Errorf("writing %s: %w", path, err)
	}
	return f.Close()
}

// writeFileAtomic replaces path with data: written aside, then renamed over.
func writeFileAtomic(path string, data []byte, mode os.FileMode) error {
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, data, mode); err != nil {
		return fmt.Errorf("writing %s: %w", path, err)
	}
	if err := os.Rename(tmp, path); err != nil {
		os.Remove(tmp)
		return fmt.Errorf("writing %s: %w", path, err)
	}
	return nil
}
