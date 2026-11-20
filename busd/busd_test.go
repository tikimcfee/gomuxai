package main

import (
	"bufio"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/x509"
	"crypto/x509/pkix"
	"encoding/json"
	"encoding/pem"
	"io"
	"math/big"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	"github.com/nats-io/nkeys"
)

func initTestIdentity(t *testing.T, dir, host string) PeerBundle {
	t.Helper()
	hostKey, err := nkeys.CreateUser()
	if err != nil {
		t.Fatal(err)
	}
	localKey, err := nkeys.CreateUser()
	if err != nil {
		t.Fatal(err)
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	serial, err := rand.Int(rand.Reader, new(big.Int).Lsh(big.NewInt(1), 128))
	if err != nil {
		t.Fatal(err)
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
		t.Fatal(err)
	}
	keyDER, err := x509.MarshalECPrivateKey(key)
	if err != nil {
		t.Fatal(err)
	}
	certPEM := pem.EncodeToMemory(&pem.Block{Type: "CERTIFICATE", Bytes: der})
	keyPEM := pem.EncodeToMemory(&pem.Block{Type: "EC PRIVATE KEY", Bytes: keyDER})
	hostSeed, _ := hostKey.Seed()
	localSeed, _ := localKey.Seed()

	if err := os.MkdirAll(filepath.Join(dir, peersDir), 0o700); err != nil {
		t.Fatal(err)
	}
	for _, f := range []struct {
		name string
		data []byte
		mode os.FileMode
	}{
		{hostNameFile, []byte(host + "\n"), 0o644},
		{tlsCertFile, certPEM, 0o644},
		{tlsKeyFile, keyPEM, 0o600},
		{localSeedFile, localSeed, 0o600},
		{hostSeedFile, hostSeed, 0o600},
	} {
		if err := os.WriteFile(filepath.Join(dir, f.name), f.data, f.mode); err != nil {
			t.Fatal(err)
		}
	}

	id, err := LoadIdentity(dir, host)
	if err != nil {
		t.Fatal(err)
	}
	pub, _ := id.HostKey.PublicKey()
	return PeerBundle{Host: host, NKey: pub, CertSHA: id.CertSHA}
}

func writePeerBundle(t *testing.T, dir string, b PeerBundle) {
	t.Helper()
	data, err := json.MarshalIndent(b, "", "  ")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dir, peersDir, b.Host+".json"), append(data, '\n'), 0o644); err != nil {
		t.Fatal(err)
	}
}

type runningBusd struct {
	cmd   *exec.Cmd
	stdin io.WriteCloser
	lines *bufio.Reader
	ready readyReply
}

func startBusd(t *testing.T, bin string, args ...string) *runningBusd {
	t.Helper()
	cmd := exec.Command(bin, args...)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		t.Fatal(err)
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	r := bufio.NewReader(stdout)
	line, err := r.ReadBytes('\n')
	if err != nil {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		t.Fatalf("reading ready line: %v", err)
	}
	var ready readyReply
	if err := json.Unmarshal(line, &ready); err != nil {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
		t.Fatalf("parsing ready line %q: %v", line, err)
	}
	rb := &runningBusd{cmd: cmd, stdin: stdin, lines: r, ready: ready}
	t.Cleanup(rb.stop)
	return rb
}

func (rb *runningBusd) links(t *testing.T) int {
	t.Helper()
	if _, err := rb.stdin.Write([]byte("links\n")); err != nil {
		t.Fatalf("writing links query: %v", err)
	}
	line, err := rb.lines.ReadBytes('\n')
	if err != nil {
		t.Fatalf("reading links reply: %v", err)
	}
	var resp linksReply
	if err := json.Unmarshal(line, &resp); err != nil {
		t.Fatalf("parsing links reply %q: %v", line, err)
	}
	return resp.Linked
}

func (rb *runningBusd) stop() {
	if rb.cmd.ProcessState != nil {
		return
	}
	_ = rb.cmd.Process.Signal(syscall.SIGTERM)
	_ = rb.stdin.Close()
	_ = rb.cmd.Wait()
}

