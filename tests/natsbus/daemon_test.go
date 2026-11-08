package natsbus_test

import (
	"context"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/nats-io/nats.go"
	"github.com/tikimcfee/gomuxai/internal/await"
	"github.com/tikimcfee/gomuxai/internal/natsbus"
)

// busHost is one simulated machine: a bus dir with an identity.
type busHost struct {
	name string
	dir  string
	id   *natsbus.Identity
}

func newBusHost(t *testing.T, name string) *busHost {
	t.Helper()
	dir := t.TempDir()
	id, err := natsbus.InitIdentity(dir, name)
	if err != nil {
		t.Fatalf("init %s: %v", name, err)
	}
	return &busHost{name: name, dir: dir, id: id}
}

func (h *busHost) start(t *testing.T, leafListen string, remoteOps []string) *natsbus.Daemon {
	t.Helper()
	d, err := natsbus.StartDaemon(natsbus.DaemonConfig{
		Dir: h.dir, Host: h.name, ClientPort: -1, LeafListen: leafListen, RemoteOps: remoteOps,
	})
	if err != nil {
		t.Fatalf("start %s: %v", h.name, err)
	}
	t.Cleanup(d.Shutdown)
	return d
}

func (h *busHost) connect(t *testing.T) *nats.Conn {
	t.Helper()
	nc, err := natsbus.ConnectLocal(h.dir, h.name)
	if err != nil {
		t.Fatalf("connect %s: %v", h.name, err)
	}
	t.Cleanup(nc.Close)
	return nc
}

func trust(t *testing.T, h *busHost, b natsbus.PeerBundle) {
	t.Helper()
	if err := natsbus.TrustPeer(h.dir, b); err != nil {
		t.Fatalf("%s trusting %s: %v", h.name, b.Host, err)
	}
}

// linkedPair starts a hub and a leaf that trust each other and waits for the
// link. Both serve every op locally; remote ops follow each host's config.
func linkedPair(t *testing.T, hubOps, leafOps []string) (hub, leaf *busHost, hubConn, leafConn *nats.Conn) {
	t.Helper()
	hub, leaf = newBusHost(t, "hub"), newBusHost(t, "leaf")
	trust(t, hub, leaf.id.PublicBundle(""))
	hubD := hub.start(t, "127.0.0.1:-1", hubOps)
	trust(t, leaf, hub.id.PublicBundle("tls://"+hubD.LeafAddr()))
	leaf.start(t, "", leafOps)
	t.Cleanup(func() {
		if t.Failed() {
			for _, h := range []*busHost{hub, leaf} {
				log, _ := os.ReadFile(filepath.Join(h.dir, "bus.log"))
				t.Logf("%s bus.log:\n%s", h.name, log)
			}
		}
	})
	waitLinked(t, hubD, 1)
	return hub, leaf, hub.connect(t), leaf.connect(t)
}

func waitLinked(t *testing.T, d *natsbus.Daemon, n int) {
	t.Helper()
	await.Until(t, fmt.Sprintf("%d linked peers", n), func() (string, bool) {
		have := d.LinkedPeers()
		return fmt.Sprintf("%d linked peers", have), have == n
	})
}

// waitLog waits for h's bus.log to show want.
func waitLog(t *testing.T, h *busHost, want string) {
	t.Helper()
	await.Until(t, fmt.Sprintf("%s's bus.log to show %q", h.name, want), func() (string, bool) {
		log, _ := os.ReadFile(filepath.Join(h.dir, "bus.log"))
		return string(log), strings.Contains(string(log), want)
	})
}

// waitInterest returns once a message pub publishes as pubHost reaches sub.
// Interest crosses links asynchronously but in the order it was made, so
// then every subscription sub made before the call has reached pub's
// server too, or been refused on the way: a check that something did not
// cross can't pass just because the interest hadn't arrived yet.
func waitInterest(t *testing.T, sub, pub *nats.Conn, pubHost string) {
	t.Helper()
	subj := natsbus.SubjectStatus(pubHost, "interest-probe")
	probe, err := sub.SubscribeSync(subj)
	if err != nil {
		t.Fatal(err)
	}
	defer probe.Unsubscribe()
	sub.Flush()
	await.Until(t, "a probe on "+subj+" to cross", func() (string, bool) {
		pub.Publish(subj, []byte("probe"))
		pub.Flush()
		_, err := probe.NextMsg(50 * time.Millisecond)
		return fmt.Sprint(err), err == nil
	})
}

