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
	"strings"
	"time"

	"github.com/nats-io/nkeys"
)

// A host's bus identity lives in its bus dir (.gomuxai/bus):
//
//	host       the host name the identity was created for
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
	return nil
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
	certPEM, keyPEM, err := selfSignedCert(host)
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
// bundle, which is how a peer's rotated identity is accepted.
func TrustPeer(dir string, b PeerBundle) error {
	if err := b.Validate(); err != nil {
		return err
	}
	data, err := json.MarshalIndent(b, "", "  ")
	if err != nil {
		return err
	}
	path := filepath.Join(dir, peersDir, b.Host+".json")
	if err := os.MkdirAll(filepath.Dir(path), 0700); err != nil {
		return err
	}
	return os.WriteFile(path, append(data, '\n'), 0644)
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

// selfSignedCert creates an ECDSA P-256 certificate for host. Peers pin its
// public key, so its names and expiry aren't what makes it trusted; the long
// validity just avoids surprise expiry.
func selfSignedCert(host string) (certPEM, keyPEM []byte, err error) {
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		return nil, nil, fmt.Errorf("generating TLS key: %w", err)
	}
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