func TestAgentBusdLifecycleAndLinks(t *testing.T) {
	tmpRoot := "/var/tmp"
	if _, err := os.Stat(tmpRoot); err != nil {
		tmpRoot = t.TempDir()
	}
	workDir, err := os.MkdirTemp(tmpRoot, "busd-test-*")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(workDir) })

	bin := filepath.Join(workDir, "agent-busd")
	build := exec.Command("go", "build", "-trimpath", "-o", bin, ".")
	build.Env = append(os.Environ(), "TMPDIR="+workDir)
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("building agent-busd: %v\n%s", err, out)
	}

	hubDir := filepath.Join(workDir, "hub")
	leafDir := filepath.Join(workDir, "leaf")
	hubBundle := initTestIdentity(t, hubDir, "hub")
	leafBundle := initTestIdentity(t, leafDir, "leaf")
	writePeerBundle(t, hubDir, leafBundle)

	hub := startBusd(t, bin, "--dir", hubDir, "--host", "hub", "--client-port", "-1", "--listen", "127.0.0.1:-1")
	if hub.ready.ClientURL == "" || hub.ready.LeafAddr == "" {
		t.Fatalf("unexpected ready payload: %+v", hub.ready)
	}
	urlBytes, err := os.ReadFile(filepath.Join(hubDir, busURLFile))
	if err != nil || string(urlBytes) != hub.ready.ClientURL {
		t.Fatalf("bus.url = %q, %v; want %q", urlBytes, err, hub.ready.ClientURL)
	}
	if n := hub.links(t); n != 0 {
		t.Fatalf("linked = %d, want 0", n)
	}

	hubBundle.URL = "tls://" + hub.ready.LeafAddr
	writePeerBundle(t, leafDir, hubBundle)
	leaf := startBusd(t, bin, "--dir", leafDir, "--host", "leaf", "--client-port", "-1", "--remote-ops", "")
	if leaf.ready.LeafAddr != "" {
		t.Fatalf("leaf should have empty leaf_addr, got %q", leaf.ready.LeafAddr)
	}

	deadline := time.Now().Add(5 * time.Second)
	for hub.links(t) != 1 {
		if time.Now().After(deadline) {
			t.Fatal("timed out waiting for leaf to link to hub")
		}
		time.Sleep(20 * time.Millisecond)
	}

	leaf.stop()
	if _, err := os.Stat(filepath.Join(leafDir, busURLFile)); !os.IsNotExist(err) {
		t.Errorf("leaf bus.url still exists after SIGTERM: %v", err)
	}
	hub.stop()
	if _, err := os.Stat(filepath.Join(hubDir, busURLFile)); !os.IsNotExist(err) {
		t.Errorf("hub bus.url still exists after SIGTERM: %v", err)
	}
}

func TestAgentBusdNoStdioModeAndErrorFile(t *testing.T) {
	tmpRoot := "/var/tmp"
	if _, err := os.Stat(tmpRoot); err != nil {
		tmpRoot = t.TempDir()
	}
	workDir, err := os.MkdirTemp(tmpRoot, "busd-nostdio-test-*")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(workDir) })

	bin := filepath.Join(workDir, "agent-busd")
	build := exec.Command("go", "build", "-trimpath", "-o", bin, ".")
	build.Env = append(os.Environ(), "TMPDIR="+workDir)
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("building agent-busd: %v\n%s", err, out)
	}

	hubDir := filepath.Join(workDir, "hub")
	leafDir := filepath.Join(workDir, "leaf")
	hubBundle := initTestIdentity(t, hubDir, "hub")
	leafBundle := initTestIdentity(t, leafDir, "leaf")
	writePeerBundle(t, hubDir, leafBundle)

	// Startup failure in --no-stdio mode writes busd.err.
	bad := exec.Command(bin, "--dir", hubDir, "--host", "wronghost", "--no-stdio")
	if err := bad.Run(); err == nil {
		t.Fatal("expected mismatched host to fail")
	}
	errBytes, err := os.ReadFile(filepath.Join(hubDir, busErrFile))
	if err != nil || len(errBytes) == 0 {
		t.Fatalf("expected non-empty busd.err, got %q, %v", errBytes, err)
	}

	// Start hub in --no-stdio mode with no pipes.
	hubCmd := exec.Command(bin, "--dir", hubDir, "--host", "hub", "--client-port", "-1", "--listen", "127.0.0.1:-1", "--no-stdio")
	if err := hubCmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if hubCmd.ProcessState == nil {
			_ = hubCmd.Process.Signal(syscall.SIGTERM)
			_ = hubCmd.Wait()
		}
	})

	var ready readyReply
	deadline := time.Now().Add(5 * time.Second)
	for {
		if b, err := os.ReadFile(filepath.Join(hubDir, busReadyFile)); err == nil {
			if json.Unmarshal(b, &ready) == nil && ready.ClientURL != "" {
				break
			}
		}
		if time.Now().After(deadline) {
			t.Fatal("timed out waiting for busd.ready")
		}
		time.Sleep(10 * time.Millisecond)
	}
	if ready.PID != hubCmd.Process.Pid {
		t.Fatalf("ready.PID = %d, want %d", ready.PID, hubCmd.Process.Pid)
	}

	hubBundle.URL = "tls://" + ready.LeafAddr
	writePeerBundle(t, leafDir, hubBundle)
	leafCmd := exec.Command(bin, "--dir", leafDir, "--host", "leaf", "--client-port", "-1", "--no-stdio")
	if err := leafCmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if leafCmd.ProcessState == nil {
			_ = leafCmd.Process.Signal(syscall.SIGTERM)
			_ = leafCmd.Wait()
		}
	})

	deadline = time.Now().Add(5 * time.Second)
	for {
		if b, err := os.ReadFile(filepath.Join(hubDir, busLinksFile)); err == nil {
			var lr linksReply
			if json.Unmarshal(b, &lr) == nil && lr.Linked == 1 {
				break
			}
		}
		if time.Now().After(deadline) {
			t.Fatal("timed out waiting for busd.links == 1")
		}
		time.Sleep(20 * time.Millisecond)
	}

	_ = leafCmd.Process.Signal(syscall.SIGTERM)
	_ = leafCmd.Wait()
	_ = hubCmd.Process.Signal(syscall.SIGTERM)
	_ = hubCmd.Wait()

	for _, name := range []string{busURLFile, busReadyFile, busLinksFile} {
		if _, err := os.Stat(filepath.Join(hubDir, name)); !os.IsNotExist(err) {
			t.Errorf("hub %s still exists after SIGTERM: %v", name, err)
		}
	}
}