// serve answers every op for host on nc, echoing which op ran.
func serve(t *testing.T, nc *nats.Conn, host string) {
	t.Helper()
	answerAll(t, nc, host, func(m *nats.Msg) string { return "ran " + m.Subject })
}

// serveSecret answers every op for host with secret.
func serveSecret(t *testing.T, nc *nats.Conn, host, secret string) {
	t.Helper()
	answerAll(t, nc, host, func(*nats.Msg) string { return secret })
}

func answerAll(t *testing.T, nc *nats.Conn, host string, answer func(*nats.Msg) string) {
	t.Helper()
	sub, err := nc.Subscribe(natsbus.SubjectRPC(host, "*"), func(m *nats.Msg) {
		m.Respond([]byte(answer(m)))
	})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { sub.Unsubscribe() })
	nc.Flush()
}

// settleLink returns once the subscriptions each side has made have crossed
// the link, or been refused on the way, so a refused request is refused
// and not just early.
func settleLink(t *testing.T, hubConn, leafConn *nats.Conn) {
	t.Helper()
	waitInterest(t, hubConn, leafConn, "leaf")
	waitInterest(t, leafConn, hubConn, "hub")
}

// requestOnce asks op on host once, for a request that must be refused:
// after settleLink, no answer is the answer.
func requestOnce(nc *nats.Conn, host, op string) (string, error) {
	msg, err := nc.Request(natsbus.SubjectRPC(host, op), []byte("{}"), time.Second)
	if err != nil {
		return "", err
	}
	return string(msg.Data), nil
}

// request asks op on host, retrying while interest propagates: for a
// request that must be answered.
func request(nc *nats.Conn, host, op string) (string, error) {
	var data string
	var lastErr error
	await.Poll(5*time.Second, func() (string, bool) {
		msg, err := nc.Request(natsbus.SubjectRPC(host, op), []byte("{}"), 200*time.Millisecond)
		if lastErr = err; err == nil {
			data = string(msg.Data)
		}
		return fmt.Sprint(err), err == nil
	})
	return data, lastErr
}

func TestBus_RPCCrossesLinkBothWays(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t, nil, nil)
	serve(t, hubConn, "hub")
	serve(t, leafConn, "leaf")

	if got, err := request(leafConn, "hub", natsbus.OpStatus); err != nil || got != "ran gomux.hub.rpc.status" {
		t.Errorf("leaf -> hub status: %q, %v", got, err)
	}
	if got, err := request(hubConn, "leaf", natsbus.OpSend); err != nil || got != "ran gomux.leaf.rpc.send" {
		t.Errorf("hub -> leaf send: %q, %v", got, err)
	}
}

func TestBus_OpsNotAllowedRemotelyAreBlockedBothWays(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t, nil, nil) // defaults: keys is local-only
	serve(t, hubConn, "hub")
	serve(t, leafConn, "leaf")
	settleLink(t, hubConn, leafConn)

	if got, err := requestOnce(leafConn, "hub", natsbus.OpKeys); err == nil {
		t.Errorf("leaf ran keys on hub: %q", got)
	}
	if got, err := requestOnce(hubConn, "leaf", natsbus.OpKeys); err == nil {
		t.Errorf("hub ran keys on leaf: %q", got)
	}
	// Still allowed locally.
	if got, err := request(hubConn, "hub", natsbus.OpKeys); err != nil {
		t.Errorf("local keys on hub failed: %q, %v", got, err)
	}
}

// Spawn starts processes on the serving host, so like keys it is local-only
// unless a host lists it.
func TestBus_SpawnIsLocalOnlyByDefault(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t, nil, nil)
	serve(t, hubConn, "hub")
	serve(t, leafConn, "leaf")
	settleLink(t, hubConn, leafConn)

	if got, err := requestOnce(leafConn, "hub", natsbus.OpSpawn); err == nil {
		t.Errorf("leaf ran spawn on hub: %q", got)
	}
	if got, err := requestOnce(hubConn, "leaf", natsbus.OpSpawn); err == nil {
		t.Errorf("hub ran spawn on leaf: %q", got)
	}
	// Still allowed locally.
	if got, err := request(hubConn, "hub", natsbus.OpSpawn); err != nil || got != "ran gomux.hub.rpc.spawn" {
		t.Errorf("local spawn on hub: %q, %v", got, err)
	}
	if got, err := request(leafConn, "leaf", natsbus.OpSpawn); err != nil || got != "ran gomux.leaf.rpc.spawn" {
		t.Errorf("local spawn on leaf: %q, %v", got, err)
	}
}

// A host that lists spawn serves it to peers; one that doesn't still refuses.
func TestBus_SpawnServedWhenAHostAllowsIt(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t,
		nil, // hub: defaults, no spawn
		append(slices.Clone(natsbus.DefaultRemoteOps), natsbus.OpSpawn)) // leaf: also spawn
	serve(t, hubConn, "hub")
	serve(t, leafConn, "leaf")
	settleLink(t, hubConn, leafConn)

	if got, err := request(hubConn, "leaf", natsbus.OpSpawn); err != nil || got != "ran gomux.leaf.rpc.spawn" {
		t.Errorf("leaf allows spawn remotely: %q, %v", got, err)
	}
	if got, err := requestOnce(leafConn, "hub", natsbus.OpSpawn); err == nil {
		t.Errorf("leaf ran spawn on hub, which doesn't allow it: %q", got)
	}
	if got, err := request(hubConn, "hub", natsbus.OpSpawn); err != nil {
		t.Errorf("local spawn on hub: %q, %v", got, err)
	}
}

// A spawn through two linked buses: the leaf's RPCClient reaches the hub's
// handler, which sees the request as sent.
func TestBus_SpawnRoundTripsAcrossTheLink(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t,
		append(slices.Clone(natsbus.DefaultRemoteOps), natsbus.OpSpawn), nil)
	fake := &fakeRPC{slow: make(chan struct{})}
	stop, err := natsbus.ServeRPC(hubConn, "hub", fake, natsbus.AllOps)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(stop)
	client := natsbus.NewRPCClient(leafConn, 2*time.Second)

	req := natsbus.SpawnRequest{Backend: "claude", Cwd: "/srv/project", TaskFile: "tasks/build.md", SpawnedBy: "lead@leaf"}
	var resp *natsbus.SpawnResponse
	await.Poll(5*time.Second, func() (string, bool) { // interest propagates across the link
		resp, err = client.Spawn(context.Background(), natsbus.AgentRef{Name: "helper", Host: "hub"}, req)
		return fmt.Sprint(err), err == nil || !strings.Contains(err.Error(), "no bus answering")
	})
	if err != nil || resp.Name != "helper" || resp.Session != "agent-helper" || resp.SpawnedBy != "lead@leaf" || resp.Cwd != "/srv/project" {
		t.Fatalf("Spawn = %+v, %v", resp, err)
	}
	req.Agent = "helper"
	if got := fake.last(); got != req {
		t.Errorf("hub's handler got %+v, want %+v", got, req)
	}
}

func TestBus_RemoteOpsAreConfigurablePerHost(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t,
		[]string{natsbus.OpStatus},                 // hub: status only
		[]string{natsbus.OpStatus, natsbus.OpKeys}) // leaf: also allows keys
	serve(t, hubConn, "hub")
	serve(t, leafConn, "leaf")
	settleLink(t, hubConn, leafConn)

	if _, err := requestOnce(leafConn, "hub", natsbus.OpSend); err == nil {
		t.Error("hub served send to a peer despite allowing only status")
	}
	if _, err := request(hubConn, "leaf", natsbus.OpKeys); err != nil {
		t.Errorf("leaf allows keys remotely, but it failed: %v", err)
	}
}