func TestAgentBusdExitsOnStdinEOFAndParentDeath(t *testing.T) {
	tmpRoot := "/var/tmp"
	if _, err := os.Stat(tmpRoot); err != nil {
		tmpRoot = t.TempDir()
	}
	workDir, err := os.MkdirTemp(tmpRoot, "busd-eof-test-*")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { os.RemoveAll(workDir) })

	bin := filepath.Join(workDir, "agent-busd")
	build := exec.Command("go", "build", "-trimpath", "-o", bin, ".")
	build.Env = append(os.Environ(), "TMPDIR="+workDir)
	if out, err := build.CombinedOutput(); err != nil {
		t.Fatalf("building agent-busd: %v\n%s", err, out)
	}

	// 1. Closing stdin in piped mode causes busd to shut down cleanly and remove bus.url.
	eofDir := filepath.Join(workDir, "eof")
	initTestIdentity(t, eofDir, "eofbox")
	rb := startBusd(t, bin, "--dir", eofDir, "--host", "eofbox", "--client-port", "-1")
	_ = rb.stdin.Close()

	done := make(chan error, 1)
	go func() { done <- rb.cmd.Wait() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("busd exited with error on stdin EOF: %v", err)
		}
	case <-time.After(5 * time.Second):
		_ = rb.cmd.Process.Kill()
		t.Fatal("busd did not exit within 5s after stdin EOF")
	}
	if _, err := os.Stat(filepath.Join(eofDir, busURLFile)); !os.IsNotExist(err) {
		t.Fatalf("bus.url still present after stdin EOF: %v", err)
	}

	// 2. In --no-stdio mode, if the parent process is SIGKILLed, busd detects
	// reparenting (os.Getppid() != parent) and exits cleanly, removing bus.url.
	deadParentDir := filepath.Join(workDir, "deadparent")
	initTestIdentity(t, deadParentDir, "orphan")
	shCmd := exec.Command("sh", "-c", `"$1" --dir "$2" --host orphan --client-port -1 --no-stdio >/dev/null 2>&1 & sleep 60`, "sh", bin, deadParentDir)
	if err := shCmd.Start(); err != nil {
		t.Fatal(err)
	}
	var ready readyReply
	deadline := time.Now().Add(5 * time.Second)
	for {
		if b, err := os.ReadFile(filepath.Join(deadParentDir, busReadyFile)); err == nil {
			if json.Unmarshal(b, &ready) == nil && ready.PID > 0 {
				break
			}
		}
		if time.Now().After(deadline) {
			_ = shCmd.Process.Kill()
			_ = shCmd.Wait()
			t.Fatal("timed out waiting for busd.ready")
		}
		time.Sleep(10 * time.Millisecond)
	}
	// SIGKILL the parent shell so agent-busd is reparented.
	_ = shCmd.Process.Kill()
	_ = shCmd.Wait()

	deadline = time.Now().Add(5 * time.Second)
	for {
		_, urlErr := os.Stat(filepath.Join(deadParentDir, busURLFile))
		procErr := syscall.Kill(ready.PID, 0)
		if os.IsNotExist(urlErr) && procErr != nil {
			break
		}
		if time.Now().After(deadline) {
			_ = syscall.Kill(ready.PID, syscall.SIGKILL)
			t.Fatalf("orphan busd (pid %d) did not exit and remove bus.url: urlErr=%v procErr=%v", ready.PID, urlErr, procErr)
		}
		time.Sleep(20 * time.Millisecond)
	}
}