func TestBus_EventsCrossLinkButPeersCantSpeakForOthers(t *testing.T) {
	_, _, hubConn, leafConn := linkedPair(t, nil, nil)

	hubSub, _ := hubConn.SubscribeSync(natsbus.SubjectAllStatus("*"))
	leafSub, _ := leafConn.SubscribeSync(natsbus.SubjectAllStatus("*"))
	hubConn.Flush()
	leafConn.Flush()
	waitInterest(t, hubConn, leafConn, "leaf")
	waitInterest(t, leafConn, hubConn, "hub")
	drain := func(sub *nats.Subscription) {
		for _, err := sub.NextMsg(0); err == nil; _, err = sub.NextMsg(0) {
		}
	}
	drain(hubSub) // the probes
	drain(leafSub)

	// Genuine events cross in both directions.
	leafConn.Publish(natsbus.SubjectStatus("leaf", "a"), []byte("leaf-real"))
	hubConn.Publish(natsbus.SubjectStatus("hub", "b"), []byte("hub-real"))
	// A peer may not speak for the hub, or for a host it doesn't own.
	leafConn.Publish(natsbus.SubjectStatus("hub", "x"), []byte("leaf-as-hub"))
	leafConn.Publish(natsbus.SubjectStatus("other", "y"), []byte("leaf-as-other"))
	// Each side's last event: messages from one connection arrive in
	// order, so once it has, any earlier one that was let through has too.
	leafConn.Publish(natsbus.SubjectStatus("leaf", "end"), []byte("leaf-end"))
	hubConn.Publish(natsbus.SubjectStatus("hub", "end"), []byte("hub-end"))
	leafConn.Flush()
	hubConn.Flush()

	got := func(sub *nats.Subscription, end string) map[string]bool {
		t.Helper()
		seen := map[string]bool{}
		for !seen[end] {
			msg, err := sub.NextMsg(await.Timeout)
			if err != nil {
				t.Fatalf("waiting for %s: %v; saw %v", end, err, seen)
			}
			seen[string(msg.Data)] = true
		}
		return seen
	}
	hubSeen, leafSeen := got(hubSub, "leaf-end"), got(leafSub, "hub-end")

	if !hubSeen["leaf-real"] || !leafSeen["hub-real"] {
		t.Errorf("genuine events missing: hub saw %v, leaf saw %v", hubSeen, leafSeen)
	}
	if hubSeen["leaf-as-hub"] {
		t.Error("hub accepted an event a peer published as the hub")
	}
	// The hub knows only "leaf", so it can't name "other" in a deny; this
	// documents that unknown-host events pass. Consumers ignore hosts they
	// don't trust, and events are hints confirmed by a status check.
	if !hubSeen["leaf-as-other"] {
		t.Log("note: events for unknown hosts are now filtered too")
	}
}

func TestBus_UntrustedPeersCantLink(t *testing.T) {
	tests := []struct {
		name   string
		tamper func(leafBundle *natsbus.PeerBundle, impostor *busHost)
	}{
		{"wrong certificate", func(b *natsbus.PeerBundle, imp *busHost) { b.CertSHA = imp.id.CertSHA }},
		{"wrong nkey", func(b *natsbus.PeerBundle, imp *busHost) { b.NKey = imp.id.PublicBundle("").NKey }},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			hub, leaf := newBusHost(t, "hub"), newBusHost(t, "leaf")
			impostor := newBusHost(t, "leaf")

			// The hub trusts a "leaf" whose credentials don't match the
			// machine that will actually connect.
			bundle := leaf.id.PublicBundle("")
			tt.tamper(&bundle, impostor)
			trust(t, hub, bundle)
			hubD := hub.start(t, "127.0.0.1:-1", nil)
			trust(t, leaf, hub.id.PublicBundle("tls://"+hubD.LeafAddr()))
			leaf.start(t, "", nil)

			// The hub has turned an attempt away (it logs each one).
			waitLog(t, hub, "Leafnode connection closed")
			if n := hubD.LinkedPeers(); n != 0 {
				t.Errorf("hub accepted %d link(s) from an untrusted peer", n)
			}
		})
	}
}

func TestBus_LeafRejectsAnImpostorHub(t *testing.T) {
	hub, leaf := newBusHost(t, "hub"), newBusHost(t, "leaf")
	impostor := newBusHost(t, "hub")
	trust(t, hub, leaf.id.PublicBundle(""))
	hubD := hub.start(t, "127.0.0.1:-1", nil)

	// The leaf pins a different key than the hub actually presents.
	bundle := hub.id.PublicBundle("tls://" + hubD.LeafAddr())
	bundle.CertSHA = impostor.id.CertSHA
	trust(t, leaf, bundle)
	leaf.start(t, "", nil)

	// The leaf has turned an attempt away (the hub logs each one closing).
	waitLog(t, hub, "Leafnode connection closed")
	if n := hubD.LinkedPeers(); n != 0 {
		t.Errorf("leaf linked to a hub whose certificate doesn't match its pin")
	}
}

func TestBus_LocalClientsMustAuthenticate(t *testing.T) {
	h := newBusHost(t, "solo")
	d := h.start(t, "", nil)

	if !strings.Contains(d.ClientURL(), "://127.0.0.1:") {
		t.Errorf("client listener should be loopback-only, got %s", d.ClientURL())
	}
	if nc, err := nats.Connect(d.ClientURL(), nats.Timeout(time.Second)); err == nil {
		nc.Close()
		t.Error("unauthenticated client connected")
	}
	other := newBusHost(t, "solo") // someone else's keys
	if nc, err := natsbus.ConnectLocal(other.dir, "solo"); err == nil {
		nc.Close()
		t.Error("client with the wrong local key connected")
	}
	h.connect(t) // the right key works
}

func TestBus_PeersCantConnectAsLocalClients(t *testing.T) {
	hub, leaf := newBusHost(t, "hub"), newBusHost(t, "leaf")
	trust(t, hub, leaf.id.PublicBundle(""))
	d := hub.start(t, "", nil)

	// A peer's host key is only valid for leaf links, not client connections.
	if nc, err := nats.Connect(d.ClientURL(), nats.Nkey(leaf.id.PublicBundle("").NKey, leaf.id.HostKey.Sign), nats.Timeout(time.Second)); err == nil {
		nc.Close()
		t.Error("peer host key accepted on the local client port")
	}
}

func TestConnectLocal_NoBus(t *testing.T) {
	h := newBusHost(t, "idle")
	start := time.Now()
	if _, err := natsbus.ConnectLocal(h.dir, "idle"); !errors.Is(err, natsbus.ErrNoBus) {
		t.Errorf("err = %v, want ErrNoBus", err)
	}

	// A stale URL file (bus died without cleanup) also means no bus.
	os.WriteFile(filepath.Join(h.dir, "bus.url"), []byte("nats://127.0.0.1:1"), 0644)
	if _, err := natsbus.ConnectLocal(h.dir, "idle"); !errors.Is(err, natsbus.ErrNoBus) {
		t.Errorf("stale URL: err = %v, want ErrNoBus", err)
	}
	if d := time.Since(start); d > time.Second {
		t.Errorf("no-bus detection took %v; hooks need it fast", d)
	}
}

func TestIdentity_RefusesOverwriteAndLooseSecrets(t *testing.T) {
	h := newBusHost(t, "box")
	if _, err := natsbus.InitIdentity(h.dir, "box"); err == nil {
		t.Error("InitIdentity replaced an existing identity")
	}
	for _, f := range []string{"host.nk", "local.nk", "tls.key"} {
		info, _ := os.Stat(filepath.Join(h.dir, f))
		if perm := info.Mode().Perm(); perm != 0600 {
			t.Errorf("%s mode %o, want 600", f, perm)
		}
	}
	os.Chmod(filepath.Join(h.dir, "host.nk"), 0644)
	if _, err := natsbus.LoadIdentity(h.dir, "box"); err == nil || !strings.Contains(err.Error(), "readable by other users") {
		t.Errorf("loose secret permissions not refused: %v", err)
	}
}

func TestTrustPeer_Validates(t *testing.T) {
	h := newBusHost(t, "box")
	good := newBusHost(t, "peer").id.PublicBundle("tls://peer:7422")
	bad := []natsbus.PeerBundle{
		{Host: "a.b", NKey: good.NKey, CertSHA: good.CertSHA},
		{Host: "peer", NKey: "not-a-key", CertSHA: good.CertSHA},
		{Host: "peer", NKey: good.NKey, CertSHA: "abc"},
		{Host: "peer", NKey: good.NKey, CertSHA: good.CertSHA, URL: "nats://peer:7422"},
	}
	for i, b := range bad {
		if err := natsbus.TrustPeer(h.dir, b); err == nil {
			t.Errorf("bad bundle %d accepted: %+v", i, b)
		}
	}
	if err := natsbus.TrustPeer(h.dir, good); err != nil {
		t.Fatalf("good bundle rejected: %v", err)
	}
	peers, err := natsbus.LoadPeers(h.dir)
	if err != nil || len(peers) != 1 || peers[0] != good {
		t.Errorf("LoadPeers = %+v, %v", peers, err)
	}
}

// An identity is bound to its host name: loading it as another host would
// sign links and publish events under a name peers don't expect.
func TestIdentity_BoundToItsHostName(t *testing.T) {
	h := newBusHost(t, "box")
	if _, err := natsbus.LoadIdentity(h.dir, "other"); err == nil || !strings.Contains(err.Error(), `for host "box"`) {
		t.Errorf("loading as another host: err = %v", err)
	}
	if _, err := natsbus.ConnectLocal(h.dir, "other"); errors.Is(err, natsbus.ErrNoBus) {
		// No bus is running, so ConnectLocal reports that first; fine.
	}
	if _, err := natsbus.LoadIdentity(h.dir, "box"); err != nil {
		t.Errorf("loading as its own host: %v", err)
	}
}
